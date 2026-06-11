#!/usr/bin/env bash
# 用「LibSSH Dev Signing」证书 + 固定 identifier 签名二进制或 .app。
# identifier 必须与 package-macos-dmg.sh 的 BUNDLE_ID 一致：钥匙串信任
# 判定（designated requirement）= 证书 + identifier，跨构建恒定。
set -euo pipefail

CERT_NAME="LibSSH Dev Signing"
IDENTIFIER="dev.libssh.LibSSH"
TARGET="${1:?usage: codesign-macos.sh <binary-or-app>}"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "Code signing is only needed on macOS; skipping." >&2
    exit 0
fi

if ! security find-certificate -c "$CERT_NAME" >/dev/null 2>&1; then
    echo "Signing certificate '$CERT_NAME' not found." >&2
    echo "Run scripts/setup-macos-codesign.sh first." >&2
    exit 1
fi

codesign --force --sign "$CERT_NAME" --identifier "$IDENTIFIER" "$TARGET"
codesign --verify --strict "$TARGET"
echo "Signed: $TARGET ($IDENTIFIER)"
