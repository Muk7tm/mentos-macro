#!/bin/sh
# Install Mentos Macro for the current user (default prefix: ~/.local).
#   PREFIX=/usr/local sudo -E ./packaging/install.sh   for a system-wide install
set -eu
cd "$(dirname "$0")/.."
PREFIX="${PREFIX:-$HOME/.local}"
BINDIR="$PREFIX/bin"

if [ -x dist/mentos-macro ] && [ "${REBUILD:-0}" != "1" ]; then
    BIN=dist/mentos-macro
else
    cargo build --release --locked
    BIN=target/release/mentos-macro
fi

install -Dm755 "$BIN" "$BINDIR/mentos-macro"
install -Dm644 assets/icon.svg "$PREFIX/share/icons/hicolor/scalable/apps/mentos-macro.svg"
install -Dm644 assets/icon.png "$PREFIX/share/icons/hicolor/256x256/apps/mentos-macro.png"
mkdir -p "$PREFIX/share/applications"
sed "s|@BINDIR@|$BINDIR|g" packaging/mentos-macro.desktop > "$PREFIX/share/applications/mentos-macro.desktop"
chmod 644 "$PREFIX/share/applications/mentos-macro.desktop"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$PREFIX/share/applications" || true

echo "Installed $BINDIR/mentos-macro"
case ":$PATH:" in *":$BINDIR:"*) ;; *) echo "Note: $BINDIR is not in PATH; the desktop entry uses the full path." ;; esac
