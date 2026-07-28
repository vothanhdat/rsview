#!/bin/sh
# jsonview installer — downloads the prebuilt binary for your platform and drops
# it on your PATH, plus `jview` / `jv` aliases. No Rust toolchain required.
#
#   curl -fsSL https://raw.githubusercontent.com/vothanhdat/rsview/stable/install.sh | sh
#
# Env overrides:
#   JSONVIEW_INSTALL_DIR   where to put the binary (default: $HOME/.local/bin)
#   JSONVIEW_VERSION       a specific tag like v0.10.0 (default: latest release)
#   JSONVIEW_NO_ALIASES    set to 1 to skip the jview/jv symlinks
# (The older JVIEW_* names still work.)
#
# It fetches from GitHub's releases/latest/download redirect, so it always tracks
# the newest release without hitting the API (no token, no rate limit).

set -eu

REPO="vothanhdat/rsview"
BIN="jsonview"
ALIASES="jview jv"
INSTALL_DIR="${JSONVIEW_INSTALL_DIR:-${JVIEW_INSTALL_DIR:-$HOME/.local/bin}}"
VERSION="${JSONVIEW_VERSION:-${JVIEW_VERSION:-latest}}"
NO_ALIASES="${JSONVIEW_NO_ALIASES:-0}"

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

asset_url() {
  if [ "$VERSION" = "latest" ]; then
    printf 'https://github.com/%s/releases/latest/download/%s' "$REPO" "$1"
  else
    printf 'https://github.com/%s/releases/download/%s/%s' "$REPO" "$VERSION" "$1"
  fi
}

printf 'install: downloading %s (%s)\n' "$BIN" "$target" >&2

# Releases up to v0.21.4 named the archive (and the binary inside it) `jview`.
# Try the current name first, then fall back so pinning an older tag still works.
got=""
for name in "$BIN" jview; do
  asset="${name}-${target}.tar.gz"
  if fetch "$(asset_url "$asset")" "$tmp/$asset" 2>/dev/null; then
    tar -xzf "$tmp/$asset" -C "$tmp" || err "extract failed (corrupt download?)"
    if [ -f "$tmp/$name" ]; then got="$name"; break; fi
  fi
done
[ -n "$got" ] || err "download failed for ${BIN}-${target}.tar.gz — see https://github.com/$REPO/releases"

mkdir -p "$INSTALL_DIR"
mv "$tmp/$got" "$INSTALL_DIR/$BIN"
chmod +x "$INSTALL_DIR/$BIN"
printf 'install: installed to %s\n' "$INSTALL_DIR/$BIN" >&2

# Short aliases. Symlink so they always point at the binary just installed;
# fall back to a copy on filesystems where symlinks aren't available.
if [ "$NO_ALIASES" != "1" ]; then
  for a in $ALIASES; do
    rm -f "$INSTALL_DIR/$a"
    if ln -s "$BIN" "$INSTALL_DIR/$a" 2>/dev/null ||
       cp "$INSTALL_DIR/$BIN" "$INSTALL_DIR/$a" 2>/dev/null; then
      printf 'install: alias %s -> %s\n' "$a" "$BIN" >&2
    fi
  done
fi

"$INSTALL_DIR/$BIN" --version 2>/dev/null || true

# Nudge if the install dir isn't on PATH.
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) printf 'install: add it to your PATH:\n  export PATH="%s:$PATH"\n' "$INSTALL_DIR" >&2 ;;
esac
