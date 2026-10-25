//! Form metadata per auth type, so the UI can render auth editors generically.
//! Keys are Postman's parameter names.

use merpati_core::AuthType;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    /// Masked single-line input.
    Secret,
    /// Multi-line input (JSON, PEM keys).
    Multiline,
    Bool,
    /// One of `(value, label)`.
    Select(&'static [(&'static str, &'static str)]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    /// Default value as text (`"true"`/`"false"` for booleans).
    pub default: &'static str,
    pub help: &'static str,
}

impl FieldSpec {
    /// Fields hidden under an "Advanced" section in the UI.
    pub fn is_advanced(&self) -> bool {
        ADVANCED.contains(&self.key)
    }
}

const ADVANCED: &[&str] = &[
    "realm",
    "nonce",
    "qop",
    "nonceCount",
    "clientNonce",
    "opaque",
    "timestamp",
    "version",
    "callback",
    "verifier",
    "addEmptyParamsToSign",
    "includeBodyHash",
    "disableHeaderEncoding",
    "state",
    "audience",
    "resource",
    "refreshTokenUrl",
    "client_authentication",
    "code_verifier",
    "user",
    "extraData",
    "app",
    "delegation",
    "workstation",
    "disableRetryRequest",
    "headersToSign",
    "baseURL",
    "maxBodySize",
];

const fn f(
    key: &'static str,
    label: &'static str,
    kind: FieldKind,
    default: &'static str,
    help: &'static str,
) -> FieldSpec {
    FieldSpec { key, label, kind, default, help }
}

use FieldKind::{Bool, Multiline, Secret, Select, Text};

const API_KEY: &[FieldSpec] = &[
    f("key", "Key", Text, "", "Header or query parameter name, e.g. X-API-Key"),
    f("value", "Value", Secret, "", ""),
    f("in", "Add to", Select(&[("header", "Header"), ("query", "Query Params")]), "header", ""),
];

const BEARER: &[FieldSpec] = &[f("token", "Token", Secret, "", "Sent as `Authorization: Bearer <token>`")];

const BASIC: &[FieldSpec] = &[f("username", "Username", Text, "", ""), f("password", "Password", Secret, "", "")];

const DIGEST_ALGS: &[(&str, &str)] = &[
    ("MD5", "MD5"),
    ("MD5-sess", "MD5-sess"),
    ("SHA-256", "SHA-256"),
    ("SHA-256-sess", "SHA-256-sess"),
    ("SHA-512-256", "SHA-512-256"),
    ("SHA-512-256-sess", "SHA-512-256-sess"),
];

const DIGEST: &[FieldSpec] = &[
    f("username", "Username", Text, "", ""),
    f("password", "Password", Secret, "", ""),
    f(
        "disableRetryRequest",
        "Disable retrying the request",
        Bool,
        "false",
        "Do not answer the server's 401 challenge automatically",
    ),
    f("realm", "Realm", Text, "", "Filled from the server challenge"),
    f("nonce", "Nonce", Text, "", "Filled from the server challenge"),
    f("algorithm", "Algorithm", Select(DIGEST_ALGS), "MD5", ""),
    f("qop", "qop", Select(&[("", "(none)"), ("auth", "auth"), ("auth-int", "auth-int")]), "", ""),
    f("nonceCount", "Nonce Count", Text, "", "e.g. 00000001"),
    f("clientNonce", "Client Nonce", Text, "", "Generated when empty"),
    f("opaque", "Opaque", Text, "", ""),
];

const JWT_ALGS: &[(&str, &str)] = &[
    ("HS256", "HS256"),
    ("HS384", "HS384"),
    ("HS512", "HS512"),
    ("RS256", "RS256"),
    ("RS384", "RS384"),
    ("RS512", "RS512"),
    ("PS256", "PS256"),
    ("PS384", "PS384"),
    ("PS512", "PS512"),
    ("ES256", "ES256"),
    ("ES384", "ES384"),
    ("ES512", "ES512"),
    ("EdDSA", "EdDSA"),
];

const JWT: &[FieldSpec] = &[
    f(
        "addTokenTo",
        "Add JWT token to",
        Select(&[("header", "Request Header"), ("queryParam", "Query Param")]),
        "header",
        "",
    ),
    f("algorithm", "Algorithm", Select(JWT_ALGS), "HS256", ""),
    f("secret", "Secret", Secret, "", "For HS* algorithms"),
    f("isSecretBase64Encoded", "Secret Base64 encoded", Bool, "false", ""),
    f("privateKey", "Private Key", Multiline, "", "PEM key for RS*, PS*, ES* and EdDSA"),
    f("payload", "Payload", Multiline, "{}", "JSON claims"),
    f("headerPrefix", "Request header prefix", Text, "Bearer", ""),
    f("queryParamKey", "Query param name", Text, "token", ""),
    f("header", "JWT headers", Multiline, "{}", "Extra JSON header fields, e.g. {\"kid\": \"...\"}"),
];

