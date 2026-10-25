//! HTTP Digest access authentication (RFC 7616, compatible with RFC 2617).

use merpati_core::{Auth, PreparedRequest};
use merpati_http::digest::{DigestInput, parse_algorithm};

use crate::challenge::{Challenge, parse_challenges};
use crate::error::AuthError;
use crate::util::{body_bytes, param, parse_url, random_hex, request_target};

/// Builds the full `Authorization` header value.
fn header_value(i: &DigestInput<'_>) -> Result<String, AuthError> {
    merpati_http::digest::header_value(i).map_err(AuthError::invalid)
}

/// Picks the qop to use from what the server (or user) offers. `auth-int`
/// needs the body bytes, so it is only chosen when they are known.
fn choose_qop(offered: &str, body_known: bool) -> String {
    let opts: Vec<String> =
        offered.split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect();
    if opts.is_empty() {
        return String::new();
    }
    if opts.iter().any(|o| o == "auth") {
        return "auth".into();
    }
    if opts.iter().any(|o| o == "auth-int") && body_known {
        return "auth-int".into();
    }
    String::new()
}

fn compute(auth: &Auth, req: &PreparedRequest) -> Result<String, AuthError> {
    let url = parse_url(req)?;
    let uri = request_target(&url);
    let body = body_bytes(req);
    let qop = choose_qop(param(auth, "qop"), body.is_some());
    let nc = match param(auth, "nonceCount") {
        "" => "00000001".to_string(),
        n => n.to_string(),
    };
    let cnonce = match param(auth, "clientNonce") {
        "" => random_hex(8),
        c => c.to_string(),
    };
    let algorithm = match param(auth, "algorithm") {
        "" => "MD5",
        a => a,
    };
    header_value(&DigestInput {
        username: auth.get("username"),
        password: auth.get("password"),
        realm: auth.get("realm"),
        nonce: auth.get("nonce"),
        algorithm,
        qop: &qop,
        nc: &nc,
        cnonce: &cnonce,
        opaque: auth.get("opaque"),
        method: &req.method,
        uri: &uri,
        body: body.unwrap_or(&[]),
    })
}

/// Preemptive digest when the user supplied realm and nonce.
pub(crate) fn apply(req: &mut PreparedRequest) -> Result<(), AuthError> {
    let auth = req.auth.clone();
    if param(&auth, "realm").is_empty() || param(&auth, "nonce").is_empty() {
        return Ok(());
    }
    let value = compute(&auth, req)?;
    req.set_header("Authorization", value);
    Ok(())
}

fn best_digest(challenges: &[Challenge]) -> Option<&Challenge> {
    challenges
        .iter()
        .filter(|c| c.scheme == "digest")
        .filter_map(|c| parse_algorithm(c.param("algorithm").unwrap_or("MD5")).map(|(a, _)| (a, c)))
        .max_by_key(|(a, _)| a.strength())
        .map(|(_, c)| c)
}

fn sent_nonce(req: &PreparedRequest) -> Option<String> {
    let h = req.header("authorization")?;
    let c = parse_challenges([h]);
    c.into_iter().find(|c| c.scheme == "digest").and_then(|c| c.param("nonce").map(str::to_string))
}

/// Answers a `401` digest challenge. Returns `true` when the request was
/// updated and should be sent again.
pub(crate) fn handle_challenge(req: &mut PreparedRequest, headers: &[(String, String)]) -> Result<bool, AuthError> {
    if req.auth.get_bool("disableRetryRequest", false) {
        return Ok(false);
    }
    let values = headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("www-authenticate")).map(|(_, v)| v.as_str());
    let challenges = parse_challenges(values);
    let Some(ch) = best_digest(&challenges) else { return Ok(false) };
    let nonce = ch.param("nonce").unwrap_or_default().to_string();
    let stale = ch.param("stale").map(|s| s.eq_ignore_ascii_case("true")).unwrap_or(false);
    // Same nonce rejected again and not stale: the credentials are wrong.
    if sent_nonce(req).as_deref() == Some(nonce.as_str()) && !stale {
        return Ok(false);
    }
    let auth = &mut req.auth;
    auth.set("realm", ch.param("realm").unwrap_or_default());
    auth.set("nonce", nonce);
    auth.set("opaque", ch.param("opaque").unwrap_or_default());
    auth.set("algorithm", ch.param("algorithm").unwrap_or("MD5"));
    auth.set("qop", ch.param("qop").unwrap_or_default());
    let auth = req.auth.clone();
    let value = compute(&auth, req)?;
    req.set_header("Authorization", value);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::AuthType;

    #[test]
    fn challenge_roundtrip() {
        let mut req = PreparedRequest {
            url: "http://example.org/dir/index.html".into(),
            auth: Auth::new(AuthType::Digest).with("username", "Mufasa").with("password", "Circle of Life"),
            ..Default::default()
        };
        apply(&mut req).unwrap();
        assert!(req.header("authorization").is_none(), "no preemptive auth without nonce");
        let headers = vec![(
            "WWW-Authenticate".to_string(),
            r#"Digest realm="http-auth@example.org", qop="auth, auth-int", algorithm=MD5, nonce="abc", opaque="xyz""#
                .to_string(),
        ), (
            "WWW-Authenticate".to_string(),
            r#"Digest realm="http-auth@example.org", qop="auth", algorithm=SHA-256, nonce="abc", opaque="xyz""#
                .to_string(),
        )];
        assert!(handle_challenge(&mut req, &headers).unwrap());
        let h = req.header("authorization").unwrap().to_string();
        assert!(h.contains("algorithm=SHA-256"), "strongest algorithm chosen: {h}");
        assert!(h.contains("qop=auth,") && h.contains("uri=\"/dir/index.html\""));
        // Rejected again with the same nonce: stop.
        assert!(!handle_challenge(&mut req, &headers).unwrap());
        // Stale nonce: retry.
        let stale = vec![(
            "www-authenticate".to_string(),
            r#"Digest realm="r", nonce="abc", stale=true, qop="auth""#.to_string(),
        )];
        assert!(handle_challenge(&mut req, &stale).unwrap());
    }

    #[test]
    fn disable_retry() {
        let mut req = PreparedRequest {
            url: "http://x/".into(),
            auth: Auth::new(AuthType::Digest).with("disableRetryRequest", true),
            ..Default::default()
        };
        let h = vec![("www-authenticate".to_string(), r#"Digest realm="r", nonce="n""#.to_string())];
        assert!(!handle_challenge(&mut req, &h).unwrap());
    }
}
