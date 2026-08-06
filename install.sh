#!/bin/sh
# zyrisd installer — Linux (.deb) / Windows (Git Bash, MSYS2).
#
# Release assets are install.sh, the .deb (Linux x86_64/aarch64) and zyrisd-setup-*.exe (Windows).
# If SHA256SUMS is attached to the release we verify; if not we skip it (it only guards transport).
set -eu

BASE_URL="${ZYRISD_BASE_URL:-https://github.com/attacca-cc/zyris-daemon/releases/latest/download}"
API_URL="${ZYRISD_API_URL:-https://api.github.com/repos/attacca-cc/zyris-daemon}"

die() { echo "error: $*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required"

# $1=TMP, $2=file name. Verifies when the release carries SHA256SUMS, warns when it does not.
verify_if_present() {
  if curl -fsSL "${BASE_URL}/SHA256SUMS" -o "${1}/SHA256SUMS" 2>/dev/null; then
    ( cd "$1" && grep " ${2}\$" SHA256SUMS | sha256sum -c - >/dev/null ) ||
      die "checksum mismatch"
  else
    echo "note: no SHA256SUMS in the release, skipping checksum verification"
  fi
}

OS="$(uname -s)"
case "$OS" in
  Linux)
    # ── Linux — install the latest .deb with sudo dpkg -i ───────────────────
    case "$(uname -m)" in
      x86_64)  DEB_ARCH=amd64 ;;
      aarch64|arm64) DEB_ARCH=arm64 ;;
      *) die "unsupported architecture: $(uname -m)" ;;
    esac

    # The .deb file name carries the version, so ask the releases/latest API for the URL.
    DEB_URL="$(
      curl -fsSL "${API_URL}/releases/latest" 2>/dev/null |
        grep -oE '"browser_download_url" *: *"[^"]+_'"${DEB_ARCH}"'\.deb"' |
        head -1 |
        sed -E 's/.*"browser_download_url" *: *"([^"]+)".*/\1/'
    )" || true
    [ -n "$DEB_URL" ] || die "no ${DEB_ARCH} .deb in the latest release"

    DEB_NAME="$(basename "$DEB_URL")"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT

    echo "downloading: ${DEB_NAME}"
    curl -fsSL "$DEB_URL" -o "${TMP}/${DEB_NAME}" || die "download failed"
    verify_if_present "$TMP" "$DEB_NAME"

    if [ "$(id -u)" = "0" ]; then
      dpkg -i "${TMP}/${DEB_NAME}" || die "install failed"
    else
      sudo dpkg -i "${TMP}/${DEB_NAME}" || die "install failed (needs sudo)"
    fi

    cat <<MSG

zyrisd is installed system-wide (/usr/bin/zyrisd).

Two steps remain — run them as your own user:

  zyrisd enroll     Register this machine with your Attacca account
  zyrisd install    Connect automatically on every boot

MSG
    ;;
  MINGW*|MSYS*|CYGWIN*)
    # ── Windows (Git Bash / MSYS2) — run the NSIS installer unattended ──────
    case "$(uname -m)" in
      x86_64)  ARCH=x86_64 ;;
      *) die "unsupported architecture: $(uname -m) (x86_64 only for now)" ;;
    esac

    INSTALLER="zyrisd-setup-${ARCH}.exe"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT

    echo "downloading: ${BASE_URL}/${INSTALLER}"
    curl -fsSL "${BASE_URL}/${INSTALLER}" -o "${TMP}/${INSTALLER}" || die "download failed"
    verify_if_present "$TMP" "$INSTALLER"

    echo "Running the installer (unattended)..."
    ( cd "$TMP" && MSYS_NO_PATHCONV=1 ./"$INSTALLER" /S )

    cat <<MSG

Ran the zyrisd installer (default: %LOCALAPPDATA%\zyrisd).

One step remains (in a new shell):

  zyrisd enroll     Register this machine with your Attacca account

Auto-connect on boot follows the option you chose in the installer.
MSG
    ;;
  Darwin)
    die "macOS is not supported yet"
    ;;
  *)
    die "unsupported OS: $OS"
    ;;
esac
