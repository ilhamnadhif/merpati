//! JWT Bearer and Atlassian ASAP: sign a token and attach it.
//!
//! `jsonwebtoken` signs every algorithm except ES512 (ECDSA P-521 with
//! SHA-512, RFC 7518 section 3.4), which `p521` signs here.

use base64::Engine;
use jsonwebtoken::{Algorithm, EncodingKey};
use serde_json::{Map, Value, json};

use merpati_core::{Auth, PreparedRequest};

use crate::error::AuthError;
use crate::util::{B64, B64_URL, der_tlv, json_object_param, now_secs, param};

/// A JWS algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Alg {
    /// Signed by `jsonwebtoken`.
    Lib(Algorithm),
    Es512,
}

pub(crate) fn parse_algorithm(s: &str) -> Result<Alg, AuthError> {
    let a = match s.trim().to_ascii_uppercase().as_str() {
        "" | "HS256" => Algorithm::HS256,
        "HS384" => Algorithm::HS384,
        "HS512" => Algorithm::HS512,
        "RS256" => Algorithm::RS256,
        "RS384" => Algorithm::RS384,
        "RS512" => Algorithm::RS512,
        "PS256" => Algorithm::PS256,
        "PS384" => Algorithm::PS384,
        "PS512" => Algorithm::PS512,
        "ES256" => Algorithm::ES256,
        "ES384" => Algorithm::ES384,
        "ES512" => return Ok(Alg::Es512),
        "EDDSA" => Algorithm::EdDSA,
        other => return Err(AuthError::invalid(format!("unsupported JWT algorithm `{other}`"))),
    };
    Ok(Alg::Lib(a))
}

fn alg_name(a: Alg) -> &'static str {
    let Alg::Lib(a) = a else { return "ES512" };
    match a {
        Algorithm::HS256 => "HS256",
        Algorithm::HS384 => "HS384",
        Algorithm::HS512 => "HS512",
        Algorithm::RS256 => "RS256",
        Algorithm::RS384 => "RS384",
        Algorithm::RS512 => "RS512",
        Algorithm::PS256 => "PS256",
        Algorithm::PS384 => "PS384",
        Algorithm::PS512 => "PS512",
        Algorithm::ES256 => "ES256",
        Algorithm::ES384 => "ES384",
        Algorithm::EdDSA => "EdDSA",
    }
}

/// PEM keys pasted into a single-line field often lose their newlines or
/// carry literal `\n`; restore them.
pub(crate) fn normalize_pem(pem: &str) -> String {
    let pem = pem.trim().replace("\\n", "\n");
    if pem.contains('\n') {
        return pem;
    }
    // "-----BEGIN X----- base64 -----END X-----" on one line.
    let Some(begin_end) = pem.find("-----").and_then(|s| pem[s + 5..].find("-----").map(|e| s + 5 + e + 5)) else {
        return pem;
    };
    let Some(end_start) = pem.rfind("-----END") else { return pem };
    if end_start < begin_end {
        return pem;
    }
    let body: String = pem[begin_end..end_start].split_whitespace().collect();
    let lines: Vec<String> = body.as_bytes().chunks(64).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
    format!("{}\n{}\n{}", &pem[..begin_end], lines.join("\n"), &pem[end_start..])
}

/// A key ready to sign with, and its algorithm.
pub(crate) enum SigningKey {
    Lib(Algorithm, EncodingKey),
    Es512(p521::ecdsa::SigningKey),
}

impl SigningKey {
    fn alg(&self) -> Alg {
        match self {
            SigningKey::Lib(a, _) => Alg::Lib(*a),
            SigningKey::Es512(_) => Alg::Es512,
        }
    }
}

pub(crate) fn encoding_key(
    alg: Alg,
    secret: &str,
    secret_b64: bool,
    private_key: &str,
) -> Result<SigningKey, AuthError> {
    let alg = match alg {
        Alg::Lib(a) => a,
        Alg::Es512 => return es512_key(private_key).map(SigningKey::Es512),
    };
    lib_encoding_key(alg, secret, secret_b64, private_key).map(|k| SigningKey::Lib(alg, k))
}

fn lib_encoding_key(
    alg: Algorithm,
    secret: &str,
    secret_b64: bool,
    private_key: &str,
) -> Result<EncodingKey, AuthError> {
    use Algorithm::*;
    match alg {
        HS256 | HS384 | HS512 => {
            if secret_b64 {
                EncodingKey::from_base64_secret(secret.trim())
                    .map_err(|e| AuthError::Jwt(format!("secret is not valid base64: {e}")))
            } else {
                Ok(EncodingKey::from_secret(secret.as_bytes()))
            }
        }
        RS256 | RS384 | RS512 | PS256 | PS384 | PS512 => {
            EncodingKey::from_rsa_pem(normalize_pem(private_key).as_bytes()).map_err(|e| AuthError::Key(e.to_string()))
        }
        ES256 | ES384 => {
            EncodingKey::from_ec_pem(normalize_pem(private_key).as_bytes()).map_err(|e| AuthError::Key(e.to_string()))
        }
        EdDSA => {
            EncodingKey::from_ed_pem(normalize_pem(private_key).as_bytes()).map_err(|e| AuthError::Key(e.to_string()))
        }
    }
}

