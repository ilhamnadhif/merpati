//! OAuth 1.0a request signing (RFC 5849).

use base64::Engine;
use rsa::RsaPrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{SignatureEncoding, Signer};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use merpati_core::{Auth, PreparedBody, PreparedRequest};

use crate::error::AuthError;
use crate::jwt::normalize_pem;
use crate::util::{
    B64, hmac_sha1, hmac_sha256, hmac_sha512, is_form_body, now_secs, param, parse_url, random_alnum, rfc3986,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    HmacSha1,
    HmacSha256,
    HmacSha512,
    RsaSha1,
    RsaSha256,
    RsaSha512,
    Plaintext,
}

impl Method {
    fn parse(s: &str) -> Result<Method, AuthError> {
        Ok(match s.trim().to_ascii_uppercase().as_str() {
            "" | "HMAC-SHA1" => Method::HmacSha1,
            "HMAC-SHA256" => Method::HmacSha256,
            "HMAC-SHA512" => Method::HmacSha512,
            "RSA-SHA1" => Method::RsaSha1,
            "RSA-SHA256" => Method::RsaSha256,
            "RSA-SHA512" => Method::RsaSha512,
            "PLAINTEXT" => Method::Plaintext,
            other => return Err(AuthError::invalid(format!("unsupported OAuth 1.0 signature method `{other}`"))),
        })
    }

    fn name(self) -> &'static str {
        match self {
            Method::HmacSha1 => "HMAC-SHA1",
            Method::HmacSha256 => "HMAC-SHA256",
            Method::HmacSha512 => "HMAC-SHA512",
            Method::RsaSha1 => "RSA-SHA1",
            Method::RsaSha256 => "RSA-SHA256",
            Method::RsaSha512 => "RSA-SHA512",
            Method::Plaintext => "PLAINTEXT",
        }
    }
}

fn rsa_key(pem: &str) -> Result<RsaPrivateKey, AuthError> {
    let pem = normalize_pem(pem);
    RsaPrivateKey::from_pkcs8_pem(&pem)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(&pem))
        .map_err(|e| AuthError::Key(e.to_string()))
}

/// Base string URI: scheme and host lowercased, default port omitted, no
/// query or fragment (RFC 5849 section 3.4.1.2).
fn base_uri(url: &url::Url) -> String {
    let mut s =
        format!("{}://{}", url.scheme().to_ascii_lowercase(), url.host_str().unwrap_or_default().to_ascii_lowercase());
    if let Some(p) = url.port() {
        s.push_str(&format!(":{p}"));
    }
    s.push_str(url.path());
    s
}

