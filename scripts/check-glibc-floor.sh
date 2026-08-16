#!/usr/bin/env bash
# Refuse to ship a Linux binary that needs a newer glibc than the oldest system we support.
#
# A dynamically linked binary runs on the glibc it was built against or newer, never older, and
# the failure is not graceful: the loader aborts before main with "version `GLIBC_2.38' not
# found". Nothing in the build says which version got baked in, so this asks the binary itself.
#
# v0.1.0 and v0.2.0 shipped built on ubuntu-24.04 (glibc 2.39) and could not start on Debian 12
# (2.36) — every Linux asset in both releases was unusable there, and the release was green.
# That is what this exists to catch.
#
# Usage: scripts/check-glibc-floor.sh <binary> [max-glibc]   (default floor 2.35 = Ubuntu 22.04)

set -euo pipefail

bin=${1:?usage: check-glibc-floor.sh <binary> [max-glibc]}
floor=${2:-2.35}

if ! command -v readelf >/dev/null 2>&1; then
  echo "check-glibc-floor: readelf not found, cannot verify $bin" >&2
  exit 2
fi

# Versioned symbol references live in .gnu.version_r as `GLIBC_2.39`. A binary that references
# none is either static or does not use glibc; both are fine and leave `found` empty.
mapfile -t found < <(
  readelf --wide --version-info "$bin" 2>/dev/null |
    grep -o 'GLIBC_[0-9]\+\.[0-9]\+' |
    sed 's/^GLIBC_//' |
    sort -u -V
)

if [ ${#found[@]} -eq 0 ]; then
  echo "✓ $bin references no versioned glibc symbols"
  exit 0
fi

highest=${found[-1]}

# sort -V puts the larger last, so the highest is over the floor exactly when it sorts after it.
if [ "$(printf '%s\n%s\n' "$floor" "$highest" | sort -V | tail -1)" != "$floor" ]; then
  echo "✗ $bin needs glibc $highest, above the $floor floor" >&2
  echo "  requested: ${found[*]}" >&2
  echo "  The build host's glibc decides this. Build on an older runner, or raise the floor" >&2
  echo "  deliberately and say in the release notes which systems just lost support." >&2
  exit 1
fi

echo "✓ $bin needs at most glibc $highest (floor $floor)"
