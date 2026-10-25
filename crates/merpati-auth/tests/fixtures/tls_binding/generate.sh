#!/bin/sh
# Regenerates the self-signed certificates used by the tls-server-end-point
# unit tests in src/channel_binding.rs: one per signature algorithm whose hash
# selection (RFC 5929 section 4.1) is not covered by rcgen-generated
# certificates. The keys are throwaway and not kept. Needs OpenSSL 3.
set -eu
cd "$(dirname "$0")"
OPENSSL="${OPENSSL:-openssl}"
DAYS=36500

"$OPENSSL" genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out rsa.key
"$OPENSSL" genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out ec.key
"$OPENSSL" genpkey -algorithm ED448 -out ed448.key

cert() { # name key [req options...]
  name=$1 key=$2
  shift 2
  "$OPENSSL" req -x509 -new -key "$key" -out "$name.pem" -days "$DAYS" -subj "/CN=merpati-$name" "$@"
}

cert rsa_md5 rsa.key -md5
cert rsa_sha1 rsa.key -sha1
cert ecdsa_sha1 ec.key -sha1
cert rsa_sha224 rsa.key -sha224
cert rsa_sha512 rsa.key -sha512
cert rsa_pss_sha384 rsa.key -sha384 -sigopt rsa_padding_mode:pss -sigopt rsa_pss_saltlen:-1
cert ed448 ed448.key

for f in *.pem; do
  printf '%s: ' "$f"
  "$OPENSSL" x509 -in "$f" -noout -text | sed -n 's/^ *Signature Algorithm: //p' | head -1
done
rm -f rsa.key ec.key ed448.key
