//! Converters between Merpati's model and other formats.
//!
//! * Importers: cURL, OpenAPI 3.x / Swagger 2.0, Insomnia, HAR, Hoppscotch,
//!   Thunder Client, WSDL (SOAP), Smithy (IDL 2.0 and JSON AST). Postman formats live in
//!   `merpati_core::postman`; [`import_auto`] dispatches to all of them.
//! * Exporters: OpenAPI 3.0 from a collection.
//! * Code generators: snippets in many languages from a prepared request.
//! * GraphQL helpers: introspection parsing, autocomplete, query skeletons.
//!
//! Nothing here performs I/O, except the explicit `docs::write_html_site`
//! and `smithy::resolve_maven_dependencies` (reads jars of the local Maven
//! repository, never the network).

pub mod codegen;
pub mod curl;
pub mod docs;
pub mod graphql;
pub mod har;
pub mod hoppscotch;
pub mod insomnia;
pub mod openapi;
pub mod smithy;
pub mod soap;
pub mod thunder;
mod util;
mod xml;

use merpati_core::postman::{self, PostmanFile};
use merpati_core::{Collection, Environment, RequestItem, Variable};

#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    /// The input could not be parsed (syntax error).
    #[error("parse error: {0}")]
    Parse(String),
    /// The input parsed but is not in a supported shape or version.
    #[error("unsupported format: {0}")]
    Format(String),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid YAML: {0}")]
    Yaml(String),
    #[error("invalid XML: {0}")]
    Xml(String),
    #[error(transparent)]
    Core(#[from] merpati_core::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImportFormat {
    PostmanCollection,
    PostmanEnvironment,
    PostmanGlobals,
    OpenApi,
    Insomnia,
    Har,
    Hoppscotch,
    Thunder,
    Wsdl,
    Smithy,
    Curl,
}

impl ImportFormat {
    pub fn label(self) -> &'static str {
        match self {
            ImportFormat::PostmanCollection => "Postman collection",
            ImportFormat::PostmanEnvironment => "Postman environment",
            ImportFormat::PostmanGlobals => "Postman globals",
            ImportFormat::OpenApi => "OpenAPI / Swagger",
            ImportFormat::Insomnia => "Insomnia",
            ImportFormat::Har => "HAR",
            ImportFormat::Hoppscotch => "Hoppscotch",
            ImportFormat::Thunder => "Thunder Client",
            ImportFormat::Wsdl => "WSDL",
            ImportFormat::Smithy => "Smithy",
            ImportFormat::Curl => "cURL",
        }
    }
}

/// Parses text that is either JSON or YAML into a JSON value.
pub(crate) fn parse_structured(text: &str) -> Result<serde_json::Value, ConvertError> {
    let trimmed = text.trim_start_matches('\u{feff}').trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Ok(serde_json::from_str(trimmed)?);
    }
    util::yaml_to_json(trimmed)
}

fn detect_json(v: &serde_json::Value) -> Option<ImportFormat> {
    use serde_json::Value;
    match postman::detect(v) {
        Some(postman::PostmanKind::Collection) => return Some(ImportFormat::PostmanCollection),
        Some(postman::PostmanKind::Environment) => {
            // Thunder Client environments also have `values`? No: they use `data`.
            return Some(ImportFormat::PostmanEnvironment);
        }
        Some(postman::PostmanKind::Globals) => return Some(ImportFormat::PostmanGlobals),
        None => {}
    }
    if v.get("openapi").is_some() || v.get("swagger").is_some() {
        return Some(ImportFormat::OpenApi);
    }
    if smithy::is_json_ast(v) {
        return Some(ImportFormat::Smithy);
    }
    if v.get("_type").and_then(Value::as_str) == Some("export") && v.get("resources").is_some() {
        return Some(ImportFormat::Insomnia);
    }
    if v.get("type").and_then(Value::as_str).map(|t| t.starts_with("collection.insomnia.rest")).unwrap_or(false) {
        return Some(ImportFormat::Insomnia);
    }
    if v.pointer("/log/entries").is_some() {
        return Some(ImportFormat::Har);
    }
    if v.get("client").and_then(Value::as_str) == Some("Thunder Client")
        || (v.get("collectionName").is_some() && v.get("requests").is_some())
    {
        return Some(ImportFormat::Thunder);
    }
    let looks_hopp = |o: &Value| o.get("requests").is_some() && o.get("folders").is_some() && o.get("name").is_some();
    match v {
        Value::Array(items) if !items.is_empty() && items.iter().all(looks_hopp) => Some(ImportFormat::Hoppscotch),
        Value::Object(_) if looks_hopp(v) => Some(ImportFormat::Hoppscotch),
        _ => None,
    }
}

