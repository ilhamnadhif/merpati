//! Cookie jar (FR-R7): RFC 6265 storage shared by all requests, viewable and
//! editable from the UI, persisted in the workspace's local folder.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use cookie_store::{CookieDomain, CookieExpiration, CookieStore};
use parking_lot::RwLock;
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use url::Url;

use merpati_core::script::ScriptCookie;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookieInfo {
    pub domain: String,
    /// True when the cookie only applies to `domain` exactly (no Domain attribute).
    pub host_only: bool,
    pub path: String,
    pub name: String,
    pub value: String,
    /// Unix seconds; `None` for session cookies.
    pub expires: Option<i64>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<String>,
}

impl CookieInfo {
    /// `Set-Cookie`-style text, used by the cookie editor.
    pub fn to_set_cookie(&self) -> String {
        let mut s = format!("{}={}; Path={}", self.name, self.value, self.path);
        if !self.host_only {
            s.push_str(&format!("; Domain={}", self.domain));
        }
        // HTTP-date (`Wed, 21 Oct 2037 07:28:00 GMT`): cookie parsers ignore an
        // Expires with a numeric offset, which turned edited cookies into
        // session cookies.
        if let Some(exp) = self.expires
            && let Ok(t) = time::OffsetDateTime::from_unix_timestamp(exp)
            && let Ok(f) = t.format(&time::format_description::well_known::Rfc2822)
            && let Some(date) = f.strip_suffix(" +0000")
        {
            s.push_str(&format!("; Expires={date} GMT"));
        }
        if self.secure {
            s.push_str("; Secure");
        }
        if self.http_only {
            s.push_str("; HttpOnly");
        }
        if let Some(ss) = &self.same_site {
            s.push_str(&format!("; SameSite={ss}"));
        }
        s
    }
}

pub struct CookieJar {
    store: RwLock<CookieStore>,
    path: Option<PathBuf>,
}

impl Default for CookieJar {
    fn default() -> Self {
        CookieJar { store: RwLock::new(CookieStore::new_with_public_suffix(Some(public_suffixes()))), path: None }
    }
}

/// The public suffix list (publicsuffix.org), parsed once. With it the store
/// ignores cookies whose Domain attribute is a public suffix (`com`, `co.uk`,
/// `github.io`), which would otherwise be sent to every site under it
/// (RFC 6265 section 5.3 step 5, as browsers and Postman do).
pub(crate) fn public_suffixes() -> publicsuffix::List {
    static LIST: OnceLock<publicsuffix::List> = OnceLock::new();
    LIST.get_or_init(|| include_str!("../data/public_suffix_list.dat").parse().unwrap_or_default()).clone()
}

fn domain_of(c: &cookie_store::Cookie<'_>) -> (String, bool) {
    match &c.domain {
        CookieDomain::HostOnly(d) => (d.clone(), true),
        CookieDomain::Suffix(d) => (d.clone(), false),
        CookieDomain::NotPresent | CookieDomain::Empty => (String::new(), true),
    }
}

fn info(c: &cookie_store::Cookie<'_>) -> CookieInfo {
    let (domain, host_only) = domain_of(c);
    CookieInfo {
        domain,
        host_only,
        path: String::from(&c.path),
        name: c.name().to_string(),
        value: c.value().to_string(),
        expires: match c.expires {
            CookieExpiration::AtUtc(t) => Some(t.unix_timestamp()),
            CookieExpiration::SessionEnd => None,
        },
        secure: c.secure().unwrap_or(false),
        http_only: c.http_only().unwrap_or(false),
        same_site: c.same_site().map(|s| s.to_string()),
    }
}

impl CookieJar {
    pub fn new() -> Self {
        CookieJar::default()
    }

    /// Loads a persisted jar; a missing or unreadable file yields an empty jar.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let store = std::fs::File::open(&path)
            .ok()
            .and_then(|f| cookie_store::serde::json::load_all(BufReader::new(f)).ok())
            .unwrap_or_default()
            .with_suffix_list(public_suffixes());
        CookieJar { store: RwLock::new(store), path: Some(path) }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Persists the jar (including session cookies, like Postman) if it has a
    /// path. Expired cookies are dropped.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let mut buf = Vec::new();
        {
            let store = self.store.read();
            let live: Vec<cookie_store::Cookie<'static>> = store.iter_unexpired().cloned().collect();
            let fresh = CookieStore::from_cookies(live.into_iter().map(Ok::<_, std::convert::Infallible>), true)
                .unwrap_or_default();
            cookie_store::serde::json::save_incl_expired_and_nonpersistent(&fresh, &mut buf)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        merpati_core::storage::write_atomic(path, &buf)
    }

    pub fn list(&self) -> Vec<CookieInfo> {
        let mut out: Vec<CookieInfo> = self.store.read().iter_unexpired().map(info).collect();
        out.sort_by(|a, b| (&a.domain, &a.path, &a.name).cmp(&(&b.domain, &b.path, &b.name)));
        out
    }

    pub fn domains(&self) -> Vec<String> {
        let mut d: Vec<String> = self.list().into_iter().map(|c| c.domain).collect();
        d.dedup();
        d
    }

    /// Cookies that would be sent to `url`.
    pub fn for_url(&self, url: &str) -> Vec<ScriptCookie> {
        let Ok(u) = Url::parse(url) else { return Vec::new() };
        self.store
            .read()
            .matches(&u)
            .into_iter()
            .map(|c| {
                let i = info(c);
                ScriptCookie {
                    name: i.name,
                    value: i.value,
                    domain: i.domain,
                    path: i.path,
                    secure: i.secure,
                    http_only: i.http_only,
                    expires: i.expires,
                }
            })
            .collect()
    }

