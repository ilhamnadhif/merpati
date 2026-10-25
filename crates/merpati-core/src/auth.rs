//! Auth configuration model.
//!
//! Auth is stored generically as a type plus a parameter map, using the same
//! parameter names as Postman (`accessTokenUrl`, `clientId`, ...). This keeps
//! Postman import/export lossless, including parameters Merpati does not use.
//! Typed views over the parameters live in `merpati-auth`.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AuthType {
    /// Use the auth of the parent folder or collection.
    #[default]
    #[serde(rename = "inherit")]
    Inherit,
    #[serde(rename = "noauth")]
    None,
    #[serde(rename = "apikey")]
    ApiKey,
    #[serde(rename = "bearer")]
    Bearer,
    #[serde(rename = "basic")]
    Basic,
    #[serde(rename = "digest")]
    Digest,
    #[serde(rename = "jwt")]
    Jwt,
    #[serde(rename = "oauth1")]
    OAuth1,
    #[serde(rename = "oauth2")]
    OAuth2,
    #[serde(rename = "hawk")]
    Hawk,
    #[serde(rename = "awsv4")]
    AwsV4,
    #[serde(rename = "ntlm")]
    Ntlm,
    #[serde(rename = "edgegrid")]
    EdgeGrid,
    #[serde(rename = "asap")]
    Asap,
}

impl AuthType {
    pub const ALL: [AuthType; 14] = [
        AuthType::Inherit,
        AuthType::None,
        AuthType::ApiKey,
        AuthType::Bearer,
        AuthType::Basic,
        AuthType::Digest,
        AuthType::Jwt,
        AuthType::OAuth1,
        AuthType::OAuth2,
        AuthType::Hawk,
        AuthType::AwsV4,
        AuthType::Ntlm,
        AuthType::EdgeGrid,
        AuthType::Asap,
    ];

    /// Postman's `type` string.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthType::Inherit => "inherit",
            AuthType::None => "noauth",
            AuthType::ApiKey => "apikey",
            AuthType::Bearer => "bearer",
            AuthType::Basic => "basic",
            AuthType::Digest => "digest",
            AuthType::Jwt => "jwt",
            AuthType::OAuth1 => "oauth1",
            AuthType::OAuth2 => "oauth2",
            AuthType::Hawk => "hawk",
            AuthType::AwsV4 => "awsv4",
            AuthType::Ntlm => "ntlm",
            AuthType::EdgeGrid => "edgegrid",
            AuthType::Asap => "asap",
        }
    }

    pub fn parse(s: &str) -> Option<AuthType> {
        AuthType::ALL.iter().copied().find(|t| t.as_str().eq_ignore_ascii_case(s))
    }

    pub fn label(self) -> &'static str {
        match self {
            AuthType::Inherit => "Inherit auth from parent",
            AuthType::None => "No Auth",
            AuthType::ApiKey => "API Key",
            AuthType::Bearer => "Bearer Token",
            AuthType::Basic => "Basic Auth",
            AuthType::Digest => "Digest Auth",
            AuthType::Jwt => "JWT Bearer",
            AuthType::OAuth1 => "OAuth 1.0",
            AuthType::OAuth2 => "OAuth 2.0",
            AuthType::Hawk => "Hawk Authentication",
            AuthType::AwsV4 => "AWS Signature",
            AuthType::Ntlm => "NTLM Authentication",
            AuthType::EdgeGrid => "Akamai EdgeGrid",
            AuthType::Asap => "ASAP (Atlassian)",
        }
    }

    /// Parameters whose values are secrets. When their value is a literal
    /// (not a pure `{{variable}}` reference) storage keeps them out of the
    /// committed files and exports.
    pub fn sensitive_params(self) -> &'static [&'static str] {
        match self {
            AuthType::ApiKey => &["value"],
            AuthType::Bearer => &["token"],
            AuthType::Basic | AuthType::Digest | AuthType::Ntlm => &["password"],
            AuthType::Jwt => &["secret", "privateKey"],
            AuthType::OAuth1 => &["consumerSecret", "tokenSecret", "privateKey", "token"],
            AuthType::OAuth2 => &["clientSecret", "password", "accessToken", "refreshToken", "code_verifier"],
            AuthType::Hawk => &["authKey"],
            AuthType::AwsV4 => &["secretKey", "sessionToken"],
            AuthType::EdgeGrid => &["clientSecret", "clientToken", "accessToken"],
            AuthType::Asap => &["privateKey"],
            AuthType::Inherit | AuthType::None => &[],
        }
    }
}

