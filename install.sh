#!/usr/bin/env bash
# turnpike installer — macOS (Apple Silicon). Windows: use install.ps1.
#
# Downloads the latest prebuilt turnpike binary from GitHub Releases and
# installs it to ~/.local/bin (override with TURNPIKE_INSTALL_DIR). Then offers
# to install the Desktop app.
#
# Environment overrides:
#   TURNPIKE_VERSION=v0.1.2   pin a release instead of "latest"
#   TURNPIKE_INSTALL_DIR=...  install somewhere other than ~/.local/bin
#   TURNPIKE_SKIP_SHA256=1    skip checksum verification (not recommended)
#   TURNPIKE_DESKTOP=0|1      skip / accept the Desktop prompt without asking
set -euo pipefail

REPO="aslamplr/turnpike"
ASSET="turnpike-aarch64-apple-darwin.tar.gz"
DESKTOP_ASSET="turnpike_desktop-aarch64-apple-darwin.dmg"
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

# --- Desktop app (optional) -------------------------------------------------

# `curl … | bash` means stdin is this script, so a bare `read` would consume the
# script's own text instead of the user's answer. Prompt on the controlling
# terminal instead, and treat its absence (CI, no tty) as "skip" rather than
# hanging or silently answering.
DESKTOP_APPS="${TURNPIKE_APP_DIR:-/Applications}"

desktop_wanted() {
  case "${TURNPIKE_DESKTOP:-}" in
    1|yes|true)  return 0 ;;
    0|no|false)  return 1 ;;
    "")          ;;
    *)           echo "warning: TURNPIKE_DESKTOP='$TURNPIKE_DESKTOP' not understood; ignoring" >&2 ;;
  esac
  [ -r /dev/tty ] || return 1
  printf '\nInstall the turnpike Desktop app (menu-bar item + config window)? [y/N] ' > /dev/tty
  local answer
  read -r answer < /dev/tty || return 1
  case "$answer" in [Yy]*) return 0 ;; *) return 1 ;; esac
}

if desktop_wanted; then
  dmg="$tmp/$DESKTOP_ASSET"
  echo
  echo "Downloading the Desktop app ($version)..."
  curl -fsSL "$base/$DESKTOP_ASSET" -o "$dmg"

  if [ "${TURNPIKE_SKIP_SHA256:-0}" != "1" ]; then
    # A separate manifest, because the desktop and CLI halves publish on their
    # own jobs and one can fail without the other. Missing it is a warning, not
    # an error: the CLI half already succeeded and hashing the wrong file is
    # worse than not hashing this one.
    if curl -fsSL "$base/SHA256SUMS-desktop" -o "$tmp/SHA256SUMS-desktop"; then
      (cd "$tmp" && grep -E "[[:space:]]$DESKTOP_ASSET\$" SHA256SUMS-desktop | shasum -a 256 -c -)
    else
      echo "warning: SHA256SUMS-desktop is not published for $version; skipping its checksum" >&2
    fi
  fi

  # Mount, copy, detach. `hdiutil attach` gives us a mount point we control, so
  # the copy cannot race another image and nothing depends on the volume name.
  mount="$(mktemp -d)"
  if hdiutil attach "$dmg" -nobrowse -readonly -mountpoint "$mount" >/dev/null; then
    if [ -d "$mount/turnpike.app" ]; then
      # Ask Finder first, so the common case keeps the usual drag-to-Applications
      # affordance; fall back to a plain copy when there is no GUI to answer
      # (headless, or Automation permission refused).
      if ! osascript -e "tell application \"Finder\" to copy (POSIX file \"$mount/turnpike.app\" as alias) to (POSIX file \"$DESKTOP_APPS\" as alias)" 2>/dev/null; then
        mkdir -p "$DESKTOP_APPS"
        rm -rf "$DESKTOP_APPS/turnpike.app"
        cp -R "$mount/turnpike.app" "$DESKTOP_APPS/turnpike.app"
      fi
    fi
    hdiutil detach "$mount" >/dev/null || true
  else
    echo "warning: could not mount $DESKTOP_ASSET; open it by hand: $dmg" >&2
  fi

  echo
  echo "turnpike Desktop installed to $DESKTOP_APPS/turnpike.app"
  # The app is ad-hoc signed and not notarized, so Gatekeeper quarantines it on
  # the first open. Saying so here is cheaper than the silent nothing that a
  # double-click otherwise does.
  echo "The app is ad-hoc signed (not notarized): on first open, right-click it"
  echo "and choose Open once. See docs/desktop.md."
fi
