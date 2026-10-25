//! A small namespace-aware XML tree on top of quick-xml, enough for WSDL.

use std::collections::HashMap;
use std::rc::Rc;

use quick_xml::events::{BytesStart, Event};

use crate::ConvertError;

pub const WSDL_NS: &str = "http://schemas.xmlsoap.org/wsdl/";
pub const WSDL2_NS: &str = "http://www.w3.org/ns/wsdl";

/// Deeper documents are rejected: the tree is dropped (and walked) recursively.
const MAX_NESTING: usize = 512;

#[derive(Debug, Clone)]
pub struct Element {
    /// Local name.
    pub name: String,
    /// Namespace URI of the element.
    pub ns: String,
    /// Attributes by their qualified name as written.
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    pub text: String,
    /// Namespace prefixes in scope (`""` is the default namespace).
    pub scope: Rc<HashMap<String, String>>,
}

impl Element {
    /// Attribute by local name (ignores any prefix).
    pub fn attr(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| {
                k == local || k.rsplit_once(':').map(|(_, l)| l == local).unwrap_or(false) && !k.starts_with("xmlns")
            })
            .map(|(_, v)| v.as_str())
    }

    pub fn children_named<'a>(&'a self, local: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == local)
    }

    pub fn child(&self, local: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == local)
    }

    /// Resolves a QName attribute value (`tns:Foo`) to (namespace, local).
    pub fn resolve_qname(&self, qname: &str) -> (String, String) {
        match qname.split_once(':') {
            Some((p, l)) => (self.scope.get(p).cloned().unwrap_or_default(), l.to_string()),
            None => (self.scope.get("").cloned().unwrap_or_default(), qname.to_string()),
        }
    }
}

fn start_element(e: &BytesStart<'_>, parent_scope: &Rc<HashMap<String, String>>) -> Result<Element, ConvertError> {
    let qname = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let (prefix, name) = match qname.split_once(':') {
        Some((p, l)) => (p.to_string(), l.to_string()),
        None => (String::new(), qname.clone()),
    };
    let mut attrs = Vec::new();
    let mut decls = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|err| ConvertError::Xml(err.to_string()))?;
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let value = a.unescape_value().map_err(|err| ConvertError::Xml(err.to_string()))?.into_owned();
        if key == "xmlns" {
            decls.push((String::new(), value.clone()));
        } else if let Some(p) = key.strip_prefix("xmlns:") {
            decls.push((p.to_string(), value.clone()));
        }
        attrs.push((key, value));
    }
    let scope = if decls.is_empty() {
        parent_scope.clone()
    } else {
        let mut s = (**parent_scope).clone();
        s.extend(decls);
        Rc::new(s)
    };
    let ns = scope.get(&prefix).cloned().unwrap_or_default();
    Ok(Element { name, ns, attrs, children: Vec::new(), text: String::new(), scope })
}

/// Parses a document and returns its root element.
pub fn parse(text: &str) -> Result<Element, ConvertError> {
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<Element> = Vec::new();
    let root_scope = Rc::new(HashMap::new());
    let mut root: Option<Element> = None;
    loop {
        let ev =
            reader.read_event().map_err(|e| ConvertError::Xml(format!("{e} at byte {}", reader.buffer_position())))?;
        match ev {
            Event::Start(e) => {
                if stack.len() >= MAX_NESTING {
                    return Err(ConvertError::Xml(format!("elements nested deeper than {MAX_NESTING} levels")));
                }
                let scope = stack.last().map(|p| p.scope.clone()).unwrap_or_else(|| root_scope.clone());
                stack.push(start_element(&e, &scope)?);
            }
            Event::Empty(e) => {
                let scope = stack.last().map(|p| p.scope.clone()).unwrap_or_else(|| root_scope.clone());
                let el = start_element(&e, &scope)?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None => {
                        root = Some(el);
                    }
                }
            }
            Event::End(_) => {
                let el = stack.pop().ok_or_else(|| ConvertError::Xml("unbalanced end tag".into()))?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None => root = Some(el),
                }
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    let raw = t.decode().map_err(|e| ConvertError::Xml(e.to_string()))?;
                    let un =
                        quick_xml::escape::unescape(&raw).map(|c| c.into_owned()).unwrap_or_else(|_| raw.into_owned());
                    top.text.push_str(&un);
                }
            }
            Event::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Event::GeneralRef(r) => {
                if let Some(top) = stack.last_mut() {
                    let name = String::from_utf8_lossy(&r).into_owned();
                    let ch = match name.as_str() {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        _ => r.resolve_char_ref().ok().flatten(),
                    };
                    if let Some(c) = ch {
                        top.text.push(c);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(ConvertError::Xml("unexpected end of document".into()));
    }
    root.ok_or_else(|| ConvertError::Xml("empty document".into()))
}

/// Cheap check used by format detection.
pub fn looks_like_wsdl(text: &str) -> bool {
    let head: String = text.chars().take(4096).collect();
    (head.contains("definitions") && head.contains(WSDL_NS)) || head.contains(WSDL2_NS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_and_text() {
        let doc = parse(
            r#"<?xml version="1.0"?><a:root xmlns:a="urn:a" xmlns="urn:d"><child x="1">t &amp; <![CDATA[c]]></child><a:e/></a:root>"#,
        )
        .unwrap();
        assert_eq!(doc.name, "root");
        assert_eq!(doc.ns, "urn:a");
        let child = doc.child("child").unwrap();
        assert_eq!(child.ns, "urn:d");
        assert_eq!(child.attr("x"), Some("1"));
        assert_eq!(child.text, "t&c");
        assert_eq!(doc.children[1].ns, "urn:a");
        assert_eq!(doc.resolve_qname("a:Foo"), ("urn:a".to_string(), "Foo".to_string()));
        assert!(parse("<a><b></a>").is_err());
    }
}