    /// Adds or replaces a cookie from `Set-Cookie` syntax. `domain` is used
    /// when the text has no Domain attribute.
    pub fn set(&self, domain: &str, set_cookie: &str) -> Result<(), String> {
        let domain = domain.trim().trim_start_matches('.');
        let url = Url::parse(&format!("https://{domain}/")).map_err(|e| format!("invalid domain: {e}"))?;
        let mut store = self.store.write();
        // Path attribute decides the cookie path; default-path from "/" is "/".
        match store.parse(set_cookie, &url) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("invalid cookie: {e}")),
        }
    }

    pub fn remove(&self, domain: &str, path: &str, name: &str) -> bool {
        self.store.write().remove(domain, path, name).is_some()
    }

    pub fn clear_domain(&self, domain: &str) {
        let doomed: Vec<CookieInfo> = self.list().into_iter().filter(|c| c.domain == domain).collect();
        let mut store = self.store.write();
        for c in doomed {
            store.remove(&c.domain, &c.path, &c.name);
        }
    }

    pub fn clear(&self) {
        self.store.write().clear();
    }
}

impl reqwest::cookie::CookieStore for CookieJar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        let cookies: Vec<cookie::Cookie<'static>> = cookie_headers
            .filter_map(|v| std::str::from_utf8(v.as_bytes()).ok())
            .filter_map(|s| cookie::Cookie::parse(s.to_owned()).ok())
            .collect();
        self.store.write().store_response_cookies(cookies.into_iter(), url);
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        let s = self
            .store
            .read()
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        if s.is_empty() { None } else { HeaderValue::from_str(&s).ok() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::cookie::CookieStore as _;

    #[test]
    fn store_list_edit_persist() {
        let dir = tempfile::tempdir().unwrap();
        let jar = CookieJar::open(dir.path().join("cookies.json"));
        let url = Url::parse("https://api.example.com/login").unwrap();
        let h1 = HeaderValue::from_static("session=abc; Path=/; HttpOnly");
        let h2 = HeaderValue::from_static("pref=dark; Domain=example.com; Path=/; Max-Age=3600");
        jar.set_cookies(&mut [&h1, &h2].into_iter(), &url);
        let header = jar.cookies(&Url::parse("https://api.example.com/x").unwrap()).unwrap();
        assert!(header.to_str().unwrap().contains("session=abc"));
        assert_eq!(jar.list().len(), 2);
        // Domain cookie applies to other subdomains, host-only does not.
        let other = jar.for_url("https://www.example.com/");
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].name, "pref");

        jar.set("example.com", "added=1; Path=/").unwrap();
        assert!(jar.list().iter().any(|c| c.name == "added"));
        jar.save().unwrap();
        let again = CookieJar::open(dir.path().join("cookies.json"));
        assert_eq!(again.list().len(), 3, "session cookies persist too");
        assert!(again.remove("api.example.com", "/", "session"));
        again.clear_domain("example.com");
        assert!(again.list().is_empty());
    }

    #[test]
    fn edited_cookies_keep_their_expiry() {
        let jar = CookieJar::new();
        let url = Url::parse("https://api.example.com/").unwrap();
        let h = HeaderValue::from_static("token=abc; Path=/; Expires=Wed, 21 Oct 2037 07:28:00 GMT; Secure");
        jar.set_cookies(&mut [&h].into_iter(), &url);
        let before = jar.list().pop().unwrap();
        assert_eq!(before.expires, Some(2139722880));
        // The cookie editor shows `to_set_cookie` and stores the edited text.
        jar.set(&before.domain, &before.to_set_cookie()).unwrap();
        assert_eq!(jar.list(), vec![before]);
    }

    #[test]
    fn public_suffix_domains_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        for jar in [CookieJar::new(), CookieJar::open(dir.path().join("cookies.json"))] {
            for (host, set_cookie) in [
                ("https://example.com/", "a=1; Domain=com; Path=/"),
                ("https://evil.co.uk/", "b=1; Domain=.co.uk; Path=/"),
                ("https://me.github.io/", "c=1; Domain=github.io; Path=/"),
            ] {
                let h = HeaderValue::from_str(set_cookie).unwrap();
                jar.set_cookies(&mut [&h].into_iter(), &Url::parse(host).unwrap());
            }
            for victim in ["https://victim.com/", "https://bank.co.uk/", "https://other.github.io/"] {
                assert_eq!(jar.cookies(&Url::parse(victim).unwrap()), None, "{victim}");
            }
            assert!(jar.list().is_empty());
            // Registrable domains, local names and the suffix's own host still work.
            for (host, set_cookie) in [
                ("https://api.example.co.uk/", "d=1; Domain=example.co.uk; Path=/"),
                ("http://app.localhost/", "e=1; Domain=localhost; Path=/"),
                ("https://github.io/", "f=1; Domain=github.io; Path=/"),
            ] {
                let h = HeaderValue::from_str(set_cookie).unwrap();
                jar.set_cookies(&mut [&h].into_iter(), &Url::parse(host).unwrap());
            }
            assert!(jar.cookies(&Url::parse("https://www.example.co.uk/").unwrap()).is_some());
            assert!(jar.cookies(&Url::parse("http://localhost/").unwrap()).is_some());
            assert!(jar.cookies(&Url::parse("https://github.io/").unwrap()).is_some());
            assert!(jar.cookies(&Url::parse("https://other.github.io/").unwrap()).is_none(), "host-only");
        }
    }
}
