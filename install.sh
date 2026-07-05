#!/bin/sh
# jview installer — downloads the prebuilt binary for your platform and drops it
# on your PATH. No Rust toolchain required.
#
#   curl -fsSL https://raw.githubusercontent.com/vothanhdat/rsview/stable/install.sh | sh
#
# Env overrides:
#   JVIEW_INSTALL_DIR   where to put the binary (default: $HOME/.local/bin)
#   JVIEW_VERSION       a specific tag like v0.10.0 (default: latest release)
#
# It fetches from GitHub's releases/latest/download redirect, so it always tracks
# the newest release without hitting the API (no token, no rate limit).

set -eu

REPO="vothanhdat/rsview"
BIN="jview"
INSTALL_DIR="${JVIEW_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${JVIEW_VERSION:-latest}"

err() { printf 'install: %s\n' "$1" >&2; exit 1; }

# Map uname -> the target triple used in the release asset names.
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64)   target="x86_64-unknown-linux-musl" ;;
      aarch64|arm64)  target="aarch64-unknown-linux-musl" ;;
      *) err "unsupported Linux arch: $arch" ;;
    esac ;;
  Darwin)
    case "$arch" in
      x86_64)         target="x86_64-apple-darwin" ;;
      arm64|aarch64)  target="aarch64-apple-darwin" ;;
      *) err "unsupported macOS arch: $arch" ;;
    esac ;;
  *)
    err "unsupported OS: $os — on Windows, grab the .zip from https://github.com/$REPO/releases" ;;
esac

asset="${BIN}-${target}.tar.gz"
if [ "$VERSION" = "latest" ]; then
  url="https://github.com/${REPO}/releases/latest/download/${asset}"
else
  url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
fi

# Pick a downloader.
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  err "need curl or wget to download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

printf 'install: downloading %s (%s)\n' "$BIN" "$target" >&2
fetch "$url" "$tmp/$asset" || err "download failed: $url"
tar -xzf "$tmp/$asset" -C "$tmp" || err "extract failed (corrupt download?)"
[ -f "$tmp/$BIN" ] || err "archive did not contain '$BIN'"

mkdir -p "$INSTALL_DIR"
mv "$tmp/$BIN" "$INSTALL_DIR/$BIN"
chmod +x "$INSTALL_DIR/$BIN"

printf 'install: installed to %s\n' "$INSTALL_DIR/$BIN" >&2
"$INSTALL_DIR/$BIN" --version 2>/dev/null || true

# Nudge if the install dir isn't on PATH.
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) printf 'install: add it to your PATH:\n  export PATH="%s:$PATH"\n' "$INSTALL_DIR" >&2 ;;
esac
