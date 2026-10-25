use merpati_core::Auth;
use merpati_core::model::{Body, BodyMode, FormField, HttpRequest, KeyValue, RawLanguage};
use merpati_core::prepared::{PrepareContext, PreparedBody, PreparedRequest, prepare};
use merpati_core::settings::RequestDefaults;
use merpati_core::variables::{Resolver, VariableScopes};

use super::*;
use crate::curl;

fn prep(req: &HttpRequest) -> PreparedRequest {
    let scopes = VariableScopes::default();
    let auth = Auth::default();
    let defaults = RequestDefaults::default();
    let ctx = PrepareContext {
        resolver: Resolver::new(&scopes),
        auth: &auth,
        auth_owner_id: None,
        defaults: &defaults,
        base_dir: None,
    };
    prepare(req, &ctx)
}

fn json_post() -> HttpRequest {
    let mut r = HttpRequest::new("POST", "https://api.example.com/v1/users?verbose=true");
    r.headers.push(KeyValue::new("Authorization", "Bearer abc.def"));
    r.body = Body::raw_text(
        "{\n  \"name\": \"Ada \\\"the\\\" Countess\",\n  \"tags\": [\"a\", \"b\"]\n}",
        RawLanguage::Json,
    );
    r
}

fn urlencoded() -> HttpRequest {
    let mut r = HttpRequest::new("POST", "https://h.test/login");
    r.body.mode = BodyMode::UrlEncoded;
    r.body.urlencoded = vec![KeyValue::new("user name", "ada&co"), KeyValue::new("pass", "p@ss'word")];
    r
}

fn multipart() -> HttpRequest {
    let mut r = HttpRequest::new("POST", "https://h.test/upload");
    r.headers.push(KeyValue::new("X-Note", "it's \"quoted\""));
    r.body.mode = BodyMode::FormData;
    let mut file = FormField::file("avatar", "/tmp/my photo.png");
    file.content_type = Some("image/png".into());
    r.body.form_data = vec![FormField::text("title", "hello; world"), file, FormField::file("doc", "/tmp/a.pdf")];
    r
}

fn file_body() -> HttpRequest {
    let mut r = HttpRequest::new("PUT", "https://h.test/blob");
    r.headers.push(KeyValue::new("Content-Type", "application/octet-stream"));
    r.body.mode = BodyMode::Binary;
    r.body.binary_path = "/tmp/data.bin".into();
    r
}

fn all_fixtures() -> Vec<(&'static str, HttpRequest)> {
    let mut get = HttpRequest::new("GET", "https://h.test/search?q=rust lang&page=2");
    get.headers.push(KeyValue::new("X-Empty", ""));
    let head = HttpRequest::new("HEAD", "https://h.test/");
    let custom = HttpRequest::new("PURGE", "https://h.test/cache/$key");
    let mut get_body = HttpRequest::new("GET", "https://h.test/q");
    get_body.body = Body::raw_text("select $1", RawLanguage::Text);
    let mut dollar = HttpRequest::new("POST", "https://h.test/t");
    dollar.body = Body::raw_text("price: $5 \\ back`tick` #{x} ${y}\ttab", RawLanguage::Text);
    vec![
        ("json", json_post()),
        ("urlencoded", urlencoded()),
        ("multipart", multipart()),
        ("file", file_body()),
        ("get", get),
        ("head", head),
        ("custom", custom),
        ("get_body", get_body),
        ("dollar", dollar),
    ]
}

#[test]
fn curl_roundtrip() {
    for (name, req) in all_fixtures() {
        let p1 = prep(&req);
        let code = generate(Language::Curl, &p1);
        let parsed = curl::parse(&code).unwrap_or_else(|e| panic!("{name}: {e}\n{code}"));
        let p2 = prep(&parsed);
        assert_eq!(p1.method, p2.method, "{name}: method\n{code}");
        assert_eq!(p1.url, p2.url, "{name}: url\n{code}");
        let mut h1 = p1.headers.clone();
        let mut h2 = p2.headers.clone();
        h1.sort();
        h2.sort();
        assert_eq!(h1, h2, "{name}: headers\n{code}");
        assert_eq!(p1.body, p2.body, "{name}: body\n{code}");
    }
}

#[test]
fn every_language_handles_every_body() {
    for (name, req) in all_fixtures() {
        let p = prep(&req);
        for lang in Language::ALL {
            let code = generate(*lang, &p);
            assert!(!code.trim().is_empty(), "{name}/{lang:?} empty");
            // Deterministic output.
            assert_eq!(code, generate(*lang, &p), "{name}/{lang:?} not deterministic");
            if !matches!(p.body, PreparedBody::Multipart(_)) || matches!(lang, Language::Http | Language::Wget) {
                continue;
            }
            assert!(code.contains("avatar"), "{name}/{lang:?} misses multipart field\n{code}");
        }
    }
}