/// Auth configuration for a collection, folder or request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Auth {
    #[serde(rename = "type")]
    pub kind: AuthType,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub params: IndexMap<String, Value>,
}

pub static NO_AUTH: std::sync::LazyLock<Auth> = std::sync::LazyLock::new(Auth::none);

impl Auth {
    pub fn new(kind: AuthType) -> Self {
        Auth { kind, params: IndexMap::new() }
    }

    pub fn none() -> Self {
        Auth::new(AuthType::None)
    }

    pub fn bearer(token: impl Into<String>) -> Self {
        Auth::new(AuthType::Bearer).with("token", token.into())
    }

    pub fn basic(user: impl Into<String>, pass: impl Into<String>) -> Self {
        Auth::new(AuthType::Basic).with("username", user.into()).with("password", pass.into())
    }

    pub fn api_key(key: impl Into<String>, value: impl Into<String>, in_query: bool) -> Self {
        Auth::new(AuthType::ApiKey)
            .with("key", key.into())
            .with("value", value.into())
            .with("in", if in_query { "query" } else { "header" })
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.params.insert(key.to_string(), value.into());
        self
    }

    pub fn is_inherit(&self) -> bool {
        self.kind == AuthType::Inherit
    }

    /// String value of a parameter. Booleans and numbers are stringified.
    pub fn get(&self, key: &str) -> &str {
        match self.params.get(key) {
            Some(Value::String(s)) => s,
            _ => "",
        }
    }

    pub fn get_string(&self, key: &str) -> String {
        match self.params.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(Value::Number(n)) => n.to_string(),
            Some(other) => other.to_string(),
        }
    }

    pub fn get_bool(&self, key: &str, default: bool) -> bool {
        match self.params.get(key) {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => match s.as_str() {
                "true" | "1" | "yes" => true,
                "false" | "0" | "no" => false,
                _ => default,
            },
            _ => default,
        }
    }

    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.params.insert(key.to_string(), value.into());
    }

    /// Blanks the literal values of sensitive parameters; pure `{{variable}}`
    /// references are kept (for local files that must not hold secrets).
    pub fn scrub_secrets(&mut self) {
        let sensitive = self.kind.sensitive_params();
        for (k, v) in self.params.iter_mut() {
            if sensitive.contains(&k.as_str())
                && let Value::String(s) = v
                && !crate::variables::is_pure_reference(s)
            {
                s.clear();
            }
        }
    }

    /// Applies `f` to every string parameter (used for variable resolution).
    pub fn map_strings(&self, mut f: impl FnMut(&str) -> String) -> Auth {
        fn map_value(v: &Value, f: &mut dyn FnMut(&str) -> String) -> Value {
            match v {
                Value::String(s) => Value::String(f(s)),
                Value::Array(a) => Value::Array(a.iter().map(|x| map_value(x, f)).collect()),
                Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), map_value(x, f))).collect()),
                other => other.clone(),
            }
        }
        Auth { kind: self.kind, params: self.params.iter().map(|(k, v)| (k.clone(), map_value(v, &mut f))).collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_shape() {
        let a = Auth::bearer("{{token}}");
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v, serde_json::json!({"type": "bearer", "params": {"token": "{{token}}"}}));
        let none: Auth = serde_json::from_value(serde_json::json!({"type": "noauth"})).unwrap();
        assert_eq!(none.kind, AuthType::None);
    }

    #[test]
    fn typed_getters() {
        let a = Auth::new(AuthType::OAuth2).with("useBrowser", true).with("tokenName", "t");
        assert!(a.get_bool("useBrowser", false));
        assert_eq!(a.get("tokenName"), "t");
        assert_eq!(a.get("missing"), "");
    }
}
