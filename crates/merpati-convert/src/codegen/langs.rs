//! One function per target language. Each renders a [`View`].

use std::fmt::Write as _;

use merpati_core::model::HttpVersion;
use merpati_core::prepared::MultipartValue;

use super::{
    BOUNDARY, BodyView, Unicode, View, dq, file_name, multipart_text, path_str, ps, sh, sh_word, sq_backslash,
};

fn js(s: &str) -> String {
    dq(s, Unicode::Json, false)
}

fn pretty_json(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Shell-ish
// ---------------------------------------------------------------------------

pub fn curl(v: &View) -> String {
    let mut first = String::from("curl");
    if v.settings.follow_redirects {
        first.push_str(" --location");
    }
    if v.method == "HEAD" {
        first.push_str(" --head");
    } else if !(v.method == "GET" && !v.has_body()) {
        write!(first, " --request {}", sh_word(&v.method)).ok();
    }
    if !v.settings.verify_ssl {
        first.push_str(" --insecure");
    }
    match v.settings.http_version {
        HttpVersion::Http1 => first.push_str(" --http1.1"),
        HttpVersion::Http2 => first.push_str(" --http2"),
        HttpVersion::Auto => {}
    }
    if let Some(ms) = v.timeout_ms() {
        write!(first, " --max-time {}", format_secs(ms)).ok();
    }
    write!(first, " {}", sh(v.url)).ok();
    let mut lines = vec![first];
    for (k, val) in &v.headers {
        if val.is_empty() {
            lines.push(format!("--header {}", sh(&format!("{k};"))));
        } else {
            lines.push(format!("--header {}", sh(&format!("{k}: {val}"))));
        }
    }
    match &v.body {
        BodyView::None => {}
        BodyView::Text(t) => lines.push(format!("--data-raw {}", sh(t))),
        BodyView::File(p) => lines.push(format!("--data-binary {}", sh(&format!("@{}", path_str(p))))),
        BodyView::Multipart(parts) if parts.iter().any(|p| p.name.contains('=')) => {
            // `-F` ends the field name at the first `=`, so such a name cannot
            // be written as an option: send the multipart body as text.
            lines.push(format!("--header {}", sh(&format!("Content-Type: multipart/form-data; boundary={BOUNDARY}"))));
            lines.push(format!("--data-binary {}", sh(&multipart_text(parts))));
            if parts.iter().any(|p| matches!(p.value, MultipartValue::File(_))) {
                lines[0].insert_str(
                    0,
                    "# A field name contains `=`, which curl --form cannot express; replace the <contents of ...> \
                     placeholders.\n",
                );
            }
        }
        BodyView::Multipart(parts) => {
            for part in parts.iter() {
                let file = matches!(part.value, MultipartValue::File(_));
                let mut spec = match &part.value {
                    MultipartValue::Text(t) => format!("{}={}", part.name, curl_form_quote(t)),
                    MultipartValue::File(p) => format!("{}=@{}", part.name, curl_form_quote(&path_str(p))),
                };
                if let Some(ct) = &part.content_type {
                    if curl_form_type_ok(ct, file) {
                        write!(spec, ";type={ct}").ok();
                    } else {
                        // `type=` cannot be quoted; a part header can.
                        write!(spec, ";headers={}", curl_form_quote(&format!("Content-Type: {ct}"))).ok();
                    }
                }
                lines.push(format!("--form {}", sh(&spec)));
            }
        }
    }
    lines.join(" \\\n")
}

/// A quoted `-F` word: curl unescapes `\\` and `\"` inside double quotes.
fn curl_form_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Whether curl's `-F ...;type=` parser keeps `ct` intact: it reads
/// `major/minor` (neither empty nor holding spaces), then everything up to
/// the next `;` and further parameters until one is a `-F` keyword
/// (`filename=`, `headers=`, `encoder=`). A file part's type also ends at `,`
/// (the separator of several files).
fn curl_form_type_ok(ct: &str, file: bool) -> bool {
    let Some((major, rest)) = ct.split_once('/') else { return false };
    let minor_len = rest.find([';', ',', ' ', '\n']).unwrap_or(rest.len());
    let keyword = |p: &str| {
        ["filename=", "headers=", "encoder="]
            .iter()
            .any(|k| p.get(..k.len()).is_some_and(|x| x.eq_ignore_ascii_case(k)))
    };
    !major.is_empty()
        && major.len() <= 127
        && !major.contains(char::is_whitespace)
        && (1..=127).contains(&minor_len)
        && !ct.contains(['\r', '\n', '"'])
        && !(file && ct.contains(','))
        && rest.split(';').skip(1).all(|p| !keyword(p.trim_start()))
}

fn format_secs(ms: u128) -> String {
    if ms.is_multiple_of(1000) { (ms / 1000).to_string() } else { format!("{:.3}", ms as f64 / 1000.0) }
}

pub fn http(v: &View) -> String {
    let (target, host) = match url::Url::parse(v.url) {
        Ok(u) => {
            let mut target = u.path().to_string();
            if let Some(q) = u.query() {
                target.push('?');
                target.push_str(q);
            }
            let host = match (u.host_str(), u.port()) {
                (Some(h), Some(p)) => format!("{h}:{p}"),
                (Some(h), None) => h.to_string(),
                _ => String::new(),
            };
            (target, host)
        }
        Err(_) => (v.url.to_string(), String::new()),
    };
    let mut out = format!("{} {} HTTP/1.1\n", v.method, target);
    if !host.is_empty() && !v.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        writeln!(out, "Host: {host}").ok();
    }
    for (k, val) in &v.headers {
        writeln!(out, "{k}: {val}").ok();
    }
    match &v.body {
        BodyView::None => {}
        BodyView::Text(t) => {
            writeln!(out, "Content-Length: {}", t.len()).ok();
            out.push('\n');
            out.push_str(t);
        }
        BodyView::File(p) => {
            out.push('\n');
            write!(out, "<file contents of {}>", path_str(p)).ok();
        }
        BodyView::Multipart(parts) => {
            writeln!(out, "Content-Type: multipart/form-data; boundary={BOUNDARY}").ok();
            out.push('\n');
            out.push_str(&multipart_text(parts).replace("\r\n", "\n"));
        }
    }
    out
}

pub fn httpie(v: &View) -> String {
    let mut first = String::from("http");
    if v.settings.follow_redirects {
        first.push_str(" --follow");
    }
    if !v.settings.verify_ssl {
        first.push_str(" --verify=no");
    }
    if let Some(ms) = v.timeout_ms() {
        write!(first, " --timeout={}", format_secs(ms)).ok();
    }
    let mut lines = Vec::new();
    let mut tail = String::new();
    match &v.body {
        BodyView::Text(t) => {
            first.push_str(" --raw ");
            first.push_str(&sh(t));
        }
        BodyView::Multipart(_) => first.push_str(" --multipart"),
        BodyView::File(p) => tail = format!(" < {}", sh(&path_str(p))),
        BodyView::None => {}
    }
    write!(first, " {} {}", sh_word(&v.method), sh(v.url)).ok();
    lines.push(first);
    for (k, val) in &v.headers {
        if val.is_empty() {
            lines.push(sh(&format!("{k};")));
        } else {
            lines.push(sh(&format!("{k}:{val}")));
        }
    }
    if let BodyView::Multipart(parts) = &v.body {
        for part in parts.iter() {
            match &part.value {
                MultipartValue::Text(t) => lines.push(sh(&format!("{}={}", part.name, t))),
                MultipartValue::File(p) => {
                    let mut spec = format!("{}@{}", part.name, path_str(p));
                    if let Some(ct) = &part.content_type {
                        write!(spec, ";type={ct}").ok();
                    }
                    lines.push(sh(&spec));
                }
            }
        }
    }
    let mut out = lines.join(" \\\n  ");
    out.push_str(&tail);
    out
}

