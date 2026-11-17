#!/usr/bin/env bash
# One-shot demo of the transient workspace messages.
#
# Builds a throwaway git repository that can produce every message, launches
# stroll against it with isolated state, and deletes everything when the
# window closes.
#
# Usage: scripts/toast-demo.sh [--keep] [work-dir]
#
#   --keep      keep the demo files after the window closes, and reuse them on
#               the next run (needed for the restored-draft check)
#   work-dir    use (and keep) this directory instead of a temporary one
#
# Nothing touches your personal session or settings: the demo passes
# STROLL_GPUI_DATA and STROLL_CONFIG inside the demo directory.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
keep=false
work=""
for argument in "$@"; do
  case "$argument" in
    --keep) keep=true ;;
    -h | --help)
      sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    -*)
      echo "unknown option: $argument" >&2
      exit 2
      ;;
    *) work="$argument" ;;
  esac
done

auto=false
if [[ -z "$work" ]]; then
  work="$(mktemp -d "${TMPDIR:-/tmp}/stroll-toast-demo.XXXXXX")"
  auto=true
fi

cleanup() {
  local status=$?
  if $keep || ! $auto; then
    echo
    echo "Demo files kept in $work"
  else
    rm -rf -- "$work"
    echo
    echo "Removed $work"
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

repo="$work/repo"
state="$work/state"
config="$work/config.toml"

if [[ -d "$repo/.git" ]]; then
  echo "Reusing $repo"
else
  rm -rf -- "$repo"
  mkdir -p "$repo/src"
  cat >"$repo/Cargo.toml" <<'CARGO'
[package]
name = "toast-demo"
version = "0.1.0"
edition = "2021"

[dependencies]
anyhow = "1"
CARGO

  cat >"$repo/src/main.rs" <<'RUST'
fn main() {
    // Put the cursor on `anyhow` and press F12: the definition lives in
    // ~/.cargo/registry, outside this repository.
    let error = anyhow::anyhow!("demo");
    println!("{error}");
}
RUST

  cat >"$repo/notes.md" <<'NOTES'
# Notes

Type here without saving, then change this file from a shell.
NOTES

  # One file with both CRLF and LF endings. The workspace opens it read-only.
  # The attribute keeps Git from normalizing the endings on checkout.
  printf 'mixed-eol.txt -text\n' >"$repo/.gitattributes"
  printf 'first line\r\nsecond line\nthird line\r\n' >"$repo/mixed-eol.txt"

  if ! (cd "$repo" && cargo fetch -q >/dev/null 2>&1); then
    echo "note: cargo fetch failed, so F12 on anyhow::anyhow needs crate sources" >&2
  fi

  git -C "$repo" init -q
  git -C "$repo" add -A
  git -C "$repo" -c user.name=demo -c user.email=demo@example.com commit -qm "demo"

  # An unstaged change, so the Git panel has a hunk to review.
  printf '\nunstaged change\n' >>"$repo/notes.md"
  # An untracked file, so the Git panel can discard it and close its tab.
  printf 'scratch\n' >"$repo/scratch.txt"
fi
mkdir -p "$state"

echo
echo "Demo repository: $repo"
echo
cat <<CHECKLIST
Checklist
1. Save. Open notes.md, type, press Ctrl+S.
   Expect: nothing appears above the editor and the text does not move.

2. Conflict. Keep notes.md open and dirty in the app, then run:
     printf 'from the shell\n' >> $repo/notes.md
   Expect: nothing yet, because the file is only compared on save. Press Ctrl+S
   once: a single prompt titled "File changed on disk" offers "Keep draft" and
   "Reload from disk", with no toast next to it. Keep draft: the tab badge turns
   warning-colored and the file stays read-only. Reload:
   the draft is dropped, the editor shows the disk copy and becomes editable.

3. Draft restore. Run this script with --keep, open notes.md, type, close the
   window, then run it again with --keep.
   Expect: the tab is dirty, the draft is back, and nothing is printed above
   the editor.

4. Mixed line endings. Open mixed-eol.txt.
   Expect: the editor is read-only and nothing is printed above it.

5. Discarded file. Open scratch.txt, then discard it from the Git panel.
   Expect: the tab closes with no message at all.

6. Definition outside the repository. Open src/main.rs, put the cursor on
   anyhow and press F12.
   Expect: a bottom-right toast saying the definition is outside the
   repository, and it can be closed.

Every toast sits in the bottom-right corner, leaves the editor layout
untouched, and closes with its hover button or a middle click.
CHECKLIST

echo
echo "Building stroll (release)…"
(cd "$root" && cargo build --release --locked)

echo "Launching stroll…"
STROLL_GPUI_DATA="$state" STROLL_CONFIG="$config" \
  "$root/target/release/stroll" "$repo"
