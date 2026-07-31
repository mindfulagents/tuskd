#!/bin/sh
# OpenTusk installer — https://get.opentusk.ai
#
#   curl -fsSL https://get.opentusk.ai | sh
#
# Options (environment variables):
#   TUSKD_VERSION      pin a release, e.g. TUSKD_VERSION=v0.5.0 (default: latest)
#   TUSKD_INSTALL_DIR  install destination (default: ~/.local/bin)
#
# The script detects your platform, downloads the release tarball, verifies
# its SHA-256 checksum against the published .sha256 file, and installs the
# single `tuskd` binary. It never uses sudo.
#
# Releases v0.5.0+ are built by CI and served from GitHub Releases
# (DECISIONS D19); older pinned versions fall back to the original
# get.opentusk.ai archive, which is immutable.

set -u

BASE="${TUSKD_BASE_URL:-https://github.com/mindfulagents/tuskd/releases}"
LEGACY_BASE="${TUSKD_LEGACY_BASE_URL:-https://get.opentusk.ai}"
INSTALL_DIR="${TUSKD_INSTALL_DIR:-$HOME/.local/bin}"

say()  { printf '%s\n' "$*" >&2; }
fail() { say "install.sh: error: $*"; exit 1; }

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar  >/dev/null 2>&1 || fail "tar is required"

# ── Platform detection ────────────────────────────────────────────────
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS-$ARCH" in
  Darwin-arm64)              TARGET="aarch64-apple-darwin" ;;
  Darwin-x86_64)             TARGET="x86_64-apple-darwin" ;;
  Linux-x86_64)              TARGET="x86_64-unknown-linux-musl" ;;
  Linux-aarch64|Linux-arm64) TARGET="aarch64-unknown-linux-musl" ;;
  *)                         fail "unsupported platform: $OS/$ARCH" ;;
esac

# ── Checksum tool ─────────────────────────────────────────────────────
if command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
elif command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
else
  fail "need shasum or sha256sum to verify the download"
fi

# ── Resolve version and URL ───────────────────────────────────────────
VERSION="${TUSKD_VERSION:-}"
TARBALL="tuskd-$TARGET.tar.gz"
if [ -z "$VERSION" ]; then
  VERSION="latest"
  URL="$BASE/latest/download/$TARBALL"
else
  VERSION="v${VERSION#v}"
  URL="$BASE/download/$VERSION/$TARBALL"
fi

# ── Download and verify ───────────────────────────────────────────────
TMP=$(mktemp -d) || fail "mktemp failed"
trap 'rm -rf "$TMP"' EXIT INT TERM

say "opentusk: downloading tuskd $VERSION ($TARGET)"
if ! curl -fsSL -o "$TMP/$TARBALL" "$URL"; then
  # Releases before v0.5.0 predate GitHub Releases: same scheme, old archive.
  if [ "$VERSION" != "latest" ]; then
    TARBALL="tuskd-$VERSION-$TARGET.tar.gz"
    URL="$LEGACY_BASE/releases/$VERSION/$TARBALL"
    say "opentusk: not on GitHub Releases, trying the legacy archive"
    curl -fsSL -o "$TMP/$TARBALL" "$URL" \
      || fail "download failed: $URL (is $VERSION a published release for $TARGET?)"
  else
    fail "download failed: $URL"
  fi
fi
curl -fsSL -o "$TMP/$TARBALL.sha256" "$URL.sha256" \
  || fail "checksum file missing: $URL.sha256"

# The .sha256 file's first token is the hash (works for both `shasum -c`
# format and bare-hash files).
WANT=$(tr -s ' \t\n' '  \n' < "$TMP/$TARBALL.sha256" | cut -d' ' -f1)
GOT=$(sha256_of "$TMP/$TARBALL")
[ -n "$WANT" ] && [ "$WANT" = "$GOT" ] \
  || fail "SHA-256 verification FAILED — refusing to install"
say "opentusk: checksum verified"

tar -xzf "$TMP/$TARBALL" -C "$TMP" || fail "could not extract $TARBALL"
# The binary sits at the archive root (legacy) or inside a top-level dir
# (cargo-dist archives).
BIN="$TMP/tuskd"
[ -f "$BIN" ] || BIN=$(find "$TMP" -maxdepth 2 -type f -name tuskd | head -1)
[ -n "$BIN" ] && [ -f "$BIN" ] || fail "tarball did not contain the tuskd binary"

# ── Install ───────────────────────────────────────────────────────────
mkdir -p "$INSTALL_DIR" || fail "cannot create $INSTALL_DIR"
install -m 0755 "$BIN" "$INSTALL_DIR/tuskd" 2>/dev/null \
  || { cp "$BIN" "$INSTALL_DIR/tuskd" && chmod 0755 "$INSTALL_DIR/tuskd"; } \
  || fail "cannot write to $INSTALL_DIR"

say "opentusk: installed $INSTALL_DIR/tuskd ($VERSION)"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) say ""
     say "  $INSTALL_DIR is not on your PATH. Add it with:"
     say "    export PATH=\"$INSTALL_DIR:\$PATH\""
     say "" ;;
esac

say "opentusk: get started →  mkdir my-vault && cd my-vault && tuskd setup"
say "opentusk: docs        →  https://opentusk.ai"