/// Sniffs the format of pasted text or file content.
pub fn detect(text: &str) -> Option<ImportFormat> {
    let t = text.trim_start_matches('\u{feff}').trim_start();
    if curl::looks_like_curl(t) {
        return Some(ImportFormat::Curl);
    }
    if t.starts_with('<') {
        return xml::looks_like_wsdl(t).then_some(ImportFormat::Wsdl);
    }
    if t.starts_with('{') || t.starts_with('[') {
        return serde_json::from_str::<serde_json::Value>(t).ok().as_ref().and_then(detect_json);
    }
    if smithy::looks_like_smithy(t) {
        return Some(ImportFormat::Smithy);
    }
    // YAML: only the formats that are commonly YAML.
    let head: String = t.lines().take(40).collect::<Vec<_>>().join("\n");
    let yamlish = head.lines().any(|l| {
        let l = l.trim_start_matches(['\'', '"']);
        l.starts_with("openapi:") || l.starts_with("swagger:") || l.starts_with("type: collection.insomnia.rest")
    });
    if yamlish && let Ok(v) = util::yaml_to_json(t) {
        return detect_json(&v);
    }
    None
}

/// Result of [`import_auto`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Imported {
    pub collections: Vec<Collection>,
    pub environments: Vec<Environment>,
    pub globals: Vec<Variable>,
    pub warnings: Vec<String>,
}

