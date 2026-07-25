#!/bin/sh
# OpenTusk installer — https://get.opentusk.ai
#
#   curl -fsSL https://get.opentusk.ai | sh
#
# Options (environment variables):
#   TUSKD_VERSION      pin a release, e.g. TUSKD_VERSION=v0.1.0 (default: latest)
#   TUSKD_INSTALL_DIR  install destination (default: ~/.local/bin)
#
# The script detects your platform, downloads the release tarball, verifies
# its SHA-256 checksum against the published .sha256 file, and installs the
# single `tuskd` binary. It never uses sudo.

set -u

BASE="${TUSKD_BASE_URL:-https://get.opentusk.ai}"
INSTALL_DIR="${TUSKD_INSTALL_DIR:-$HOME/.local/bin}"

say()  { printf '%s\n' "$*" >&2; }
fail() { say "install.sh: error: $*"; exit 1; }

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar  >/dev/null 2>&1 || fail "tar is required"

# ── Platform detection ────────────────────────────────────────────────
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS-$ARCH" in
  Darwin-arm64)            TARGET="aarch64-apple-darwin" ;;
  Darwin-x86_64)           fail "Intel macOS builds are not available yet (Apple Silicon only for now)" ;;
  Linux-x86_64)            fail "Linux x86_64 builds are planned but not published yet" ;;
  Linux-aarch64|Linux-arm64) fail "Linux arm64 builds are planned but not published yet" ;;
  *)                       fail "unsupported platform: $OS/$ARCH" ;;
esac

# ── Checksum tool ─────────────────────────────────────────────────────
if command -v shasum >/dev/null 2>&1; then
  SHA_CHECK="shasum -a 256 -c"
elif command -v sha256sum >/dev/null 2>&1; then
  SHA_CHECK="sha256sum -c"
else
  fail "need shasum or sha256sum to verify the download"
fi

# ── Resolve version ───────────────────────────────────────────────────
VERSION="${TUSKD_VERSION:-}"
if [ -z "$VERSION" ]; then
  VERSION=$(curl -fsSL "$BASE/releases/latest.json" 2>/dev/null \
    | tr -d '\n' | sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
  [ -n "$VERSION" ] || fail "could not determine the latest version from $BASE/releases/latest.json"
fi
VERSION="v${VERSION#v}"

TARBALL="tuskd-$VERSION-$TARGET.tar.gz"
URL="$BASE/releases/$VERSION/$TARBALL"

# ── Download and verify ───────────────────────────────────────────────
TMP=$(mktemp -d) || fail "mktemp failed"
trap 'rm -rf "$TMP"' EXIT INT TERM

say "opentusk: downloading tuskd $VERSION ($TARGET)"
curl -fsSL -o "$TMP/$TARBALL" "$URL" \
  || fail "download failed: $URL (is $VERSION a published release?)"
curl -fsSL -o "$TMP/$TARBALL.sha256" "$URL.sha256" \
  || fail "checksum file missing: $URL.sha256"

( cd "$TMP" && $SHA_CHECK "$TARBALL.sha256" >/dev/null 2>&1 ) \
  || fail "SHA-256 verification FAILED — refusing to install"
say "opentusk: checksum verified"

tar -xzf "$TMP/$TARBALL" -C "$TMP" || fail "could not extract $TARBALL"
[ -f "$TMP/tuskd" ] || fail "tarball did not contain the tuskd binary"

# ── Install ───────────────────────────────────────────────────────────
mkdir -p "$INSTALL_DIR" || fail "cannot create $INSTALL_DIR"
install -m 0755 "$TMP/tuskd" "$INSTALL_DIR/tuskd" 2>/dev/null \
  || { cp "$TMP/tuskd" "$INSTALL_DIR/tuskd" && chmod 0755 "$INSTALL_DIR/tuskd"; } \
  || fail "cannot write to $INSTALL_DIR"

say "opentusk: installed $INSTALL_DIR/tuskd ($VERSION)"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say ""
     say "  $INSTALL_DIR is not on your PATH. Add it with:"
     say "    export PATH=\"$INSTALL_DIR:\$PATH\""
     say "" ;;
esac

say "opentusk: get started →  mkdir my-vault && cd my-vault && tuskd init"
say "opentusk: docs        →  https://opentusk.ai"