fn decode_pairs(s: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(s.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

/// Normalized parameter string (section 3.4.1.3.2).
fn normalize_params(params: &[(String, String)]) -> String {
    let mut enc: Vec<(String, String)> = params.iter().map(|(k, v)| (rfc3986(k), rfc3986(v))).collect();
    enc.sort();
    enc.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

pub(crate) fn signature_base_string(method: &str, url: &url::Url, params: &[(String, String)]) -> String {
    format!(
        "{}&{}&{}",
        rfc3986(&method.to_ascii_uppercase()),
        rfc3986(&base_uri(url)),
        rfc3986(&normalize_params(params))
    )
}

fn sign(
    m: Method,
    base: &str,
    consumer_secret: &str,
    token_secret: &str,
    private_key: &str,
) -> Result<String, AuthError> {
    let key = format!("{}&{}", rfc3986(consumer_secret), rfc3986(token_secret));
    Ok(match m {
        Method::HmacSha1 => B64.encode(hmac_sha1(key.as_bytes(), base.as_bytes())),
        Method::HmacSha256 => B64.encode(hmac_sha256(key.as_bytes(), base.as_bytes())),
        Method::HmacSha512 => B64.encode(hmac_sha512(key.as_bytes(), base.as_bytes())),
        Method::Plaintext => key,
        Method::RsaSha1 => {
            let k = rsa::pkcs1v15::SigningKey::<Sha1>::new(rsa_key(private_key)?);
            B64.encode(k.sign(base.as_bytes()).to_bytes())
        }
        Method::RsaSha256 => {
            let k = rsa::pkcs1v15::SigningKey::<Sha256>::new(rsa_key(private_key)?);
            B64.encode(k.sign(base.as_bytes()).to_bytes())
        }
        Method::RsaSha512 => {
            let k = rsa::pkcs1v15::SigningKey::<Sha512>::new(rsa_key(private_key)?);
            B64.encode(k.sign(base.as_bytes()).to_bytes())
        }
    })
}

/// Fixed inputs that are normally generated (for tests).
#[derive(Default)]
pub(crate) struct Overrides {
    pub timestamp: Option<String>,
    pub nonce: Option<String>,
}

pub(crate) fn apply(req: &mut PreparedRequest) -> Result<(), AuthError> {
    apply_with(req, &Overrides::default())
}

pub(crate) fn apply_with(req: &mut PreparedRequest, ov: &Overrides) -> Result<(), AuthError> {
    let auth: Auth = req.auth.clone();
    let method = Method::parse(auth.get("signatureMethod"))?;
    let consumer_key = auth.get("consumerKey");
    let url = parse_url(req)?;
    let add_empty = auth.get_bool("addEmptyParamsToSign", false);

    let timestamp = ov.timestamp.clone().unwrap_or_else(|| match param(&auth, "timestamp") {
        "" => now_secs().to_string(),
        t => t.to_string(),
    });
    let nonce = ov.nonce.clone().unwrap_or_else(|| match param(&auth, "nonce") {
        "" => random_alnum(11),
        n => n.to_string(),
    });
    let version = match auth.params.get("version") {
        None => "1.0",
        Some(_) => param(&auth, "version"),
    };

    let mut oauth: Vec<(String, String)> = vec![
        ("oauth_consumer_key".into(), consumer_key.into()),
        ("oauth_signature_method".into(), method.name().into()),
        ("oauth_timestamp".into(), timestamp),
        ("oauth_nonce".into(), nonce),
    ];
    for (key, name) in [("token", "oauth_token"), ("callback", "oauth_callback"), ("verifier", "oauth_verifier")] {
        let v = auth.get(key);
        if !v.is_empty() || add_empty {
            oauth.push((name.into(), v.into()));
        }
    }
    if !version.is_empty() {
        oauth.push(("oauth_version".into(), version.into()));
    }

    let form_body = is_form_body(req);
    if auth.get_bool("includeBodyHash", false)
        && !form_body
        && let Some(body) = req.body.as_bytes()
    {
        let hash = match method {
            Method::HmacSha256 | Method::RsaSha256 => B64.encode(Sha256::digest(body)),
            Method::HmacSha512 | Method::RsaSha512 => B64.encode(Sha512::digest(body)),
            _ => B64.encode(Sha1::digest(body)),
        };
        oauth.push(("oauth_body_hash".into(), hash));
    }

    // Signature parameters: query + oauth_* + form body.
    let mut sig_params: Vec<(String, String)> = url.query().map(decode_pairs).unwrap_or_default();
    sig_params.extend(oauth.iter().cloned());
    if form_body && let Some(b) = req.body.as_bytes() {
        sig_params.extend(decode_pairs(&String::from_utf8_lossy(b)));
    }
    let base = signature_base_string(&req.method, &url, &sig_params);
    let signature = sign(method, &base, auth.get("consumerSecret"), auth.get("tokenSecret"), auth.get("privateKey"))?;
    oauth.push(("oauth_signature".into(), signature));

    if auth.get_bool("addParamsToHeader", false) {
        let encode = !auth.get_bool("disableHeaderEncoding", false);
        let mut parts = Vec::new();
        let realm = auth.get("realm");
        if !realm.is_empty() {
            parts.push(format!("realm=\"{realm}\""));
        }
        for (k, v) in &oauth {
            let v = if encode { rfc3986(v) } else { v.clone() };
            parts.push(format!("{k}=\"{v}\""));
        }
        req.set_header("Authorization", format!("OAuth {}", parts.join(",")));
    } else if form_body && req.method != "GET" && req.method != "HEAD" {
        let mut body = req.body.as_bytes().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
        for (k, v) in &oauth {
            if !body.is_empty() {
                body.push('&');
            }
            body.push_str(&format!("{}={}", rfc3986(k), rfc3986(v)));
        }
        req.body = PreparedBody::Bytes(body.into());
    } else {
        for (k, v) in &oauth {
            req.add_query(k, v);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::AuthType;

    fn twitter_request(add_to_header: bool) -> PreparedRequest {
        let auth = Auth::new(AuthType::OAuth1)
            .with("consumerKey", "xvz1evFS4wEEPTGEFPHBog")
            .with("consumerSecret", "kAcSOqF21Fu85e7zjz7ZN2U4ZRhfV3WpwPAoE3Z7kBw")
            .with("token", "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb")
            .with("tokenSecret", "LswwdoUaIvS8ltyTt5jkRh4J50vUPVVHtR2YPi5kE")
            .with("signatureMethod", "HMAC-SHA1")
            .with("timestamp", "1318622958")
            .with("nonce", "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg")
            .with("version", "1.0")
            .with("addParamsToHeader", add_to_header);
        PreparedRequest {
            method: "POST".into(),
            url: "https://api.twitter.com/1.1/statuses/update.json?include_entities=true".into(),
            headers: vec![("Content-Type".into(), "application/x-www-form-urlencoded".into())],
            body: PreparedBody::Bytes(
                "status=Hello%20Ladies%20%2b%20Gentlemen%2c%20a%20signed%20OAuth%20request%21".into(),
            ),
            auth,
            ..Default::default()
        }
    }

    #[test]
    fn twitter_reference_signature() {
        // https://developer.twitter.com/en/docs/authentication/oauth-1-0a/creating-a-signature
        let mut req = twitter_request(true);
        apply(&mut req).unwrap();
        let h = req.header("authorization").unwrap();
        assert!(h.starts_with("OAuth oauth_consumer_key=\"xvz1evFS4wEEPTGEFPHBog\""), "{h}");
        assert!(h.contains("oauth_signature=\"hCtSmYh%2BiHYCEqBWrE7C7hYmtUk%3D\""), "{h}");
        assert!(h.contains("oauth_token=\"370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb\""));
    }

    #[test]
    fn twitter_base_string() {
        let url = url::Url::parse("https://api.twitter.com/1.1/statuses/update.json?include_entities=true").unwrap();
        let mut params = decode_pairs("include_entities=true");
        params.extend(decode_pairs("status=Hello%20Ladies%20%2b%20Gentlemen%2c%20a%20signed%20OAuth%20request%21"));
        for (k, v) in [
            ("oauth_consumer_key", "xvz1evFS4wEEPTGEFPHBog"),
            ("oauth_nonce", "kYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg"),
            ("oauth_signature_method", "HMAC-SHA1"),
            ("oauth_timestamp", "1318622958"),
            ("oauth_token", "370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb"),
            ("oauth_version", "1.0"),
        ] {
            params.push((k.into(), v.into()));
        }
        assert_eq!(
            signature_base_string("POST", &url, &params),
            "POST&https%3A%2F%2Fapi.twitter.com%2F1.1%2Fstatuses%2Fupdate.json&include_entities%3Dtrue%26oauth_consumer_key%3Dxvz1evFS4wEEPTGEFPHBog%26oauth_nonce%3DkYjzVBB8Y0ZFabxSWbWovY3uYSQ2pTgmZeNu2VS4cg%26oauth_signature_method%3DHMAC-SHA1%26oauth_timestamp%3D1318622958%26oauth_token%3D370773112-GmHxMAgYyLbNEtIKZeRNFsMKPR9EyMZeS9weJAEb%26oauth_version%3D1.0%26status%3DHello%2520Ladies%2520%252B%2520Gentlemen%252C%2520a%2520signed%2520OAuth%2520request%2521"
        );
    }

    #[test]
    fn oauth_core_appendix_a5() {
        // OAuth Core 1.0 Appendix A.5.1 (photos.example.net), HMAC-SHA1.
        let auth = Auth::new(AuthType::OAuth1)
            .with("consumerKey", "dpf43f3p2l4k3l03")
            .with("consumerSecret", "kd94hf93k423kf44")
            .with("token", "nnch734d00sl2jdk")
            .with("tokenSecret", "pfkkdhi9sl3r4s00")
            .with("timestamp", "1191242096")
            .with("nonce", "kllo9940pd9333jh")
            .with("addParamsToHeader", true);
        let mut req = PreparedRequest {
            url: "http://photos.example.net/photos?file=vacation.jpg&size=original".into(),
            auth,
            ..Default::default()
        };
        apply(&mut req).unwrap();
        assert!(
            req.header("authorization").unwrap().contains("oauth_signature=\"tR3%2BTy81lMeYAr%2FFid0kMTYa%2FWM%3D\"")
        );
    }

    #[test]
    fn body_and_query_placement() {
        // Not in header: params go into the form body for POST.
        let mut req = twitter_request(false);
        apply(&mut req).unwrap();
        let body = String::from_utf8(req.body.as_bytes().unwrap().to_vec()).unwrap();
        assert!(body.contains("oauth_signature=hCtSmYh%2BiHYCEqBWrE7C7hYmtUk%3D"), "{body}");
        assert!(req.header("authorization").is_none());
        // GET: query string.
        let auth = Auth::new(AuthType::OAuth1).with("consumerKey", "ck").with("consumerSecret", "cs");
        let mut req = PreparedRequest { url: "http://h/p?a=1".into(), auth, ..Default::default() };
        apply(&mut req).unwrap();
        assert!(req.url.contains("oauth_consumer_key=ck"));
        assert!(req.url.contains("oauth_signature="));
    }

    #[test]
    fn plaintext_and_rsa() {
        let auth = Auth::new(AuthType::OAuth1)
            .with("consumerKey", "ck")
            .with("consumerSecret", "c s")
            .with("tokenSecret", "t&s")
            .with("signatureMethod", "PLAINTEXT")
            .with("addParamsToHeader", true);
        let mut req = PreparedRequest { url: "http://h/".into(), auth, ..Default::default() };
        apply(&mut req).unwrap();
        // key "c%20s&t%26s", then percent-encoded again in the header.
        assert!(req.header("authorization").unwrap().contains("oauth_signature=\"c%2520s%26t%2526s\""));

        let key = include_str!("../tests/fixtures/rsa_pkcs1.pem");
        for m in ["RSA-SHA1", "RSA-SHA256", "RSA-SHA512"] {
            let auth = Auth::new(AuthType::OAuth1)
                .with("consumerKey", "ck")
                .with("signatureMethod", m)
                .with("privateKey", key)
                .with("addParamsToHeader", true);
            let mut req = PreparedRequest { url: "http://h/".into(), auth, ..Default::default() };
            apply(&mut req).unwrap();
            assert!(req.header("authorization").unwrap().contains(&format!("oauth_signature_method=\"{m}\"")));
        }
    }

    #[test]
    fn body_hash_for_non_form_bodies() {
        let auth = Auth::new(AuthType::OAuth1)
            .with("consumerKey", "ck")
            .with("includeBodyHash", true)
            .with("addParamsToHeader", true);
        let mut req = PreparedRequest {
            method: "POST".into(),
            url: "http://h/".into(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: PreparedBody::Bytes("Hello World!".into()),
            auth,
            ..Default::default()
        };
        apply(&mut req).unwrap();
        // SHA-1("Hello World!") base64 = Lve95gjOVATpfV8EL5X4nxwjKHE=
        assert!(req.header("authorization").unwrap().contains("oauth_body_hash=\"Lve95gjOVATpfV8EL5X4nxwjKHE%3D\""));
    }
}
