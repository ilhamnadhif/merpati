//! WSDL 1.1 import (FR-X6): every SOAP 1.1/1.2 binding operation becomes a
//! POST request with an envelope template (SoapUI-style `?` placeholders),
//! the right Content-Type and SOAPAction, plus an example response envelope.

use std::collections::HashMap;

use merpati_core::model::{Body, Collection, Example, Folder, HttpRequest, Item, KeyValue, RawLanguage, RequestItem};
use merpati_core::variables::Variable;

use crate::ConvertError;
use crate::xml::{self, Element, WSDL2_NS};

const SOAP11_BINDING_NS: &str = "http://schemas.xmlsoap.org/wsdl/soap/";
const SOAP12_BINDING_NS: &str = "http://schemas.xmlsoap.org/wsdl/soap12/";
const SOAP11_ENV: &str = "http://schemas.xmlsoap.org/soap/envelope/";
const SOAP12_ENV: &str = "http://www.w3.org/2003/05/soap-envelope";
const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema";
const MAX_DEPTH: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SoapVersion {
    V11,
    V12,
}

struct Schema<'a> {
    target_ns: String,
    qualified: bool,
    el: &'a Element,
}

struct Wsdl<'a> {
    root: &'a Element,
    schemas: Vec<Schema<'a>>,
    warnings: Vec<String>,
    /// Components expanded through a `ref` or a base type, with the depth:
    /// those hops keep the depth, so a cycle would never reach `MAX_DEPTH`.
    expanding: Vec<(&'a Element, usize)>,
}

impl<'a> Wsdl<'a> {
    fn warn(&mut self, msg: String) {
        if !self.warnings.contains(&msg) {
            self.warnings.push(msg);
        }
    }

    /// Marks `el` as being expanded at `depth`; false if it already is.
    fn enter(&mut self, el: &'a Element, depth: usize) -> bool {
        if self.expanding.iter().any(|(e, d)| std::ptr::eq(*e, el) && *d == depth) {
            return false;
        }
        self.expanding.push((el, depth));
        true
    }