#[test]
fn language_ids_roundtrip() {
    for lang in Language::ALL {
        assert_eq!(Language::from_id(lang.id()), Some(*lang));
        assert!(!lang.label().is_empty());
        assert!(!lang.syntax().is_empty());
    }
}

#[test]
fn skip_default_headers() {
    let p = prep(&HttpRequest::new("GET", "https://h.test/"));
    let all = generate(Language::Curl, &p);
    assert!(all.contains("User-Agent: MerpatiRuntime"));
    let trimmed = generate_with(Language::Curl, &p, Options { skip_default_headers: true });
    assert!(!trimmed.contains("User-Agent"));
    assert!(!trimmed.contains("Accept: */*"));
    assert_eq!(trimmed, "curl --location 'https://h.test/'\n");
}

fn compact(req: &HttpRequest) -> PreparedRequest {
    let mut p = prep(req);
    p.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("user-agent") && !k.eq_ignore_ascii_case("accept"));
    p
}

#[test]
fn golden_curl() {
    let code = generate(Language::Curl, &compact(&json_post()));
    let expected = r#"curl --location --request POST 'https://api.example.com/v1/users?verbose=true' \
--header 'Authorization: Bearer abc.def' \
--header 'Content-Type: application/json' \
--data-raw '{
  "name": "Ada \"the\" Countess",
  "tags": ["a", "b"]
}'
"#;
    assert_eq!(code, expected);
}

#[test]
fn golden_python() {
    let code = generate(Language::Python, &compact(&json_post()));
    let expected = r#"import requests

url = "https://api.example.com/v1/users?verbose=true"

payload = "{\n  \"name\": \"Ada \\\"the\\\" Countess\",\n  \"tags\": [\"a\", \"b\"]\n}"
headers = {
  "Authorization": "Bearer abc.def",
  "Content-Type": "application/json"
}

response = requests.request("POST", url, headers=headers, data=payload)

print(response.text)
"#;
    assert_eq!(code, expected);
}

#[test]
fn golden_js_fetch() {
    let code = generate(Language::JavaScriptFetch, &compact(&json_post()));
    let expected = r#"const myHeaders = new Headers();
myHeaders.append("Authorization", "Bearer abc.def");
myHeaders.append("Content-Type", "application/json");

const raw = JSON.stringify({
  "name": "Ada \"the\" Countess",
  "tags": [
    "a",
    "b"
  ]
});

const requestOptions = {
  method: "POST",
  headers: myHeaders,
  body: raw,
  redirect: "follow"
};

fetch("https://api.example.com/v1/users?verbose=true", requestOptions)
  .then((response) => response.text())
  .then((result) => console.log(result))
  .catch((error) => console.error(error));
"#;
    assert_eq!(code, expected);
}

#[test]
fn golden_http() {
    let code = generate(Language::Http, &compact(&urlencoded()));
    let expected = "POST /login HTTP/1.1\nHost: h.test\nContent-Type: application/x-www-form-urlencoded\nContent-Length: 34\n\nuser+name=ada%26co&pass=p%40ss%27word\n";
    // Content-Length must match the body.
    let body = "user+name=ada%26co&pass=p%40ss%27word";
    assert_eq!(code, expected.replace("34", &body.len().to_string()));
}

#[test]
fn golden_go_multipart() {
    let code = generate(Language::Go, &compact(&multipart()));
    assert!(code.contains("\"mime/multipart\""));
    assert!(code.contains("\"net/textproto\""));
    assert!(code.contains("_ = writer.WriteField(\"title\", \"hello; world\")"));
    assert!(code.contains("h1.Set(\"Content-Type\", \"image/png\")"));
    assert!(code.contains("writer.CreateFormFile(\"doc\", \"a.pdf\")"));
    assert!(code.contains("req.Header.Set(\"Content-Type\", writer.FormDataContentType())"));
    // Imports sorted.
    let imports: Vec<&str> = code.lines().skip_while(|l| *l != "import (").skip(1).take_while(|l| *l != ")").collect();
    let mut sorted = imports.clone();
    sorted.sort();
    assert_eq!(imports, sorted);
}

