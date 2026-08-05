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
# Also drop the input-libei feature, which exists only in the patched tree — zyris-capkit at
# the pinned rev has no such feature, and leaving it in breaks dependency resolution.
sed -i 's/, *"input-libei"//; s/"input-libei", *//; s/"input-libei"//' display/Cargo.toml
if grep -q 'input-libei' display/Cargo.toml; then
  echo "✗ failed to strip the input-libei feature" >&2
  exit 1
fi
echo "✓ stripped [patch] and input-libei from display/Cargo.toml (building at the pinned rev)"
