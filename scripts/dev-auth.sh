#!/usr/bin/env bash
#
# Auth Service de desenvolvimento.
#
# Gera um par de chaves RSA, publica a JWKS correspondente e assina tokens, para
# que o gateway possa ser exercitado com autenticação real sem depender de um Auth
# Service de verdade.
#
# A chave é gerada na sua máquina e fica em .dev-auth/, que é ignorada pelo git.
# Nenhum material criptográfico é versionado, e nenhum destes tokens vale fora do
# seu ambiente de desenvolvimento.
#
#   ./scripts/dev-auth.sh init                      gera a chave e a JWKS
#   ./scripts/dev-auth.sh token                     token com scope user.read
#   ./scripts/dev-auth.sh token "user.read admin"   token com os scopes dados
#   ./scripts/dev-auth.sh token "user.read" -60     token que expirou há 60s

set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.dev-auth"
KEY="$DIR/key.pem"
KID="dev-key-1"
ISSUER="https://auth.dev.local"
AUDIENCE="rust-gateway"

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

require() {
    command -v "$1" >/dev/null 2>&1 || { echo "erro: $1 não encontrado no PATH" >&2; exit 1; }
}

init() {
    require openssl
    require xxd

    mkdir -p "$DIR"
    openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$KEY" 2>/dev/null
    chmod 600 "$KEY"

    # A JWKS publica o módulo e o expoente da chave, em base64url. É exatamente
    # o que um Auth Service real serve em /.well-known/jwks.json.
    local modulus n e
    modulus=$(openssl rsa -in "$KEY" -noout -modulus | cut -d= -f2)
    n=$(printf '%s' "$modulus" | xxd -r -p | b64url)
    e=$(printf '\x01\x00\x01' | b64url)   # 65537, o expoente público padrão

    printf '{"keys":[{"kty":"RSA","use":"sig","kid":"%s","alg":"RS256","n":"%s","e":"%s"}]}\n' \
        "$KID" "$n" "$e" > "$DIR/jwks.json"

    echo "chave em      $KEY"
    echo "JWKS em       $DIR/jwks.json"
    echo "issuer        $ISSUER"
    echo "audience      $AUDIENCE"
}

token() {
    require openssl

    [ -f "$KEY" ] || { echo "erro: rode './scripts/dev-auth.sh init' primeiro" >&2; exit 1; }

    local scopes="${1:-user.read}"
    local offset="${2:-3600}"
    local now exp header payload signature
    now=$(date +%s)
    exp=$((now + offset))

    header=$(printf '{"alg":"RS256","typ":"JWT","kid":"%s"}' "$KID" | b64url)
    payload=$(printf '{"sub":"dev-user-1","iss":"%s","aud":"%s","iat":%s,"exp":%s,"scope":"%s"}' \
        "$ISSUER" "$AUDIENCE" "$now" "$exp" "$scopes" | b64url)

    signature=$(printf '%s.%s' "$header" "$payload" \
        | openssl dgst -sha256 -sign "$KEY" -binary | b64url)

    printf '%s.%s.%s\n' "$header" "$payload" "$signature"
}

case "${1:-}" in
    init)  init ;;
    token) shift; token "$@" ;;
    *)     sed -n '3,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
