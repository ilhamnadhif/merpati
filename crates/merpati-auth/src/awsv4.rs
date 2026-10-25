//! AWS Signature Version 4.

use chrono::{DateTime, NaiveDateTime, Utc};

use merpati_core::{PreparedBody, PreparedRequest};

use crate::error::AuthError;
use crate::util::{hmac_sha256, host_header, param, parse_url, rfc3986, sha256};

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const UNSIGNED: &str = "UNSIGNED-PAYLOAD";
const PRESIGN_EXPIRES: u32 = 86_400;
/// RFC 3986 unreserved characters and `/` are left alone in S3 object keys.
const S3_KEY: &percent_encoding::AsciiSet = &crate::util::RFC3986.remove(b'/');

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Canonical URI. S3 signs the object key URI-encoded once: the path is
/// decoded (`+` is a space in S3 paths) and every byte but the unreserved
/// ones and `/` encoded, as Postman's aws4 does, so characters the URL leaves
/// as is (`=`, `(`, `!`, ...) match. Other services encode each segment as
/// sent once more (as the AWS SDKs do).
fn canonical_uri(url: &url::Url, s3: bool) -> String {
    let path = url.path();
    let path = if path.is_empty() { "/" } else { path };
    if s3 {
        let decoded = percent_encoding::percent_decode_str(&path.replace('+', " ")).collect::<Vec<u8>>();
        return percent_encoding::percent_encode(&decoded, S3_KEY).to_string();
    }
    path.split('/').map(rfc3986_keep_nothing).collect::<Vec<_>>().join("/")
}

fn rfc3986_keep_nothing(seg: &str) -> String {
    rfc3986(seg)
}

fn canonical_query(url: &url::Url, extra: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (rfc3986(&k), rfc3986(&v)))
        .chain(extra.iter().map(|(k, v)| (rfc3986(k), rfc3986(v))))
        .collect();
    pairs.sort();
    pairs.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

