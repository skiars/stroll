#!/usr/bin/env bash
# Build a native macOS application bundle and archive it for installation.
set -euo pipefail

case "${1:-}" in
  -h|--help)
    echo 'Usage: scripts/package-macos.sh'
    echo 'Build a native Stroll.app ZIP under the Cargo target directory (macos/).'
    echo 'Create or reuse a per-user, self-signed identity for local installation.'
    exit 0 ;;
esac
if [[ $# -ne 0 ]]; then
  echo "Unknown argument: $1" >&2
  exit 1
fi
if [[ $(uname -s) != Darwin ]]; then
  echo 'Run this script on macOS.' >&2
  exit 1
fi

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo"
target=$(rustc -vV | sed -n 's/^host: //p')
case "$target" in
  aarch64-apple-darwin) arch=arm64 ;;
  x86_64-apple-darwin) arch=x86_64 ;;
  *) echo "Unsupported Rust host: $target" >&2; exit 1 ;;
esac

metadata=$(cargo metadata --locked --no-deps --format-version 1)
version=$(plutil -extract packages.0.version raw -o - - <<< "$metadata")
target_dir=$(plutil -extract target_directory raw -o - - <<< "$metadata")
bins="$target_dir/$target/release"
output="$target_dir/macos"
archive="Stroll-$version-$arch.zip"

cargo build --release --locked --target "$target" --bin stroll

mkdir -p "$output"
stage=$(mktemp -d "$output/.package.XXXXXXXX")
trap 'rm -rf "$stage"' EXIT
app="$stage/Stroll.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 755 "$bins/stroll" "$app/Contents/MacOS/"
install -m 644 packaging/macos/Stroll.icns "$app/Contents/Resources/"
install -m 644 packaging/macos/Info.plist "$app/Contents/"
plist="$app/Contents/Info.plist"
bundle_version=${version%%[-+]*}
plutil -insert CFBundleShortVersionString -string "$bundle_version" "$plist"
plutil -insert CFBundleVersion -string "$bundle_version" "$plist"
plutil -lint "$plist"

signing_dir="$HOME/Library/Application Support/Stroll/signing"
identity="$signing_dir/identity.pem"
if [[ ! -f "$identity" ]]; then
  (
    umask 077
    mkdir -p "$signing_dir"
    identity_stage=$(mktemp -d "$signing_dir/.identity.XXXXXXXX")
    trap 'rm -rf "$identity_stage"' EXIT
    /usr/bin/openssl req -new -newkey rsa:2048 -x509 -days 3650 -nodes \
      -subj '/CN=Stroll Local Code Signing' -config /dev/stdin \
      -out "$identity_stage/certificate.pem" -keyout "$identity_stage/key.pem" <<'OPENSSL'
[req]
distinguished_name = subject
x509_extensions = code_signing
[subject]
[code_signing]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
OPENSSL
    cat "$identity_stage/certificate.pem" "$identity_stage/key.pem" > "$identity_stage/identity.pem"
    # Publish the complete identity without replacing one created by another build.
    if ! ln "$identity_stage/identity.pem" "$identity" 2>/dev/null && [[ ! -f "$identity" ]]; then
      echo "Could not save the local signing identity at $identity." >&2
      exit 1
    fi
  )
fi

keychain="$stage/signing.keychain-db"
keychain_password=$(/usr/bin/openssl rand -hex 32)
/usr/bin/openssl pkcs12 -export -in "$identity" -inkey "$identity" \
  -out "$stage/signing.p12" -passout "pass:$keychain_password"
/usr/bin/security create-keychain -p "$keychain_password" "$keychain"
/usr/bin/security unlock-keychain -p "$keychain_password" "$keychain"
/usr/bin/security import "$stage/signing.p12" -k "$keychain" -f pkcs12 \
  -P "$keychain_password" -T /usr/bin/codesign >/dev/null
/usr/bin/security set-key-partition-list -S apple-tool:,apple:,codesign: \
  -s -k "$keychain_password" "$keychain" >/dev/null

fingerprint=$(/usr/bin/openssl x509 -in "$identity" -noout -fingerprint -sha1 | sed 's/.*=//;s/://g')
bundle_id=$(plutil -extract CFBundleIdentifier raw -o - "$plist")
# Bind updates to the signing certificate and bundle ID across rebuilds.
requirement="identifier \"$bundle_id\" and certificate leaf = H\"$fingerprint\""
/usr/bin/codesign --force --sign "$fingerprint" --keychain "$keychain" --timestamp=none \
  -r="designated => $requirement" "$app"
/usr/bin/codesign --verify --deep --strict -R="$requirement" "$app"
ditto -c -k --keepParent "$app" "$stage/$archive"
mv -f "$stage/$archive" "$output/$archive"
printf '%s\n' "$output/$archive"
