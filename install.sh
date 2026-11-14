#!/bin/sh
# Installs the sparkline binary from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/be-rock/sparkline/main/install.sh | sh
#
# For a private repository, run it from a clone (or pipe it from `gh api`);
# downloads then go through an authenticated `gh` CLI.
#
# Environment:
#   SPARKLINE_REPO         owner/name            (default: be-rock/sparkline)
#   SPARKLINE_VERSION      tag, e.g. v0.1.0      (default: latest release)
#   SPARKLINE_INSTALL_DIR  install directory     (default: ~/.local/bin)
set -eu

REPO="${SPARKLINE_REPO:-be-rock/sparkline}"
VERSION="${SPARKLINE_VERSION:-latest}"
INSTALL_DIR="${SPARKLINE_INSTALL_DIR:-$HOME/.local/bin}"

err() { echo "install.sh: $*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-gnu ;;
  *) err "unsupported OS $(uname -s); build from source with 'cargo install --path crates/sparkline'" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) err "unsupported CPU $(uname -m)" ;;
esac
ASSET="sparkline-$arch-$os.tar.gz"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

download() {
  # Prefer gh (works for private repos); fall back to public URLs.
  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    tag_arg=""
    [ "$VERSION" = latest ] || tag_arg="$VERSION"
    # shellcheck disable=SC2086
    gh release download $tag_arg -R "$REPO" -p "$ASSET" -p "$ASSET.sha256" -D "$TMP"
  else
    if [ "$VERSION" = latest ]; then
      base="https://github.com/$REPO/releases/latest/download"
    else
      base="https://github.com/$REPO/releases/download/$VERSION"
    fi
    curl -fsSL "$base/$ASSET" -o "$TMP/$ASSET" || err "download failed: $base/$ASSET"
    curl -fsSL "$base/$ASSET.sha256" -o "$TMP/$ASSET.sha256" || err "download failed: $base/$ASSET.sha256"
  fi
}

echo "Downloading $ASSET ($VERSION) from $REPO..."
download

(
  cd "$TMP"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 -c "$ASSET.sha256" >/dev/null
  else
    sha256sum -c "$ASSET.sha256" >/dev/null
  fi
) || err "checksum mismatch for $ASSET"

tar -xzf "$TMP/$ASSET" -C "$TMP"
mkdir -p "$INSTALL_DIR"
install -m 0755 "$TMP/sparkline-$arch-$os/sparkline" "$INSTALL_DIR/sparkline"
echo "Installed $("$INSTALL_DIR/sparkline" --version) to $INSTALL_DIR/sparkline"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "Note: $INSTALL_DIR is not on your PATH. Add it, e.g.: export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