const OAUTH1_METHODS: &[(&str, &str)] = &[
    ("HMAC-SHA1", "HMAC-SHA1"),
    ("HMAC-SHA256", "HMAC-SHA256"),
    ("HMAC-SHA512", "HMAC-SHA512"),
    ("RSA-SHA1", "RSA-SHA1"),
    ("RSA-SHA256", "RSA-SHA256"),
    ("RSA-SHA512", "RSA-SHA512"),
    ("PLAINTEXT", "PLAINTEXT"),
];

const OAUTH1: &[FieldSpec] = &[
    f(
        "addParamsToHeader",
        "Add auth data to request headers",
        Bool,
        "true",
        "Otherwise to the body (form POST) or URL",
    ),
    f("signatureMethod", "Signature Method", Select(OAUTH1_METHODS), "HMAC-SHA1", ""),
    f("consumerKey", "Consumer Key", Text, "", ""),
    f("consumerSecret", "Consumer Secret", Secret, "", ""),
    f("token", "Access Token", Text, "", ""),
    f("tokenSecret", "Token Secret", Secret, "", ""),
    f("privateKey", "Private Key", Multiline, "", "PEM RSA key for RSA-SHA*"),
    f("callback", "Callback URL", Text, "", ""),
    f("verifier", "Verifier", Text, "", ""),
    f("timestamp", "Timestamp", Text, "", "Generated when empty"),
    f("nonce", "Nonce", Text, "", "Generated when empty"),
    f("version", "Version", Text, "1.0", ""),
    f("realm", "Realm", Text, "", ""),
    f("includeBodyHash", "Include body hash", Bool, "false", "Adds oauth_body_hash for non-form bodies"),
    f("addEmptyParamsToSign", "Add empty parameters to signature", Bool, "false", ""),
    f("disableHeaderEncoding", "Disable header encoding", Bool, "false", ""),
];

const OAUTH2_GRANTS: &[(&str, &str)] = &[
    ("authorization_code", "Authorization Code"),
    ("authorization_code_with_pkce", "Authorization Code (With PKCE)"),
    ("implicit", "Implicit"),
    ("password_credentials", "Password Credentials"),
    ("client_credentials", "Client Credentials"),
];

const OAUTH2: &[FieldSpec] = &[
    f("accessToken", "Access Token", Secret, "", "Paste a token manually, or use Get New Access Token"),
    f("headerPrefix", "Header Prefix", Text, "Bearer", ""),
    f(
        "addTokenTo",
        "Add token to",
        Select(&[("header", "Request Headers"), ("queryParams", "Request URL")]),
        "header",
        "",
    ),
    f("tokenName", "Token Name", Text, "", ""),
    f("grant_type", "Grant Type", Select(OAUTH2_GRANTS), "authorization_code", ""),
    f("redirect_uri", "Callback URL", Text, "http://localhost:5555/callback", "Must be http://localhost:<port>/..."),
    f("authUrl", "Auth URL", Text, "", ""),
    f("accessTokenUrl", "Access Token URL", Text, "", ""),
    f("clientId", "Client ID", Text, "", ""),
    f("clientSecret", "Client Secret", Secret, "", ""),
    f("username", "Username", Text, "", "Password Credentials grant"),
    f("password", "Password", Secret, "", "Password Credentials grant"),
    f("challengeAlgorithm", "Code Challenge Method", Select(&[("S256", "SHA-256"), ("plain", "Plain")]), "S256", ""),
    f("code_verifier", "Code Verifier", Secret, "", "Generated when empty"),
    f("scope", "Scope", Text, "", "Space separated"),
    f("state", "State", Text, "", "Generated when empty"),
    f(
        "client_authentication",
        "Client Authentication",
        Select(&[("header", "Send as Basic Auth header"), ("body", "Send client credentials in body")]),
        "header",
        "",
    ),
    f("audience", "Audience", Text, "", ""),
    f("resource", "Resource", Text, "", ""),
    f("refreshTokenUrl", "Refresh Token URL", Text, "", "Defaults to the Access Token URL"),
    f("autoRefresh", "Auto-refresh token", Bool, "true", "Refresh expired tokens when a refresh token is available"),
];

