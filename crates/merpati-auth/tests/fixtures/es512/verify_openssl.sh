#!/bin/sh
# Verifies an ES512 JWS (RFC 7515 compact form) with the OpenSSL command line
# only, as an independent check of Merpati's P-521 signatures:
#   verify_openssl.sh <token> <public key PEM> <empty work directory>
# The JWS signature is the raw R || S (66 + 66 bytes); OpenSSL wants an ASN.1
# ECDSA-Sig-Value, built here with `asn1parse -genconf`. Prints "Verified OK"
# and exits 0 for a valid signature. Run by `es512_token_verifies_with_openssl`
# in src/jwt.rs; by hand, sign a token in the app with
# tests/fixtures/ec_p521.pem and pass tests/fixtures/ec_p521_pub.pem.
set -eu
token=$1 pub=$2 work=$3
OPENSSL="${OPENSSL:-openssl}"

signed=${token%.*}
sig=${token##*.}
# base64url -> base64 with padding.
sig=$(printf '%s' "$sig" | tr '_-' '/+')
while [ $(( ${#sig} % 4 )) -ne 0 ]; do sig="$sig="; done
hex=$(printf '%s' "$sig" | "$OPENSSL" base64 -d -A | od -An -v -tx1 | tr -d ' \n')
if [ ${#hex} -ne 264 ]; then
  echo "signature is $(( ${#hex} / 2 )) bytes, expected 132" >&2
  exit 1
fi
r=$(printf '%s' "$hex" | cut -c1-132)
s=$(printf '%s' "$hex" | cut -c133-264)
printf 'asn1=SEQUENCE:sig\n[sig]\nr=INTEGER:0x%s\ns=INTEGER:0x%s\n' "$r" "$s" > "$work/sig.cnf"
"$OPENSSL" asn1parse -genconf "$work/sig.cnf" -out "$work/sig.der" -noout
printf '%s' "$signed" | "$OPENSSL" dgst -sha512 -verify "$pub" -signature "$work/sig.der"
