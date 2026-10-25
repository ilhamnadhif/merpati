//! Run reporters (FR-L2): CLI, JSON, JUnit and HTML.

use std::io::{self, Write};
use std::time::Duration;

use merpati_core::util::{human_bytes, human_ms};

use crate::runner::{RequestResult, RunEvent, RunSummary};

/// Header names whose values are credentials.
const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "apikey",
    "x-auth-token",
    "x-access-token",
    "x-amz-security-token",
    "x-csrf-token",
    "x-xsrf-token",
];

/// Query parameter names whose values are credentials.
const SECRET_PARAMS: &[&str] = &[
    "access_token",
    "id_token",
    "refresh_token",
    "token",
    "api_key",
    "apikey",
    "api-key",
    "key",
    "client_secret",
    "password",
    "secret",
    "signature",
    "sig",
    "x-amz-signature",
    "x-amz-credential",
    "x-amz-security-token",
    "code_verifier",
];

/// What masked credentials are replaced with.
pub const MASK: &str = "***";

/// Whether the value of header `name` is a credential.
pub fn is_secret_header(name: &str) -> bool {
    SECRET_HEADERS.iter().any(|h| name.trim().eq_ignore_ascii_case(h))
}

/// `value`, or [`MASK`] when header `name` carries a credential.
pub fn redact_header<'a>(name: &str, value: &'a str) -> &'a str {
    if is_secret_header(name) { MASK } else { value }
}

/// `url` with the password of its `user:password@` part and the values of
/// credential query parameters (`access_token`, `api_key`, AWS signatures,
/// ...) masked. Works on any text that looks like a URL.
pub fn redact_url(url: &str) -> String {
    let (before_fragment, fragment) = match url.find('#') {
        Some(i) => url.split_at(i),
        None => (url, ""),
    };
    let (base, query) = match before_fragment.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (before_fragment, None),
    };
    let mut out = String::with_capacity(url.len());
    match base.split_once("://") {
        Some((scheme, rest)) => {
            let end = rest.find('/').unwrap_or(rest.len());
            let (authority, path) = rest.split_at(end);
            out.push_str(scheme);
            out.push_str("://");
            match authority.rsplit_once('@') {
                Some((userinfo, host)) => {
                    match userinfo.split_once(':') {
                        Some((user, _)) => out.push_str(&format!("{user}:{MASK}")),
                        None => out.push_str(userinfo),
                    }
                    out.push('@');
                    out.push_str(host);
                }
                None => out.push_str(authority),
            }
            out.push_str(path);
        }
        None => out.push_str(base),
    }
    if let Some(q) = query {
        out.push('?');
        let pairs: Vec<String> = q
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((k, _)) if SECRET_PARAMS.iter().any(|p| k.eq_ignore_ascii_case(p)) => format!("{k}={MASK}"),
                _ => pair.to_string(),
            })
            .collect();
        out.push_str(&pairs.join("&"));
    }
    out.push_str(fragment);
    out
}

/// Masks the credentials of a result (URL, request and response headers)
/// for reports and console output.
pub fn redact_result(r: &mut RequestResult) {
    r.url = redact_url(&r.url);
    for (k, v) in r.request_headers.iter_mut().chain(r.response_headers.iter_mut()) {
        if is_secret_header(k) {
            *v = MASK.to_string();
        }
    }
}

pub trait Reporter: Send {
    fn on_event(&mut self, event: &RunEvent);
    /// Called once after the run; writes files/output.
    fn finish(&mut self, summary: &RunSummary) -> io::Result<()>;
}

/// Fans events out to several reporters.
#[derive(Default)]
pub struct Reporters(pub Vec<Box<dyn Reporter>>);

impl Reporters {
    pub fn on_event(&mut self, e: &RunEvent) {
        for r in &mut self.0 {
            r.on_event(e);
        }
    }