pub fn wget(v: &View) -> String {
    let mut lines = vec![String::from("wget --quiet")];
    if !v.settings.verify_ssl {
        lines[0].push_str(" --no-check-certificate");
    }
    lines.push(format!("--method {}", sh_word(&v.method)));
    lines.push(format!("--timeout={}", v.timeout_ms().map(format_secs).unwrap_or_else(|| "0".into())));
    if !v.settings.follow_redirects {
        lines.push("--max-redirect=0".into());
    }
    for (k, val) in &v.headers {
        lines.push(format!("--header {}", sh(&format!("{k}: {val}"))));
    }
    let mut note = String::new();
    match &v.body {
        BodyView::None => {}
        BodyView::Text(t) => lines.push(format!("--body-data {}", sh(t))),
        BodyView::File(p) => lines.push(format!("--body-file={}", sh(&path_str(p)))),
        BodyView::Multipart(parts) => {
            lines.push(format!("--header {}", sh(&format!("Content-Type: multipart/form-data; boundary={BOUNDARY}"))));
            lines.push(format!("--body-data {}", sh(&multipart_text(parts))));
            if parts.iter().any(|p| matches!(p.value, MultipartValue::File(_))) {
                note = "# wget cannot attach files to multipart bodies; replace the <contents of ...> placeholders.\n"
                    .into();
            }
        }
    }
    lines.push(format!("--output-document - {}", sh(v.url)));
    format!("{note}{}", lines.join(" \\\n  "))
}

pub fn powershell(v: &View) -> String {
    let mut out = String::new();
    let ct = v.content_type().map(str::to_string);
    let headers: Vec<(&str, String)> =
        v.merged_headers().into_iter().filter(|(k, _)| !k.eq_ignore_ascii_case("content-type")).collect();
    out.push_str("$headers = New-Object \"System.Collections.Generic.Dictionary[[String],[String]]\"\n");
    for (k, val) in &headers {
        writeln!(out, "$headers.Add({}, {})", ps(k), ps(val)).ok();
    }
    out.push('\n');
    let mut args = format!("Invoke-RestMethod {} -Method {} -Headers $headers", ps(v.url), ps(&v.method));
    match &v.body {
        BodyView::None => {}
        BodyView::Text(t) => {
            writeln!(out, "$body = {}\n", ps(t)).ok();
            args.push_str(" -Body $body");
        }
        BodyView::File(p) => {
            write!(args, " -InFile {}", ps(&path_str(p))).ok();
        }
        BodyView::Multipart(parts) => {
            out.push_str("$form = @{\n");
            for part in parts.iter() {
                match &part.value {
                    MultipartValue::Text(t) => writeln!(out, "    {} = {}", ps(&part.name), ps(t)).ok(),
                    MultipartValue::File(p) => {
                        writeln!(out, "    {} = Get-Item -Path {}", ps(&part.name), ps(&path_str(p))).ok()
                    }
                };
            }
            out.push_str("}\n\n");
            args.push_str(" -Form $form");
        }
    }
    if let (Some(ct), false) = (&ct, matches!(v.body, BodyView::Multipart(_))) {
        write!(args, " -ContentType {}", ps(ct)).ok();
    }
    if !v.settings.verify_ssl {
        args.push_str(" -SkipCertificateCheck");
    }
    if !v.settings.follow_redirects {
        args.push_str(" -MaximumRedirection 0");
    }
    if let Some(ms) = v.timeout_ms() {
        write!(args, " -TimeoutSec {}", ms.div_ceil(1000)).ok();
    }
    writeln!(out, "$response = {args}").ok();
    out.push_str("$response | ConvertTo-Json\n");
    out
}

// ---------------------------------------------------------------------------
// JavaScript
// ---------------------------------------------------------------------------

pub fn js_fetch(v: &View) -> String {
    let mut out = String::new();
    let headers = v.headers.clone();
    let form = v.form_pairs();
    if !headers.is_empty() {
        out.push_str("const myHeaders = new Headers();\n");
        for (k, val) in &headers {
            writeln!(out, "myHeaders.append({}, {});", js(k), js(val)).ok();
        }
        out.push('\n');
    }
    let body_var = match &v.body {
        BodyView::None => None,
        BodyView::Text(t) => {
            if let Some(json) = v.json_body() {
                writeln!(out, "const raw = JSON.stringify({});\n", pretty_json(&json)).ok();
                Some("raw")
            } else if let Some(pairs) = form {
                out.push_str("const urlencoded = new URLSearchParams();\n");
                for (k, val) in pairs {
                    writeln!(out, "urlencoded.append({}, {});", js(&k), js(&val)).ok();
                }
                out.push('\n');
                Some("urlencoded")
            } else {
                writeln!(out, "const raw = {};\n", js(t)).ok();
                Some("raw")
            }
        }
        BodyView::File(p) => {
            writeln!(out, "// Replace with the contents of {}", path_str(p)).ok();
            out.push_str("const file = \"<file contents here>\";\n\n");
            Some("file")
        }
        BodyView::Multipart(parts) => {
            out.push_str("const formdata = new FormData();\n");
            for part in parts.iter() {
                match &part.value {
                    MultipartValue::Text(t) => writeln!(out, "formdata.append({}, {});", js(&part.name), js(t)).ok(),
                    MultipartValue::File(p) => {
                        writeln!(out, "formdata.append({}, fileInput.files[0], {});", js(&part.name), js(&path_str(p)))
                            .ok()
                    }
                };
            }
            out.push('\n');
            Some("formdata")
        }
    };
    out.push_str("const requestOptions = {\n");
    writeln!(out, "  method: {},", js(&v.method)).ok();
    if !headers.is_empty() {
        out.push_str("  headers: myHeaders,\n");
    }
    if let Some(b) = body_var {
        writeln!(out, "  body: {b},").ok();
    }
    writeln!(out, "  redirect: \"{}\"", if v.settings.follow_redirects { "follow" } else { "manual" }).ok();
    out.push_str("};\n\n");
    writeln!(out, "fetch({}, requestOptions)", js(v.url)).ok();
    out.push_str("  .then((response) => response.text())\n");
    out.push_str("  .then((result) => console.log(result))\n");
    out.push_str("  .catch((error) => console.error(error));\n");
    out
}