const HAWK: &[FieldSpec] = &[
    f("authId", "Hawk Auth ID", Text, "", ""),
    f("authKey", "Hawk Auth Key", Secret, "", ""),
    f("algorithm", "Algorithm", Select(&[("sha256", "SHA256"), ("sha1", "SHA1")]), "sha256", ""),
    f("user", "User", Text, "", ""),
    f("nonce", "Nonce", Text, "", "Generated when empty"),
    f("extraData", "ext", Text, "", ""),
    f("app", "app", Text, "", ""),
    f("delegation", "dlg", Text, "", ""),
    f("timestamp", "Timestamp", Text, "", "Generated when empty"),
    f("includePayloadHash", "Include payload hash", Bool, "false", ""),
];

const AWSV4: &[FieldSpec] = &[
    f("accessKey", "AccessKey", Text, "", ""),
    f("secretKey", "SecretKey", Secret, "", ""),
    f("region", "AWS Region", Text, "us-east-1", ""),
    f("service", "Service Name", Text, "", "e.g. s3, execute-api"),
    f("sessionToken", "Session Token", Secret, "", ""),
    f("addAuthDataToQuery", "Add auth data to URL", Bool, "false", "Presigned URL instead of headers"),
];

const NTLM: &[FieldSpec] = &[
    f("username", "Username", Text, "", ""),
    f("password", "Password", Secret, "", ""),
    f("domain", "Domain", Text, "", ""),
    f("workstation", "Workstation", Text, "", ""),
    f("disableRetryRequest", "Disable retrying the request", Bool, "false", ""),
];

const EDGEGRID: &[FieldSpec] = &[
    f("accessToken", "Access Token", Secret, "", ""),
    f("clientToken", "Client Token", Secret, "", ""),
    f("clientSecret", "Client Secret", Secret, "", ""),
    f("nonce", "Nonce", Text, "", "Generated when empty"),
    f("timestamp", "Timestamp", Text, "", "Generated when empty"),
    f("baseURL", "Base URL", Text, "", "Host signed instead of the request URL's"),
    f("headersToSign", "Headers to Sign", Text, "", "Comma separated header names"),
    f("maxBodySize", "Max Body Size", Text, "131072", "POST body bytes covered by the content hash"),
];

const ASAP_ALGS: &[(&str, &str)] = &[
    ("RS256", "RS256"),
    ("RS384", "RS384"),
    ("RS512", "RS512"),
    ("PS256", "PS256"),
    ("PS384", "PS384"),
    ("PS512", "PS512"),
    ("ES256", "ES256"),
    ("ES384", "ES384"),
    ("ES512", "ES512"),
];

const ASAP: &[FieldSpec] = &[
    f("alg", "Algorithm", Select(ASAP_ALGS), "RS256", ""),
    f("iss", "Issuer", Text, "", ""),
    f("sub", "Subject", Text, "", "Defaults to the issuer"),
    f("aud", "Audience", Text, "", "Comma separated for several"),
    f("kid", "Key ID", Text, "", "Must start with `<issuer>/`; read from a data URI key when empty"),
    f("privateKey", "Private Key", Multiline, "", "PEM, or data:application/pkcs8;kid=...;base64,..."),
    f("claims", "Additional claims", Multiline, "{}", "JSON; overrides the claims above"),
    f("exp", "Expiry (seconds)", Text, "3600", "At most 3600"),
];

pub fn fields(kind: AuthType) -> &'static [FieldSpec] {
    match kind {
        AuthType::Inherit | AuthType::None => &[],
        AuthType::ApiKey => API_KEY,
        AuthType::Bearer => BEARER,
        AuthType::Basic => BASIC,
        AuthType::Digest => DIGEST,
        AuthType::Jwt => JWT,
        AuthType::OAuth1 => OAUTH1,
        AuthType::OAuth2 => OAUTH2,
        AuthType::Hawk => HAWK,
        AuthType::AwsV4 => AWSV4,
        AuthType::Ntlm => NTLM,
        AuthType::EdgeGrid => EDGEGRID,
        AuthType::Asap => ASAP,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_params_are_secret_or_multiline() {
        for kind in AuthType::ALL {
            let specs = fields(kind);
            for s in kind.sensitive_params() {
                // OAuth1 token is not a secret on its own; refreshToken is never edited.
                if matches!(*s, "token" | "refreshToken") && kind == AuthType::OAuth1 || *s == "refreshToken" {
                    continue;
                }
                let spec = specs.iter().find(|f| f.key == *s);
                if let Some(spec) = spec {
                    assert!(matches!(spec.kind, FieldKind::Secret | FieldKind::Multiline), "{kind:?}.{s}");
                }
            }
            let mut keys: Vec<_> = specs.iter().map(|f| f.key).collect();
            keys.sort();
            keys.dedup();
            assert_eq!(keys.len(), specs.len(), "duplicate keys in {kind:?}");
        }
    }
}
