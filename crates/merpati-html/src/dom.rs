//! A small arena DOM built by html5ever's tree builder.

use std::borrow::Cow;
use std::cell::{Ref, RefCell};

use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{Attribute, QualName, local_name, ns};

pub(crate) type NodeId = usize;

/// Deepest node depth kept (the document is depth 0). Every pass over the
/// tree (styles, layout, painting) recurses once per level, so unclosed
/// tags repeated in a loop would overflow the UI thread's stack. Like
/// Chrome's parser (which stops at 512), deeper content is kept but
/// flattened into the last allowed level; the limit is lower here because
/// each level costs far more stack while painting.
const MAX_DEPTH: usize = 256;

#[derive(Debug)]
pub(crate) enum NodeData {
    Document,
    Element { attrs: Vec<(String, String)>, template_contents: Option<NodeId> },
    Text(String),
    Other,
}

#[derive(Debug)]
pub(crate) struct Node {
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    /// Element name (empty for other nodes).
    pub name: QualName,
    pub data: NodeData,
}

/// Parsed document. Node 0 is the document.
#[derive(Debug)]
pub(crate) struct Dom {
    pub nodes: Vec<Node>,
}

impl Dom {
    pub fn parse(html: &str) -> Dom {
        let sink = Sink { nodes: RefCell::new(vec![new_node(NodeData::Document, empty_name())]) };
        let mut dom: Dom = html5ever::parse_document(sink, Default::default()).one(html);
        dom.limit_depth();
        dom
    }

    /// Moves everything below [`MAX_DEPTH`] up to that depth, in document
    /// order (the nodes there have no children).
    fn limit_depth(&mut self) {
        let mut stack = vec![(0, 0)];
        while let Some((id, depth)) = stack.pop() {
            if depth + 1 < MAX_DEPTH {
                stack.extend(self.nodes[id].children.iter().rev().map(|c| (*c, depth + 1)));
                continue;
            }
            // `id`'s children are at the limit: their subtrees become siblings.
            let mut flat = Vec::new();
            let mut todo: Vec<NodeId> = std::mem::take(&mut self.nodes[id].children);
            todo.reverse();
            while let Some(n) = todo.pop() {
                flat.push(n);
                self.nodes[n].parent = Some(id);
                todo.extend(std::mem::take(&mut self.nodes[n].children).into_iter().rev());
            }
            self.nodes[id].children = flat;
        }
    }

    pub fn tag(&self, id: NodeId) -> Option<&str> {
        match self.nodes[id].data {
            NodeData::Element { .. } => Some(&self.nodes[id].name.local),
            _ => None,
        }
    }

    pub fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Element { attrs, .. } => {
                attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
            }
            _ => None,
        }
    }

    pub fn is_element(&self, id: NodeId) -> bool {
        matches!(self.nodes[id].data, NodeData::Element { .. })
    }

    /// Element children of `id`.
    pub fn element_children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.nodes[id].children.iter().copied().filter(|c| self.is_element(*c))
    }

    /// Concatenated text of all descendants.
    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.collect_text(id, &mut out);
        out
    }

    fn collect_text(&self, id: NodeId, out: &mut String) {
        match &self.nodes[id].data {
            NodeData::Text(t) => out.push_str(t),
            _ => {
                for c in &self.nodes[id].children {
                    self.collect_text(*c, out);
                }
            }
        }
    }

    /// First element with this tag name in document order.
    pub fn find(&self, tag: &str) -> Option<NodeId> {
        (0..self.nodes.len()).find(|i| self.tag(*i) == Some(tag) && self.is_attached(*i))
    }

    /// Every element with this tag name in document order.
    pub fn find_all(&self, tag: &str) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.walk(0, &mut |id| {
            if self.tag(id) == Some(tag) {
                out.push(id);
            }
        });
        out
    }

    fn walk(&self, id: NodeId, f: &mut dyn FnMut(NodeId)) {
        f(id);
        for c in &self.nodes[id].children {
            self.walk(*c, f);
        }
    }

    fn is_attached(&self, mut id: NodeId) -> bool {
        while let Some(p) = self.nodes[id].parent {
            id = p;
        }
        id == 0
    }
}

fn empty_name() -> QualName {
    QualName::new(None, ns!(), local_name!(""))
}

fn new_node(data: NodeData, name: QualName) -> Node {
    Node { parent: None, children: Vec::new(), name, data }
}

struct Sink {
    nodes: RefCell<Vec<Node>>,
}

impl Sink {
    fn push(&self, node: Node) -> NodeId {
        let mut nodes = self.nodes.borrow_mut();
        nodes.push(node);
        nodes.len() - 1
    }

    fn detach(nodes: &mut [Node], id: NodeId) {
        if let Some(p) = nodes[id].parent.take() {
            nodes[p].children.retain(|c| *c != id);
        }
    }

    /// Appends text to `parent`, merging with a trailing text node.
    fn append_text(nodes: &mut Vec<Node>, parent: NodeId, text: &str) {
        if let Some(&last) = nodes[parent].children.last()
            && let NodeData::Text(t) = &mut nodes[last].data
        {
            t.push_str(text);
            return;
        }
        let mut n = new_node(NodeData::Text(text.to_string()), empty_name());
        n.parent = Some(parent);
        nodes.push(n);
        let id = nodes.len() - 1;
        nodes[parent].children.push(id);
    }
}