pub fn node_axios(v: &View) -> String {
    let mut out = String::from("const axios = require(\"axios\");\n");
    let multipart = matches!(v.body, BodyView::Multipart(_));
    if multipart {
        out.push_str("const FormData = require(\"form-data\");\n");
    }
    let needs_fs = matches!(v.body, BodyView::File(_))
        || matches!(&v.body, BodyView::Multipart(parts) if parts.iter().any(|p| matches!(p.value, MultipartValue::File(_))));
    if needs_fs {
        out.push_str("const fs = require(\"fs\");\n");
    }
    if !v.settings.verify_ssl {
        out.push_str("const https = require(\"https\");\n");
    }
    match &v.body {
        BodyView::None => {}
        BodyView::Text(t) => match v.json_body() {
            Some(json) => {
                writeln!(out, "let data = JSON.stringify({});", pretty_json(&json)).ok();
            }
            None => {
                writeln!(out, "let data = {};", js(t)).ok();
            }
        },
        BodyView::File(p) => {
            writeln!(out, "let data = fs.readFileSync({});", js(&path_str(p))).ok();
        }
        BodyView::Multipart(parts) => {
            out.push_str("let data = new FormData();\n");
            for part in parts.iter() {
                match &part.value {
                    MultipartValue::Text(t) => writeln!(out, "data.append({}, {});", js(&part.name), js(t)).ok(),
                    MultipartValue::File(p) => {
                        writeln!(out, "data.append({}, fs.createReadStream({}));", js(&part.name), js(&path_str(p)))
                            .ok()
                    }
                };
            }
        }
    }
    out.push('\n');
    out.push_str("let config = {\n");
    writeln!(out, "  method: {},", js(&v.method.to_lowercase())).ok();
    out.push_str("  maxBodyLength: Infinity,\n");
    writeln!(out, "  url: {},", js(v.url)).ok();
    let headers = v.merged_headers();
    if !headers.is_empty() || multipart {
        out.push_str("  headers: {");
        let mut entries: Vec<String> = headers.iter().map(|(k, val)| format!("\n    {}: {}", js(k), js(val))).collect();
        if multipart {
            entries.push("\n    ...data.getHeaders()".into());
        }
        out.push_str(&entries.join(","));
        out.push_str("\n  },\n");
    }
    if !v.settings.follow_redirects {
        out.push_str("  maxRedirects: 0,\n");
    }
    if let Some(ms) = v.timeout_ms() {
        writeln!(out, "  timeout: {ms},").ok();
    }
    if !v.settings.verify_ssl {
        out.push_str("  httpsAgent: new https.Agent({ rejectUnauthorized: false }),\n");
    }
    if v.has_body() {
        out.push_str("  data: data\n");
    } else {
        // Remove the trailing comma of the last entry.
        if out.ends_with(",\n") {
            out.truncate(out.len() - 2);
            out.push('\n');
        }
    }
    out.push_str("};\n\n");
    out.push_str("axios.request(config)\n");
    out.push_str("  .then((response) => {\n    console.log(JSON.stringify(response.data));\n  })\n");
    out.push_str("  .catch((error) => {\n    console.log(error);\n  });\n");
    out
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

fn py(s: &str) -> String {
    dq(s, Unicode::Json, false)
}

pub fn python(v: &View) -> String {
    let mut out = String::from("import requests\n\n");
    writeln!(out, "url = {}\n", py(v.url)).ok();
    let mut args = vec!["headers=headers".to_string()];
    match &v.body {
        BodyView::None => {}
        BodyView::Text(t) => {
            writeln!(out, "payload = {}", py(t)).ok();
            args.push("data=payload".into());
        }
        BodyView::File(p) => {
            writeln!(out, "payload = open({}, \"rb\")", py(&path_str(p))).ok();
            args.push("data=payload".into());
        }
        BodyView::Multipart(parts) => {
            let texts: Vec<String> = parts
                .iter()
                .filter_map(|p| match &p.value {
                    MultipartValue::Text(t) => Some(format!("  ({}, {})", py(&p.name), py(t))),
                    _ => None,
                })
                .collect();
            let files: Vec<String> = parts
                .iter()
                .filter_map(|p| match &p.value {
                    MultipartValue::File(f) => Some(match &p.content_type {
                        Some(ct) => format!(
                            "  ({}, ({}, open({}, \"rb\"), {}))",
                            py(&p.name),
                            py(&file_name(f)),
                            py(&path_str(f)),
                            py(ct)
                        ),
                        None => {
                            format!("  ({}, ({}, open({}, \"rb\")))", py(&p.name), py(&file_name(f)), py(&path_str(f)))
                        }
                    }),
                    _ => None,
                })
                .collect();
            if texts.is_empty() {
                out.push_str("payload = []\n");
            } else {
                writeln!(out, "payload = [\n{}\n]", texts.join(",\n")).ok();
            }
            args.push("data=payload".into());
            if !files.is_empty() {
                writeln!(out, "files = [\n{}\n]", files.join(",\n")).ok();
                args.push("files=files".into());
            }
        }
    }
    let headers = v.merged_headers();
    if headers.is_empty() {
        out.push_str("headers = {}\n");
    } else {
        let entries: Vec<String> = headers.iter().map(|(k, val)| format!("  {}: {}", py(k), py(val))).collect();
        writeln!(out, "headers = {{\n{}\n}}", entries.join(",\n")).ok();
    }
    if !v.settings.verify_ssl {
        args.push("verify=False".into());
    }
    if !v.settings.follow_redirects {
        args.push("allow_redirects=False".into());
    }
    if let Some(ms) = v.timeout_ms() {
        args.push(format!("timeout={}", format_secs(ms)));
    }
    writeln!(out, "\nresponse = requests.request({}, url, {})\n", py(&v.method), args.join(", ")).ok();
    out.push_str("print(response.text)\n");
    out
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

fn go_str(s: &str) -> String {
    dq(s, Unicode::Json, false)
}

pub fn go(v: &View) -> String {
    let mut imports = vec!["fmt", "io", "net/http"];
    let mut body = String::new();
    let mut payload = "nil";
    let mut set_multipart_ct = false;
    match &v.body {
        BodyView::None => {}
        BodyView::Text(t) => {
            imports.push("strings");
            writeln!(body, "\tpayload := strings.NewReader({})\n", go_str(t)).ok();
            payload = "payload";
        }
        BodyView::File(p) => {
            imports.push("os");
            writeln!(body, "\tpayload, err := os.Open({})", go_str(&path_str(p))).ok();
            body.push_str("\tif err != nil {\n\t\tfmt.Println(err)\n\t\treturn\n\t}\n\tdefer payload.Close()\n\n");
            payload = "payload";
        }
        BodyView::Multipart(parts) => {
            imports.push("bytes");
            imports.push("mime/multipart");
            body.push_str("\tpayload := &bytes.Buffer{}\n\twriter := multipart.NewWriter(payload)\n");
            let mut n = 0;
            for part in parts.iter() {
                match &part.value {
                    MultipartValue::Text(t) => {
                        writeln!(body, "\t_ = writer.WriteField({}, {})", go_str(&part.name), go_str(t)).ok();
                    }
                    MultipartValue::File(f) => {
                        n += 1;
                        if !imports.contains(&"os") {
                            imports.push("os");
                        }
                        writeln!(body, "\tfile{n}, errFile{n} := os.Open({})", go_str(&path_str(f))).ok();
                        writeln!(body, "\tif errFile{n} != nil {{\n\t\tfmt.Println(errFile{n})\n\t\treturn\n\t}}").ok();
                        writeln!(body, "\tdefer file{n}.Close()").ok();
                        match &part.content_type {
                            Some(ct) => {
                                if !imports.contains(&"net/textproto") {
                                    imports.push("net/textproto");
                                }
                                writeln!(body, "\th{n} := make(textproto.MIMEHeader)").ok();
                                let disp = format!(
                                    "form-data; name=\"{}\"; filename=\"{}\"",
                                    part.name.replace('"', "\\\""),
                                    file_name(f).replace('"', "\\\"")
                                );
                                writeln!(body, "\th{n}.Set(\"Content-Disposition\", {})", go_str(&disp)).ok();
                                writeln!(body, "\th{n}.Set(\"Content-Type\", {})", go_str(ct)).ok();
                                writeln!(body, "\tpart{n}, errFile{n} := writer.CreatePart(h{n})").ok();
                            }
                            None => {
                                writeln!(
                                    body,
                                    "\tpart{n}, errFile{n} := writer.CreateFormFile({}, {})",
                                    go_str(&part.name),
                                    go_str(&file_name(f))
                                )
                                .ok();
                            }
                        }
                        writeln!(body, "\tif errFile{n} != nil {{\n\t\tfmt.Println(errFile{n})\n\t\treturn\n\t}}").ok();
                        writeln!(body, "\t_, errFile{n} = io.Copy(part{n}, file{n})").ok();
                        writeln!(body, "\tif errFile{n} != nil {{\n\t\tfmt.Println(errFile{n})\n\t\treturn\n\t}}").ok();
                    }
                }
            }
            body.push_str("\terr := writer.Close()\n\tif err != nil {\n\t\tfmt.Println(err)\n\t\treturn\n\t}\n\n");
            payload = "payload";
            set_multipart_ct = true;
        }
    }
    let mut client_fields = Vec::new();
    if let Some(ms) = v.timeout_ms() {
        imports.push("time");
        client_fields.push(format!("\t\tTimeout: {ms} * time.Millisecond,"));
    }
    if !v.settings.verify_ssl {
        imports.push("crypto/tls");
        client_fields.push(
            "\t\tTransport: &http.Transport{\n\t\t\tTLSClientConfig: &tls.Config{InsecureSkipVerify: true},\n\t\t},"
                .to_string(),
        );
    }
    if !v.settings.follow_redirects {
        client_fields.push(
            "\t\tCheckRedirect: func(req *http.Request, via []*http.Request) error {\n\t\t\treturn http.ErrUseLastResponse\n\t\t},"
                .to_string(),
        );
    }
    imports.sort();
    imports.dedup();

    let mut out = String::from("package main\n\nimport (\n");
    for i in &imports {
        writeln!(out, "\t\"{i}\"").ok();
    }
    out.push_str(")\n\nfunc main() {\n");
    writeln!(out, "\turl := {}", go_str(v.url)).ok();
    writeln!(out, "\tmethod := {}\n", go_str(&v.method)).ok();
    out.push_str(&body);
    if client_fields.is_empty() {
        out.push_str("\tclient := &http.Client{}\n");
    } else {
        writeln!(out, "\tclient := &http.Client{{\n{}\n\t}}", client_fields.join("\n")).ok();
    }
    // `req` is always new, so `:=` is valid even when `err` already exists.
    writeln!(out, "\treq, err := http.NewRequest(method, url, {payload})").ok();
    out.push_str("\n\tif err != nil {\n\t\tfmt.Println(err)\n\t\treturn\n\t}\n");
    for (k, val) in &v.headers {
        writeln!(out, "\treq.Header.Add({}, {})", go_str(k), go_str(val)).ok();
    }
    if set_multipart_ct {
        out.push_str("\treq.Header.Set(\"Content-Type\", writer.FormDataContentType())\n");
    }
    out.push_str("\n\tres, err := client.Do(req)\n\tif err != nil {\n\t\tfmt.Println(err)\n\t\treturn\n\t}\n");
    out.push_str("\tdefer res.Body.Close()\n\n");
    out.push_str("\tbody, err := io.ReadAll(res.Body)\n\tif err != nil {\n\t\tfmt.Println(err)\n\t\treturn\n\t}\n");
    out.push_str("\tfmt.Println(string(body))\n}\n");
    out
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

fn rust_str(s: &str) -> String {
    let printable = s.chars().all(|c| c == '\n' || c == '\t' || !c.is_control());
    if printable && (s.contains('"') || s.contains('\n')) && !s.contains("\"#") {
        format!("r#\"{s}\"#")
    } else {
        dq(s, Unicode::Braces, false)
    }
}

fn rust_method(m: &str) -> String {
    match m {
        "GET" | "POST" | "PUT" | "DELETE" | "HEAD" | "OPTIONS" | "CONNECT" | "PATCH" | "TRACE" => {
            format!("reqwest::Method::{m}")
        }
        other => format!("reqwest::Method::from_bytes({})?", dq(other, Unicode::Braces, false).replacen('"', "b\"", 1)),
    }
}

pub fn rust(v: &View) -> String {
    let mut out = String::from("#[tokio::main]\nasync fn main() -> Result<(), Box<dyn std::error::Error>> {\n");
    out.push_str("    let client = reqwest::Client::builder()\n");
    if !v.settings.verify_ssl {
        out.push_str("        .danger_accept_invalid_certs(true)\n");
    }
    if !v.settings.follow_redirects {
        out.push_str("        .redirect(reqwest::redirect::Policy::none())\n");
    }
    if let Some(ms) = v.timeout_ms() {
        writeln!(out, "        .timeout(std::time::Duration::from_millis({ms}))").ok();
    }
    out.push_str("        .build()?;\n\n");

    if let BodyView::Multipart(parts) = &v.body {
        out.push_str("    let form = reqwest::multipart::Form::new()");
        for part in parts.iter() {
            match &part.value {
                MultipartValue::Text(t) => {
                    write!(out, "\n        .text({}, {})", rust_str(&part.name), rust_str(t)).ok();
                }
                MultipartValue::File(f) => {
                    write!(
                        out,
                        "\n        .part({}, reqwest::multipart::Part::bytes(std::fs::read({})?).file_name({})",
                        rust_str(&part.name),
                        rust_str(&path_str(f)),
                        rust_str(&file_name(f))
                    )
                    .ok();
                    if let Some(ct) = &part.content_type {
                        write!(out, ".mime_str({})?", rust_str(ct)).ok();
                    }
                    out.push(')');
                }
            }
        }
        out.push_str(";\n\n");
    }
    if let BodyView::Text(t) = &v.body {
        writeln!(out, "    let data = {};\n", rust_str(t)).ok();
    }

    writeln!(out, "    let request = client.request({}, {})", rust_method(&v.method), rust_str(v.url)).ok();
    for (k, val) in &v.headers {
        writeln!(out, "        .header({}, {})", rust_str(k), rust_str(val)).ok();
    }
    match &v.body {
        BodyView::None => {}
        BodyView::Text(_) => out.push_str("        .body(data)\n"),
        BodyView::File(p) => {
            writeln!(out, "        .body(std::fs::read({})?)", rust_str(&path_str(p))).ok();
        }
        BodyView::Multipart(_) => out.push_str("        .multipart(form)\n"),
    }
    // Put the semicolon on the last builder line.
    if out.ends_with('\n') {
        out.pop();
    }
    out.push_str(";\n\n");
    out.push_str("    let response = request.send().await?;\n");
    out.push_str("    let body = response.text().await?;\n\n");
    out.push_str("    println!(\"{}\", body);\n\n");
    out.push_str("    Ok(())\n}\n");
    out
}

// ---------------------------------------------------------------------------
// JVM
// ---------------------------------------------------------------------------

fn java_str(s: &str) -> String {
    dq(s, Unicode::Json, false)
}

fn requires_body(method: &str) -> bool {
    matches!(method, "POST" | "PUT" | "PATCH")
}

pub fn java_okhttp(v: &View) -> String {
    let mut out = String::from("OkHttpClient client = new OkHttpClient().newBuilder()\n");
    if !v.settings.follow_redirects {
        out.push_str("  .followRedirects(false)\n");
    }
    if let Some(ms) = v.timeout_ms() {
        writeln!(out, "  .callTimeout({ms}, TimeUnit.MILLISECONDS)").ok();
    }
    out.push_str("  .build();\n");
    if !v.settings.verify_ssl {
        out.push_str("// Note: certificate verification is disabled for this request in Merpati.\n");
    }
    let ct = v.content_type().unwrap_or("text/plain");
    let mut body_expr = "null";
    match &v.body {
        BodyView::None => {
            if requires_body(&v.method) {
                writeln!(out, "MediaType mediaType = MediaType.parse({});", java_str(ct)).ok();
                out.push_str("RequestBody body = RequestBody.create(mediaType, \"\");\n");
                body_expr = "body";
            }
        }
        BodyView::Text(t) => {
            writeln!(out, "MediaType mediaType = MediaType.parse({});", java_str(ct)).ok();
            writeln!(out, "RequestBody body = RequestBody.create(mediaType, {});", java_str(t)).ok();
            body_expr = "body";
        }
        BodyView::File(p) => {
            writeln!(
                out,
                "MediaType mediaType = MediaType.parse({});",
                java_str(v.content_type().unwrap_or("application/octet-stream"))
            )
            .ok();
            writeln!(out, "RequestBody body = RequestBody.create(mediaType, new File({}));", java_str(&path_str(p)))
                .ok();
            body_expr = "body";
        }
        BodyView::Multipart(parts) => {
            out.push_str("RequestBody body = new MultipartBody.Builder().setType(MultipartBody.FORM)\n");
            for part in parts.iter() {
                match &part.value {
                    MultipartValue::Text(t) => {
                        writeln!(out, "  .addFormDataPart({}, {})", java_str(&part.name), java_str(t)).ok();
                    }
                    MultipartValue::File(f) => {
                        writeln!(
                            out,
                            "  .addFormDataPart({}, {},\n    RequestBody.create(MediaType.parse({}),\n    new File({})))",
                            java_str(&part.name),
                            java_str(&file_name(f)),
                            java_str(part.content_type.as_deref().unwrap_or("application/octet-stream")),
                            java_str(&path_str(f))
                        )
                        .ok();
                    }
                }
            }
            out.push_str("  .build();\n");
            body_expr = "body";
        }
    }
    out.push_str("Request request = new Request.Builder()\n");
    writeln!(out, "  .url({})", java_str(v.url)).ok();
    writeln!(out, "  .method({}, {body_expr})", java_str(&v.method)).ok();
    for (k, val) in &v.headers {
        writeln!(out, "  .addHeader({}, {})", java_str(k), java_str(val)).ok();
    }
    out.push_str("  .build();\n");
    out.push_str("Response response = client.newCall(request).execute();\n");
    out
}

pub fn java_httpclient(v: &View) -> String {
    let mut out = String::new();
    let mut builder = String::from("HttpClient client = HttpClient.newBuilder()\n");
    builder.push_str(if v.settings.follow_redirects {
        "    .followRedirects(HttpClient.Redirect.NORMAL)\n"
    } else {
        "    .followRedirects(HttpClient.Redirect.NEVER)\n"
    });
    if let Some(ms) = v.timeout_ms() {
        writeln!(builder, "    .connectTimeout(Duration.ofMillis({ms}))").ok();
    }
    match v.settings.http_version {
        HttpVersion::Http1 => builder.push_str("    .version(HttpClient.Version.HTTP_1_1)\n"),
        HttpVersion::Http2 => builder.push_str("    .version(HttpClient.Version.HTTP_2)\n"),
        HttpVersion::Auto => {}
    }
    builder.push_str("    .build();\n");
    if !v.settings.verify_ssl {
        builder.push_str("// Note: certificate verification is disabled for this request in Merpati.\n");
    }
    let publisher = match &v.body {
        BodyView::None => "HttpRequest.BodyPublishers.noBody()".to_string(),
        BodyView::Text(t) => format!("HttpRequest.BodyPublishers.ofString({})", java_str(t)),
        BodyView::File(p) => format!("HttpRequest.BodyPublishers.ofFile(Path.of({}))", java_str(&path_str(p))),
        BodyView::Multipart(parts) => {
            writeln!(out, "String boundary = {};", java_str(BOUNDARY)).ok();
            out.push_str("ByteArrayOutputStream parts = new ByteArrayOutputStream();\n");
            for part in parts.iter() {
                match &part.value {
                    MultipartValue::Text(t) => {
                        let mut head = format!("Content-Disposition: form-data; name=\"{}\"\r\n", part.name);
                        if let Some(ct) = &part.content_type {
                            write!(head, "Content-Type: {ct}\r\n").ok();
                        }
                        head.push_str("\r\n");
                        writeln!(
                            out,
                            "parts.write((\"--\" + boundary + \"\\r\\n\" + {}).getBytes(StandardCharsets.UTF_8));",
                            java_str(&format!("{head}{t}\r\n"))
                        )
                        .ok();
                    }
                    MultipartValue::File(f) => {
                        let head = format!(
                            "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
                            part.name,
                            file_name(f),
                            part.content_type.as_deref().unwrap_or("application/octet-stream")
                        );
                        writeln!(
                            out,
                            "parts.write((\"--\" + boundary + \"\\r\\n\" + {}).getBytes(StandardCharsets.UTF_8));",
                            java_str(&head)
                        )
                        .ok();
                        writeln!(out, "parts.write(Files.readAllBytes(Path.of({})));", java_str(&path_str(f))).ok();
                        out.push_str("parts.write(\"\\r\\n\".getBytes(StandardCharsets.UTF_8));\n");
                    }
                }
            }
            out.push_str("parts.write((\"--\" + boundary + \"--\\r\\n\").getBytes(StandardCharsets.UTF_8));\n\n");
            "HttpRequest.BodyPublishers.ofByteArray(parts.toByteArray())".to_string()
        }
    };
    let mut req = String::from("HttpRequest request = HttpRequest.newBuilder()\n");
    writeln!(req, "    .uri(URI.create({}))", java_str(v.url)).ok();
    if let Some(ms) = v.timeout_ms() {
        writeln!(req, "    .timeout(Duration.ofMillis({ms}))").ok();
    }
    // HttpClient forbids setting some headers itself.
    const RESTRICTED: [&str; 5] = ["connection", "content-length", "expect", "host", "upgrade"];
    for (k, val) in &v.headers {
        if RESTRICTED.iter().any(|r| r.eq_ignore_ascii_case(k)) {
            continue;
        }
        writeln!(req, "    .header({}, {})", java_str(k), java_str(val)).ok();
    }
    if matches!(v.body, BodyView::Multipart(_)) {
        req.push_str("    .header(\"Content-Type\", \"multipart/form-data; boundary=\" + boundary)\n");
    }
    writeln!(req, "    .method({}, {publisher})", java_str(&v.method)).ok();
    req.push_str("    .build();\n");
    let mut result = builder;
    result.push_str(&out);
    result.push_str(&req);
    result.push_str("HttpResponse<String> response = client.send(request, HttpResponse.BodyHandlers.ofString());\n");
    result.push_str("System.out.println(response.body());\n");
    result
}

fn kt_str(s: &str) -> String {
    dq(s, Unicode::Json, true)
}

pub fn kotlin(v: &View) -> String {
    let mut out = String::new();
    if v.settings.follow_redirects && v.timeout_ms().is_none() {
        out.push_str("val client = OkHttpClient()\n");
    } else {
        out.push_str("val client = OkHttpClient.Builder()\n");
        if !v.settings.follow_redirects {
            out.push_str("  .followRedirects(false)\n");
        }
        if let Some(ms) = v.timeout_ms() {
            writeln!(out, "  .callTimeout({ms}, TimeUnit.MILLISECONDS)").ok();
        }
        out.push_str("  .build()\n");
    }
    if !v.settings.verify_ssl {
        out.push_str("// Note: certificate verification is disabled for this request in Merpati.\n");
    }
    let ct = v.content_type().unwrap_or("text/plain");
    let mut body_expr = "null";
    match &v.body {
        BodyView::None => {
            if requires_body(&v.method) {
                writeln!(out, "val body = \"\".toRequestBody({}.toMediaType())", kt_str(ct)).ok();
                body_expr = "body";
            }
        }
        BodyView::Text(t) => {
            writeln!(out, "val mediaType = {}.toMediaType()", kt_str(ct)).ok();
            writeln!(out, "val body = {}.toRequestBody(mediaType)", kt_str(t)).ok();
            body_expr = "body";
        }
        BodyView::File(p) => {
            writeln!(
                out,
                "val mediaType = {}.toMediaType()",
                kt_str(v.content_type().unwrap_or("application/octet-stream"))
            )
            .ok();
            writeln!(out, "val body = File({}).asRequestBody(mediaType)", kt_str(&path_str(p))).ok();
            body_expr = "body";
        }
        BodyView::Multipart(parts) => {
            out.push_str("val body = MultipartBody.Builder().setType(MultipartBody.FORM)\n");
            for part in parts.iter() {
                match &part.value {
                    MultipartValue::Text(t) => {
                        writeln!(out, "  .addFormDataPart({}, {})", kt_str(&part.name), kt_str(t)).ok();
                    }
                    MultipartValue::File(f) => {
                        writeln!(
                            out,
                            "  .addFormDataPart({}, {},\n    File({}).asRequestBody({}.toMediaType()))",
                            kt_str(&part.name),
                            kt_str(&file_name(f)),
                            kt_str(&path_str(f)),
                            kt_str(part.content_type.as_deref().unwrap_or("application/octet-stream"))
                        )
                        .ok();
                    }
                }
            }
            out.push_str("  .build()\n");
            body_expr = "body";
        }
    }
    out.push_str("val request = Request.Builder()\n");
    writeln!(out, "  .url({})", kt_str(v.url)).ok();
    writeln!(out, "  .method({}, {body_expr})", kt_str(&v.method)).ok();
    for (k, val) in &v.headers {
        writeln!(out, "  .addHeader({}, {})", kt_str(k), kt_str(val)).ok();
    }
    out.push_str("  .build()\n");
    out.push_str("val response = client.newCall(request).execute()\n");
    out.push_str("println(response.body!!.string())\n");
    out
}

// ---------------------------------------------------------------------------
// C#
// ---------------------------------------------------------------------------

fn cs(s: &str) -> String {
    dq(s, Unicode::Json, false)
}

const CONTENT_HEADERS: [&str; 10] = [
    "content-type",
    "content-length",
    "content-encoding",
    "content-language",
    "content-location",
    "content-md5",
    "content-range",
    "content-disposition",
    "expires",
    "last-modified",
];

fn cs_method(m: &str) -> String {
    let known = ["Get", "Post", "Put", "Delete", "Head", "Options", "Patch", "Trace"];
    match known.iter().find(|k| k.eq_ignore_ascii_case(m)) {
        Some(k) => format!("HttpMethod.{k}"),
        None => format!("new HttpMethod({})", cs(m)),
    }
}

pub fn csharp(v: &View) -> String {
    let mut out = String::new();
    let custom_handler = !v.settings.verify_ssl || !v.settings.follow_redirects;
    if custom_handler {
        out.push_str("var handler = new HttpClientHandler();\n");
        if !v.settings.verify_ssl {
            out.push_str(
                "handler.ServerCertificateCustomValidationCallback = HttpClientHandler.DangerousAcceptAnyServerCertificateValidator;\n",
            );
        }
        if !v.settings.follow_redirects {
            out.push_str("handler.AllowAutoRedirect = false;\n");
        }
        out.push_str("var client = new HttpClient(handler);\n");
    } else {
        out.push_str("var client = new HttpClient();\n");
    }
    if let Some(ms) = v.timeout_ms() {
        writeln!(out, "client.Timeout = TimeSpan.FromMilliseconds({ms});").ok();
    }
    writeln!(out, "var request = new HttpRequestMessage({}, {});", cs_method(&v.method), cs(v.url)).ok();
    let is_content = |k: &str| CONTENT_HEADERS.iter().any(|c| c.eq_ignore_ascii_case(k));
    for (k, val) in &v.headers {
        if !is_content(k) {
            writeln!(out, "request.Headers.Add({}, {});", cs(k), cs(val)).ok();
        }
    }
    let content_headers: Vec<(&str, &str)> =
        v.headers.iter().filter(|(k, _)| is_content(k) && !k.eq_ignore_ascii_case("content-type")).copied().collect();
    let mut has_content = true;
    match &v.body {
        BodyView::None => has_content = false,
        BodyView::Text(t) => {
            writeln!(out, "var content = new StringContent({});", cs(t)).ok();
        }
        BodyView::File(p) => {
            writeln!(out, "var content = new StreamContent(File.OpenRead({}));", cs(&path_str(p))).ok();
        }
        BodyView::Multipart(parts) => {
            out.push_str("var content = new MultipartFormDataContent();\n");
            let mut n = 0;
            for part in parts.iter() {
                match &part.value {
                    MultipartValue::Text(t) => {
                        writeln!(out, "content.Add(new StringContent({}), {});", cs(t), cs(&part.name)).ok();
                    }
                    MultipartValue::File(f) => {
                        n += 1;
                        writeln!(out, "var file{n} = new StreamContent(File.OpenRead({}));", cs(&path_str(f))).ok();
                        if let Some(ct) = &part.content_type {
                            writeln!(
                                out,
                                "file{n}.Headers.ContentType = System.Net.Http.Headers.MediaTypeHeaderValue.Parse({});",
                                cs(ct)
                            )
                            .ok();
                        }
                        writeln!(out, "content.Add(file{n}, {}, {});", cs(&part.name), cs(&file_name(f))).ok();
                    }
                }
            }
        }
    }
    if has_content {
        if let (Some(ct), false) = (v.content_type(), matches!(v.body, BodyView::Multipart(_))) {
            writeln!(
                out,
                "content.Headers.ContentType = System.Net.Http.Headers.MediaTypeHeaderValue.Parse({});",
                cs(ct)
            )
            .ok();
        }
        for (k, val) in &content_headers {
            writeln!(out, "content.Headers.Add({}, {});", cs(k), cs(val)).ok();
        }
        out.push_str("request.Content = content;\n");
    }
    out.push_str("var response = await client.SendAsync(request);\n");
    out.push_str("response.EnsureSuccessStatusCode();\n");
    out.push_str("Console.WriteLine(await response.Content.ReadAsStringAsync());\n");
    out
}

// ---------------------------------------------------------------------------
// PHP
// ---------------------------------------------------------------------------

pub fn php(v: &View) -> String {
    let q = sq_backslash;
    let mut out = String::from("<?php\n\n$curl = curl_init();\n\ncurl_setopt_array($curl, array(\n");
    writeln!(out, "  CURLOPT_URL => {},", q(v.url)).ok();
    out.push_str("  CURLOPT_RETURNTRANSFER => true,\n");
    out.push_str("  CURLOPT_ENCODING => '',\n");
    writeln!(out, "  CURLOPT_MAXREDIRS => {},", v.settings.max_redirects).ok();
    match v.timeout_ms() {
        Some(ms) => {
            writeln!(out, "  CURLOPT_TIMEOUT_MS => {ms},").ok();
        }
        None => out.push_str("  CURLOPT_TIMEOUT => 0,\n"),
    }
    writeln!(out, "  CURLOPT_FOLLOWLOCATION => {},", v.settings.follow_redirects).ok();
    match v.settings.http_version {
        HttpVersion::Http1 => out.push_str("  CURLOPT_HTTP_VERSION => CURL_HTTP_VERSION_1_1,\n"),
        HttpVersion::Http2 => out.push_str("  CURLOPT_HTTP_VERSION => CURL_HTTP_VERSION_2_0,\n"),
        HttpVersion::Auto => {}
    }
    if !v.settings.verify_ssl {
        out.push_str("  CURLOPT_SSL_VERIFYPEER => false,\n  CURLOPT_SSL_VERIFYHOST => 0,\n");
    }
    writeln!(out, "  CURLOPT_CUSTOMREQUEST => {},", q(&v.method)).ok();
    if v.method == "HEAD" {
        out.push_str("  CURLOPT_NOBODY => true,\n");
    }
    match &v.body {
        BodyView::None => {}
        BodyView::Text(t) => {
            writeln!(out, "  CURLOPT_POSTFIELDS => {},", q(t)).ok();
        }
        BodyView::File(p) => {
            writeln!(out, "  CURLOPT_POSTFIELDS => file_get_contents({}),", q(&path_str(p))).ok();
        }
        BodyView::Multipart(parts) => {
            let entries: Vec<String> = parts
                .iter()
                .map(|p| match &p.value {
                    MultipartValue::Text(t) => format!("{} => {}", q(&p.name), q(t)),
                    MultipartValue::File(f) => match &p.content_type {
                        Some(ct) => format!("{} => new CURLFILE({}, {})", q(&p.name), q(&path_str(f)), q(ct)),
                        None => format!("{} => new CURLFILE({})", q(&p.name), q(&path_str(f))),
                    },
                })
                .collect();
            writeln!(out, "  CURLOPT_POSTFIELDS => array({}),", entries.join(", ")).ok();
        }
    }
    if !v.headers.is_empty() {
        out.push_str("  CURLOPT_HTTPHEADER => array(\n");
        let hs: Vec<String> = v.headers.iter().map(|(k, val)| format!("    {}", q(&format!("{k}: {val}")))).collect();
        out.push_str(&hs.join(",\n"));
        out.push_str("\n  ),\n");
    }
    out.push_str("));\n\n$response = curl_exec($curl);\n\ncurl_close($curl);\necho $response;\n");
    out
}

// ---------------------------------------------------------------------------
// Swift
// ---------------------------------------------------------------------------

fn swift_str(s: &str) -> String {
    dq(s, Unicode::Braces, false)
}

pub fn swift(v: &View) -> String {
    let mut out =
        String::from("import Foundation\n#if canImport(FoundationNetworking)\nimport FoundationNetworking\n#endif\n\n");
    if !v.settings.verify_ssl || !v.settings.follow_redirects {
        out.push_str("// Note: Merpati sends this request with");
        if !v.settings.verify_ssl {
            out.push_str(" certificate verification disabled");
        }
        if !v.settings.follow_redirects {
            if !v.settings.verify_ssl {
                out.push_str(" and");
            }
            out.push_str(" redirects disabled");
        }
        out.push_str("; replicate it with a URLSessionDelegate.\n");
    }
    match &v.body {
        BodyView::Text(t) => {
            writeln!(out, "let parameters = {}", swift_str(t)).ok();
            out.push_str("let postData = parameters.data(using: .utf8)\n\n");
        }
        BodyView::File(p) => {
            writeln!(out, "let postData = try! Data(contentsOf: URL(fileURLWithPath: {}))\n", swift_str(&path_str(p)))
                .ok();
        }
        BodyView::Multipart(parts) => {
            writeln!(out, "let boundary = {}", swift_str(BOUNDARY)).ok();
            out.push_str("var body = Data()\n");
            let append = |out: &mut String, s: &str| {
                writeln!(out, "body.append({}.data(using: .utf8)!)", swift_str(s)).ok();
            };
            for part in parts.iter() {
                append(&mut out, &format!("--{BOUNDARY}\r\n"));
                match &part.value {
                    MultipartValue::Text(t) => {
                        let mut head = format!("Content-Disposition: form-data; name=\"{}\"\r\n", part.name);
                        if let Some(ct) = &part.content_type {
                            write!(head, "Content-Type: {ct}\r\n").ok();
                        }
                        head.push_str("\r\n");
                        append(&mut out, &head);
                        append(&mut out, &format!("{t}\r\n"));
                    }
                    MultipartValue::File(f) => {
                        append(
                            &mut out,
                            &format!(
                                "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
                                part.name,
                                file_name(f),
                                part.content_type.as_deref().unwrap_or("application/octet-stream")
                            ),
                        );
                        writeln!(
                            out,
                            "body.append(try! Data(contentsOf: URL(fileURLWithPath: {})))",
                            swift_str(&path_str(f))
                        )
                        .ok();
                        append(&mut out, "\r\n");
                    }
                }
            }
            append(&mut out, &format!("--{BOUNDARY}--\r\n"));
            out.push_str("let postData = body\n\n");
        }
        BodyView::None => {}
    }
    let timeout = match v.timeout_ms() {
        Some(ms) => format_secs(ms),
        None => "Double.infinity".into(),
    };
    writeln!(out, "var request = URLRequest(url: URL(string: {})!, timeoutInterval: {timeout})", swift_str(v.url)).ok();
    for (k, val) in &v.headers {
        writeln!(out, "request.addValue({}, forHTTPHeaderField: {})", swift_str(val), swift_str(k)).ok();
    }
    if matches!(v.body, BodyView::Multipart(_)) {
        out.push_str(
            "request.addValue(\"multipart/form-data; boundary=\\(boundary)\", forHTTPHeaderField: \"Content-Type\")\n",
        );
    }
    out.push('\n');
    writeln!(out, "request.httpMethod = {}", swift_str(&v.method)).ok();
    if v.has_body() {
        out.push_str("request.httpBody = postData\n");
    }
    out.push_str(
        "\nlet semaphore = DispatchSemaphore(value: 0)\n\
         let task = URLSession.shared.dataTask(with: request) { data, response, error in\n\
         \x20 guard let data = data else {\n\
         \x20   print(String(describing: error))\n\
         \x20   semaphore.signal()\n\
         \x20   return\n\
         \x20 }\n\
         \x20 print(String(data: data, encoding: .utf8)!)\n\
         \x20 semaphore.signal()\n\
         }\n\n\
         task.resume()\n\
         semaphore.wait()\n",
    );
    out
}

// ---------------------------------------------------------------------------
// Ruby
// ---------------------------------------------------------------------------

fn ruby_class(m: &str) -> Option<&'static str> {
    Some(match m {
        "GET" => "Get",
        "POST" => "Post",
        "PUT" => "Put",
        "PATCH" => "Patch",
        "DELETE" => "Delete",
        "HEAD" => "Head",
        "OPTIONS" => "Options",
        "TRACE" => "Trace",
        _ => return None,
    })
}

pub fn ruby(v: &View) -> String {
    let q = sq_backslash;
    let mut out = String::from("require 'uri'\nrequire 'net/http'\n");
    if !v.settings.verify_ssl {
        out.push_str("require 'openssl'\n");
    }
    out.push('\n');
    writeln!(out, "url = URI({})\n", q(v.url)).ok();
    out.push_str("http = Net::HTTP.new(url.host, url.port)\n");
    if v.url.to_ascii_lowercase().starts_with("https") {
        out.push_str("http.use_ssl = true\n");
    }
    if !v.settings.verify_ssl {
        out.push_str("http.verify_mode = OpenSSL::SSL::VERIFY_NONE\n");
    }
    if let Some(ms) = v.timeout_ms() {
        let secs = format_secs(ms);
        writeln!(out, "http.open_timeout = {secs}\nhttp.read_timeout = {secs}").ok();
    }
    out.push('\n');
    match ruby_class(&v.method) {
        Some(c) => writeln!(out, "request = Net::HTTP::{c}.new(url)").ok(),
        None => writeln!(out, "request = Net::HTTPGenericRequest.new({}, true, true, url)", q(&v.method)).ok(),
    };
    for (k, val) in &v.headers {
        writeln!(out, "request[{}] = {}", q(k), q(val)).ok();
    }
    match &v.body {
        BodyView::None => {}
        BodyView::Text(t) => {
            writeln!(out, "request.body = {}", q(t)).ok();
        }
        BodyView::File(p) => {
            writeln!(out, "request.body = File.binread({})", q(&path_str(p))).ok();
        }
        BodyView::Multipart(parts) => {
            let entries: Vec<String> = parts
                .iter()
                .map(|p| match &p.value {
                    MultipartValue::Text(t) => format!("[{}, {}]", q(&p.name), q(t)),
                    MultipartValue::File(f) => match &p.content_type {
                        Some(ct) => {
                            format!("[{}, File.open({}), {{ content_type: {} }}]", q(&p.name), q(&path_str(f)), q(ct))
                        }
                        None => format!("[{}, File.open({})]", q(&p.name), q(&path_str(f))),
                    },
                })
                .collect();
            writeln!(out, "form_data = [{}]", entries.join(", ")).ok();
            out.push_str("request.set_form form_data, 'multipart/form-data'\n");
        }
    }
    out.push_str("\nresponse = http.request(request)\nputs response.read_body\n");
    out
}

// ---------------------------------------------------------------------------
// Dart
// ---------------------------------------------------------------------------

fn dart_str(s: &str) -> String {
    dq(s, Unicode::Json, true)
}

pub fn dart(v: &View) -> String {
    let mut out = String::new();
    let multipart = matches!(v.body, BodyView::Multipart(_));
    let typed_file = matches!(&v.body, BodyView::Multipart(parts) if parts.iter().any(|p| matches!(p.value, MultipartValue::File(_)) && p.content_type.is_some()));
    if matches!(v.body, BodyView::File(_)) {
        out.push_str("import 'dart:io';\n");
    }
    out.push_str("import 'package:http/http.dart' as http;\n");
    if typed_file {
        out.push_str("import 'package:http_parser/http_parser.dart';\n");
    }
    out.push_str("\nvoid main() async {\n");
    let headers = v.merged_headers();
    if !headers.is_empty() {
        out.push_str("  var headers = {\n");
        let entries: Vec<String> =
            headers.iter().map(|(k, val)| format!("    {}: {}", dart_str(k), dart_str(val))).collect();
        out.push_str(&entries.join(",\n"));
        out.push_str("\n  };\n");
    }
    if multipart {
        writeln!(
            out,
            "  var request = http.MultipartRequest({}, Uri.parse({}));",
            dart_str(&v.method),
            dart_str(v.url)
        )
        .ok();
    } else {
        writeln!(out, "  var request = http.Request({}, Uri.parse({}));", dart_str(&v.method), dart_str(v.url)).ok();
    }
    match &v.body {
        BodyView::None => {}
        BodyView::Text(t) => {
            writeln!(out, "  request.body = {};", dart_str(t)).ok();
        }
        BodyView::File(p) => {
            writeln!(out, "  request.bodyBytes = await File({}).readAsBytes();", dart_str(&path_str(p))).ok();
        }
        BodyView::Multipart(parts) => {
            for part in parts.iter() {
                match &part.value {
                    MultipartValue::Text(t) => {
                        writeln!(out, "  request.fields[{}] = {};", dart_str(&part.name), dart_str(t)).ok();
                    }
                    MultipartValue::File(f) => {
                        let extra = match &part.content_type {
                            Some(ct) => format!(", contentType: MediaType.parse({})", dart_str(ct)),
                            None => String::new(),
                        };
                        writeln!(
                            out,
                            "  request.files.add(await http.MultipartFile.fromPath({}, {}{extra}));",
                            dart_str(&part.name),
                            dart_str(&path_str(f))
                        )
                        .ok();
                    }
                }
            }
        }
    }
    if !headers.is_empty() {
        out.push_str("  request.headers.addAll(headers);\n");
    }
    if !v.settings.follow_redirects {
        out.push_str("  request.followRedirects = false;\n");
    }
    if !v.settings.verify_ssl {
        out.push_str("  // Note: certificate verification is disabled for this request in Merpati.\n");
    }
    match v.timeout_ms() {
        Some(ms) => {
            writeln!(
                out,
                "\n  http.StreamedResponse response = await request.send().timeout(Duration(milliseconds: {ms}));\n"
            )
            .ok();
        }
        None => out.push_str("\n  http.StreamedResponse response = await request.send();\n\n"),
    }
    out.push_str("  if (response.statusCode == 200) {\n    print(await response.stream.bytesToString());\n  }\n");
    out.push_str("  else {\n    print(response.reasonPhrase);\n  }\n}\n");
    out
}
