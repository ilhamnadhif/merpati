//! Cookie jar files of `merpati run`: `--cookie-jar` reads newman's format
//! (tough-cookie's serialized `CookieJar`, what `newman run
//! --export-cookie-jar` writes) or a Merpati jar (the app's `cookies.json`);
//! `--export-cookie-jar` writes the tough-cookie format so newman can read
//! the file back.

use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value, json};

use merpati_http::{CookieInfo, CookieJar};

/// What `serializeSync()` of the tough-cookie newman uses writes; readers
/// only look at `cookies`.
const TOUGH_COOKIE_VERSION: &str = "tough-cookie@4.1.3";

/// Loads `--cookie-jar`. The jar is only read: nothing writes it back
/// (newman semantics; `--export-cookie-jar` writes a file). Returns
/// warnings about cookies that were left out.
pub fn load(path: &Path) -> Result<(CookieJar, Vec<String>)> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("could not read cookie jar {}", path.display()))?;
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("cookie jar {} is not valid JSON", path.display()))?;
    match &v {
        Value::Object(o) if o.get("cookies").is_some_and(Value::is_array) => Ok(from_tough_cookie(&v["cookies"])),
        // A Merpati jar is a list of stored cookies; the jar loaded from it
        // remembers the path but is never saved here.
        Value::Array(list) if list.iter().all(|c| c.get("raw_cookie").is_some()) => {
            Ok((CookieJar::open(path), Vec::new()))
        }
        _ => bail!(
            "{} is not a cookie jar (expected newman's tough-cookie JSON or a Merpati cookie jar)",
            path.display()
        ),
    }
}

fn from_tough_cookie(cookies: &Value) -> (CookieJar, Vec<String>) {
    let jar = CookieJar::new();
    let mut warnings = Vec::new();
    let now = Utc::now().timestamp();
    for c in cookies.as_array().into_iter().flatten() {
        let Some(info) = tough_cookie_info(c, now) else {
            warnings.push(format!("cookie jar: skipped an entry without key or domain: {c}"));
            continue;
        };
        // Expired cookies are not loaded (the store would refuse them).
        if info.expires.is_some_and(|e| e <= now) {
            continue;
        }
        let host = if info.domain.contains(':') { format!("[{}]", info.domain) } else { info.domain.clone() };
        if let Err(e) = jar.set(&host, &info.to_set_cookie()) {
            warnings.push(format!("cookie jar: skipped cookie \"{}\" for {}: {e}", info.name, info.domain));
        }
    }
    (jar, warnings)
}

/// One cookie of tough-cookie's `Cookie.toJSON()`. Its `maxAge` counts from
/// `creation` and wins over `expires`, as in tough-cookie's `expiryTime()`.
fn tough_cookie_info(c: &Value, now: i64) -> Option<CookieInfo> {
    let name = c.get("key")?.as_str().filter(|k| !k.is_empty())?;
    let domain = c.get("domain")?.as_str()?.trim().trim_start_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return None;
    }
    let date = |key: &str| {
        c.get(key).and_then(Value::as_str).and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.timestamp())
    };
    // `"Infinity"` (or no `expires`) is a session cookie.
    let mut expires = date("expires");
    match c.get("maxAge") {
        Some(Value::Number(n)) => {
            let age = n.as_f64().unwrap_or(0.0);
            let base = date("creation").unwrap_or(now);
            expires = Some(if age <= 0.0 { i64::MIN } else { base.saturating_add(age as i64) });
        }
        Some(Value::String(s)) if s == "-Infinity" => expires = Some(i64::MIN),
        _ => {}
    }
    let same_site = c.get("sameSite").and_then(Value::as_str).and_then(|s| match s.to_ascii_lowercase().as_str() {
        "strict" => Some("Strict".to_string()),
        "lax" => Some("Lax".to_string()),
        "none" => Some("None".to_string()),
        _ => None,
    });
    Some(CookieInfo {
        domain,
        host_only: c.get("hostOnly").and_then(Value::as_bool).unwrap_or(false),
        path: c.get("path").and_then(Value::as_str).filter(|p| p.starts_with('/')).unwrap_or("/").to_string(),
        name: name.to_string(),
        value: c.get("value").and_then(Value::as_str).unwrap_or_default().to_string(),
        expires,
        secure: c.get("secure").and_then(Value::as_bool).unwrap_or(false),
        http_only: c.get("httpOnly").and_then(Value::as_bool).unwrap_or(false),
        same_site,
    })
}