    pub fn finish(&mut self, s: &RunSummary) -> io::Result<()> {
        for r in &mut self.0 {
            r.finish(s)?;
        }
        Ok(())
    }
}

struct Style {
    color: bool,
}

impl Style {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
}

/// The symbols of the CLI reporter: Unicode, or plain ASCII for terminals
/// and log viewers that cannot show it (newman `--disable-unicode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Symbols {
    pub folder: &'static str,
    pub request: &'static str,
    pub pass: &'static str,
    /// First line of a console block (e.g. the request in `--verbose`).
    pub block_start: &'static str,
    /// Following lines of a console block.
    pub block_line: &'static str,
    /// The `→` of the runner's network log lines.
    pub arrow: &'static str,
    unicode: bool,
}

impl Symbols {
    pub const UNICODE: Symbols =
        Symbols {
            folder: "❏", request: "↳", pass: "✓", block_start: "┌", block_line: "│", arrow: "→", unicode: true
        };
    pub const ASCII: Symbols = Symbols {
        folder: "+",
        request: "->",
        pass: "Pass",
        block_start: "+",
        block_line: "|",
        arrow: "->",
        unicode: false,
    };

    pub fn new(unicode: bool) -> Self {
        if unicode { Symbols::UNICODE } else { Symbols::ASCII }
    }

    /// `line` with box drawing characters replaced by `+`, `-` and `|` in
    /// ASCII mode.
    pub fn boxed(&self, line: &str) -> String {
        if self.unicode {
            return line.to_string();
        }
        line.chars()
            .map(|c| match c {
                '─' => '-',
                '│' => '|',
                '┌' | '┬' | '┐' | '├' | '┼' | '┤' | '┴' | '└' | '┘' => '+',
                c => c,
            })
            .collect()
    }
}

/// Human readable progress, newman style.
pub struct CliReporter<W: Write + Send> {
    out: W,
    style: Style,
    symbols: Symbols,
    failures: Vec<(usize, String, String, String)>,
    iterations: usize,
    current_folder: Vec<String>,
}

impl<W: Write + Send> CliReporter<W> {
    pub fn new(out: W, color: bool) -> Self {
        CliReporter {
            out,
            style: Style { color },
            symbols: Symbols::UNICODE,
            failures: Vec::new(),
            iterations: 1,
            current_folder: Vec::new(),
        }
    }

    /// Prints with these symbols instead of the Unicode ones.
    pub fn with_symbols(mut self, symbols: Symbols) -> Self {
        self.symbols = symbols;
        self
    }

    fn request(&mut self, r: &RequestResult) {
        let s = &self.style;
        if r.path != self.current_folder {
            if !r.path.is_empty() {
                let _ = writeln!(self.out, "\n{}", s.bold(&format!("{} {}", self.symbols.folder, r.path.join(" / "))));
            }
            self.current_folder = r.path.clone();
        }
        let _ = writeln!(self.out, "{} {}", self.symbols.request, r.name);
        if r.skipped {
            let _ = writeln!(self.out, "  {}", s.dim("skipped"));
            return;
        }
        match (&r.error, r.status) {
            (Some(err), _) => {
                let _ = writeln!(self.out, "  {} {}  {}", r.method, s.dim(&r.url), s.red(&format!("[errored] {err}")));
                self.failures.push((r.iteration, r.name.clone(), "Error".into(), err.clone()));
            }
            (None, Some(code)) => {
                let status = format!("{code} {}", r.status_text);
                let status = if code < 400 { s.green(&status) } else { s.yellow(&status) };
                let _ = writeln!(
                    self.out,
                    "  {} {} [{}, {}, {}]",
                    r.method,
                    s.dim(&r.url),
                    status,
                    human_bytes(r.response_size),
                    human_ms(r.response_time_ms)
                );
            }
            _ => {}
        }
        for t in &r.tests {
            if t.skipped {
                let _ = writeln!(self.out, "  {} {}", s.dim("-"), s.dim(&t.name));
            } else if t.passed {
                let _ = writeln!(self.out, "  {} {}", s.green(self.symbols.pass), t.name);
            } else {
                let n = self.failures.len() + 1;
                let _ = writeln!(self.out, "  {} {}", s.red(&format!("{n}.")), s.red(&t.name));
                self.failures.push((
                    r.iteration,
                    r.name.clone(),
                    t.name.clone(),
                    t.error.clone().unwrap_or_else(|| "assertion failed".into()),
                ));
            }
        }
        for e in &r.script_errors {
            let n = self.failures.len() + 1;
            let _ = writeln!(
                self.out,
                "  {} {}",
                s.red(&format!("{n}.")),
                s.red(&format!("{}: {}", e.location, e.message))
            );
            self.failures.push((r.iteration, r.name.clone(), format!("{:?} script error", e.event), e.message.clone()));
        }
    }
}

