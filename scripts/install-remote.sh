#!/usr/bin/env bash
# Build the SSH remote helper and install it for the current user.
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bin_dir="$HOME/.local/bin"

usage() {
  cat <<'EOF'
Usage: scripts/install-remote.sh [--build-only] [--bin-dir DIR]

  --build-only  Build target/release/stroll-remote without installing it.
  --bin-dir     Directory for the installed binary (default: ~/.local/bin).
EOF
}

build_only=0
while (($# > 0)); do
  case $1 in
    --build-only) build_only=1; shift ;;
    --bin-dir)
      if (($# < 2)); then
        echo "--bin-dir requires a directory" >&2
        exit 1
      fi
      bin_dir=$2
      shift 2
      ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
  esac
done

target_dir=${CARGO_TARGET_DIR:-$repo/target}
if [[ $target_dir != /* ]]; then
  target_dir="$repo/$target_dir"
fi
binary="$target_dir/release/stroll-remote"

# The helper does not need the desktop feature or its GUI dependencies.
(cd "$repo" && cargo build --release --locked --no-default-features --bin stroll-remote)

if ((build_only)); then
  echo "Built $binary"
  exit 0
fi

install -Dm755 "$binary" "$bin_dir/stroll-remote"
echo "Installed $bin_dir/stroll-remote"
if [[ ":$PATH:" != *":$bin_dir:"* ]]; then
  echo "Note: $bin_dir is not in PATH; the desktop client also checks this location over SSH." >&2
fi
