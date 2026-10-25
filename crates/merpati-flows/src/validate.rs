//! Static checks of a flow, reported per block for the canvas.

use std::collections::{HashMap, HashSet, VecDeque};

use indexmap::IndexMap;
use merpati_core::model::{Collection, RequestKind};
use serde::Serialize;
use serde_json::Value;

use crate::model::*;
use crate::template::{Segment, collect_refs, flow_root, parse_path};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The flow cannot run correctly; the CLI refuses to run it.
    Error,
    /// Suspicious but runnable.
    Warning,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Block id the problem is about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block: Option<String>,
    /// Index into `Flow::edges`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge: Option<usize>,
    /// Port or config field concerned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Stable machine-readable code (`cycle`, `unknown_block_ref`, ...).
    pub code: &'static str,
    pub message: String,
}

impl Diagnostic {
    fn new(severity: Severity, code: &'static str, message: impl Into<String>) -> Self {
        Diagnostic { severity, block: None, edge: None, field: None, code, message: message.into() }
    }

    fn block(mut self, id: &str) -> Self {
        self.block = Some(id.to_string());
        self
    }

    fn edge(mut self, i: usize) -> Self {
        self.edge = Some(i);
        self
    }

    fn field(mut self, f: &str) -> Self {
        self.field = Some(f.to_string());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

pub fn has_errors(diags: &[Diagnostic]) -> bool {
    diags.iter().any(Diagnostic::is_error)
}

/// Diagnostics grouped by block id (flow-level ones under `""`).
pub fn by_block(diags: &[Diagnostic]) -> IndexMap<String, Vec<&Diagnostic>> {
    let mut out: IndexMap<String, Vec<&Diagnostic>> = IndexMap::new();
    for d in diags {
        out.entry(d.block.clone().unwrap_or_default()).or_default().push(d);
    }
    out
}

fn err(code: &'static str, msg: impl Into<String>) -> Diagnostic {
    Diagnostic::new(Severity::Error, code, msg)
}

fn warn(code: &'static str, msg: impl Into<String>) -> Diagnostic {
    Diagnostic::new(Severity::Warning, code, msg)
}

/// Edges whose blocks and ports exist.
fn valid_edges(flow: &Flow) -> Vec<(usize, &Edge)> {
    flow.edges
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            let (Some(a), Some(b)) = (flow.block(&e.from_block), flow.block(&e.to_block)) else { return false };
            a.kind().output_ports().contains(&e.from_port.as_str())
                && b.kind().input_ports().contains(&e.to_port.as_str())
        })
        .collect()
}

/// Blocks reachable from `start` following `edges` (excluding `start` unless
/// it is reached again). `stop_at` blocks are reached but not traversed.
fn reachable(edges: &[(usize, &Edge)], start: &[(&str, Option<&str>)], stop_at: Option<&str>) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> =
        start.iter().map(|(b, p)| (b.to_string(), p.map(str::to_string))).collect();
    while let Some((b, port)) = queue.pop_front() {
        for (_, e) in
            edges.iter().filter(|(_, e)| e.from_block == b && port.as_deref().is_none_or(|p| e.from_port == p))
        {
            if seen.insert(e.to_block.clone()) && stop_at != Some(e.to_block.as_str()) {
                queue.push_back((e.to_block.clone(), None));
            }
        }
    }
    seen
}

/// Loop id → blocks in its body (reachable from its `item` port).
pub(crate) fn loop_bodies(flow: &Flow) -> HashMap<String, HashSet<String>> {
    let edges = valid_edges(flow);
    flow.blocks
        .iter()
        .filter(|b| b.kind() == BlockKind::Loop)
        .map(|b| {
            let mut body = reachable(&edges, &[(&b.id, Some(PORT_ITEM))], Some(&b.id));
            body.remove(&b.id);
            (b.id.clone(), body)
        })
        .collect()
}