impl<W: Write + Send> Reporter for CliReporter<W> {
    fn on_event(&mut self, event: &RunEvent) {
        match event {
            RunEvent::Started { iterations, .. } => self.iterations = *iterations,
            RunEvent::IterationStarted { iteration } if self.iterations > 1 => {
                let _ = writeln!(
                    self.out,
                    "\n{}",
                    self.style.dim(&format!("Iteration {}/{}", iteration + 1, self.iterations))
                );
                self.current_folder.clear();
            }
            RunEvent::RequestFinished(r) => self.request(r),
            _ => {}
        }
    }

    fn finish(&mut self, s: &RunSummary) -> io::Result<()> {
        let st = &self.style;
        let rows = [
            ("iterations", s.iterations, 0),
            ("requests", s.executed, s.requests_failed),
            ("test-scripts / assertions", s.tests_passed + s.tests_failed, s.tests_failed),
            ("script errors", s.script_errors, s.script_errors),
        ];
        let symbols = self.symbols;
        let b = |line: &str| symbols.boxed(line);
        writeln!(self.out)?;
        writeln!(self.out, "{}", b("┌───────────────────────────┬──────────┬──────────┐"))?;
        writeln!(self.out, "{}", b("│                           │ executed │   failed │"))?;
        writeln!(self.out, "{}", b("├───────────────────────────┼──────────┼──────────┤"))?;
        let v = b("│");
        for (label, exec, failed) in rows {
            let f = format!("{failed:>8}");
            let f = if failed > 0 { st.red(&f) } else { f };
            writeln!(self.out, "{v} {label:>25} {v} {exec:>8} {v} {f} {v}")?;
        }
        writeln!(self.out, "{}", b("├───────────────────────────┴──────────┴──────────┤"))?;
        let line = format!(
            "total run duration: {}  received: {}  avg response: {} [min {}, max {}]",
            human_ms(s.total_time_ms),
            human_bytes(s.total_bytes),
            human_ms(s.response_time_avg_ms()),
            human_ms(s.response_time_min_ms),
            human_ms(s.response_time_max_ms)
        );
        writeln!(self.out, "{v} {line}")?;
        writeln!(self.out, "{}", b("└──────────────────────────────────────────────────┘"))?;
        if !self.failures.is_empty() {
            writeln!(self.out, "\n  #  failure                         detail")?;
            for (i, (iter, req, what, detail)) in self.failures.iter().enumerate() {
                writeln!(
                    self.out,
                    "\n {:>2}. {} {}",
                    i + 1,
                    st.red(what),
                    st.dim(&format!("(iteration {}, \"{req}\")", iter + 1))
                )?;
                writeln!(self.out, "     {detail}")?;
            }
        }
        if s.cancelled {
            writeln!(self.out, "\n{}", st.yellow("Run cancelled"))?;
        }
        if s.bailed {
            writeln!(self.out, "\n{}", st.yellow("Run stopped at the first failure (--bail)"))?;
        }
        self.out.flush()
    }
}

