#!/usr/bin/env bash
set -euo pipefail

APP_NAME="LibSSH"
BUNDLE_ID="dev.libssh.LibSSH"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_PATH="${1:-$ROOT/target/release/$APP_NAME}"
DIST_DIR="${2:-$ROOT/dist}"
ARCH="$(uname -m)"
# Keep the bundle version in lock-step with Cargo.toml so every release's
# Info.plist reports the correct version (the release pipeline tags by version).
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
VERSION="${VERSION:-0.0.0}"
APP_DIR="$DIST_DIR/$APP_NAME.app"
DMG_ROOT="$DIST_DIR/dmg-root"
CONTENTS="$APP_DIR/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"
DMG="$DIST_DIR/$APP_NAME-macos-$ARCH.dmg"

if [[ ! -f "$BIN_PATH" ]]; then
    echo "Binary not found: $BIN_PATH" >&2
    echo "Run cargo build --release first, or pass the binary path as the first argument." >&2
    exit 1
fi

mkdir -p "$MACOS" "$RESOURCES"
rm -rf "$APP_DIR"
mkdir -p "$MACOS" "$RESOURCES"

install -m 0755 "$BIN_PATH" "$MACOS/$APP_NAME"

if [[ -f "$ROOT/assets/LibSSH.icns" ]]; then
    install -m 0644 "$ROOT/assets/LibSSH.icns" "$RESOURCES/LibSSH.icns"
fi

cat > "$CONTENTS/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleIconFile</key>
    <string>LibSSH</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

# 有本地签名证书就签 .app（钥匙串信任跨构建恒定）；没有（如 CI）保持 adhoc。
if [[ "$(uname -s)" == "Darwin" ]] \
    && security find-certificate -c "LibSSH Dev Signing" >/dev/null 2>&1; then
    "$ROOT/scripts/codesign-macos.sh" "$APP_DIR"
else
    echo "Signing certificate 'LibSSH Dev Signing' not found; packaging with adhoc signature." >&2
fi

rm -f "$DMG"
rm -rf "$DMG_ROOT"
mkdir -p "$DMG_ROOT"
cp -R "$APP_DIR" "$DMG_ROOT/$APP_NAME.app"
ln -s /Applications "$DMG_ROOT/Applications"

hdiutil create -volname "$APP_NAME" -srcfolder "$DMG_ROOT" -ov -format UDZO "$DMG"
echo "$DMG"
