//! API key, Bearer token and Basic auth.

use base64::Engine;
use merpati_core::{Auth, PreparedRequest};

use crate::util::{B64, param};

pub(crate) fn api_key(auth: &Auth, req: &mut PreparedRequest) {
    let key = param(auth, "key");
    if key.is_empty() {
        return;
    }
    let value = auth.get("value");
    if auth.get("in").eq_ignore_ascii_case("query") {
        req.add_query(key, value);
    } else {
        req.set_header(key, value);
    }
}

pub(crate) fn bearer(auth: &Auth, req: &mut PreparedRequest) {
    let token = param(auth, "token");
    if !token.is_empty() {
        req.set_header("Authorization", format!("Bearer {token}"));
    }
}

pub(crate) fn basic_value(username: &str, password: &str) -> String {
    format!("Basic {}", B64.encode(format!("{username}:{password}")))
}

pub(crate) fn basic(auth: &Auth, req: &mut PreparedRequest) {
    let user = auth.get("username");
    let pass = auth.get("password");
    if user.is_empty() && pass.is_empty() {
        return;
    }
    req.set_header("Authorization", basic_value(user, pass));
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::AuthType;

    fn req() -> PreparedRequest {
        PreparedRequest { url: "https://api.test/v1?x=1".into(), ..Default::default() }
    }

    #[test]
    fn basic_rfc7617() {
        // RFC 7617 section 2 example.
        assert_eq!(basic_value("Aladdin", "open sesame"), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
        let mut r = req();
        basic(&Auth::basic("Aladdin", "open sesame"), &mut r);
        assert_eq!(r.header("authorization"), Some("Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="));
        // UTF-8 credentials (RFC 7617 section 2.1 example).
        assert_eq!(basic_value("test", "123£"), "Basic dGVzdDoxMjPCow==");
    }

    #[test]
    fn api_key_header_and_query() {
        let mut r = req();
        api_key(&Auth::api_key("X-Api-Key", "k1", false), &mut r);
        assert_eq!(r.header("x-api-key"), Some("k1"));
        let mut r = req();
        api_key(&Auth::api_key("api key", "a/b", true), &mut r);
        assert_eq!(r.url, "https://api.test/v1?x=1&api%20key=a%2Fb");
        let mut r = req();
        api_key(&Auth::new(AuthType::ApiKey), &mut r);
        assert_eq!(r, req());
    }

    #[test]
    fn bearer_replaces_existing() {
        let mut r = req();
        r.headers.push(("authorization".into(), "old".into()));
        bearer(&Auth::bearer("abc"), &mut r);
        assert_eq!(r.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("authorization")).count(), 1);
        assert_eq!(r.header("Authorization"), Some("Bearer abc"));
    }
}