    /// A top-level schema component with its schema's target namespace and
    /// whether local elements are qualified.
    fn find_top(&self, kind: &str, ns: &str, name: &str) -> Option<(&'a Element, String, bool)> {
        let lookup = |s: &Schema<'a>| -> Option<(&'a Element, String, bool)> {
            let el: &'a Element = s.el;
            el.children
                .iter()
                .find(|e| e.name == kind && e.attr("name") == Some(name))
                .map(|e| (e, s.target_ns.clone(), s.qualified))
        };
        self.schemas
            .iter()
            .filter(|s| s.target_ns == ns)
            .find_map(lookup)
            // Tolerate wrong prefixes: match by local name only.
            .or_else(|| self.schemas.iter().find_map(lookup))
    }

    fn message(&self, qname_holder: &Element, qname: &str) -> Option<&'a Element> {
        let (_, local) = qname_holder.resolve_qname(qname);
        self.root.children_named("message").find(|m| m.attr("name") == Some(local.as_str()))
    }

    /// XML for one schema element declaration, indented by `depth`.
    /// `qualified` applies to this element's tag, `form_qualified` to its
    /// local child elements (the schema's `elementFormDefault`).
    #[allow(clippy::too_many_arguments)]
    fn render_element(
        &mut self,
        decl: &'a Element,
        schema_ns: &str,
        qualified: bool,
        form_qualified: bool,
        depth: usize,
        out: &mut String,
        prefix_ns: &mut Vec<(String, String)>,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        let pad = "  ".repeat(depth);
        // `ref="tns:Other"` points at a top-level element.
        if let Some(r) = decl.attr("ref") {
            let (ns, local) = decl.resolve_qname(r);
            if let Some((target, tns, form)) = self.find_top("element", &ns, &local)
                && self.enter(target, depth)
            {
                self.render_element(target, &tns, true, form, depth, out, prefix_ns);
                self.expanding.pop();
            }
            return;
        }
        let Some(name) = decl.attr("name") else { return };
        let tag = if qualified { format!("{}:{name}", prefix_for(schema_ns, prefix_ns)) } else { name.to_string() };
        if decl.attr("minOccurs") == Some("0") {
            out.push_str(&format!("{pad}<!--Optional:-->\n"));
        }
        if matches!(decl.attr("maxOccurs"), Some(m) if m == "unbounded" || m.parse::<u32>().map(|n| n > 1).unwrap_or(false))
        {
            out.push_str(&format!("{pad}<!--Zero or more repetitions:-->\n"));
        }
        // Inline complex type, named type reference, or simple content.
        let complex = decl.child("complexType").or_else(|| {
            let ty = decl.attr("type")?;
            let (ns, local) = decl.resolve_qname(ty);
            if ns == XSD_NS {
                return None;
            }
            self.find_top("complexType", &ns, &local).map(|(e, _, _)| e)
        });
        match complex {
            Some(ct) => {
                let mut inner = String::new();
                self.render_particles(ct, schema_ns, form_qualified, depth + 1, &mut inner, prefix_ns);
                if inner.is_empty() {
                    out.push_str(&format!("{pad}<{tag}/>\n"));
                } else {
                    out.push_str(&format!("{pad}<{tag}>\n{inner}{pad}</{tag}>\n"));
                }
            }
            None => out.push_str(&format!("{pad}<{tag}>?</{tag}>\n")),
        }
    }

    /// Children of a complexType: sequence/all/choice and complexContent extensions.
    fn render_particles(
        &mut self,
        ct: &'a Element,
        schema_ns: &str,
        qualified: bool,
        depth: usize,
        out: &mut String,
        prefix_ns: &mut Vec<(String, String)>,
    ) {
        for c in &ct.children {
            match c.name.as_str() {
                "sequence" | "all" => self.render_particles(c, schema_ns, qualified, depth, out, prefix_ns),
                "choice" => {
                    out.push_str(&format!("{}<!--You have a CHOICE of the next items:-->\n", "  ".repeat(depth)));
                    self.render_particles(c, schema_ns, qualified, depth, out, prefix_ns);
                }
                "element" => self.render_element(c, schema_ns, qualified, qualified, depth, out, prefix_ns),
                "complexContent" | "simpleContent" => {
                    for ext in c.children.iter().filter(|e| e.name == "extension" || e.name == "restriction") {
                        if let Some(base) = ext.attr("base") {
                            let (ns, local) = ext.resolve_qname(base);
                            if ns != XSD_NS
                                && let Some((base_ct, _, _)) = self.find_top("complexType", &ns, &local)
                                && self.enter(base_ct, depth)
                            {
                                self.render_particles(base_ct, schema_ns, qualified, depth, out, prefix_ns);
                                self.expanding.pop();
                            }
                        }
                        self.render_particles(ext, schema_ns, qualified, depth, out, prefix_ns);
                    }
                }
                _ => {}
            }
        }
    }

    /// Body content for a message (document/literal parts or rpc wrapper).
    fn message_body(
        &mut self,
        msg: &'a Element,
        op_name: &str,
        rpc: bool,
        rpc_ns: &str,
        prefix_ns: &mut Vec<(String, String)>,
    ) -> String {
        let mut out = String::new();
        let parts: Vec<&'a Element> = msg.children_named("part").collect();
        if rpc {
            let p = prefix_for(rpc_ns, prefix_ns);
            out.push_str(&format!("    <{p}:{op_name}>\n"));
            for part in &parts {
                let name = part.attr("name").unwrap_or("part");
                out.push_str(&format!("      <{name}>?</{name}>\n"));
            }
            out.push_str(&format!("    </{p}:{op_name}>\n"));
            return out;
        }
        for part in parts {
            if let Some(el) = part.attr("element") {
                let (ns, local) = part.resolve_qname(el);
                match self.find_top("element", &ns, &local) {
                    // Top-level elements are always qualified.
                    Some((decl, tns, form)) => self.render_element(decl, &tns, true, form, 2, &mut out, prefix_ns),
                    None => self.warn(format!("schema element `{local}` not found; body left empty")),
                }
            } else if part.attr("type").is_some() {
                let name = part.attr("name").unwrap_or("part");
                out.push_str(&format!("    <{name}>?</{name}>\n"));
            }
        }
        out
    }
}

