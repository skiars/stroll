#!/usr/bin/env bash
# Build an Arch package from the current working tree and optionally install it.
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$repo/Cargo.toml" | head -n1)
if [[ -z $version ]]; then
  echo "Could not read the package version from Cargo.toml" >&2
  exit 1
fi

usage() {
  cat <<'EOF'
Usage: scripts/package-arch.sh [--build-only] [--clean] [makepkg options...]

  --build-only  Build the package into target/arch without installing it.
  --clean       Pack a binary built from source in an isolated tree instead of
                reusing the development build in target/release. The isolated
                build strips temporary build paths from the binary.
EOF
}

build_only=0
clean=0
while (($# > 0)); do
  case $1 in
    --build-only) build_only=1; shift ;;
    --clean) clean=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) break ;;
  esac
done

arch_dir="$repo/target/arch"
build_dir="$arch_dir/build"
mkdir -p "$arch_dir" "$build_dir"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/stroll-$version"

# Package the working tree, including local source changes.
tar -C "$repo" \
  --exclude=./.git \
  --exclude=./.env \
  --exclude=./.runtime \
  --exclude=./packaging \
  --exclude=./target \
  -cf - . | tar -C "$stage/stroll-$version" -xf -

# Keep the makepkg working directory stable so $srcdir does not change between
# runs; an isolated --clean build then keeps its cargo target cache as well.
cp "$repo/packaging/PKGBUILD" \
  "$repo/packaging/io.stroll.Stroll.desktop" \
  "$repo/packaging/io.stroll.Stroll.svg" \
  "$build_dir/"
sed -i "s/^pkgver=.*/pkgver=$version/" "$build_dir/PKGBUILD"
tar -C "$stage" -czf "$build_dir/stroll-$version.tar.gz" "stroll-$version"

args=(--force --syncdeps)
if ((!build_only)); then
  args+=(--install)
fi

if ((clean)); then
  rm -rf "$build_dir/prebuilt" "$build_dir/src"
  args+=(--cleanbuild)
else
  # Reuse the development build in target/release, so packaging only costs an
  # incremental cargo build and the makepkg run.
  target_dir=${CARGO_TARGET_DIR:-$repo/target}
  if [[ $target_dir != /* ]]; then
    target_dir="$repo/$target_dir"
  fi
  (cd "$repo" && cargo build --release --locked --bin stroll)
  echo "Packaging $target_dir/release/stroll"
  mkdir -p "$build_dir/prebuilt"
  install -m755 "$target_dir/release/stroll" "$build_dir/prebuilt/stroll"
  rm -rf "${build_dir:?}/src/stroll-$version"
fi

cd "$build_dir"
PKGDEST="$arch_dir" makepkg "${args[@]}" "$@"
