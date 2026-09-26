#!/usr/bin/env bash
# Builds bp_photos and installs it for the current user, so it appears in the app launcher and in
# the file manager's "Open With" menu for photos (without becoming the default handler).
#
# Unlike macOS, libheif is taken from the distribution here: on Linux it is a normal shared
# library the package manager keeps up to date, and distributions already build it decode-capable.
#
# Needs: Rust, and the build dependencies listed in the README.
# Pass --prefix /usr/local (as root) to install system-wide instead of into ~/.local.
set -euo pipefail
cd "$(dirname "$0")/.."

PREFIX="${HOME}/.local"
if [ "${1:-}" = "--prefix" ]; then
  PREFIX="${2:?--prefix needs a directory}"
fi

step() { printf '\n\033[1m%s\033[0m\n' "$*"; }

step "1/3  Build"
cargo build --release

step "2/3  Install into $PREFIX"
install -Dm755 target/release/bp_photos "$PREFIX/bin/bp_photos"
install -Dm644 assets/bp_photos.desktop "$PREFIX/share/applications/bp_photos.desktop"
# hicolor wants the directory to match the icon's real size: assets/icon.png is 1024x1024.
install -Dm644 assets/icon.png "$PREFIX/share/icons/hicolor/1024x1024/apps/bp_photos.png"

step "3/3  Refresh the desktop database"
# Both are caches: the install is already valid without them, so don't fail if they're missing.
update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true

printf '\nInstalled %s\n' "$PREFIX/bin/bp_photos"
case ":$PATH:" in
  *":$PREFIX/bin:"*) ;;
  *) printf 'Note: %s/bin is not on your PATH.\n' "$PREFIX" ;;
esac