fn trim_value(v: &str) -> String {
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_signed_header(name: &str) -> bool {
    name == "host" || name == "content-type" || name.starts_with("x-amz-")
}

fn amz_now(req: &PreparedRequest) -> DateTime<Utc> {
    req.header("x-amz-date")
        .and_then(|d| NaiveDateTime::parse_from_str(d.trim(), "%Y%m%dT%H%M%SZ").ok())
        .map(|n| n.and_utc())
        .unwrap_or_else(Utc::now)
}

pub(crate) fn apply(req: &mut PreparedRequest) -> Result<(), AuthError> {
    let now = amz_now(req);
    apply_at(req, now)
}

pub(crate) fn apply_at(req: &mut PreparedRequest, now: DateTime<Utc>) -> Result<(), AuthError> {
    let auth = req.auth.clone();
    let access = param(&auth, "accessKey");
    let secret = auth.get("secretKey");
    if access.is_empty() {
        return Err(AuthError::Missing { scheme: "AWS Signature", field: "accessKey" });
    }
    if secret.is_empty() {
        return Err(AuthError::Missing { scheme: "AWS Signature", field: "secretKey" });
    }
    let region = match param(&auth, "region") {
        "" => "us-east-1",
        r => r,
    };
    let service = match param(&auth, "service") {
        "" => "execute-api",
        s => s,
    };
    let session = param(&auth, "sessionToken");
    let to_query = auth.get_bool("addAuthDataToQuery", false);
    let s3 = service.eq_ignore_ascii_case("s3");

    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let url = parse_url(req)?;

    // Presigned S3 URLs leave the payload unsigned; other services sign it.
    let payload_hash = if to_query && s3 {
        UNSIGNED.to_string()
    } else {
        match &req.body {
            PreparedBody::None => hex::encode(sha256(b"")),
            PreparedBody::Bytes(b) => hex::encode(sha256(b)),
            _ => UNSIGNED.to_string(),
        }
    };

    let mut extra_query = Vec::new();
    if to_query {
        extra_query.push(("X-Amz-Algorithm".to_string(), ALGORITHM.to_string()));
        extra_query.push(("X-Amz-Credential".to_string(), format!("{access}/{scope}")));
        extra_query.push(("X-Amz-Date".to_string(), amz_date.clone()));
        extra_query.push(("X-Amz-Expires".to_string(), PRESIGN_EXPIRES.to_string()));
        if !session.is_empty() {
            extra_query.push(("X-Amz-Security-Token".to_string(), session.to_string()));
        }
        extra_query.push(("X-Amz-SignedHeaders".to_string(), "host".to_string()));
    } else {
        req.set_header("X-Amz-Date", amz_date.clone());
        if !session.is_empty() {
            req.set_header("X-Amz-Security-Token", session.to_string());
        }
        if s3 {
            req.set_header("X-Amz-Content-Sha256", payload_hash.clone());
        }
    }

    // Canonical headers.
    let mut headers: Vec<(String, String)> = vec![("host".into(), host_header(&url))];
    if !to_query {
        for (k, v) in &req.headers {
            let lk = k.to_ascii_lowercase();
            if lk != "host" && is_signed_header(&lk) {
                match headers.iter_mut().find(|(n, _)| *n == lk) {
                    Some((_, existing)) => {
                        existing.push(',');
                        existing.push_str(&trim_value(v));
                    }
                    None => headers.push((lk, trim_value(v))),
                }
            }
        }
    }
    headers.sort();
    let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method.to_ascii_uppercase(),
        canonical_uri(&url, s3),
        canonical_query(&url, &extra_query),
        canonical_headers,
        signed_headers,
        payload_hash
    );
    let string_to_sign =
        format!("{ALGORITHM}\n{amz_date}\n{scope}\n{}", hex::encode(sha256(canonical_request.as_bytes())));
    let signature = hex::encode(hmac_sha256(&signing_key(secret, &date, region, service), string_to_sign.as_bytes()));

    if to_query {
        for (k, v) in &extra_query {
            req.add_query(k, v);
        }
        req.add_query("X-Amz-Signature", &signature);
    } else {
        req.set_header(
            "Authorization",
            format!("{ALGORITHM} Credential={access}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::{Auth, AuthType};

    fn suite_auth() -> Auth {
        // aws-sig-v4-test-suite credentials.
        Auth::new(AuthType::AwsV4)
            .with("accessKey", "AKIDEXAMPLE")
            .with("secretKey", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY")
            .with("region", "us-east-1")
            .with("service", "service")
    }

    #[test]
    fn get_vanilla() {
        let mut req = PreparedRequest {
            url: "https://example.amazonaws.com/".into(),
            headers: vec![("X-Amz-Date".into(), "20150830T123600Z".into())],
            auth: suite_auth(),
            ..Default::default()
        };
        apply(&mut req).unwrap();
        assert_eq!(
            req.header("authorization").unwrap(),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn post_x_www_form_urlencoded() {
        let mut req = PreparedRequest {
            method: "POST".into(),
            url: "https://example.amazonaws.com/".into(),
            headers: vec![
                ("Content-Type".into(), "application/x-www-form-urlencoded".into()),
                ("X-Amz-Date".into(), "20150830T123600Z".into()),
            ],
            body: PreparedBody::Bytes("Param1=value1".into()),
            auth: suite_auth(),
            ..Default::default()
        };
        apply(&mut req).unwrap();
        assert_eq!(
            req.header("authorization").unwrap(),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a"
        );
    }

    #[test]
    fn signing_key_example() {
        // AWS docs "Deriving the signing key" example.
        let key = signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20120215", "us-east-1", "iam");
        assert_eq!(hex::encode(key), "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d");
    }

    #[test]
    fn s3_and_session_and_presign() {
        let mut req = PreparedRequest {
            url: "https://bucket.s3.amazonaws.com/my%20file.txt".into(),
            auth: suite_auth().with("service", "s3").with("sessionToken", "tok"),
            ..Default::default()
        };
        apply(&mut req).unwrap();
        assert!(req.header("x-amz-content-sha256").is_some());
        assert_eq!(req.header("x-amz-security-token"), Some("tok"));
        let h = req.header("authorization").unwrap();
        assert!(h.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token"), "{h}");

        let mut req = PreparedRequest {
            url: "https://example.amazonaws.com/?a=1".into(),
            auth: suite_auth().with("addAuthDataToQuery", true),
            ..Default::default()
        };
        apply_at(&mut req, Utc::now()).unwrap();
        assert!(req.header("authorization").is_none());
        assert!(req.url.contains("X-Amz-Signature="));
        assert!(req.url.contains("X-Amz-Credential=AKIDEXAMPLE%2F"));
    }

    #[test]
    fn s3_presigned_url_docs_example() {
        // Amazon S3 docs, "Authenticating Requests: Using Query Parameters".
        let mut req = PreparedRequest {
            url: "https://examplebucket.s3.amazonaws.com/test.txt".into(),
            auth: Auth::new(AuthType::AwsV4)
                .with("accessKey", "AKIAIOSFODNN7EXAMPLE")
                .with("secretKey", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY")
                .with("region", "us-east-1")
                .with("service", "s3")
                .with("addAuthDataToQuery", true),
            ..Default::default()
        };
        let now = NaiveDateTime::parse_from_str("20130524T000000Z", "%Y%m%dT%H%M%SZ").unwrap().and_utc();
        apply_at(&mut req, now).unwrap();
        assert!(
            req.url.ends_with("&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"),
            "{}",
            req.url
        );
    }

    #[test]
    fn presigned_url_for_other_services_signs_the_payload_hash() {
        // UNSIGNED-PAYLOAD is S3 only; other services (API Gateway, IoT, ...)
        // expect the payload hash in presigned URLs too, as Postman's aws4 signs.
        let mut req = PreparedRequest {
            url: "https://example.amazonaws.com/".into(),
            auth: suite_auth().with("addAuthDataToQuery", true),
            ..Default::default()
        };
        let now = NaiveDateTime::parse_from_str("20150830T123600Z", "%Y%m%dT%H%M%SZ").unwrap().and_utc();
        apply_at(&mut req, now).unwrap();
        let query = "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIDEXAMPLE%2F20150830%2Fus-east-1%2Fservice%2Faws4_request&X-Amz-Date=20150830T123600Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host";
        let canonical = format!("GET\n/\n{query}\nhost:example.amazonaws.com\n\nhost\n{}", hex::encode(sha256(b"")));
        let to_sign = format!(
            "{ALGORITHM}\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n{}",
            hex::encode(sha256(canonical.as_bytes()))
        );
        let key = signing_key("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "20150830", "us-east-1", "service");
        let signature = hex::encode(hmac_sha256(&key, to_sign.as_bytes()));
        assert_eq!(req.url, format!("https://example.amazonaws.com/?{query}&X-Amz-Signature={signature}"));
    }

    #[test]
    fn s3_canonical_uri_encodes_the_key_once() {
        // S3 signs the URI-encoded object key (everything but unreserved
        // characters and `/`), whatever the URL left unescaped, and reads `+`
        // in the path as a space (like Postman's aws4).
        let uri =
            |path: &str| canonical_uri(&url::Url::parse(&format!("https://b.s3.amazonaws.com{path}")).unwrap(), true);
        assert_eq!(uri("/test.txt"), "/test.txt");
        assert_eq!(uri("/my%20file.txt"), "/my%20file.txt");
        assert_eq!(uri("/year=2024/photo (1)!.jpg"), "/year%3D2024/photo%20%281%29%21.jpg");
        assert_eq!(uri("/a+b/c,d;e:f@g$h&i'j*k"), "/a%20b/c%2Cd%3Be%3Af%40g%24h%26i%27j%2Ak");
        assert_eq!(uri("/%E1%88%B4/x%2Fy"), "/%E1%88%B4/x/y");
        assert_eq!(uri(""), "/");
        // Other services keep encoding the path as sent once more.
        let other = |path: &str| canonical_uri(&url::Url::parse(&format!("https://h{path}")).unwrap(), false);
        assert_eq!(other("/my%20file/a=b"), "/my%2520file/a%3Db");
    }

    #[test]
    fn missing_keys() {
        let mut req =
            PreparedRequest { url: "https://x/".into(), auth: Auth::new(AuthType::AwsV4), ..Default::default() };
        assert!(matches!(apply(&mut req), Err(AuthError::Missing { field: "accessKey", .. })));
    }
}