/// One entry point for the UI/CLI "Import" action. Postman formats delegate
/// to `merpati_core::postman`.
pub fn import_auto(text: &str) -> Result<Imported, ConvertError> {
    let format = detect(text).ok_or_else(|| {
        ConvertError::Format(
            "unrecognized content; supported: Postman, OpenAPI/Swagger, Insomnia, HAR, Hoppscotch, \
             Thunder Client, WSDL, Smithy, cURL"
                .into(),
        )
    })?;
    let mut out = Imported::default();
    match format {
        ImportFormat::PostmanCollection | ImportFormat::PostmanEnvironment | ImportFormat::PostmanGlobals => {
            let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
            let (file, report) = postman::import_any(&v)?;
            out.warnings = report.warnings;
            match file {
                PostmanFile::Collection(c) => out.collections.push(c),
                PostmanFile::Environment(e) => out.environments.push(e),
                PostmanFile::Globals(g) => out.globals = g,
            }
        }
        ImportFormat::OpenApi => {
            let (c, w) = openapi::import(text)?;
            out.collections.push(c);
            out.warnings = w;
        }
        ImportFormat::Insomnia => {
            let (c, e, w) = insomnia::import(text)?;
            out.collections = c;
            out.environments = e;
            out.warnings = w;
        }
        ImportFormat::Har => {
            let (c, w) = har::import(text)?;
            out.collections.push(c);
            out.warnings = w;
        }
        ImportFormat::Hoppscotch => {
            let (c, w) = hoppscotch::import(text)?;
            out.collections = c;
            out.warnings = w;
        }
        ImportFormat::Thunder => {
            let (c, w) = thunder::import(text)?;
            out.collections = c;
            out.warnings = w;
        }
        ImportFormat::Wsdl => {
            let (c, w) = soap::import_wsdl(text)?;
            out.collections.push(c);
            out.warnings = w;
        }
        ImportFormat::Smithy => {
            let (c, w) = smithy::import(text)?;
            out.collections.push(c);
            out.warnings = w;
        }
        ImportFormat::Curl => {
            let (req, w) = curl::parse_with_warnings(text)?;
            let name = util::request_name_from_url(&req.method, &req.url);
            let mut c = Collection::new("Imported from cURL");
            c.items.push(RequestItem::new(name, req).into());
            out.collections.push(c);
            out.warnings = w;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_formats() {
        assert_eq!(detect("curl https://x.test"), Some(ImportFormat::Curl));
        assert_eq!(detect("  $ curl -X POST x"), Some(ImportFormat::Curl));
        assert_eq!(detect("openapi: 3.0.0\ninfo:\n  title: x\npaths: {}"), Some(ImportFormat::OpenApi));
        assert_eq!(detect("{\"swagger\": \"2.0\", \"paths\": {}}"), Some(ImportFormat::OpenApi));
        assert_eq!(detect("{\"log\": {\"entries\": []}}"), Some(ImportFormat::Har));
        assert_eq!(detect("{\"_type\": \"export\", \"resources\": []}"), Some(ImportFormat::Insomnia));
        assert_eq!(detect("{\"info\": {\"name\": \"x\"}, \"item\": []}"), Some(ImportFormat::PostmanCollection));
        assert_eq!(detect("{\"name\": \"e\", \"values\": []}"), Some(ImportFormat::PostmanEnvironment));
        assert_eq!(
            detect("{\"client\": \"Thunder Client\", \"collectionName\": \"c\", \"requests\": []}"),
            Some(ImportFormat::Thunder)
        );
        assert_eq!(
            detect("[{\"v\": 2, \"name\": \"c\", \"folders\": [], \"requests\": []}]"),
            Some(ImportFormat::Hoppscotch)
        );
        assert_eq!(detect("hello world"), None);
        assert_eq!(detect("{\"random\": 1}"), None);
    }

    #[test]
    fn import_auto_curl() {
        let out = import_auto("curl -X POST https://api.test/users -d '{\"a\":1}' -H 'Content-Type: application/json'")
            .unwrap();
        assert_eq!(out.collections.len(), 1);
        let r = out.collections[0].requests()[0];
        assert_eq!(r.http().unwrap().method, "POST");
        assert_eq!(r.name, "POST /users");
        assert!(out.warnings.is_empty());
        let out = import_auto("curl https://api.test/n --data-urlencode msg@note.txt").unwrap();
        assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
    }

    #[test]
    fn every_fixture_is_detected_and_imports() {
        let cases: &[(&str, ImportFormat)] = &[
            (include_str!("../testdata/petstore.openapi.yaml"), ImportFormat::OpenApi),
            (include_str!("../testdata/petstore.swagger.json"), ImportFormat::OpenApi),
            (include_str!("../testdata/insomnia_v4.json"), ImportFormat::Insomnia),
            (include_str!("../testdata/insomnia_v5.yaml"), ImportFormat::Insomnia),
            (include_str!("../testdata/sample.har"), ImportFormat::Har),
            (include_str!("../testdata/hoppscotch.json"), ImportFormat::Hoppscotch),
            (include_str!("../testdata/thunder.json"), ImportFormat::Thunder),
            (include_str!("../testdata/users.wsdl"), ImportFormat::Wsdl),
            (include_str!("../testdata/weather.smithy"), ImportFormat::Smithy),
            (include_str!("../testdata/weather.smithy.json"), ImportFormat::Smithy),
            (
                include_str!("../../merpati-core/testdata/sample.postman_collection.json"),
                ImportFormat::PostmanCollection,
            ),
        ];
        for (text, fmt) in cases {
            assert_eq!(detect(text), Some(*fmt), "{}", &text[..60.min(text.len())]);
            let out = import_auto(text).unwrap_or_else(|e| panic!("{fmt:?}: {e}"));
            assert!(!out.collections.is_empty(), "{fmt:?}");
            assert!(out.collections[0].request_count() > 0, "{fmt:?}");
        }
    }

    #[test]
    fn import_auto_rejects_unknown() {
        assert!(matches!(import_auto("nope"), Err(ConvertError::Format(_))));
    }
}