const ENCRYPTED_KEY: &str = "encrypted private keys are not supported; export it unencrypted";

/// The body of the first `-----BEGIN <label>-----` block of `pem`, decoded.
/// Lines may have any length (unlike the strict RFC 7468 parsers).
fn pem_der(pem: &str, label: &str) -> Option<Result<Vec<u8>, AuthError>> {
    let begin = format!("-----BEGIN {label}-----");
    let start = pem.find(&begin)? + begin.len();
    let len = pem[start..].find(&format!("-----END {label}-----"))?;
    let body = &pem[start..start + len];
    if body.contains(':') {
        // RFC 1421 headers (Proc-Type, DEK-Info) of an encrypted key.
        return Some(Err(AuthError::Key(ENCRYPTED_KEY.into())));
    }
    let b64: String = body.split_whitespace().collect();
    Some(B64.decode(b64).map_err(|_| AuthError::Key(format!("the {label} PEM block is not valid base64"))))
}

/// secp521r1 (1.3.132.0.35), DER contents.
const P521_OID: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x23];

/// The named curve OID in the parameters of a SEC1 `ECPrivateKey`, if any.
fn sec1_curve(der: &[u8]) -> Option<&[u8]> {
    let (_, mut fields, _) = der_tlv(der)?;
    while let Some((tag, contents, rest)) = der_tlv(fields) {
        // parameters [0] EXPLICIT ECParameters (namedCurve OID).
        if tag == 0xa0 {
            return der_tlv(contents).filter(|(t, _, _)| *t == 0x06).map(|(_, oid, _)| oid);
        }
        fields = rest;
    }
    None
}

/// A P-521 private key from PKCS#8 (`PRIVATE KEY`) or SEC1 (`EC PRIVATE
/// KEY`, also after the `EC PARAMETERS` block `openssl ecparam` writes) PEM.
fn es512_key(private_key: &str) -> Result<p521::ecdsa::SigningKey, AuthError> {
    use p521::pkcs8::DecodePrivateKey;
    let pem = normalize_pem(private_key);
    let wrong_key = |e: &dyn std::fmt::Display| AuthError::Key(format!("ES512 needs a P-521 (secp521r1) EC key: {e}"));
    let secret = if let Some(der) = pem_der(&pem, "EC PRIVATE KEY") {
        let der = der?;
        // The parser does not check the curve itself.
        if sec1_curve(&der).is_some_and(|oid| oid != P521_OID) {
            return Err(wrong_key(&"the key is on another curve"));
        }
        p521::SecretKey::from_sec1_der(&der).map_err(|e| wrong_key(&e))?
    } else if let Some(der) = pem_der(&pem, "PRIVATE KEY") {
        p521::SecretKey::from_pkcs8_der(&der?).map_err(|e| wrong_key(&e))?
    } else if pem.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----") {
        return Err(AuthError::Key(ENCRYPTED_KEY.into()));
    } else {
        return Err(AuthError::Key(
            "ES512 needs a P-521 private key PEM (BEGIN PRIVATE KEY or BEGIN EC PRIVATE KEY)".into(),
        ));
    };
    p521::ecdsa::SigningKey::from_bytes(&secret.to_bytes()).map_err(|e| wrong_key(&e))
}

/// Encodes `header.payload` and signs it. The header always gets `alg` (and
/// `typ: JWT` unless overridden) so arbitrary extra header fields are kept.
pub(crate) fn sign(
    mut header: Map<String, Value>,
    payload: &Map<String, Value>,
    key: &SigningKey,
) -> Result<String, AuthError> {
    header.insert("alg".into(), json!(alg_name(key.alg())));
    header.entry("typ").or_insert(json!("JWT"));
    let h = B64_URL.encode(serde_json::to_vec(&Value::Object(header)).unwrap_or_default());
    let p = B64_URL.encode(serde_json::to_vec(&Value::Object(payload.clone())).unwrap_or_default());
    let message = format!("{h}.{p}");
    let sig = match key {
        SigningKey::Lib(alg, key) => {
            jsonwebtoken::crypto::sign(message.as_bytes(), key, *alg).map_err(|e| AuthError::Jwt(e.to_string()))?
        }
        SigningKey::Es512(key) => {
            use p521::ecdsa::signature::Signer;
            // JWS wants the fixed-size R || S (66 + 66 bytes), not DER.
            let sig: p521::ecdsa::Signature =
                key.try_sign(message.as_bytes()).map_err(|e| AuthError::Jwt(e.to_string()))?;
            B64_URL.encode(sig.to_bytes())
        }
    };
    Ok(format!("{message}.{sig}"))
}

fn place(req: &mut PreparedRequest, token: &str, prefix: &str, to_query: bool, query_key: &str) {
    if to_query {
        let key = if query_key.is_empty() { "token" } else { query_key };
        req.add_query(key, token);
    } else if prefix.is_empty() {
        req.set_header("Authorization", token.to_string());
    } else {
        req.set_header("Authorization", format!("{prefix} {token}"));
    }
}

