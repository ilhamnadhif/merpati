# ASAP test tokens

`atlassian_jwt_auth_tokens.json` holds tokens signed by the reference library
atlassian-jwt-auth 22.0.1 (<https://github.com/atlassian/asap-authentication-python>,
MIT license) with `generate_tokens.py gen` (run with `python -I`). The script
fixes the signer's clock (`_now`) and the random part of its `jti`. Every
token was also accepted by the library's `JWTAuthVerifier`.

Keys are the crate's test fixtures (`../rsa_pkcs8.pem`, `../ec_p256.pem`,
`../ec_p384.pem` and their public halves), generated for these tests only.

`asap_matches_atlassian_jwt_auth` in `src/jwt.rs` signs the same inputs with
Merpati (reusing the reference `jti`): RS256/384/512 tokens are identical
byte for byte; PS* and ES* signatures are randomized, so header and claims
must be identical and the signature valid.

Merpati's own tokens (random `jti`, current time) for all of these cases were
checked with `generate_tokens.py verify`, which runs `JWTAuthVerifier`
(signature, `aud`, `kid` owned by `iss`, `sub` = `iss`, lifetime <= 1 hour,
`jti` present) with a wide clock leeway.
