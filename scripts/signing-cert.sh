#!/usr/bin/env bash
# 固定の自己署名証明書を login キーチェーンに1つ作る(**このマシンで一度だけ**)。
#
# なぜ要るか: install.sh の ad-hoc 署名は身元が中身のハッシュ(cdhash)なので、ビルドし直す
# たびに macOS からは「別のアプリ」に見える。TCC(デスクトップ/他アプリのデータ)もファイア
# ウォールも許可を出し直させる ← 起動のたびにダイアログが並ぶ原因。
# 身元の固定した証明書で署名すれば、要求は `identifier + 証明書のリーフ` で一致するので、
# 何度ビルドし直しても許可はそのまま残る。
#
# 途中で2回、macOS がパスワードを聞く(キーチェーンの信頼設定と鍵の使用許可)。sudo は要らない。
set -euo pipefail

name="${SIGN_IDENTITY:-agentgw dev}"
keychain="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -v -p codesigning 2>/dev/null | grep -qF "$name"; then
  echo "==> すでに使えます: $name"
  exit 0
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

if security find-certificate -c "$name" >/dev/null 2>&1; then
  # 証明書はもう居る(=前回、信頼設定の手前で止まった)。作り直さず取り出すだけ
  echo "==> 証明書はすでにあります。信頼設定だけ入れます"
  security find-certificate -c "$name" -p > "$tmp/cert.pem"
else
  echo "==> 証明書を作る: $name"
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -keyout "$tmp/key.pem" -out "$tmp/cert.pem" -subj "/CN=$name" \
    -addext "basicConstraints=critical,CA:false" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=critical,codeSigning" 2>/dev/null
  # **パスワードは空にしない。** security(1) は空パスワードの p12 を「MAC verification failed」で
  # 撥ねる。-legacy も要る(既定の AES-256 な p12 を security は読めない)
  openssl pkcs12 -export -legacy -passout pass:tmp \
    -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -out "$tmp/id.p12" 2>/dev/null
  # -A: codesign が鍵を使うたびに許可を聞かれないようにする
  security import "$tmp/id.p12" -k "$keychain" -P tmp -A -T /usr/bin/codesign
fi

echo "==> コード署名用に信頼する(パスワードを聞かれます)"
security add-trusted-cert -r trustRoot -p codeSign -k "$keychain" "$tmp/cert.pem"

security find-identity -v -p codesigning | grep -F "$name"
echo
echo "できました。あとは ./scripts/install.sh がこの証明書で署名します。"
echo "入れ直した直後の1回だけダイアログが出て、それ以降は出ません。"
