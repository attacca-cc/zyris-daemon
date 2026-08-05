#!/usr/bin/env bash
# CI only: strips the dev-only [patch] section out of display/Cargo.toml.
#
# display/Cargo.toml aims its [patch] at worktrees on the dev machine (/home/ruma/zyris,
# /home/ruma/enigo-local). Those paths don't exist on CI, so release builds strip the patch and
# build against the pinned git rev (the one scripts/check-zyris-pin.sh guards). The committed
# file is never touched — this script only ever runs on the CI working copy.
set -euo pipefail
cd "$(dirname "$0")/.."

sed -i '/^\[patch\./,$d' display/Cargo.toml
if grep -q '^\[patch\.' display/Cargo.toml; then
  echo "✗ failed to strip the [patch] section" >&2
  exit 1
fi
echo "✓ stripped [patch] from display/Cargo.toml (build re-resolves Cargo.lock to the git rev)"
