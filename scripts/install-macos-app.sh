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

# 发版安装走 clean build，兑现 build.rs 中「发版走 clean build」的既定假设：先清掉
# release 编译产物再重建。否则增量构建可能复用 stale 产物（已知两类：Slint UI 的
# build-script codegen、链接期 undefined symbols），打出「源码已修、产物却是旧版」
# 的包——这正是快速连接中文连接名反复消失的真因（stale 增量产物，而非 UI 代码本身；
# 06-19/06-21 两次 UI 修复都正确，却因发版增量构建被旧 codegen 覆盖而复发）。
# 发版属低频操作，全量重编 release 的代价可接受。
cargo clean --release --manifest-path "$ROOT/Cargo.toml"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
"$ROOT/scripts/package-macos-dmg.sh" "$ROOT/target/release/$APP_NAME" "$ROOT/dist"

rm -rf "/Applications/$APP_NAME.app"
ditto "$ROOT/dist/$APP_NAME.app" "/Applications/$APP_NAME.app"
echo "Installed: /Applications/$APP_NAME.app"
