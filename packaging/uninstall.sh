#!/bin/sh
# Remove Mentos Macro. Settings and profiles are kept unless --purge is given.
set -eu
PREFIX="${PREFIX:-$HOME/.local}"
rm -f "$PREFIX/bin/mentos-macro" \
      "$PREFIX/share/applications/mentos-macro.desktop" \
      "$PREFIX/share/icons/hicolor/scalable/apps/mentos-macro.svg" \
      "$PREFIX/share/icons/hicolor/256x256/apps/mentos-macro.png"
if [ "${1:-}" = "--purge" ]; then
    rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/mentos-macro"
    echo "Removed Mentos Macro and its settings."
else
    echo "Removed Mentos Macro. Settings remain in ${XDG_CONFIG_HOME:-$HOME/.config}/mentos-macro (use --purge to delete)."
fi
