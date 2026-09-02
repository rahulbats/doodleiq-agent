#!/bin/sh
# DoodleIQ provider agent installer (macOS / Linux).
#
#   curl -fsSL https://get.doodleiq.com/install.sh | sh
#
# Env overrides:
#   DOODLEIQ_VERSION   release to install (default: latest)
#   DOODLEIQ_BASE_URL  where release archives live (default: baked in at release
#                      time; falls back to GitHub releases)
#   DOODLEIQ_BIN_DIR   install dir (default: /usr/local/bin if writable, else ~/.local/bin)
set -eu

# These are substituted by release-local.sh at publish time; the @@...@@ fallbacks
# apply when the script is run straight from the git repo.
REPO="@@REPO@@"
case "$REPO" in *@@*) REPO="rahulbats/doodleiq-agent" ;; esac
BASE_URL="${DOODLEIQ_BASE_URL:-@@BASE_URL@@}"
case "$BASE_URL" in *@@*) BASE_URL="" ;; esac

VERSION="${DOODLEIQ_VERSION:-latest}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin) slug="macos-universal" ;;
  Linux)
    case "$arch" in
      x86_64|amd64)  slug="linux-x86_64" ;;
      aarch64|arm64) slug="linux-aarch64" ;;
      *) echo "unsupported Linux arch: $arch" >&2; exit 1 ;;
    esac ;;
  *) echo "unsupported OS: $os (Windows: use install.ps1)" >&2; exit 1 ;;
esac

archive="doodleiq-${slug}.tar.gz"
if [ -n "$BASE_URL" ]; then
  url="${BASE_URL%/}/${VERSION}/${archive}"
elif [ "$VERSION" = "latest" ]; then
  url="https://github.com/${REPO}/releases/latest/download/${archive}"
else
  url="https://github.com/${REPO}/releases/download/${VERSION}/${archive}"
fi

bindir="${DOODLEIQ_BIN_DIR:-}"
if [ -z "$bindir" ]; then
  if [ -w /usr/local/bin ] 2>/dev/null; then bindir=/usr/local/bin; else bindir="$HOME/.local/bin"; fi
fi
mkdir -p "$bindir"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "Downloading $url"
curl -fsSL "$url" -o "$tmp/a.tar.gz"
tar -xzf "$tmp/a.tar.gz" -C "$tmp"
install -m 0755 "$tmp/doodleiq-${slug}/doodleiq" "$bindir/doodleiq"

echo "Installed doodleiq to $bindir/doodleiq"
case ":$PATH:" in *":$bindir:"*) ;; *) echo "Add $bindir to your PATH." ;; esac
echo
echo "Next:"
echo "  doodleiq configure      # point at your model runtime (Ollama/LM Studio/oMLX/...)"
echo "  doodleiq pair           # link this machine to your DoodleIQ account"
echo "  doodleiq run            # start serving"