impl TreeSink for Sink {
    type Handle = NodeId;
    type Output = Dom;
    type ElemName<'a> = Ref<'a, QualName>;

    fn finish(self) -> Dom {
        Dom { nodes: self.nodes.into_inner() }
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> NodeId {
        0
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> Ref<'a, QualName> {
        Ref::map(self.nodes.borrow(), |n| &n[*target].name)
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
        let attrs = attrs.into_iter().map(|a| (a.name.local.to_string(), a.value.to_string())).collect();
        let template_contents = flags.template.then(|| self.push(new_node(NodeData::Document, empty_name())));
        self.push(new_node(NodeData::Element { attrs, template_contents }, name))
    }

    fn create_comment(&self, _text: StrTendril) -> NodeId {
        self.push(new_node(NodeData::Other, empty_name()))
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
        self.push(new_node(NodeData::Other, empty_name()))
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        let mut nodes = self.nodes.borrow_mut();
        match child {
            NodeOrText::AppendNode(c) => {
                Self::detach(&mut nodes, c);
                nodes[c].parent = Some(*parent);
                nodes[*parent].children.push(c);
            }
            NodeOrText::AppendText(t) => Self::append_text(&mut nodes, *parent, &t),
        }
    }

    fn append_based_on_parent_node(&self, element: &NodeId, prev_element: &NodeId, child: NodeOrText<NodeId>) {
        let has_parent = self.nodes.borrow()[*element].parent.is_some();
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(&self, _name: StrTendril, _public_id: StrTendril, _system_id: StrTendril) {}

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        match self.nodes.borrow()[*target].data {
            NodeData::Element { template_contents: Some(t), .. } => t,
            _ => *target,
        }
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &NodeId, child: NodeOrText<NodeId>) {
        let mut nodes = self.nodes.borrow_mut();
        let Some(parent) = nodes[*sibling].parent else { return };
        let Some(pos) = nodes[parent].children.iter().position(|c| c == sibling) else { return };
        match child {
            NodeOrText::AppendNode(c) => {
                Self::detach(&mut nodes, c);
                let pos = nodes[parent].children.iter().position(|x| x == sibling).unwrap_or(pos);
                nodes[c].parent = Some(parent);
                nodes[parent].children.insert(pos, c);
            }
            NodeOrText::AppendText(t) => {
                if pos > 0 {
                    let prev = nodes[parent].children[pos - 1];
                    if let NodeData::Text(existing) = &mut nodes[prev].data {
                        existing.push_str(&t);
                        return;
                    }
                }
                let mut n = new_node(NodeData::Text(t.to_string()), empty_name());
                n.parent = Some(parent);
                nodes.push(n);
                let id = nodes.len() - 1;
                nodes[parent].children.insert(pos, id);
            }
        }
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        let mut nodes = self.nodes.borrow_mut();
        if let NodeData::Element { attrs: existing, .. } = &mut nodes[*target].data {
            for a in attrs {
                let name = a.name.local.to_string();
                if !existing.iter().any(|(k, _)| *k == name) {
                    existing.push((name, a.value.to_string()));
                }
            }
        }
    }

    fn remove_from_parent(&self, target: &NodeId) {
        Self::detach(&mut self.nodes.borrow_mut(), *target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut nodes = self.nodes.borrow_mut();
        let children = std::mem::take(&mut nodes[*node].children);
        for c in &children {
            nodes[*c].parent = Some(*new_parent);
        }
        nodes[*new_parent].children.extend(children);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_tree_with_implied_tags() {
        let dom = Dom::parse("<p>one<p>two<table><tr><td>a<td>b</table>");
        let body = dom.find("body").unwrap();
        let tags: Vec<&str> = dom.element_children(body).filter_map(|c| dom.tag(c)).collect();
        assert_eq!(tags, ["p", "p"]);
        let tbody = dom.find("tbody").expect("implied tbody");
        let tr = dom.element_children(tbody).next().unwrap();
        assert_eq!(dom.element_children(tr).count(), 2);
        assert_eq!(dom.text_content(tr), "ab");
    }

    #[test]
    fn attributes_and_text_merge() {
        let dom = Dom::parse("<div id=x class='a b'>a &amp; b<!-- c -->c</div>");
        let div = dom.find("div").unwrap();
        assert_eq!(dom.attr(div, "id"), Some("x"));
        assert_eq!(dom.attr(div, "CLASS"), Some("a b"));
        assert_eq!(dom.text_content(div), "a & bc");
    }

    #[test]
    fn deep_nesting_is_flattened_in_document_order() {
        let dom = Dom::parse(&format!("{}1<b>2<i>3</i></b>4", "<div>".repeat(MAX_DEPTH + 50)));
        let mut deepest = 0;
        let mut stack = vec![(0, 0)];
        while let Some((id, depth)) = stack.pop() {
            deepest = deepest.max(depth);
            stack.extend(dom.nodes[id].children.iter().map(|c| (*c, depth + 1)));
        }
        assert_eq!(deepest, MAX_DEPTH);
        assert_eq!(dom.text_content(dom.find("body").unwrap()), "1234");
        assert_eq!(dom.find_all("div").len(), MAX_DEPTH + 50);
    }

    #[test]
    fn foster_parenting_keeps_text() {
        let dom = Dom::parse("<table>stray<tr><td>x</td></tr></table>");
        let body = dom.find("body").unwrap();
        assert_eq!(dom.text_content(body), "strayx");
    }
}