pub(crate) fn jwt_token(auth: &Auth) -> Result<String, AuthError> {
    let alg = parse_algorithm(auth.get("algorithm"))?;
    let key =
        encoding_key(alg, auth.get("secret"), auth.get_bool("isSecretBase64Encoded", false), auth.get("privateKey"))?;
    let payload = json_object_param(auth, "payload")?;
    let header = json_object_param(auth, "header")?;
    sign(header, &payload, &key)
}

pub(crate) fn apply_jwt(req: &mut PreparedRequest) -> Result<(), AuthError> {
    let auth = req.auth.clone();
    let token = jwt_token(&auth)?;
    let prefix = match auth.params.get("headerPrefix") {
        None => "Bearer",
        Some(_) => param(&auth, "headerPrefix"),
    };
    let to_query = param(&auth, "addTokenTo").eq_ignore_ascii_case("queryParam");
    place(req, &token, prefix, to_query, param(&auth, "queryParamKey"));
    Ok(())
}

/// Algorithms ASAP allows: the JWA asymmetric ones. EdDSA is not JWA and the
/// reference verifier rejects it.
const ASAP_ALGORITHMS: &[Alg] = &[
    Alg::Lib(Algorithm::RS256),
    Alg::Lib(Algorithm::RS384),
    Alg::Lib(Algorithm::RS512),
    Alg::Lib(Algorithm::PS256),
    Alg::Lib(Algorithm::PS384),
    Alg::Lib(Algorithm::PS512),
    Alg::Lib(Algorithm::ES256),
    Alg::Lib(Algorithm::ES384),
    Alg::Es512,
];

/// Maximum token lifetime (`exp - iat`), a hard limit of the ASAP spec.
const ASAP_MAX_LIFETIME: i64 = 3600;

/// First non-empty string parameter among `keys` (Postman's name first, then
/// the names older Merpati versions and the Insomnia import used).
fn first_param<'a>(auth: &'a Auth, keys: &[&str]) -> &'a str {
    keys.iter().map(|k| param(auth, k)).find(|v| !v.is_empty()).unwrap_or("")
}

