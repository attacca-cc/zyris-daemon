#!/bin/sh
# zyrisd install script.
#
# **It only drops the binaries.** Enrolling and enabling the service is done by hand afterwards.
# Running `zyrisd install` here would start the daemon unenrolled, it would die with exit 2, and
# RestartPreventExitStatus=2 would freeze the unit as failed. With Type=simple, `enable --now`
# returns success right after the fork, so this script would never notice and would print
# "install complete".
set -eu

BASE_URL="${ZYRISD_BASE_URL:-https://github.com/attacca-cc/zyris-daemon/releases/latest/download}"
PREFIX="${ZYRISD_PREFIX:-$HOME/.local}"

die() { echo "error: $*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || die "Linux only for now (uname: $(uname -s))"
case "$(uname -m)" in
  x86_64)  ARCH=x86_64 ;;
  aarch64) ARCH=aarch64 ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar  >/dev/null 2>&1 || die "tar is required"
command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required"

# Don't judge by the exit code of `is-system-running` — one unrelated degraded unit makes it
# call a healthy machine broken (reproduces on this dev machine).
[ -n "${XDG_RUNTIME_DIR:-}" ] || die "XDG_RUNTIME_DIR is unset. Run this from a login session"
systemctl --user show -p Version >/dev/null 2>&1 || die "systemctl --user is unavailable"

TARBALL="zyrisd-${ARCH}-linux.tar.gz"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "downloading: ${BASE_URL}/${TARBALL}"
curl -fsSL "${BASE_URL}/${TARBALL}" -o "${TMP}/${TARBALL}" || die "download failed"
curl -fsSL "${BASE_URL}/SHA256SUMS" -o "${TMP}/SHA256SUMS" || die "could not fetch checksums"

# Checksums come from the same host as the tarball — **this only catches transfer corruption.**
# It is not a supply-chain guarantee — there are no signatures yet.
( cd "$TMP" && grep " ${TARBALL}\$" SHA256SUMS | sha256sum -c - >/dev/null ) ||
  die "checksum mismatch"

mkdir -p "${PREFIX}/bin" "${PREFIX}/libexec"
tar xzf "${TMP}/${TARBALL}" -C "$TMP"
install -m 755 "${TMP}/zyrisd" "${PREFIX}/bin/zyrisd"
if [ -f "${TMP}/zyrisd-display" ]; then
  install -m 755 "${TMP}/zyrisd-display" "${PREFIX}/libexec/zyrisd-display"
fi

# Point at the absolute path we just unpacked. Resolving by name on a machine with the .deb
# already installed can pick up the old /usr/bin/zyrisd.
cat <<MSG

Installed zyrisd to ${PREFIX}/bin/zyrisd.

Two steps remain:

  ${PREFIX}/bin/zyrisd enroll     Register this machine with your Attacca account
  ${PREFIX}/bin/zyrisd install    Connect automatically on every boot

MSG
