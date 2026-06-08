#!/usr/bin/env sh
set -eu

APP_NAME="LibSSH"
APP_ID="LibSSH"
PREFIX="${PREFIX:-$HOME/.local}"
BIN_SOURCE="${1:-target/release/$APP_NAME}"
BIN_DIR="$PREFIX/bin"
APP_DIR="$PREFIX/share/applications"
ICON_DIR="$PREFIX/share/icons/hicolor/512x512/apps"

if [ ! -f "$BIN_SOURCE" ]; then
    echo "Binary not found: $BIN_SOURCE" >&2
    echo "Run cargo build --release, or pass the binary path as the first argument." >&2
    exit 1
fi

mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"

install -m 0755 "$BIN_SOURCE" "$BIN_DIR/$APP_NAME"
install -m 0644 "$(dirname "$0")/icon@512.png" "$ICON_DIR/$APP_ID.png"

cat > "$APP_DIR/$APP_ID.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=LibSSH
Comment=Lightweight Rust + Slint SSH terminal client
Exec=$BIN_DIR/$APP_NAME
Icon=$APP_ID
Terminal=false
Categories=Network;RemoteAccess;TerminalEmulator;
StartupNotify=true
StartupWMClass=LibSSH
EOF

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
fi

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache "$PREFIX/share/icons/hicolor" >/dev/null 2>&1 || true
fi

echo "Installed $APP_NAME to $BIN_DIR/$APP_NAME"
echo "Installed launcher to $APP_DIR/$APP_ID.desktop"