/// The private key PEM and, for Atlassian's
/// `data:application/pkcs8;kid=<key id>;base64,<DER>` form, its key id.
fn asap_private_key(raw: &str) -> Result<(String, Option<String>), AuthError> {
    let trimmed = raw.trim();
    let unquoted = trimmed.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(trimmed);
    if !unquoted.starts_with("data:") {
        return Ok((raw.to_string(), None));
    }
    let bad = || AuthError::Key("expected data:application/pkcs8;kid=<key id>;base64,<key>".into());
    let compact: String = unquoted.split_whitespace().collect();
    let decoded = percent_encoding::percent_decode_str(&compact).decode_utf8().map_err(|_| bad())?;
    let rest = decoded.strip_prefix("data:application/pkcs8;kid=").ok_or_else(bad)?;
    let (kid, data) = rest.split_once(";base64,").ok_or_else(bad)?;
    let der = crate::util::B64.decode(data).map_err(|_| bad())?;
    let body = crate::util::B64.encode(der);
    let lines: Vec<&str> = body.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap_or_default()).collect();
    Ok((format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", lines.join("\n")), Some(kid.into())))
}

/// ASAP key id rules: non-empty `/`-separated parts of ASCII letters, digits
/// and `_ . - +`, no `.`/`..` parts (spec; atlassian-jwt-auth also rejects any
/// `..`), owned by the issuer: `<iss>/...` (or exactly `<iss>`, which the
/// reference verifier accepts too).
fn validate_kid(kid: &str, issuer: &str) -> Result<(), AuthError> {
    let charset = kid.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-+/".contains(&b));
    let parts = kid.split('/').all(|p| !p.is_empty() && p != ".");
    if !charset || !parts || kid.contains("..") {
        return Err(AuthError::invalid(format!(
            "ASAP: invalid key id `{kid}`: use `/`-separated parts of letters, digits, `_`, `.`, `-` or `+` \
             (no empty, `.` or `..` parts)"
        )));
    }
    if kid != issuer && !kid.strip_prefix(issuer).is_some_and(|rest| rest.starts_with('/')) {
        return Err(AuthError::invalid(format!("ASAP: key id `{kid}` must start with the issuer: `{issuer}/...`")));
    }
    Ok(())
}

/// Builds an ASAP token (https://s2sauth.bitbucket.io/spec/) the way
/// atlassian-jwt-auth's signer does: header `alg`, `kid`, `typ`; claims
/// `iss`, `exp`, `iat`, `aud`, `jti`, `nbf`, `sub`, then the `claims`
/// parameter, which overrides any of them (like Postman).
pub(crate) fn asap_token(auth: &Auth, now: i64) -> Result<String, AuthError> {
    let alg = parse_algorithm(match param(auth, "alg") {
        "" => "RS256",
        a => a,
    })?;
    if !ASAP_ALGORITHMS.contains(&alg) {
        return Err(AuthError::invalid(format!(
            "ASAP does not allow {}: use RS256/384/512, PS256/384/512 or ES256/384/512",
            alg_name(alg)
        )));
    }
    let extra = json_object_param(auth, "claims")?;
    let claim = |k: &str| extra.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let issuer = claim("iss").unwrap_or_else(|| first_param(auth, &["iss", "issuer"]));
    if issuer.is_empty() {
        return Err(AuthError::Missing { scheme: "ASAP", field: "iss" });
    }
    let subject = first_param(auth, &["sub", "subject"]);
    let audience: Value = match extra.get("aud") {
        Some(aud @ (Value::String(_) | Value::Array(_))) => aud.clone(),
        _ => {
            let list: Vec<&str> =
                first_param(auth, &["aud", "audience"]).split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
            match list[..] {
                [] => return Err(AuthError::Missing { scheme: "ASAP", field: "aud" }),
                [one] => json!(one),
                _ => json!(list),
            }
        }
    };
    let (pem, uri_kid) = asap_private_key(auth.get("privateKey"))?;
    let kid = match (param(auth, "kid"), uri_kid.as_deref()) {
        ("", Some(k)) => k.to_string(),
        ("", None) => return Err(AuthError::Missing { scheme: "ASAP", field: "kid" }),
        (k, Some(u)) if k != u => {
            return Err(AuthError::invalid(format!("ASAP: key id `{k}` does not match the private key's `{u}`")));
        }
        (k, _) => k.to_string(),
    };
    validate_kid(&kid, issuer)?;
    let lifetime: i64 = {
        // Numbers or strings (Postman exports either).
        let raw = match auth.get_string("exp") {
            e if e.trim().is_empty() => auth.get_string("expiresIn"),
            e => e,
        };
        match raw.trim() {
            "" => ASAP_MAX_LIFETIME,
            s => s.parse().map_err(|_| AuthError::invalid(format!("ASAP: expiry `{s}` is not a number of seconds")))?,
        }
    };
    if !(1..=ASAP_MAX_LIFETIME).contains(&lifetime) {
        return Err(AuthError::invalid(format!(
            "ASAP: expiry must be between 1 and {ASAP_MAX_LIFETIME} seconds (got {lifetime})"
        )));
    }
    let key = encoding_key(alg, "", false, &pem)?;
    let mut claims = Map::new();
    claims.insert("iss".into(), json!(issuer));
    claims.insert("exp".into(), json!(now + lifetime));
    claims.insert("iat".into(), json!(now));
    claims.insert("aud".into(), audience);
    claims.insert("jti".into(), json!(uuid::Uuid::new_v4().to_string()));
    claims.insert("nbf".into(), json!(now));
    claims.insert("sub".into(), json!(if subject.is_empty() { issuer } else { subject }));
    for (k, v) in extra.iter() {
        claims.insert(k.clone(), v.clone());
    }
    let mut header = Map::new();
    header.insert("alg".into(), json!(alg_name(alg)));
    header.insert("kid".into(), json!(kid));
    sign(header, &claims, &key)
}

pub(crate) fn apply_asap(req: &mut PreparedRequest) -> Result<(), AuthError> {
    let token = asap_token(&req.auth.clone(), now_secs())?;
    req.set_header("Authorization", format!("Bearer {token}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{DecodingKey, Validation};
    use merpati_core::AuthType;

    const RSA_PKCS1: &str = include_str!("../tests/fixtures/rsa_pkcs1.pem");
    const RSA_PKCS8: &str = include_str!("../tests/fixtures/rsa_pkcs8.pem");
    const RSA_PUB: &str = include_str!("../tests/fixtures/rsa_pub.pem");
    const EC_KEY: &str = include_str!("../tests/fixtures/ec_p256.pem");
    const EC_PUB: &str = include_str!("../tests/fixtures/ec_p256_pub.pem");
    const ED_KEY: &str = include_str!("../tests/fixtures/ed25519.pem");
    const ED_PUB: &str = include_str!("../tests/fixtures/ed25519_pub.pem");

    /// The `jsonwebtoken` algorithm, for verifying with it.
    fn lib(alg: Alg) -> Algorithm {
        match alg {
            Alg::Lib(a) => a,
            Alg::Es512 => panic!("jsonwebtoken cannot verify ES512"),
        }
    }

    fn decode(token: &str, alg: Algorithm, key: &DecodingKey) -> Value {
        let mut v = Validation::new(alg);
        v.required_spec_claims.clear();
        v.validate_exp = false;
        v.validate_aud = false;
        jsonwebtoken::decode::<Value>(token, key, &v).unwrap().claims
    }

    #[test]
    fn hs256_known_token() {
        // jwt.io default example.
        let auth = Auth::new(AuthType::Jwt)
            .with("algorithm", "HS256")
            .with("secret", "your-256-bit-secret")
            .with("payload", r#"{"sub":"1234567890","name":"John Doe","iat":1516239022}"#);
        // jwt.io orders header as {"alg","typ"} which matches ours.
        assert_eq!(
            jwt_token(&auth).unwrap(),
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
        );
    }

    #[test]
    fn base64_secret_and_placement() {
        let auth = Auth::new(AuthType::Jwt)
            .with("algorithm", "HS512")
            .with("secret", "c2VjcmV0")
            .with("isSecretBase64Encoded", true)
            .with("payload", r#"{"a":1}"#)
            .with("addTokenTo", "queryParam")
            .with("queryParamKey", "jwt");
        let mut req = PreparedRequest { url: "http://x/p".into(), auth, ..Default::default() };
        apply_jwt(&mut req).unwrap();
        let token = req.url.split("jwt=").nth(1).unwrap().to_string();
        let claims = decode(&token, Algorithm::HS512, &DecodingKey::from_secret(b"secret"));
        assert_eq!(claims["a"], 1);
    }

    #[test]
    fn asymmetric_algorithms() {
        for (alg, key, pubkey) in [
            ("RS256", RSA_PKCS1, RSA_PUB),
            ("PS384", RSA_PKCS8, RSA_PUB),
            ("ES256", EC_KEY, EC_PUB),
            ("EdDSA", ED_KEY, ED_PUB),
        ] {
            let auth = Auth::new(AuthType::Jwt)
                .with("algorithm", alg)
                .with("privateKey", key)
                .with("payload", r#"{"sub":"me"}"#)
                .with("header", r#"{"kid":"k1"}"#)
                .with("headerPrefix", "JWT");
            let mut req = PreparedRequest { url: "http://x/".into(), auth, ..Default::default() };
            apply_jwt(&mut req).unwrap();
            let h = req.header("authorization").unwrap();
            let token = h.strip_prefix("JWT ").unwrap();
            let a = lib(parse_algorithm(alg).unwrap());
            let dk = match a {
                Algorithm::ES256 => DecodingKey::from_ec_pem(pubkey.as_bytes()).unwrap(),
                Algorithm::EdDSA => DecodingKey::from_ed_pem(pubkey.as_bytes()).unwrap(),
                _ => DecodingKey::from_rsa_pem(pubkey.as_bytes()).unwrap(),
            };
            assert_eq!(decode(token, a, &dk)["sub"], "me", "{alg}");
            let header = jsonwebtoken::decode_header(token).unwrap();
            assert_eq!(header.kid.as_deref(), Some("k1"));
        }
    }

    #[test]
    fn one_line_pem_is_repaired() {
        let one_line = RSA_PKCS8.replace('\n', " ");
        let auth = Auth::new(AuthType::Jwt).with("algorithm", "RS256").with("privateKey", one_line);
        assert!(jwt_token(&auth).is_ok());
        let escaped = RSA_PKCS8.replace('\n', "\\n");
        let auth = Auth::new(AuthType::Jwt).with("algorithm", "RS256").with("privateKey", escaped);
        assert!(jwt_token(&auth).is_ok());
    }

    #[test]
    fn invalid_inputs_are_errors() {
        let bad_payload = Auth::new(AuthType::Jwt).with("secret", "s").with("payload", "{nope");
        assert!(matches!(jwt_token(&bad_payload), Err(AuthError::Invalid(_))));
        let bad_key = Auth::new(AuthType::Jwt).with("algorithm", "RS256").with("privateKey", "garbage");
        assert!(matches!(jwt_token(&bad_key), Err(AuthError::Key(_))));
    }

    #[test]
    fn asap_claims() {
        let auth = Auth::new(AuthType::Asap)
            .with("alg", "RS256")
            .with("issuer", "svc-a")
            .with("audience", "svc-b")
            .with("kid", "svc-a/key1")
            .with("privateKey", RSA_PKCS8)
            .with("claims", r#"{"scope":"read"}"#)
            .with("exp", "600");
        let token = asap_token(&auth, 1_700_000_000).unwrap();
        let claims = decode(&token, Algorithm::RS256, &DecodingKey::from_rsa_pem(RSA_PUB.as_bytes()).unwrap());
        assert_eq!(claims["iss"], "svc-a");
        assert_eq!(claims["sub"], "svc-a");
        assert_eq!(claims["aud"], "svc-b");
        assert_eq!(claims["exp"], 1_700_000_600);
        assert_eq!(claims["scope"], "read");
        assert_eq!(jsonwebtoken::decode_header(&token).unwrap().kid.as_deref(), Some("svc-a/key1"));
        // The names used above (older Merpati, Insomnia import) still work;
        // errors name Postman's parameters.
        let missing = Auth::new(AuthType::Asap).with("privateKey", RSA_PKCS8);
        assert!(matches!(asap_token(&missing, 0), Err(AuthError::Missing { field: "iss", .. })));
        let missing = missing.with("iss", "svc-a");
        assert!(matches!(asap_token(&missing, 0), Err(AuthError::Missing { field: "aud", .. })));
        let missing = missing.with("aud", "svc-b");
        assert!(matches!(asap_token(&missing, 0), Err(AuthError::Missing { field: "kid", .. })));
    }

    const EC384_KEY: &str = include_str!("../tests/fixtures/ec_p384.pem");
    const EC384_PUB: &str = include_str!("../tests/fixtures/ec_p384_pub.pem");

    fn fixture_key(name: &str) -> &'static str {
        match name {
            "rsa_pkcs8.pem" => RSA_PKCS8,
            "rsa_pub.pem" => RSA_PUB,
            "ec_p256.pem" => EC_KEY,
            "ec_p256_pub.pem" => EC_PUB,
            "ec_p384.pem" => EC384_KEY,
            "ec_p384_pub.pem" => EC384_PUB,
            other => panic!("unknown key fixture {other}"),
        }
    }

    fn decoding_key(alg: Algorithm, pem: &str) -> DecodingKey {
        match alg {
            Algorithm::ES256 | Algorithm::ES384 => DecodingKey::from_ec_pem(pem.as_bytes()).unwrap(),
            _ => DecodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
        }
    }

    fn unverified_claims(token: &str) -> Map<String, Value> {
        let payload = token.split('.').nth(1).unwrap();
        serde_json::from_slice(&B64_URL.decode(payload).unwrap()).unwrap()
    }

    /// Postman-named ASAP auth for a reference case.
    fn asap_auth(c: &Value) -> Auth {
        let aud = match &c["aud"] {
            Value::Array(a) => a.iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>().join(", "),
            v => v.as_str().unwrap().to_string(),
        };
        Auth::new(AuthType::Asap)
            .with("alg", c["alg"].clone())
            .with("iss", c["iss"].clone())
            .with("aud", aud)
            .with("kid", c["kid"].clone())
            .with("privateKey", fixture_key(c["private_key"].as_str().unwrap()))
            .with("exp", c["lifetime"].clone())
    }

    /// Tokens from atlassian-jwt-auth 22.0.1 for the same inputs (see the
    /// fixture's README): identical for the deterministic RS* signatures,
    /// identical header and claims (and a valid signature) otherwise.
    #[test]
    fn asap_matches_atlassian_jwt_auth() {
        let doc: Value =
            serde_json::from_str(include_str!("../tests/fixtures/asap/atlassian_jwt_auth_tokens.json")).unwrap();
        let cases = doc["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 12);
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let reference = c["token"].as_str().unwrap();
            let alg = lib(parse_algorithm(c["alg"].as_str().unwrap()).unwrap());
            // The reference's jti is "<time>:<random>"; reuse it.
            let mut extra = c["claims"].as_object().unwrap().clone();
            extra.insert("jti".into(), unverified_claims(reference)["jti"].clone());
            let auth = asap_auth(c).with("claims", Value::Object(extra));
            let token = asap_token(&auth, c["now"].as_i64().unwrap()).unwrap();
            let public = decoding_key(alg, fixture_key(c["public_key"].as_str().unwrap()));
            if matches!(alg, Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512) {
                assert_eq!(token, reference, "{name}");
            } else {
                let signed_part = |t: &str| t.rsplit_once('.').unwrap().0.to_string();
                assert_eq!(signed_part(&token), signed_part(reference), "{name}");
                decode(reference, alg, &public);
            }
            assert_eq!(decode(&token, alg, &public)["iss"], c["iss"], "{name}");
        }
    }

    #[test]
    fn asap_defaults_and_postman_names() {
        let auth = Auth::new(AuthType::Asap)
            .with("iss", "svc-a")
            .with("aud", "svc-b, svc-c")
            .with("kid", "svc-a/key1")
            .with("privateKey", RSA_PKCS8);
        let token = asap_token(&auth, 1_700_000_000).unwrap();
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert_eq!((header.alg, header.kid.as_deref()), (Algorithm::RS256, Some("svc-a/key1")));
        let claims = unverified_claims(&token);
        let keys: Vec<&str> = claims.keys().map(String::as_str).collect();
        assert_eq!(keys, ["iss", "exp", "iat", "aud", "jti", "nbf", "sub"]);
        assert_eq!(claims["exp"], 1_700_003_600);
        assert_eq!(claims["nbf"], 1_700_000_000);
        assert_eq!(claims["aud"], json!(["svc-b", "svc-c"]));
        assert_eq!(claims["sub"], "svc-a");
        assert!(uuid::Uuid::parse_str(claims["jti"].as_str().unwrap()).is_ok());
        // jti differs per token.
        assert_ne!(unverified_claims(&asap_token(&auth, 1_700_000_000).unwrap())["jti"], claims["jti"]);
        // `claims` overrides (Postman gives the claims object priority).
        let over = auth.clone().with("claims", r#"{"sub":"user-1","aud":["x"],"exp":1700000100,"jti":"j"}"#);
        let c = unverified_claims(&asap_token(&over, 1_700_000_000).unwrap());
        assert_eq!(
            (c["sub"].clone(), c["aud"].clone(), c["exp"].clone(), c["jti"].clone()),
            (json!("user-1"), json!(["x"]), json!(1_700_000_100), json!("j"))
        );
    }

    #[test]
    fn asap_key_id_rules() {
        let auth = |kid: &str| {
            Auth::new(AuthType::Asap)
                .with("iss", "svc-a")
                .with("aud", "b")
                .with("kid", kid)
                .with("privateKey", RSA_PKCS8)
        };
        for ok in ["svc-a/key1", "svc-a", "svc-a/2025/k.1-x+y_z.pem"] {
            assert!(asap_token(&auth(ok), 0).is_ok(), "{ok}");
        }
        for bad in [
            "svc-a/",
            "svc-a//k",
            "/svc-a/k",
            "svc-a/./k",
            "svc-a/../k",
            "svc-a/k..1",
            "svc-a/k 1",
            "svc-a/kä",
            "other/key",
            "svc-ab/key",
            "svc-a\\key",
        ] {
            assert!(matches!(asap_token(&auth(bad), 0), Err(AuthError::Invalid(_))), "{bad}");
        }
    }

    #[test]
    fn asap_lifetime_and_algorithms() {
        let auth = Auth::new(AuthType::Asap)
            .with("iss", "a")
            .with("aud", "b")
            .with("kid", "a/k")
            .with("privateKey", RSA_PKCS8);
        for (exp, ok) in [("3600", true), ("1", true), ("3601", false), ("0", false), ("-5", false), ("1h", false)] {
            assert_eq!(asap_token(&auth.clone().with("exp", exp), 0).is_ok(), ok, "{exp}");
        }
        let numeric = asap_token(&auth.clone().with("exp", 600), 0).unwrap();
        assert_eq!(unverified_claims(&numeric)["exp"], 600);
        assert!(asap_token(&auth.clone().with("exp", 7200), 0).is_err());
        for alg in ["HS256", "EdDSA", "none"] {
            let err = asap_token(&auth.clone().with("alg", alg).with("privateKey", ED_KEY), 0).unwrap_err();
            assert!(matches!(err, AuthError::Invalid(_)), "{alg}: {err}");
        }
        for (alg, key) in [("PS512", RSA_PKCS1), ("ES256", EC_KEY), ("ES384", EC384_KEY), ("ES512", EC521_KEY)] {
            let token = asap_token(&auth.clone().with("alg", alg).with("privateKey", key), 0).unwrap();
            assert_eq!(header(&token)["alg"], alg);
        }
    }

    const EC521_KEY: &str = include_str!("../tests/fixtures/ec_p521.pem");
    const EC521_SEC1: &str = include_str!("../tests/fixtures/ec_p521_sec1.pem");
    const EC521_PUB: &str = include_str!("../tests/fixtures/ec_p521_pub.pem");

    fn header(token: &str) -> Value {
        serde_json::from_slice(&B64_URL.decode(token.split('.').next().unwrap()).unwrap()).unwrap()
    }

    /// Verifies an ES512 JWS with p521 against the fixture public key.
    fn verify_es512(token: &str) {
        use p521::ecdsa::signature::Verifier;
        use p521::pkcs8::DecodePublicKey;
        let der = pem_der(EC521_PUB, "PUBLIC KEY").unwrap().unwrap();
        let public = p521::PublicKey::from_public_key_der(&der).unwrap();
        let key = p521::ecdsa::VerifyingKey::from_affine(*public.as_affine()).unwrap();
        let (message, sig) = token.rsplit_once('.').unwrap();
        let sig = B64_URL.decode(sig).unwrap();
        assert_eq!(sig.len(), 132, "R || S, 66 bytes each");
        let sig = p521::ecdsa::Signature::from_slice(&sig).unwrap();
        key.verify(message.as_bytes(), &sig).expect("valid ES512 signature");
        assert!(key.verify(format!("{message}x").as_bytes(), &sig).is_err());
    }

    #[test]
    fn es512_jwt_and_asap() {
        // PKCS#8, SEC1, SEC1 after the EC PARAMETERS block, one-line PEM.
        let with_params =
            format!("-----BEGIN EC PARAMETERS-----\nBgUrgQQAIw==\n-----END EC PARAMETERS-----\n{EC521_SEC1}");
        for key in [EC521_KEY.to_string(), EC521_SEC1.to_string(), with_params, EC521_KEY.replace('\n', " ")] {
            let auth = Auth::new(AuthType::Jwt)
                .with("algorithm", "ES512")
                .with("privateKey", key.as_str())
                .with("payload", r#"{"sub":"me"}"#)
                .with("header", r#"{"kid":"k1"}"#);
            let mut req = PreparedRequest { url: "http://x/".into(), auth, ..Default::default() };
            apply_jwt(&mut req).unwrap_or_else(|e| panic!("{e}: {key}"));
            let token = req.header("authorization").unwrap().strip_prefix("Bearer ").unwrap().to_string();
            assert_eq!(header(&token), json!({"kid": "k1", "alg": "ES512", "typ": "JWT"}));
            assert_eq!(unverified_claims(&token)["sub"], "me");
            verify_es512(&token);
        }
        let auth = Auth::new(AuthType::Asap)
            .with("alg", "es512")
            .with("iss", "svc-a")
            .with("aud", "svc-b")
            .with("kid", "svc-a/p521")
            .with("privateKey", EC521_SEC1);
        let token = asap_token(&auth, 1_700_000_000).unwrap();
        assert_eq!(header(&token), json!({"alg": "ES512", "kid": "svc-a/p521", "typ": "JWT"}));
        assert_eq!(unverified_claims(&token)["exp"], 1_700_003_600);
        verify_es512(&token);
    }

    #[test]
    fn es512_key_errors() {
        let jwt = |key: &str| Auth::new(AuthType::Jwt).with("algorithm", "ES512").with("privateKey", key);
        for (key, needle) in [
            (EC_KEY.to_string(), "P-521"),
            (EC384_KEY.to_string(), "P-521"),
            (RSA_PKCS8.to_string(), "P-521"),
            (RSA_PKCS1.to_string(), "BEGIN EC PRIVATE KEY"),
            (EC521_PUB.to_string(), "BEGIN EC PRIVATE KEY"),
            ("garbage".to_string(), "BEGIN EC PRIVATE KEY"),
            (EC521_KEY.replace("PRIVATE KEY", "ENCRYPTED PRIVATE KEY"), "encrypted"),
            (EC521_SEC1.replace("-----\n", "-----\nProc-Type: 4,ENCRYPTED\n\n"), "encrypted"),
            ("-----BEGIN PRIVATE KEY-----\nMIH*\n-----END PRIVATE KEY-----".to_string(), "base64"),
        ] {
            let err = jwt_token(&jwt(&key)).unwrap_err();
            assert!(matches!(err, AuthError::Key(_)) && err.to_string().contains(needle), "{needle}: {err}");
        }
        // A SEC1 key whose parameters name another curve (P-256 here).
        let sec1 = pem_der(EC521_SEC1, "EC PRIVATE KEY").unwrap().unwrap();
        let mut other = sec1.clone();
        let at = other.windows(P521_OID.len()).position(|w| w == P521_OID).unwrap();
        other.splice(at..at + P521_OID.len(), [0x2b, 0x81, 0x04, 0x00, 0x22]);
        assert_eq!(sec1_curve(&sec1), Some(P521_OID));
        assert_eq!(sec1_curve(&other), Some(&[0x2b, 0x81, 0x04, 0x00, 0x22][..]));
        let pem = format!("-----BEGIN EC PRIVATE KEY-----\n{}\n-----END EC PRIVATE KEY-----\n", B64.encode(other));
        let err = jwt_token(&jwt(&pem)).unwrap_err();
        assert!(err.to_string().contains("another curve"), "{err}");
    }

    /// One token checked by the OpenSSL command line, independently of the
    /// RustCrypto code (`tests/fixtures/es512/verify_openssl.sh`). Skipped
    /// when `openssl` is not installed.
    #[cfg(unix)]
    #[test]
    fn es512_token_verifies_with_openssl() {
        if std::process::Command::new("openssl").arg("version").output().is_err() {
            eprintln!("openssl not found; skipping the OpenSSL cross-check");
            return;
        }
        let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        let verify = |token: &str| {
            let work = tempfile::tempdir().unwrap();
            let out = std::process::Command::new("sh")
                .arg(format!("{fixtures}/es512/verify_openssl.sh"))
                .arg(token)
                .arg(format!("{fixtures}/ec_p521_pub.pem"))
                .arg(work.path())
                .output()
                .unwrap();
            let log = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            (out.status.success() && log.contains("Verified OK"), log)
        };
        let auth = Auth::new(AuthType::Jwt)
            .with("algorithm", "ES512")
            .with("privateKey", EC521_KEY)
            .with("payload", r#"{"sub":"openssl"}"#);
        let token = jwt_token(&auth).unwrap();
        let (ok, log) = verify(&token);
        assert!(ok, "{token}\n{log}");
        // A tampered payload is rejected.
        let (head, rest) = token.split_once('.').unwrap();
        let sig = rest.split_once('.').unwrap().1;
        let forged = format!("{head}.{}.{sig}", B64_URL.encode(br#"{"sub":"root"}"#));
        assert!(!verify(&forged).0, "forged token accepted");
    }

    #[test]
    fn asap_data_uri_private_key() {
        // Atlassian's key format: data:application/pkcs8;kid=<url-encoded kid>;base64,<PKCS#8 DER>.
        let der: String = RSA_PKCS8.lines().filter(|l| !l.starts_with("-----")).collect();
        let uri = format!("data:application/pkcs8;kid=svc-a%2Fkey1;base64,{der}");
        let pem_auth = Auth::new(AuthType::Asap)
            .with("iss", "svc-a")
            .with("aud", "b")
            .with("kid", "svc-a/key1")
            .with("privateKey", RSA_PKCS8)
            .with("claims", r#"{"jti":"fixed"}"#);
        let expected = asap_token(&pem_auth, 0).unwrap();
        // kid taken from the URI, or given and equal; quotes and line breaks tolerated.
        let from_uri = pem_auth
            .clone()
            .with("kid", "")
            .with("privateKey", format!("\"{}\"", uri.replace(";base64,", ";base64,\n")));
        assert_eq!(asap_token(&from_uri, 0).unwrap(), expected);
        assert_eq!(asap_token(&pem_auth.clone().with("privateKey", uri.clone()), 0).unwrap(), expected);
        let mismatch = pem_auth.clone().with("kid", "svc-a/other").with("privateKey", uri);
        assert!(matches!(asap_token(&mismatch, 0), Err(AuthError::Invalid(_))));
        let malformed = pem_auth.with("privateKey", "data:application/pkcs8;base64,AAAA");
        assert!(matches!(asap_token(&malformed, 0), Err(AuthError::Key(_))));
    }
}
