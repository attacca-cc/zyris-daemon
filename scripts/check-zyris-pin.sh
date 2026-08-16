#!/usr/bin/env bash
# Checks that the zyris git dependencies are pinned to the same rev in both workspaces.
#
# Declared without a rev, Cargo.lock is the only thing holding it, and one `cargo update` slides
# silently to the default branch tip. For a product that ships binaries, builds must not drift.
set -euo pipefail
cd "$(dirname "$0")/.."

REV="cacaf0252f61bf1aa99e9491cde2a2a38dcb1102"
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

# During development display/ may point [patch] at a local working tree (see display/Cargo.toml).
# While that patch is in place display/Cargo.lock carries no git rev (a path patch makes the
# source disappear) — so skip the lock check and verify instead that the patched tree HEAD has
# the pinned rev as an ancestor. That keeps the guarantee we are not building from a random tree.
patched_zyris=0
if grep -qE '^\[patch\."https://github.com/attacca-cc/zyris"\]' display/Cargo.toml; then
  patched_zyris=1
fi

for lock in Cargo.lock display/Cargo.lock; do
  [[ -f "$lock" ]] || continue
  if [[ "$lock" == "display/Cargo.lock" && $patched_zyris -eq 1 ]]; then
    local_paths=$(
      awk '
        /^\[patch\."https:\/\/github\.com\/attacca-cc\/zyris"\]/ { inpatch=1; next }
        /^\[/ { inpatch=0 }
        inpatch && /path = / { print }
      ' display/Cargo.toml | sed -E 's/.*path = "([^"]+)".*/\1/'
    )
    if [[ -z "$local_paths" ]]; then
      echo "✗ no local path found in the [patch] of display/Cargo.toml"; fail=1
      continue
    fi
    while IFS= read -r p; do
      if git -C "$p" merge-base --is-ancestor "$REV" HEAD 2>/dev/null; then
        echo "ℹ display uses a local patch ($p @ $(git -C "$p" rev-parse --short HEAD)) — skipping the lock rev check"
      else
        echo "✗ patched tree $p does not have ${REV} as an ancestor"; fail=1
      fi
    done <<< "$local_paths"
    continue
  fi
  if ! grep -q "rev=${REV}#${REV}" "$lock"; then
    echo "✗ ${lock} does not point at ${REV}"; fail=1
  fi
done

if [[ $fail -eq 0 ]]; then
  echo "✓ zyris deps in both workspaces are pinned to ${REV}"
fi
exit $fail
