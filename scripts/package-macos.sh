#!/usr/bin/env bash
# Build a native macOS application bundle and archive it for installation.
set -euo pipefail

case "${1:-}" in
  -h|--help)
    echo 'Usage: scripts/package-macos.sh'
    echo 'Build a native Stroll.app ZIP under the Cargo target directory (macos/).'
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

codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"
ditto -c -k --keepParent "$app" "$stage/$archive"
mv -f "$stage/$archive" "$output/$archive"
printf '%s\n' "$output/$archive"
