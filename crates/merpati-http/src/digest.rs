//! The computation of HTTP Digest access authentication (RFC 7616,
//! compatible with RFC 2617 and RFC 2069), shared by the Digest auth scheme
//! (`merpati-auth`) and the proxy tunnels (`Proxy-Authenticate: Digest`).

use md5::Md5;
use sha2::{Digest as _, Sha256, Sha512_256};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    Md5,
    Sha256,
    Sha512_256,
}

impl Algorithm {
    fn hash(self, data: &[u8]) -> String {
        match self {
            Algorithm::Md5 => hex::encode(Md5::digest(data)),
            Algorithm::Sha256 => hex::encode(Sha256::digest(data)),
            Algorithm::Sha512_256 => hex::encode(Sha512_256::digest(data)),
        }
    }

    /// Orders the algorithms from weakest (MD5) to strongest, to pick the
    /// best of several challenges.
    pub fn strength(self) -> u8 {
        match self {
            Algorithm::Md5 => 0,
            Algorithm::Sha256 => 1,
            Algorithm::Sha512_256 => 2,
        }
    }
}

/// Parses `MD5`, `MD5-sess`, `SHA-256`, `SHA-512-256-sess`, ... into
/// (algorithm, is_session); an empty name is MD5.
pub fn parse_algorithm(s: &str) -> Option<(Algorithm, bool)> {
    let upper = s.trim().to_ascii_uppercase();
    let (base, sess) = match upper.strip_suffix("-SESS") {
        Some(b) => (b.to_string(), true),
        None => (upper, false),
    };
    let alg = match base.as_str() {
        "" | "MD5" => Algorithm::Md5,
        "SHA-256" => Algorithm::Sha256,
        "SHA-512-256" => Algorithm::Sha512_256,
        _ => return None,
    };
    Some((alg, sess))
}

/// Inputs of one digest computation.
pub struct DigestInput<'a> {
    pub username: &'a str,
    pub password: &'a str,
    pub realm: &'a str,
    pub nonce: &'a str,
    pub algorithm: &'a str,
    /// `auth`, `auth-int` or empty (RFC 2069 compatibility).
    pub qop: &'a str,
    pub nc: &'a str,
    pub cnonce: &'a str,
    pub opaque: &'a str,
    pub method: &'a str,
    /// The request target: `/path?query`, or `host:port` for `CONNECT`.
    pub uri: &'a str,
    pub body: &'a [u8],
}

/// Computes the `response` value; fails on an unsupported algorithm.
pub fn response(i: &DigestInput<'_>) -> Result<String, String> {
    let (alg, sess) =
        parse_algorithm(i.algorithm).ok_or_else(|| format!("unsupported digest algorithm `{}`", i.algorithm))?;
    let mut ha1 = alg.hash(format!("{}:{}:{}", i.username, i.realm, i.password).as_bytes());
    if sess {
        ha1 = alg.hash(format!("{ha1}:{}:{}", i.nonce, i.cnonce).as_bytes());
    }
    let ha2 = if i.qop.eq_ignore_ascii_case("auth-int") {
        alg.hash(format!("{}:{}:{}", i.method, i.uri, alg.hash(i.body)).as_bytes())
    } else {
        alg.hash(format!("{}:{}", i.method, i.uri).as_bytes())
    };
    Ok(if i.qop.is_empty() {
        alg.hash(format!("{ha1}:{}:{ha2}", i.nonce).as_bytes())
    } else {
        alg.hash(format!("{ha1}:{}:{}:{}:{}:{ha2}", i.nonce, i.nc, i.cnonce, i.qop).as_bytes())
    })
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Builds the full `Authorization` (or `Proxy-Authorization`) header value.
pub fn header_value(i: &DigestInput<'_>) -> Result<String, String> {
    let resp = response(i)?;
    let mut parts = vec![
        format!("username={}", quote(i.username)),
        format!("realm={}", quote(i.realm)),
        format!("nonce={}", quote(i.nonce)),
        format!("uri={}", quote(i.uri)),
    ];
    if !i.algorithm.is_empty() {
        parts.push(format!("algorithm={}", i.algorithm));
    }
    if !i.qop.is_empty() {
        parts.push(format!("qop={}", i.qop));
        parts.push(format!("nc={}", i.nc));
        parts.push(format!("cnonce={}", quote(i.cnonce)));
    }
    parts.push(format!("response={}", quote(&resp)));
    if !i.opaque.is_empty() {
        parts.push(format!("opaque={}", quote(i.opaque)));
    }
    Ok(format!("Digest {}", parts.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rfc7616(algorithm: &'static str) -> DigestInput<'static> {
        DigestInput {
            username: "Mufasa",
            password: "Circle of Life",
            realm: "http-auth@example.org",
            nonce: "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v",
            algorithm,
            qop: "auth",
            nc: "00000001",
            cnonce: "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ",
            opaque: "FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS",
            method: "GET",
            uri: "/dir/index.html",
            body: b"",
        }
    }

    #[test]
    fn rfc7616_md5_and_sha256() {
        // RFC 7616 section 3.9.1.
        assert_eq!(response(&rfc7616("MD5")).unwrap(), "8ca523f5e9506fed4657c9700eebdbec");
        assert_eq!(
            response(&rfc7616("SHA-256")).unwrap(),
            "753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1"
        );
    }

    #[test]
    fn rfc2617_example() {
        // RFC 2617 section 3.5.
        let i = DigestInput {
            username: "Mufasa",
            password: "Circle Of Life",
            realm: "testrealm@host.com",
            nonce: "dcd98b7102dd2f0e8b11d0f600bfb0c093",
            algorithm: "MD5",
            qop: "auth",
            nc: "00000001",
            cnonce: "0a4f113b",
            opaque: "5ccc069c403ebaf9f0171e9517f40e41",
            method: "GET",
            uri: "/dir/index.html",
            body: b"",
        };
        assert_eq!(response(&i).unwrap(), "6629fae49393a05397450978507c4ef1");
        let h = header_value(&i).unwrap();
        assert!(h.starts_with("Digest username=\"Mufasa\", realm=\"testrealm@host.com\""));
        assert!(h.contains("response=\"6629fae49393a05397450978507c4ef1\""));
        assert!(h.contains("nc=00000001"));
    }

    #[test]
    fn session_and_int_variants_differ() {
        let base = response(&rfc7616("SHA-256")).unwrap();
        assert_ne!(response(&rfc7616("SHA-256-sess")).unwrap(), base);
        assert_eq!(response(&rfc7616("SHA-512-256")).unwrap().len(), 64);
        let mut i = rfc7616("MD5");
        i.qop = "auth-int";
        i.body = b"hello";
        assert_ne!(response(&i).unwrap(), "8ca523f5e9506fed4657c9700eebdbec");
        assert!(parse_algorithm("SHA-1").is_none());
        assert!(response(&rfc7616("SHA-1")).unwrap_err().contains("SHA-1"));
    }
}