/// Machine readable JSON report (summary + every request result).
pub struct JsonReporter {
    path: std::path::PathBuf,
    results: Vec<RequestResult>,
}

impl JsonReporter {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        JsonReporter { path: path.into(), results: Vec::new() }
    }
}

impl Reporter for JsonReporter {
    fn on_event(&mut self, event: &RunEvent) {
        if let RunEvent::RequestFinished(r) = event {
            self.results.push((**r).clone());
        }
    }

    fn finish(&mut self, s: &RunSummary) -> io::Result<()> {
        let v = serde_json::json!({ "summary": s, "results": self.results });
        let text = serde_json::to_string_pretty(&v).map_err(io::Error::other)?;
        write_file(&self.path, text.as_bytes())
    }
}

fn write_file(path: &std::path::Path, data: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, data)
}

/// JUnit XML in newman's layout: one test suite per executed request, one test
/// case per assertion.
pub struct JunitReporter {
    path: std::path::PathBuf,
    collection: String,
    report: quick_junit::Report,
}

impl JunitReporter {
    pub fn new(path: impl Into<std::path::PathBuf>, collection: &str) -> Self {
        JunitReporter {
            path: path.into(),
            collection: collection.to_string(),
            report: quick_junit::Report::new(collection.to_string()),
        }
    }

    pub fn render(&self) -> String {
        self.report.to_string().unwrap_or_default()
    }
}

impl Reporter for JunitReporter {
    fn on_event(&mut self, event: &RunEvent) {
        use quick_junit::{NonSuccessKind, TestCase, TestCaseStatus, TestSuite};
        let RunEvent::RequestFinished(r) = event else { return };
        let mut name = r.path.clone();
        name.push(r.name.clone());
        let mut suite = TestSuite::new(name.join(" / "));
        suite.set_time(Duration::from_secs_f64(r.response_time_ms / 1000.0));
        suite.add_property(quick_junit::Property::new("iteration", (r.iteration + 1).to_string()));
        let classname = format!("{}.{}", self.collection, r.name);
        if let Some(err) = &r.error {
            let mut status = TestCaseStatus::non_success(NonSuccessKind::Error);
            status.set_message(err.clone()).set_type("RequestError");
            let mut tc = TestCase::new(format!("{} {}", r.method, r.url), status);
            tc.set_classname(classname.clone());
            suite.add_test_case(tc);
        }
        for e in &r.script_errors {
            let mut status = TestCaseStatus::non_success(NonSuccessKind::Error);
            status.set_message(e.message.clone()).set_type("ScriptError");
            let mut tc = TestCase::new(e.location.clone(), status);
            tc.set_classname(classname.clone());
            suite.add_test_case(tc);
        }
        for t in &r.tests {
            let status = if t.skipped {
                TestCaseStatus::skipped()
            } else if t.passed {
                TestCaseStatus::success()
            } else {
                let mut s = TestCaseStatus::non_success(NonSuccessKind::Failure);
                s.set_message(t.error.clone().unwrap_or_default()).set_type("AssertionFailure");
                s
            };
            let mut tc = TestCase::new(t.name.clone(), status);
            tc.set_classname(classname.clone());
            suite.add_test_case(tc);
        }
        self.report.add_test_suite(suite);
    }

