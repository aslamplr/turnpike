#!/usr/bin/env bash
# turnpike installer — macOS (Apple Silicon). Windows: use install.ps1.
#
# Downloads the latest prebuilt turnpike binary from GitHub Releases and
# installs it to ~/.local/bin (override with TURNPIKE_INSTALL_DIR).
#
# Environment overrides:
#   TURNPIKE_VERSION=v0.1.0   pin a release instead of "latest"
#   TURNPIKE_INSTALL_DIR=...  install somewhere other than ~/.local/bin
#   TURNPIKE_SKIP_SHA256=1    skip checksum verification (not recommended)
set -euo pipefail

REPO="aslamplr/turnpike"
ASSET="turnpike-aarch64-apple-darwin.tar.gz"
INSTALL_DIR="${TURNPIKE_INSTALL_DIR:-$HOME/.local/bin}"

if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
  echo "error: prebuilt turnpike binaries are published for macOS (Apple Silicon)" >&2
  echo "       and Windows (x86_64) only. On this platform, build from source:" >&2
  echo "         cargo build --release" >&2
  exit 1
fi

version="${TURNPIKE_VERSION:-latest}"
case "$version" in
  latest) base="https://github.com/$REPO/releases/latest/download" ;;
  v*)     base="https://github.com/$REPO/releases/download/$version" ;;
  *)      base="https://github.com/$REPO/releases/download/v$version" ;;
esac

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading turnpike ($version)..."
curl -fsSL "$base/$ASSET" -o "$tmp/$ASSET"

if [ "${TURNPIKE_SKIP_SHA256:-0}" != "1" ]; then
  curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"
  # Verify only our asset's line (the manifest also covers the Windows zip).
  (cd "$tmp" && grep -E "[[:space:]]$ASSET\$" SHA256SUMS | shasum -a 256 -c -)
fi

tar -xzf "$tmp/$ASSET" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 0755 "$tmp/turnpike" "$INSTALL_DIR/turnpike"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo
    echo "$INSTALL_DIR is not on your PATH. Add it once, e.g.:"
    echo "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc && source ~/.zshrc"
    ;;
esac

echo
echo "turnpike $version installed to $INSTALL_DIR/turnpike"
echo "Start the gateway with: turnpike serve --init"