/// Blocks on cycles that do not close through a Loop (an edge from a loop's
/// body back into the loop, or into any `break` port, is fine).
fn cycle_blocks(flow: &Flow, bodies: &HashMap<String, HashSet<String>>) -> Vec<String> {
    let edges: Vec<(usize, &Edge)> = valid_edges(flow)
        .into_iter()
        .filter(|(_, e)| {
            if e.to_port == PORT_BREAK {
                return false;
            }
            !bodies.get(&e.to_block).map(|body| body.contains(&e.from_block)).unwrap_or(false)
        })
        .collect();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for (_, e) in &edges {
        adj.entry(e.from_block.as_str()).or_default().push(e.to_block.as_str());
    }
    // Tarjan's strongly connected components.
    struct T<'a> {
        adj: &'a HashMap<&'a str, Vec<&'a str>>,
        index: HashMap<&'a str, usize>,
        low: HashMap<&'a str, usize>,
        stack: Vec<&'a str>,
        on: HashSet<&'a str>,
        next: usize,
        out: Vec<Vec<&'a str>>,
    }
    fn strong<'a>(t: &mut T<'a>, v: &'a str) {
        t.index.insert(v, t.next);
        t.low.insert(v, t.next);
        t.next += 1;
        t.stack.push(v);
        t.on.insert(v);
        let adj = t.adj;
        for &w in adj.get(v).map(Vec::as_slice).unwrap_or(&[]) {
            if !t.index.contains_key(w) {
                strong(t, w);
                let lw = t.low[w];
                let lv = t.low.get_mut(v).expect("visited");
                *lv = (*lv).min(lw);
            } else if t.on.contains(w) {
                let iw = t.index[w];
                let lv = t.low.get_mut(v).expect("visited");
                *lv = (*lv).min(iw);
            }
        }
        if t.low[v] == t.index[v] {
            let mut comp = Vec::new();
            while let Some(w) = t.stack.pop() {
                t.on.remove(w);
                comp.push(w);
                if w == v {
                    break;
                }
            }
            t.out.push(comp);
        }
    }
    let mut t = T {
        adj: &adj,
        index: HashMap::new(),
        low: HashMap::new(),
        stack: Vec::new(),
        on: HashSet::new(),
        next: 0,
        out: Vec::new(),
    };
    for b in &flow.blocks {
        if !t.index.contains_key(b.id.as_str()) {
            strong(&mut t, b.id.as_str());
        }
    }
    let mut out = Vec::new();
    for comp in t.out {
        let self_loop = comp.len() == 1 && adj.get(comp[0]).map(|v| v.contains(&comp[0])).unwrap_or(false);
        if comp.len() > 1 || self_loop {
            out.extend(comp.into_iter().map(str::to_string));
        }
    }
    let order: HashMap<&str, usize> = flow.blocks.iter().enumerate().map(|(i, b)| (b.id.as_str(), i)).collect();
    out.sort_by_key(|id| order.get(id.as_str()).copied().unwrap_or(usize::MAX));
    out.dedup();
    out
}

/// Joins reached outside of parallel branches, with the edge that reaches
/// them. Follows how runs nest parallel groups: an edge from a Parallel's
/// `branches` port goes one level in, a Join reached inside a group fires
/// one level out; a Join reached at the top level passes arrivals through.
fn joins_outside_parallel(flow: &Flow, edges: &[(usize, &Edge)], start: &str) -> Vec<(String, usize)> {
    const MAX_LEVEL: usize = 16;
    let mut out: Vec<(String, usize)> = Vec::new();
    let mut seen: HashSet<(String, usize)> = HashSet::from([(start.to_string(), 0)]);
    let mut queue = VecDeque::from([(start.to_string(), 0usize)]);
    while let Some((b, level)) = queue.pop_front() {
        let parallel = flow.block(&b).is_some_and(|x| x.kind() == BlockKind::Parallel);
        for (i, e) in edges.iter().filter(|(_, e)| e.from_block == b) {
            let Some(to) = flow.block(&e.to_block) else { continue };
            let mut next = if parallel && e.from_port == PORT_BRANCHES { level + 1 } else { level };
            if to.kind() == BlockKind::Join {
                if next == 0 {
                    if !out.iter().any(|(j, _)| *j == to.id) {
                        out.push((to.id.clone(), *i));
                    }
                } else {
                    next -= 1;
                }
            }
            let next = next.min(MAX_LEVEL);
            if seen.insert((to.id.clone(), next)) {
                queue.push_back((to.id.clone(), next));
            }
        }
    }
    out
}

