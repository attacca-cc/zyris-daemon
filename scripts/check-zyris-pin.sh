#!/usr/bin/env bash
# Checks that the zyris git dependencies are pinned to the same rev in both workspaces.
#
# Declared without a rev, Cargo.lock is the only thing holding it, and one `cargo update` slides
# silently to the default branch tip. For a product that ships binaries, builds must not drift.
set -euo pipefail
cd "$(dirname "$0")/.."

REV="75f7e5d0f98baa1f72c63c762fb8e577d4b0638e"
fail=0

check_manifest() {
  local manifest="$1"; shift
  for c in "$@"; do
    local line
    line=$(grep -E "^${c} = " "$manifest" || true)
    if [[ -z "$line" ]]; then
      echo "✗ ${manifest} has no ${c} declaration"; fail=1; continue
    fi
    if [[ "$line" != *"rev = \"${REV}\""* ]]; then
      echo "✗ ${manifest}: ${c} is not pinned to ${REV}"; fail=1
    fi
  done
}

check_manifest Cargo.toml zyris zyris-caps zyris-capkit
check_manifest display/Cargo.toml zyris zyris-caps zyris-capkit

# A member crate that declares the git dependency itself bypasses the workspace pin, links the
# same crate twice, and gets you the "same type, different types" error.
for m in crates/*/Cargo.toml; do
  if grep -qE '^zyris[a-z-]* = \{ git' "$m"; then
    echo "✗ ${m} declares zyris directly. Use workspace = true"; fail=1
  fi
done

for lock in Cargo.lock display/Cargo.lock; do
  [[ -f "$lock" ]] || continue
  if ! grep -q "rev=${REV}#${REV}" "$lock"; then
    echo "✗ ${lock} does not point at ${REV}"; fail=1
  fi
done

if [[ $fail -eq 0 ]]; then
  echo "✓ zyris deps in both workspaces are pinned to ${REV}"
fi
exit $fail
