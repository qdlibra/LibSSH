#!/usr/bin/env bash
# 构建 → 打包（内含签名）→ 部署到 /Applications。
# GUI「启用全局 CLI」创建的 ~/.local/bin/LibSSH 符号链接指向 .app 内
# 二进制，部署后 AI 工具调用立即用上新构建，且签名恒定不触发钥匙串弹窗。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="LibSSH"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "This installer is macOS-only." >&2
    exit 1
fi

cargo build --release --manifest-path "$ROOT/Cargo.toml"
"$ROOT/scripts/package-macos-dmg.sh" "$ROOT/target/release/$APP_NAME" "$ROOT/dist"

rm -rf "/Applications/$APP_NAME.app"
ditto "$ROOT/dist/$APP_NAME.app" "/Applications/$APP_NAME.app"
echo "Installed: /Applications/$APP_NAME.app"
