#!/bin/sh
# Regenerates the client-certificate fixtures used by tests/client_certs.rs
# (and copied to crates/merpati-grpc/tests/fixtures/certs). Needs OpenSSL 3
# with the legacy provider (for the RC2-40 bundle). Password: "merpati".
set -eu
cd "$(dirname "$0")"
OPENSSL="${OPENSSL:-openssl}"
DAYS=36500

cat > client.ext <<'EXT'
basicConstraints=CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=clientAuth
EXT

"$OPENSSL" req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days "$DAYS" \
  -subj "/CN=Merpati Test CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"

for kind in rsa ec; do
  if [ "$kind" = rsa ]; then
    "$OPENSSL" genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "client-$kind.key"
  else
    "$OPENSSL" genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "client-$kind.key"
  fi
  "$OPENSSL" req -new -key "client-$kind.key" -out "client-$kind.csr" -subj "/CN=merpati-client-$kind"
  "$OPENSSL" x509 -req -in "client-$kind.csr" -CA ca.pem -CAkey ca.key -CAcreateserial \
    -out "client-$kind.pem" -days "$DAYS" -extfile client.ext
done

# OpenSSL 3 default: PBES2 (PBKDF2-HMAC-SHA256, AES-256-CBC), HMAC-SHA256 MAC.
"$OPENSSL" pkcs12 -export -in client-rsa.pem -inkey client-rsa.key -certfile ca.pem \
  -name "merpati rsa" -passout pass:merpati -out client-rsa-aes.p12
"$OPENSSL" pkcs12 -export -in client-ec.pem -inkey client-ec.key -certfile ca.pem \
  -name "merpati ec" -passout pass:merpati -out client-ec-aes.pfx
# Legacy (OpenSSL 1.x / Windows XP era): RC2-40 certificates, 3DES key, SHA-1 MAC.
"$OPENSSL" pkcs12 -export -legacy -in client-rsa.pem -inkey client-rsa.key -certfile ca.pem \
  -passout pass:merpati -out client-rsa-legacy.p12
# 3DES for both bags, SHA-1 MAC (Windows certificate export "TripleDES-SHA1").
"$OPENSSL" pkcs12 -export -in client-rsa.pem -inkey client-rsa.key -certfile ca.pem \
  -certpbe PBE-SHA1-3DES -keypbe PBE-SHA1-3DES -macalg sha1 -passout pass:merpati -out client-rsa-3des.p12
# Empty password.
"$OPENSSL" pkcs12 -export -in client-ec.pem -inkey client-ec.key -passout pass: -out client-ec-nopass.p12
# Encrypted PKCS#8 PEM key (PBES2 / AES-256-CBC).
"$OPENSSL" pkcs8 -topk8 -v2 aes-256-cbc -in client-ec.key -passout pass:merpati -out client-ec-encrypted.key

rm -f ca.key ca.srl client-*.csr client.ext
