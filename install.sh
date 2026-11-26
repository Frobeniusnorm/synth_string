#!/usr/bin/env bash
# Build and install Synth String for the current user (~/.local).
#   ./install.sh            install / update
#   ./install.sh --remove   uninstall
#
# The dock / dash icon comes from the installed .desktop file: GNOME matches the window's app id
# (dev.synthstring.SynthString) to dev.synthstring.SynthString.desktop and shows its Icon.
set -euo pipefail
cd "$(dirname "$0")"

APP_ID=dev.synthstring.SynthString
BIN="$HOME/.local/bin/synth-string"
DESKTOP="$HOME/.local/share/applications/$APP_ID.desktop"
ICON="$HOME/.local/share/icons/hicolor/scalable/apps/$APP_ID.svg"

refresh() {
    gtk4-update-icon-cache -q -t -f "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
    update-desktop-database -q "$HOME/.local/share/applications" 2>/dev/null || true
}

if [[ "${1:-}" == "--remove" ]]; then
    rm -f "$BIN" "$DESKTOP" "$ICON"
    refresh
    echo "Removed."
    exit 0
fi

cargo build --release
install -Dm755 target/release/synth-string "$BIN"
install -Dm644 data/icons/$APP_ID.svg "$ICON"
# absolute Exec path: ~/.local/bin is not always on the PATH of the desktop session
mkdir -p "$(dirname "$DESKTOP")"
sed -e "s|^Exec=synth-string|Exec=$BIN|" -e "s|^TryExec=.*|TryExec=$BIN|" \
    data/$APP_ID.desktop > "$DESKTOP"
chmod 644 "$DESKTOP"
refresh
echo "Installed. Launch “Synth String” from the app grid (or run: $BIN)."