fn prefix_for(ns: &str, prefixes: &mut Vec<(String, String)>) -> String {
    if let Some((p, _)) = prefixes.iter().find(|(_, u)| u == ns) {
        return p.clone();
    }
    let p = if prefixes.is_empty() { "tns".to_string() } else { format!("ns{}", prefixes.len() + 1) };
    prefixes.push((p.clone(), ns.to_string()));
    p
}

fn envelope(version: SoapVersion, prefixes: &[(String, String)], body: &str) -> String {
    let env_ns = match version {
        SoapVersion::V11 => SOAP11_ENV,
        SoapVersion::V12 => SOAP12_ENV,
    };
    let mut decl = format!("xmlns:soap=\"{env_ns}\"");
    for (p, u) in prefixes {
        decl.push_str(&format!(" xmlns:{p}=\"{u}\""));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<soap:Envelope {decl}>\n  <soap:Header/>\n  <soap:Body>\n{body}  </soap:Body>\n</soap:Envelope>"
    )
}

/// Imports a WSDL 1.1 document.
pub fn import_wsdl(text: &str) -> Result<(Collection, Vec<String>), ConvertError> {
    let root = xml::parse(text)?;
    if root.ns == WSDL2_NS || root.name == "description" {
        return Err(ConvertError::Format("WSDL 2.0 is not supported; only WSDL 1.1".into()));
    }
    if root.name != "definitions" {
        return Err(ConvertError::Format("not a WSDL document (missing <definitions>)".into()));
    }
    let mut schema_els = Vec::new();
    if let Some(types) = root.child("types") {
        schema_els.extend(types.children_named("schema"));
    }
    let schemas = schema_els
        .into_iter()
        .map(|s| Schema {
            target_ns: s.attr("targetNamespace").unwrap_or("").to_string(),
            qualified: s.attr("elementFormDefault") == Some("qualified"),
            el: s,
        })
        .collect();
    let mut w = Wsdl { root: &root, schemas, warnings: Vec::new(), expanding: Vec::new() };
    if root.child("types").map(|t| t.children.iter().any(|c| c.name == "import")).unwrap_or(false)
        || root.children.iter().any(|c| c.name == "import")
    {
        w.warn("WSDL/XSD imports are not followed; types from imported files are left empty".into());
    }
    let target_ns = root.attr("targetNamespace").unwrap_or("").to_string();

    // Port address per binding.
    let mut address: HashMap<String, String> = HashMap::new();
    let mut service_name = None;
    for svc in root.children_named("service") {
        service_name.get_or_insert_with(|| svc.attr("name").unwrap_or("").to_string());
        for port in svc.children_named("port") {
            if let (Some(b), Some(addr)) =
                (port.attr("binding"), port.child("address").and_then(|a| a.attr("location")))
            {
                let (_, local) = port.resolve_qname(b);
                address.entry(local).or_insert_with(|| addr.to_string());
            }
        }
    }

    let name = root
        .attr("name")
        .map(str::to_string)
        .or(service_name)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "SOAP service".into());
    let mut c = Collection::new(name);
    if let Some(doc) = root.child("documentation") {
        c.description = doc.text.trim().to_string();
    }
    let mut folders: Vec<Folder> = Vec::new();
    let mut needs_base = false;

    let bindings: Vec<&Element> = root.children_named("binding").collect();
    for binding in &bindings {
        let soap_binding = binding.children.iter().find(|c| c.name == "binding");
        let version = match soap_binding.map(|b| b.ns.as_str()) {
            Some(SOAP11_BINDING_NS) => SoapVersion::V11,
            Some(SOAP12_BINDING_NS) => SoapVersion::V12,
            _ => continue, // HTTP bindings etc.
        };
        let style = soap_binding.and_then(|b| b.attr("style")).unwrap_or("document").to_string();
        let bname = binding.attr("name").unwrap_or("Binding").to_string();
        let port_type = binding.attr("type").map(|t| binding.resolve_qname(t).1);
        let pt = port_type
            .as_ref()
            .and_then(|p| root.children_named("portType").find(|x| x.attr("name") == Some(p.as_str())));
        let url = match address.get(&bname) {
            Some(a) => a.clone(),
            None => {
                needs_base = true;
                "{{baseUrl}}".into()
            }
        };
        let mut folder =
            Folder::new(format!("{bname} ({})", if version == SoapVersion::V11 { "SOAP 1.1" } else { "SOAP 1.2" }));
        for op in binding.children_named("operation") {
            let op_name = op.attr("name").unwrap_or("Operation").to_string();
            let action = op.child("operation").and_then(|o| o.attr("soapAction")).unwrap_or("").to_string();
            let op_style = op.child("operation").and_then(|o| o.attr("style")).unwrap_or(&style).to_string();
            let rpc = op_style == "rpc";
            let rpc_ns = op
                .child("input")
                .and_then(|i| i.child("body"))
                .and_then(|b| b.attr("namespace"))
                .unwrap_or(&target_ns)
                .to_string();
            let abstract_op =
                pt.and_then(|p| p.children_named("operation").find(|o| o.attr("name") == Some(op_name.as_str())));
            let mut prefixes = Vec::new();
            let mut body = String::new();
            let mut response_body = String::new();
            let mut out_prefixes = Vec::new();
            match abstract_op {
                Some(aop) => {
                    if let Some(input) = aop.child("input").and_then(|i| i.attr("message").map(|m| (i, m)))
                        && let Some(msg) = w.message(input.0, input.1)
                    {
                        body = w.message_body(msg, &op_name, rpc, &rpc_ns, &mut prefixes);
                    }
                    if let Some(output) = aop.child("output").and_then(|o| o.attr("message").map(|m| (o, m)))
                        && let Some(msg) = w.message(output.0, output.1)
                    {
                        response_body =
                            w.message_body(msg, &format!("{op_name}Response"), rpc, &rpc_ns, &mut out_prefixes);
                    }
                }
                None => w.warn(format!("operation `{op_name}` has no portType definition")),
            }
            let mut req = HttpRequest::new("POST", url.clone());
            match version {
                SoapVersion::V11 => {
                    req.headers.push(KeyValue::new("Content-Type", "text/xml; charset=utf-8"));
                    req.headers.push(KeyValue::new("SOAPAction", format!("\"{action}\"")));
                }
                SoapVersion::V12 => {
                    let ct = if action.is_empty() {
                        "application/soap+xml; charset=utf-8".to_string()
                    } else {
                        format!("application/soap+xml; charset=utf-8; action=\"{action}\"")
                    };
                    req.headers.push(KeyValue::new("Content-Type", ct));
                }
            }
            req.body = Body::raw_text(envelope(version, &prefixes, &body), RawLanguage::Xml);
            let mut item = RequestItem::new(op_name.clone(), req.clone());
            if let Some(doc) = abstract_op.and_then(|a| a.child("documentation")) {
                item.description = doc.text.trim().to_string();
            }
            if !response_body.is_empty() {
                let ct = match version {
                    SoapVersion::V11 => "text/xml; charset=utf-8",
                    SoapVersion::V12 => "application/soap+xml; charset=utf-8",
                };
                item.examples.push(Example {
                    name: format!("{op_name} response"),
                    request: Some(req),
                    status: 200,
                    status_text: "OK".into(),
                    headers: vec![KeyValue::new("Content-Type", ct)],
                    body: envelope(version, &out_prefixes, &response_body),
                    language: Some("xml".into()),
                    ..Default::default()
                });
            }
            folder.items.push(Item::Request(item));
        }
        if !folder.items.is_empty() {
            folders.push(folder);
        }
    }
    if folders.is_empty() {
        w.warn("no SOAP 1.1/1.2 bindings found".into());
    }
    if folders.len() == 1 {
        c.items = std::mem::take(&mut folders[0].items);
    } else {
        c.items = folders.into_iter().map(Item::Folder).collect();
    }
    if needs_base {
        c.variables.push(Variable::new("baseUrl", ""));
        w.warn("some bindings have no service address; set the baseUrl variable".into());
    }
    let warnings = w.warnings;
    Ok((c, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_wsdl() {
        let (c, warnings) = import_wsdl(include_str!("../testdata/users.wsdl")).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(c.name, "UserService");
        assert_eq!(c.items.len(), 2, "one folder per SOAP version");
        let v11 = c.items[0].as_folder().unwrap();
        assert_eq!(v11.name, "UserBinding (SOAP 1.1)");
        let get = v11.items[0].as_request().unwrap();
        let h = get.http().unwrap();
        assert_eq!(h.method, "POST");
        assert_eq!(h.url, "http://example.com/soap/users");
        assert_eq!(h.header("SOAPAction"), Some("\"http://example.com/users/GetUser\""));
        assert_eq!(h.header("Content-Type"), Some("text/xml; charset=utf-8"));
        let body = &h.body.raw;
        assert!(body.contains("<soap:Envelope xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\" xmlns:tns=\"http://example.com/users\">"), "{body}");
        assert!(body.contains("<tns:GetUser>"), "{body}");
        assert!(body.contains("<tns:id>?</tns:id>"), "{body}");
        assert!(body.contains("<!--Optional:-->"), "{body}");
        assert!(body.contains("<tns:street>?</tns:street>"), "named complex types are expanded: {body}");
        assert_eq!(get.examples.len(), 1);
        assert!(get.examples[0].body.contains("<tns:GetUserResponse>"));

        let v12 = c.items[1].as_folder().unwrap();
        let h = v12.items[0].as_request().unwrap().http().unwrap();
        assert_eq!(
            h.header("Content-Type"),
            Some("application/soap+xml; charset=utf-8; action=\"http://example.com/users/GetUser\"")
        );
        assert!(h.body.raw.contains("http://www.w3.org/2003/05/soap-envelope"));
    }

    /// A WSDL whose single operation uses `element` from a schema with `types`.
    fn wsdl_with(types: &str, element: &str) -> String {
        format!(
            r#"<definitions xmlns="http://schemas.xmlsoap.org/wsdl/" xmlns:soap="http://schemas.xmlsoap.org/wsdl/soap/"
                 xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:tns="urn:t" targetNamespace="urn:t">
              <types><xs:schema targetNamespace="urn:t">{types}</xs:schema></types>
              <message name="In"><part name="p" element="tns:{element}"/></message>
              <portType name="PT"><operation name="Op"><input message="tns:In"/></operation></portType>
              <binding name="B" type="tns:PT"><soap:binding style="document"/>
                <operation name="Op"><soap:operation soapAction="a"/></operation></binding>
              <service name="S"><port name="P" binding="tns:B"><soap:address location="http://h/s"/></port></service>
            </definitions>"#
        )
    }

    #[test]
    fn self_referencing_schemas_do_not_overflow_the_stack() {
        for (types, element) in [
            (r#"<xs:element name="A" ref="tns:A"/>"#, "A"),
            (r#"<xs:element name="A" ref="tns:B"/><xs:element name="B" ref="tns:A"/>"#, "A"),
            (
                r#"<xs:complexType name="T"><xs:complexContent><xs:extension base="tns:T">
                     <xs:sequence><xs:element name="x" type="xs:string"/></xs:sequence>
                   </xs:extension></xs:complexContent></xs:complexType>
                   <xs:element name="E" type="tns:T"/>"#,
                "E",
            ),
        ] {
            let (c, _) = import_wsdl(&wsdl_with(types, element)).unwrap();
            assert_eq!(c.request_count(), 1);
        }
    }

    #[test]
    fn deeply_nested_xml_is_rejected_without_overflow() {
        let depth = 100_000;
        let text = format!(
            "<definitions xmlns=\"http://schemas.xmlsoap.org/wsdl/\">{}{}</definitions>",
            "<a>".repeat(depth),
            "</a>".repeat(depth)
        );
        assert!(import_wsdl(&text).is_err());
        assert!(crate::import_auto(&text).is_err());
    }

    #[test]
    fn rejects_other_xml() {
        assert!(import_wsdl("<root/>").is_err());
        assert!(import_wsdl("<description xmlns=\"http://www.w3.org/ns/wsdl\"/>").is_err());
    }
}