/// The jar as tough-cookie's serialized `CookieJar` (newman
/// `--export-cookie-jar`), session cookies included.
pub fn to_tough_cookie(jar: &CookieJar) -> Value {
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let cookies: Vec<Value> = jar
        .list()
        .into_iter()
        .map(|c| {
            // Properties at their tough-cookie default are left out, as
            // `Cookie.toJSON()` does.
            let mut o = Map::new();
            o.insert("key".into(), json!(c.name));
            o.insert("value".into(), json!(c.value));
            if let Some(t) = c.expires.and_then(|e| DateTime::<Utc>::from_timestamp(e, 0)) {
                o.insert("expires".into(), json!(t.to_rfc3339_opts(SecondsFormat::Millis, true)));
            }
            o.insert("domain".into(), json!(c.domain));
            o.insert("path".into(), json!(c.path));
            if c.secure {
                o.insert("secure".into(), json!(true));
            }
            if c.http_only {
                o.insert("httpOnly".into(), json!(true));
            }
            o.insert("hostOnly".into(), json!(c.host_only));
            o.insert("creation".into(), json!(now));
            o.insert("lastAccessed".into(), json!(now));
            if let Some(s) = &c.same_site {
                o.insert("sameSite".into(), json!(s.to_ascii_lowercase()));
            }
            Value::Object(o)
        })
        .collect();
    json!({
        "version": TOUGH_COOKIE_VERSION,
        "storeType": "MemoryCookieStore",
        "rejectPublicSuffixes": true,
        "enableLooseMode": false,
        "allowSpecialUseDomain": true,
        "prefixSecurity": "silent",
        "cookies": cookies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tough_cookie_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let input = json!({
            "version": "tough-cookie@4.1.3",
            "storeType": "MemoryCookieStore",
            "rejectPublicSuffixes": true,
            "cookies": [
                {"key": "sid", "value": "abc", "domain": "api.example.com", "path": "/", "hostOnly": true,
                 "httpOnly": true, "creation": "2026-01-01T00:00:00.000Z"},
                {"key": "pref", "value": "dark", "expires": "2099-01-01T00:00:00.000Z", "domain": "example.com",
                 "path": "/app", "secure": true, "hostOnly": false, "sameSite": "lax"},
                {"key": "aged", "value": "1", "maxAge": 3600, "domain": "example.com", "path": "/",
                 "hostOnly": false, "creation": "2099-01-01T00:00:00.000Z"},
                {"key": "old", "value": "1", "expires": "2001-01-01T00:00:00.000Z", "domain": "example.com",
                 "path": "/", "hostOnly": false},
                {"key": "gone", "value": "1", "maxAge": 0, "domain": "example.com", "path": "/"},
                {"key": "nodomain", "value": "1"}
            ]
        });
        let path = dir.path().join("jar.json");
        std::fs::write(&path, input.to_string()).unwrap();
        let (jar, warnings) = load(&path).unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let names: Vec<String> = jar.list().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["sid", "aged", "pref"]);
        let mut sent: Vec<String> = jar.for_url("https://www.example.com/app/x").into_iter().map(|c| c.name).collect();
        sent.sort();
        assert_eq!(sent, vec!["aged", "pref"]);
        assert!(jar.for_url("https://www.example.com/").iter().all(|c| c.name != "pref"), "path /app");
        assert!(jar.for_url("http://www.example.com/app").iter().all(|c| c.name != "pref"), "secure");
        assert!(jar.for_url("https://other.api.example.com/").iter().all(|c| c.name != "sid"), "host-only");

        let out = to_tough_cookie(&jar);
        assert_eq!(out["storeType"], "MemoryCookieStore");
        let pref = out["cookies"].as_array().unwrap().iter().find(|c| c["key"] == "pref").unwrap();
        assert_eq!(pref["expires"], "2099-01-01T00:00:00.000Z");
        assert_eq!(pref["domain"], "example.com");
        assert_eq!(pref["path"], "/app");
        assert_eq!(pref["secure"], true);
        assert_eq!(pref["hostOnly"], false);
        assert_eq!(pref["sameSite"], "lax");
        assert!(pref.get("httpOnly").is_none());
        let aged = out["cookies"].as_array().unwrap().iter().find(|c| c["key"] == "aged").unwrap();
        assert_eq!(aged["expires"], "2099-01-01T01:00:00.000Z", "maxAge counts from creation");
        let sid = out["cookies"].as_array().unwrap().iter().find(|c| c["key"] == "sid").unwrap();
        assert!(sid.get("expires").is_none(), "session cookie");
        assert_eq!((sid["hostOnly"].as_bool(), sid["httpOnly"].as_bool()), (Some(true), Some(true)));

        // The export reads back the same.
        std::fs::write(&path, out.to_string()).unwrap();
        let (again, warnings) = load(&path).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(again.list(), jar.list());
    }

    #[test]
    fn merpati_jar_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cookies.json");
        let jar = CookieJar::open(&path);
        jar.set("example.com", "m=1; Path=/").unwrap();
        jar.save().unwrap();
        let before = std::fs::read(&path).unwrap();
        let (loaded, _) = load(&path).unwrap();
        assert_eq!(loaded.list().len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), before);

        assert!(load(&dir.path().join("missing.json")).is_err());
        for bad in ["nope", "{\"a\": 1}", "[{\"x\": 1}]"] {
            std::fs::write(&path, bad).unwrap();
            assert!(load(&path).is_err(), "{bad}");
        }
    }
}
