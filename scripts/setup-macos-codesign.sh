#!/usr/bin/env bash
# 一次性：创建并信任本地自签名代码签名证书「LibSSH Dev Signing」。
# 之后 scripts/codesign-macos.sh 用它签名，钥匙串信任跨构建恒定，
# CLI 读会话密码不再每次构建后重新弹授权框。幂等：已存在即跳过。
set -euo pipefail

CERT_NAME="LibSSH Dev Signing"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "Code signing setup is only needed on macOS; nothing to do." >&2
    exit 0
fi

if security find-certificate -c "$CERT_NAME" >/dev/null 2>&1; then
    echo "Certificate '$CERT_NAME' already exists; nothing to do."
    exit 0
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
P12_PASS="$(uuidgen)"

cat > "$TMP_DIR/cert.cnf" <<'EOF'
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = LibSSH Dev Signing
[ext]
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
basicConstraints = critical,CA:false
EOF

openssl req -x509 -newkey rsa:2048 -days 3650 -nodes \
    -keyout "$TMP_DIR/key.pem" -out "$TMP_DIR/cert.pem" \
    -config "$TMP_DIR/cert.cnf"

openssl pkcs12 -export -inkey "$TMP_DIR/key.pem" -in "$TMP_DIR/cert.pem" \
    -out "$TMP_DIR/cert.p12" -passout "pass:$P12_PASS"

security import "$TMP_DIR/cert.p12" \
    -k "$HOME/Library/Keychains/login.keychain-db" \
    -P "$P12_PASS" -T /usr/bin/codesign

echo "Marking the certificate as trusted for code signing (admin password required)..."
sudo security add-trusted-cert -d -r trustRoot -p codeSign \
    -k /Library/Keychains/System.keychain "$TMP_DIR/cert.pem"

echo "Certificate '$CERT_NAME' created and trusted."
echo "Note: the first codesign run may prompt once for keychain access — choose 'Always Allow'."
