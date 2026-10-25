//! OAuth 2.0 token storage. Tokens are kept per auth owner (the collection,
//! folder or request that defines the auth) and never in collection files.

use std::collections::HashMap;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuth2Token {
    pub access_token: String,
    #[serde(default)]
    pub token_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    /// Unix seconds when the access token expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Unix seconds when obtained.
    #[serde(default)]
    pub obtained_at: i64,
    /// Display name chosen by the user (`tokenName`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

impl OAuth2Token {
    /// True if the token expires within `skew_secs`.
    pub fn is_expired(&self, skew_secs: i64) -> bool {
        match self.expires_at {
            Some(exp) => chrono::Utc::now().timestamp() + skew_secs >= exp,
            None => false,
        }
    }
}

pub trait TokenStore: Send + Sync {
    fn get(&self, owner_id: &str) -> Option<OAuth2Token>;
    fn set(&self, owner_id: &str, token: Option<OAuth2Token>);
}

#[derive(Default)]
pub struct MemoryTokenStore {
    tokens: Mutex<HashMap<String, OAuth2Token>>,
}

impl TokenStore for MemoryTokenStore {
    fn get(&self, owner_id: &str) -> Option<OAuth2Token> {
        self.tokens.lock().get(owner_id).cloned()
    }

    fn set(&self, owner_id: &str, token: Option<OAuth2Token>) {
        let mut map = self.tokens.lock();
        match token {
            Some(t) => {
                map.insert(owner_id.to_string(), t);
            }
            None => {
                map.remove(owner_id);
            }
        }
    }
}