#[test]
fn escaping_per_language() {
    let mut r = HttpRequest::new("POST", "https://h.test/t");
    r.body = Body::raw_text("a\"b\\c\n$d'e#{f}", RawLanguage::Text);
    let p = compact(&r);
    let kotlin = generate(Language::Kotlin, &p);
    assert!(kotlin.contains(r#""a\"b\\c\n\$d'e#{f}""#), "{kotlin}");
    let dart = generate(Language::Dart, &p);
    assert!(dart.contains(r#""a\"b\\c\n\$d'e#{f}""#), "{dart}");
    let php = generate(Language::Php, &p);
    assert!(php.contains("'a\"b\\\\c\n$d\\'e#{f}'"), "{php}");
    let ruby = generate(Language::Ruby, &p);
    assert!(ruby.contains("'a\"b\\\\c\n$d\\'e#{f}'"), "{ruby}");
    let pwsh = generate(Language::PowerShell, &p);
    assert!(pwsh.contains("'a\"b\\c\n$d''e#{f}'"), "{pwsh}");
    let rust = generate(Language::Rust, &p);
    assert!(rust.contains("r#\"a\"b\\c\n$d'e#{f}\"#"), "{rust}");
    let shell = generate(Language::Curl, &p);
    assert!(shell.contains("'a\"b\\c\n$d'\\''e#{f}'"), "{shell}");
    let swift = generate(Language::Swift, &p);
    assert!(swift.contains(r#""a\"b\\c\n$d'e#{f}""#), "{swift}");
}

#[test]
fn settings_are_reflected() {
    let mut r = HttpRequest::new("GET", "https://h.test/");
    r.settings.verify_ssl = Some(false);
    r.settings.follow_redirects = Some(false);
    r.settings.timeout_ms = Some(2500);
    let p = compact(&r);
    let c = generate(Language::Curl, &p);
    assert!(c.contains("--insecure") && c.contains("--max-time 2.500") && !c.contains("--location"));
    let py = generate(Language::Python, &p);
    assert!(py.contains("verify=False") && py.contains("allow_redirects=False") && py.contains("timeout=2.500"));
    let rs = generate(Language::Rust, &p);
    assert!(rs.contains("danger_accept_invalid_certs(true)") && rs.contains("Policy::none()"));
    let go = generate(Language::Go, &p);
    assert!(go.contains("InsecureSkipVerify: true") && go.contains("2500 * time.Millisecond"));
    let cs = generate(Language::CSharp, &p);
    assert!(cs.contains("AllowAutoRedirect = false"));
}

#[test]
fn bytes_body_without_utf8() {
    let p = PreparedRequest {
        method: "POST".into(),
        url: "https://h.test/".into(),
        body: PreparedBody::Bytes(bytes::Bytes::from_static(&[0xff, 0x00, b'a'])),
        ..Default::default()
    };
    for lang in Language::ALL {
        let _ = generate(*lang, &p);
    }
}

/// Writes every snippet to `$MERPATI_SNIPPET_DIR` so they can be checked with
/// real toolchains: `MERPATI_SNIPPET_DIR=/tmp/s cargo test -p merpati-convert dump_snippets -- --ignored`.
#[test]
#[ignore]
fn dump_snippets() {
    let dir = std::path::PathBuf::from(std::env::var("MERPATI_SNIPPET_DIR").expect("MERPATI_SNIPPET_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, req) in all_fixtures() {
        let mut req = req;
        req.settings.timeout_ms = Some(1500);
        let p = prep(&req);
        for lang in Language::ALL {
            let mut r2 = req.clone();
            if name == "get" {
                r2.settings.verify_ssl = Some(false);
                r2.settings.follow_redirects = Some(false);
            }
            let p2 = if name == "get" { prep(&r2) } else { p.clone() };
            std::fs::write(dir.join(format!("{name}.{}", lang.id())), generate(*lang, &p2)).unwrap();
        }
    }
}

#[test]
fn js_snippets_keep_big_integers() {
    // JS numbers are doubles: `JSON.stringify(12345678901234567890)` sends 12345678901234567000.
    let mut r = HttpRequest::new("POST", "https://h.test/");
    r.body = Body::raw_text("{\"id\": 12345678901234567890, \"neg\": -9007199254740993}", RawLanguage::Json);
    let p = compact(&r);
    for lang in [Language::JavaScriptFetch, Language::NodeAxios] {
        let code = generate(lang, &p);
        assert!(!code.contains("JSON.stringify({"), "{lang:?}\n{code}");
        assert!(code.contains(r#""{\"id\": 12345678901234567890, \"neg\": -9007199254740993}""#), "{lang:?}\n{code}");
    }
    // Numbers JS represents exactly keep the readable JSON.stringify form.
    r.body = Body::raw_text("{\"n\": 9007199254740991, \"f\": 1.5, \"neg\": -3}", RawLanguage::Json);
    let code = generate(Language::JavaScriptFetch, &compact(&r));
    assert!(code.contains("JSON.stringify("), "{code}");
}

#[test]
fn powershell_doubles_typographic_single_quotes() {
    // PowerShell also ends a verbatim string at U+2018..U+201B, not only at `'`.
    let mut r = HttpRequest::new("POST", "https://h.test/");
    r.headers.push(KeyValue::new("X-Note", "it‘s"));
    r.body = Body::raw_text("Don’t ‚x‛ 'y'", RawLanguage::Text);
    let code = generate(Language::PowerShell, &compact(&r));
    assert!(code.contains("$body = 'Don’’t ‚‚x‛‛ ''y'''"), "{code}");
    assert!(code.contains("$headers.Add('X-Note', 'it‘‘s')"), "{code}");
}

#[test]
fn csharp_escapes_unicode_line_separators() {
    // C# regular string literals cannot contain U+0085, U+2028 or U+2029.
    let mut r = HttpRequest::new("POST", "https://h.test/");
    r.body = Body::raw_text("a\u{85}b\u{2028}c\u{2029}d", RawLanguage::Text);
    let code = generate(Language::CSharp, &compact(&r));
    assert!(code.contains(r#"new StringContent("a\u0085b\u2028c\u2029d")"#), "{code}");
    let swift = generate(Language::Swift, &compact(&r));
    assert!(swift.contains(r#""a\u{85}b\u{2028}c\u{2029}d""#), "{swift}");
}

#[test]
fn shell_snippets_quote_unusual_methods() {
    // The method comes from imported collections; it must not run commands when pasted.
    let r = HttpRequest::new("PURGE;touch /tmp/pwned $(id)", "https://h.test/");
    let p = compact(&r);
    let method = "'PURGE;TOUCH /TMP/PWNED $(ID)'";
    assert!(generate(Language::Curl, &p).contains(&format!("--request {method} ")));
    assert!(generate(Language::Wget, &p).contains(&format!("--method {method} ")));
    assert!(generate(Language::Httpie, &p).contains(&format!(" {method} 'https://h.test/'")));
    // Ordinary methods stay bare.
    let p = compact(&HttpRequest::new("PURGE", "https://h.test/"));
    assert!(generate(Language::Curl, &p).contains("--request PURGE "));
    assert!(generate(Language::Wget, &p).contains("--method PURGE "));
    assert!(generate(Language::Httpie, &p).contains(" PURGE 'https://h.test/'"));
}

#[test]
fn curl_form_names_and_types_survive_curl_parsing() {
    let mut r = HttpRequest::new("POST", "https://h.test/upload");
    r.body.mode = BodyMode::FormData;
    let mut a = FormField::text("we;ird \"name\"", "v;a\"l\\ue");
    a.content_type = Some("text/plain; charset=utf-8".into());
    // A parameter named like a -F keyword would end `type=` early.
    let mut b = FormField::text("b", "x");
    b.content_type = Some("application/x-custom; filename=evil".into());
    // A comma ends a file part's type (it separates several files).
    let mut c = FormField::file("f", "/tmp/a,b \"c\".bin");
    c.content_type = Some("application/vnd.a+json; v=1, q=2".into());
    let mut d = FormField::text("d", "y");
    d.content_type = Some("nonsense".into());
    r.body.form_data = vec![a, b, c, d];
    let p1 = prep(&r);
    let code = generate(Language::Curl, &p1);
    assert!(code.contains(r#"--form 'we;ird "name"="v;a\"l\\ue";type=text/plain; charset=utf-8'"#), "{code}");
    assert!(code.contains(r#"--form 'b="x";headers="Content-Type: application/x-custom; filename=evil"'"#), "{code}");
    assert!(
        code.contains(r#"--form 'f=@"/tmp/a,b \"c\".bin";headers="Content-Type: application/vnd.a+json; v=1, q=2"'"#)
    );
    assert!(code.contains(r#"--form 'd="y";headers="Content-Type: nonsense"'"#), "{code}");
    let p2 = prep(&curl::parse(&code).unwrap());
    assert_eq!(p1.body, p2.body, "{code}");

    // `-F` cannot name a field `a=b`: the body is sent as text.
    r.body.form_data = vec![FormField::text("a=b", "1"), FormField::text("q\"", "2")];
    let code = generate(Language::Curl, &prep(&r));
    assert!(!code.contains("--form"), "{code}");
    let parsed = curl::parse(&code).unwrap();
    assert!(parsed.header("content-type").unwrap().starts_with("multipart/form-data; boundary="), "{code}");
    assert!(parsed.body.raw.contains("name=\"a=b\"\r\n\r\n1\r\n"), "{code}");
    assert!(parsed.body.raw.contains("name=\"q%22\"\r\n"), "{code}");
}