fn find_item<'a>(collections: &'a [Collection], r: &ItemRef) -> Option<&'a merpati_core::model::RequestItem> {
    if !r.collection_id.is_empty() {
        return collections.iter().find(|c| c.id == r.collection_id)?.find_request(&r.request_id);
    }
    collections.iter().find_map(|c| c.find_request(&r.request_id))
}

/// Template-bearing values of a block (field name, JSON value to scan).
fn template_values(block: &Block, collections: Option<&[Collection]>) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> =
        block.config.templates().into_iter().map(|(f, t)| (f.to_string(), Value::String(t.to_string()))).collect();
    if let BlockConfig::Request(c) = &block.config {
        out.retain(|(f, _)| f != "url" && f != "headers" && f != "body");
        match &c.source {
            RequestSource::Inline { request, .. } => {
                out.push(("request".into(), serde_json::to_value(request).unwrap_or_default()));
            }
            RequestSource::Saved(r) => {
                if let Some(item) = collections.and_then(|cs| find_item(cs, r)) {
                    out.push(("request".into(), serde_json::to_value(item).unwrap_or_default()));
                }
            }
        }
    }
    out
}

/// Checks a flow. Pass the workspace collections to also check references
/// to saved requests.
pub fn validate(flow: &Flow, collections: Option<&[Collection]>) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    // Blocks.
    let starts: Vec<&Block> = flow.blocks.iter().filter(|b| b.kind() == BlockKind::Start).collect();
    match starts.len() {
        0 => out.push(err("no_start", "The flow has no Start block")),
        1 => {}
        _ => {
            for b in &starts[1..] {
                out.push(err("multiple_starts", "Only one Start block is allowed").block(&b.id));
            }
        }
    }
    let mut ids = HashSet::new();
    for b in &flow.blocks {
        if b.id.trim().is_empty() {
            out.push(err("empty_id", format!("Block \"{}\" has an empty id", b.display_name())));
        } else if !ids.insert(b.id.as_str()) {
            out.push(err("duplicate_id", format!("Another block already has the id \"{}\"", b.id)).block(&b.id));
        }
    }
    if flow.settings.max_concurrent == 1
        && flow
            .blocks
            .iter()
            .any(|b| b.kind() == BlockKind::Parallel && flow.edges_from(&b.id, PORT_BRANCHES).nth(1).is_some())
    {
        out.push(
            warn(
                "sequential_branches",
                "max_concurrent is 1, so the branches of Parallel blocks run one after another",
            )
            .field("max_concurrent"),
        );
    }

    // Edges.
    let mut seen_edges = HashSet::new();
    for (i, e) in flow.edges.iter().enumerate() {
        let from = flow.block(&e.from_block);
        let to = flow.block(&e.to_block);
        if from.is_none() {
            out.push(err("unknown_block", format!("Connection from unknown block \"{}\"", e.from_block)).edge(i));
        }
        if to.is_none() {
            let d = err("unknown_block", format!("Connection to unknown block \"{}\"", e.to_block)).edge(i);
            out.push(if from.is_some() { d.block(&e.from_block) } else { d });
        }
        if let Some(a) = from
            && !a.kind().output_ports().contains(&e.from_port.as_str())
        {
            out.push(
                err(
                    "unknown_port",
                    format!(
                        "{} blocks have no \"{}\" output (available: {})",
                        a.kind().label(),
                        e.from_port,
                        a.kind().output_ports().join(", ")
                    ),
                )
                .block(&a.id)
                .edge(i)
                .field(&e.from_port),
            );
        }
        if let Some(b) = to
            && !b.kind().input_ports().contains(&e.to_port.as_str())
        {
            let msg = if b.kind().input_ports().is_empty() {
                format!("{} blocks have no inputs", b.kind().label())
            } else {
                format!(
                    "{} blocks have no \"{}\" input (available: {})",
                    b.kind().label(),
                    e.to_port,
                    b.kind().input_ports().join(", ")
                )
            };
            out.push(err("unknown_port", msg).block(&b.id).edge(i).field(&e.to_port));
        }
        if e.from_block == e.to_block && from.is_some() {
            out.push(err("self_edge", "A block cannot be connected to itself").block(&e.from_block).edge(i));
        }
        if !seen_edges.insert(e) {
            out.push(warn("duplicate_edge", "Duplicate connection").block(&e.from_block).edge(i));
        }
    }

    let edges = valid_edges(flow);
    let bodies = loop_bodies(flow);

    // Reachability.
    if let Some(start) = starts.first() {
        let reached = reachable(&edges, &[(&start.id, None)], None);
        for b in flow.blocks.iter().filter(|b| b.kind() != BlockKind::Start) {
            let lone_join = b.kind() == BlockKind::Join && flow.edges_to(&b.id).next().is_none();
            if !reached.contains(&b.id) && !lone_join {
                let msg = if flow.edges_to(&b.id).next().is_none() {
                    "Not connected: nothing leads to this block, so it never runs"
                } else {
                    "Not reachable from Start, so it never runs"
                };
                out.push(warn("unreachable", msg).block(&b.id).field(PORT_IN));
            }
        }
        if flow.edges_from(&start.id, PORT_NEXT).next().is_none() && flow.blocks.len() > 1 {
            out.push(warn("start_unconnected", "Start is not connected to anything").block(&start.id).field(PORT_NEXT));
        }
        for (join, edge) in joins_outside_parallel(flow, &edges, &start.id) {
            out.push(
                warn(
                    "join_outside_parallel",
                    "Reached outside the branches of a Parallel block: it does not wait and passes each arrival on",
                )
                .block(&join)
                .edge(edge)
                .field(PORT_IN),
            );
        }
    }

    // Cycles.
    for id in cycle_blocks(flow, &bodies) {
        out.push(
            err("cycle", "Part of a cycle that does not go through a Loop; use a Loop block to repeat blocks")
                .block(&id),
        );
    }

    // Upstream sets for reference checks.
    let preds = {
        let mut m: HashMap<&str, Vec<&str>> = HashMap::new();
        for (_, e) in &edges {
            m.entry(e.to_block.as_str()).or_default().push(e.from_block.as_str());
        }
        m
    };
    let upstream = |id: &str| -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([id.to_string()]);
        while let Some(b) = queue.pop_front() {
            for p in preds.get(b.as_str()).map(Vec::as_slice).unwrap_or(&[]) {
                if seen.insert(p.to_string()) {
                    queue.push_back(p.to_string());
                }
            }
        }
        seen
    };
    let declared_inputs: HashSet<&str> = flow.inputs.iter().map(|i| i.name.as_str()).collect();
    let mut output_names: HashMap<String, usize> = HashMap::new();

    for b in &flow.blocks {
        let id = b.id.as_str();
        let connected = |port: &str| flow.edges_from(id, port).next().is_some();
        match &b.config {
            BlockConfig::Start(_) => {}
            BlockConfig::Request(c) => {
                match &c.source {
                    RequestSource::Inline { request, collection_id, .. } => {
                        if request.url.trim().is_empty() {
                            out.push(err("missing_url", "Enter a URL").block(id).field("url"));
                        }
                        if let Some(cs) = collections
                            && !collection_id.is_empty()
                            && !cs.iter().any(|c| &c.id == collection_id)
                        {
                            out.push(
                                err("collection_not_found", format!("Collection {collection_id} not found"))
                                    .block(id)
                                    .field("collection_id"),
                            );
                        }
                    }
                    RequestSource::Saved(r) => {
                        if r.request_id.trim().is_empty() {
                            out.push(err("missing_request", "Choose a saved request").block(id).field("request_id"));
                        } else if let Some(cs) = collections {
                            match find_item(cs, r) {
                                None => out.push(
                                    err("request_not_found", format!("Saved request {} not found", r.request_id))
                                        .block(id)
                                        .field("request_id"),
                                ),
                                Some(item) if !matches!(item.request, RequestKind::Http(_)) => out.push(
                                    err(
                                        "not_http",
                                        format!(
                                            "\"{}\" is a {} request; Request blocks run HTTP requests",
                                            item.name,
                                            item.request.protocol_label()
                                        ),
                                    )
                                    .block(id)
                                    .field("request_id"),
                                ),
                                Some(_) => {}
                            }
                        }
                    }
                }
                if let Err(e) = crate::exec::parse_status_spec(&c.success_status) {
                    out.push(err("bad_status", e).block(id).field("success_status"));
                }
            }
            BlockConfig::Condition(c) => {
                match c.mode {
                    ConditionMode::Compare => {
                        if c.left.trim().is_empty() {
                            out.push(err("missing_left", "Enter the value to test").block(id).field("left"));
                        }
                        if c.op == CompareOp::Matches
                            && !c.right.contains("{{")
                            && let Err(e) = regex::Regex::new(&c.right)
                        {
                            out.push(
                                err("invalid_regex", format!("Invalid regular expression: {e}"))
                                    .block(id)
                                    .field("right"),
                            );
                        }
                    }
                    ConditionMode::Expression => {
                        if c.expression.trim().is_empty() {
                            out.push(err("missing_expression", "Enter an expression").block(id).field("expression"));
                        }
                    }
                }
                if !connected(PORT_TRUE) && !connected(PORT_FALSE) {
                    out.push(warn("condition_unconnected", "Neither true nor false is connected").block(id));
                }
            }
            BlockConfig::Loop(c) => {
                match c.mode {
                    LoopMode::ForEach if c.items.trim().is_empty() => {
                        out.push(err("missing_items", "Enter the array to loop over").block(id).field("items"));
                    }
                    LoopMode::Count if c.count.trim().is_empty() => {
                        out.push(err("missing_count", "Enter the number of iterations").block(id).field("count"));
                    }
                    LoopMode::Count
                        if !c.count.contains("{{")
                            && !c.count.trim().parse::<f64>().is_ok_and(|n| n.is_finite() && n >= 0.0) =>
                    {
                        out.push(err("bad_count", "The count is not a non-negative number").block(id).field("count"));
                    }
                    _ => {}
                }
                if c.max_iterations == Some(0) {
                    out.push(warn("zero_iterations", "max_iterations is 0: the loop always fails").block(id));
                }
                if !connected(PORT_ITEM) {
                    out.push(warn("loop_without_body", "Nothing is connected to item").block(id).field(PORT_ITEM));
                }
            }
            BlockConfig::Script(c) => {
                if c.code.trim().is_empty() {
                    out.push(warn("empty_script", "The script is empty").block(id).field("code"));
                }
            }
            BlockConfig::Ai(c) => match &c.request {
                None => {
                    if c.model.trim().is_empty() {
                        out.push(err("missing_model", "Choose a model").block(id).field("model"));
                    }
                    if c.prompt.trim().is_empty() {
                        out.push(err("missing_prompt", "Enter a prompt").block(id).field("prompt"));
                    }
                }
                Some(r) => {
                    if let Some(cs) = collections {
                        match find_item(cs, r) {
                            None => out.push(
                                err("request_not_found", format!("Saved AI request {} not found", r.request_id))
                                    .block(id)
                                    .field("request_id"),
                            ),
                            Some(item) if !matches!(item.request, RequestKind::Ai(_)) => out.push(
                                err("not_ai", format!("\"{}\" is not an AI request", item.name))
                                    .block(id)
                                    .field("request_id"),
                            ),
                            Some(_) => {}
                        }
                    }
                }
            },
            BlockConfig::Delay(c) => {
                if c.ms.trim().is_empty() {
                    out.push(err("missing_delay", "Enter the delay in milliseconds").block(id).field("ms"));
                } else if !c.ms.contains("{{")
                    && !c
                        .ms
                        .trim()
                        .parse::<f64>()
                        .is_ok_and(|v| std::time::Duration::try_from_secs_f64(v / 1000.0).is_ok())
                {
                    out.push(err("bad_delay", "The delay is not a non-negative number").block(id).field("ms"));
                }
            }
            BlockConfig::SetVariable(c) => {
                if c.name.trim().is_empty() {
                    out.push(err("missing_name", "Enter a variable name").block(id).field("name"));
                }
            }
            BlockConfig::Template(c) => {
                if c.mode == TemplateMode::Expression && c.expression.trim().is_empty() {
                    out.push(err("missing_expression", "Enter an expression").block(id).field("expression"));
                }
            }
            BlockConfig::Output(c) => {
                if c.name.trim().is_empty() {
                    out.push(err("missing_name", "Enter an output name").block(id).field("name"));
                } else {
                    *output_names.entry(c.name.trim().to_string()).or_default() += 1;
                    if output_names[c.name.trim()] == 2 {
                        out.push(
                            warn("duplicate_output", format!("Another Output block also sets \"{}\"", c.name.trim()))
                                .block(id)
                                .field("name"),
                        );
                    }
                }
            }
            BlockConfig::Log(c) => {
                if c.message.trim().is_empty() {
                    out.push(warn("empty_message", "The message is empty").block(id).field("message"));
                }
            }
            BlockConfig::Parallel(_) => match flow.edges_from(id, PORT_BRANCHES).count() {
                0 => out.push(warn("few_branches", "Nothing is connected to branches").block(id).field(PORT_BRANCHES)),
                1 => out.push(
                    warn("few_branches", "Only one branch: connect two or more blocks to run them at the same time")
                        .block(id)
                        .field(PORT_BRANCHES),
                ),
                _ => {}
            },
            BlockConfig::Join(_) => {
                if flow.edges_to(id).next().is_none() {
                    out.push(
                        warn("join_no_inputs", "Nothing is connected to this Join, so it never runs")
                            .block(id)
                            .field(PORT_IN),
                    );
                }
            }
        }

        // References in templates.
        let mut refs: Vec<(String, String)> = Vec::new();
        for (field, v) in template_values(b, collections) {
            let mut names = Vec::new();
            collect_refs(&v, &mut names);
            refs.extend(names.into_iter().map(|n| (field.clone(), n)));
        }
        if refs.is_empty() {
            continue;
        }
        let ups = upstream(id);
        let in_loop = bodies.values().any(|body| body.contains(id));
        for (field, name) in refs {
            let Some(segs) = parse_path(&name) else {
                out.push(err("bad_reference", format!("Cannot parse {{{{{name}}}}}")).block(id).field(&field));
                continue;
            };
            match flow_root(&name) {
                Some("blocks") => {
                    let Some(key) = segs.get(1) else { continue };
                    let key = match key {
                        Segment::Key(k) => k.clone(),
                        Segment::Index(i) => i.to_string(),
                    };
                    match flow.find_block(&key) {
                        None => out.push(
                            err("unknown_block_ref", format!("{{{{{name}}}}}: there is no block \"{key}\""))
                                .block(id)
                                .field(&field),
                        ),
                        Some(target) => {
                            let same_label = flow
                                .blocks
                                .iter()
                                .filter(|x| {
                                    x.label.eq_ignore_ascii_case(&key)
                                        || merpati_core::util::slugify(&x.label) == merpati_core::util::slugify(&key)
                                })
                                .count();
                            if flow.block(&key).is_none() && same_label > 1 {
                                out.push(
                                    warn(
                                        "ambiguous_ref",
                                        format!(
                                            "{{{{{name}}}}}: several blocks are called \"{key}\"; the first is used"
                                        ),
                                    )
                                    .block(id)
                                    .field(&field),
                                );
                            }
                            if target.id != b.id && !ups.contains(&target.id) {
                                out.push(
                                    warn(
                                        "not_upstream",
                                        format!(
                                            "{{{{{name}}}}}: \"{}\" is not connected before this block and may not have run yet",
                                            target.display_name()
                                        ),
                                    )
                                    .block(id)
                                    .field(&field),
                                );
                            }
                        }
                    }
                }
                Some("inputs") => {
                    if let Some(Segment::Key(k)) = segs.get(1)
                        && !declared_inputs.contains(k.as_str())
                    {
                        out.push(
                            warn("undeclared_input", format!("{{{{{name}}}}}: the flow has no input \"{k}\""))
                                .block(id)
                                .field(&field),
                        );
                    }
                }
                Some("loop") if !in_loop => {
                    out.push(
                        warn("loop_outside", format!("{{{{{name}}}}} is used outside of a loop body"))
                            .block(id)
                            .field(&field),
                    );
                }
                _ => {}
            }
        }
    }
    out
}