    fn finish(&mut self, s: &RunSummary) -> io::Result<()> {
        self.report.set_time(Duration::from_secs_f64(s.total_time_ms / 1000.0));
        let xml = self.report.to_string().map_err(io::Error::other)?;
        write_file(&self.path, xml.as_bytes())
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A single self-contained HTML file.
pub struct HtmlReporter {
    path: std::path::PathBuf,
    rows: Vec<RequestResult>,
}

impl HtmlReporter {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        HtmlReporter { path: path.into(), rows: Vec::new() }
    }

    pub fn render(&self, s: &RunSummary) -> String {
        let mut body = String::new();
        for r in &self.rows {
            let status = match (&r.error, r.status) {
                (Some(e), _) => format!("<span class=bad>{}</span>", esc(e)),
                (None, Some(c)) => format!("{c} {}", esc(&r.status_text)),
                _ => "skipped".into(),
            };
            let tests: String = r
                .tests
                .iter()
                .map(|t| {
                    let (cls, mark) = if t.skipped {
                        ("skip", "–")
                    } else if t.passed {
                        ("ok", "✓")
                    } else {
                        ("bad", "✗")
                    };
                    let err = t.error.as_deref().map(|e| format!(" <small>{}</small>", esc(e))).unwrap_or_default();
                    format!("<li class={cls}>{mark} {}{err}</li>", esc(&t.name))
                })
                .collect();
            let errors: String = r
                .script_errors
                .iter()
                .map(|e| format!("<li class=bad>✗ {}: {}</li>", esc(&e.location), esc(&e.message)))
                .collect();
            let mut name = r.path.clone();
            name.push(r.name.clone());
            body.push_str(&format!(
                "<tr class={}><td>{}</td><td>{}</td><td><code>{} {}</code></td><td>{}</td><td>{}</td><td>{}</td><td><ul>{}{}</ul></td></tr>\n",
                if r.failed() { "failed" } else { "passed" },
                r.iteration + 1,
                esc(&name.join(" / ")),
                esc(&r.method),
                esc(&r.url),
                status,
                human_ms(r.response_time_ms),
                human_bytes(r.response_size),
                tests,
                errors
            ));
        }
        format!(
            r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title} – Merpati run report</title>
<style>
:root{{color-scheme:light dark;--bg:#fff;--fg:#1b1f24;--muted:#6a737d;--ok:#1a7f37;--bad:#cf222e;--line:#d0d7de}}
@media (prefers-color-scheme:dark){{:root{{--bg:#0d1117;--fg:#e6edf3;--muted:#8b949e;--ok:#3fb950;--bad:#f85149;--line:#30363d}}}}
body{{font:14px/1.45 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;background:var(--bg);color:var(--fg);margin:24px}}
h1{{font-size:20px;margin:0 0 4px}}.muted{{color:var(--muted)}}
.cards{{display:flex;gap:12px;flex-wrap:wrap;margin:16px 0}}.card{{border:1px solid var(--line);border-radius:8px;padding:10px 14px;min-width:120px}}
.card b{{display:block;font-size:20px}}table{{border-collapse:collapse;width:100%}}td,th{{border-bottom:1px solid var(--line);padding:6px 8px;text-align:left;vertical-align:top}}
code{{font-size:12px;word-break:break-all}}ul{{margin:0;padding-left:16px}}li{{list-style:none;margin-left:-16px}}.ok{{color:var(--ok)}}.bad{{color:var(--bad)}}.skip{{color:var(--muted)}}
tr.failed td:first-child{{border-left:3px solid var(--bad)}}tr.passed td:first-child{{border-left:3px solid var(--ok)}}
</style></head><body>
<h1>{title}</h1><div class=muted>Started {started} · finished {finished} · {duration}</div>
<div class=cards>
<div class=card><span class=muted>Iterations</span><b>{iterations}</b></div>
<div class=card><span class=muted>Requests</span><b>{executed}</b></div>
<div class=card><span class=muted>Failed requests</span><b class="{req_cls}">{req_failed}</b></div>
<div class=card><span class=muted>Assertions passed</span><b class=ok>{passed}</b></div>
<div class=card><span class=muted>Assertions failed</span><b class="{test_cls}">{failed}</b></div>
<div class=card><span class=muted>Avg response</span><b>{avg}</b></div>
</div>
<table><thead><tr><th>#</th><th>Request</th><th>URL</th><th>Status</th><th>Time</th><th>Size</th><th>Tests</th></tr></thead><tbody>
{body}</tbody></table></body></html>"#,
            title = esc(&s.collection),
            started = esc(&s.started_at),
            finished = esc(&s.finished_at),
            duration = human_ms(s.total_time_ms),
            iterations = s.iterations,
            executed = s.executed,
            req_failed = s.requests_failed,
            req_cls = if s.requests_failed > 0 { "bad" } else { "" },
            passed = s.tests_passed,
            failed = s.tests_failed,
            test_cls = if s.tests_failed > 0 { "bad" } else { "" },
            avg = human_ms(s.response_time_avg_ms()),
            body = body
        )
    }
}

impl Reporter for HtmlReporter {
    fn on_event(&mut self, event: &RunEvent) {
        if let RunEvent::RequestFinished(r) = event {
            let mut r = (**r).clone();
            r.body_preview = None;
            r.console.clear();
            self.rows.push(r);
        }
    }

    fn finish(&mut self, s: &RunSummary) -> io::Result<()> {
        let html = self.render(s);
        write_file(&self.path, html.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_reporter_ascii_symbols() {
        let result = RequestResult {
            name: "Login".into(),
            path: vec!["Auth".into()],
            method: "GET".into(),
            url: "http://h/".into(),
            status: Some(200),
            status_text: "OK".into(),
            tests: vec![merpati_core::script::TestResult {
                name: "ok".into(),
                passed: true,
                error: None,
                skipped: false,
            }],
            ..Default::default()
        };
        let summary = RunSummary { executed: 1, tests_passed: 1, ..Default::default() };
        let render = |symbols: Symbols| {
            let mut out = Vec::new();
            let mut r = CliReporter::new(&mut out, false).with_symbols(symbols);
            r.on_event(&RunEvent::RequestFinished(Box::new(result.clone())));
            r.finish(&summary).unwrap();
            drop(r);
            String::from_utf8(out).unwrap()
        };
        let unicode = render(Symbols::UNICODE);
        assert!(unicode.contains("❏ Auth") && unicode.contains("↳ Login") && unicode.contains("✓ ok"), "{unicode}");
        let ascii = render(Symbols::new(false));
        assert!(ascii.is_ascii(), "{ascii}");
        assert!(ascii.contains("+ Auth") && ascii.contains("-> Login") && ascii.contains("Pass ok"), "{ascii}");
        assert!(ascii.contains("+---------------------------+----------+----------+"), "{ascii}");
        assert!(ascii.contains("|                  requests |        1 |        0 |"), "{ascii}");
        // Same layout, character for character.
        assert_eq!(unicode.lines().count(), ascii.lines().count());
    }

    #[test]
    fn credentials_are_masked() {
        assert_eq!(
            redact_url("https://bob:hunter2@api.test/v1?api_key=sk1&page=2&X-Amz-Signature=abc#top"),
            "https://bob:***@api.test/v1?api_key=***&page=2&X-Amz-Signature=***#top"
        );
        assert_eq!(redact_url("http://api.test/?token"), "http://api.test/?token");
        assert_eq!(redact_url("{{base}}/x?key={{k}}"), "{{base}}/x?key=***");
        assert_eq!(redact_url("http://user@h/p"), "http://user@h/p");
        let mut r = RequestResult {
            url: "http://h/?access_token=t".into(),
            request_headers: vec![("Authorization".into(), "Bearer t".into()), ("Accept".into(), "*/*".into())],
            response_headers: vec![("Set-Cookie".into(), "sid=1".into())],
            ..Default::default()
        };
        redact_result(&mut r);
        assert_eq!(r.url, "http://h/?access_token=***");
        assert_eq!(r.request_headers[0].1, MASK);
        assert_eq!(r.request_headers[1].1, "*/*");
        assert_eq!(r.response_headers[0].1, MASK);
        assert_eq!(redact_header("x-api-key", "k"), MASK);
        assert_eq!(redact_header("Content-Type", "json"), "json");
    }
}
