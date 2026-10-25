//! The flow document: blocks on a canvas connected by edges between ports.
//!
//! Everything here is plain serde data written as pretty JSON with a fixed
//! key order, so flow files diff well in Git. Optional fields that hold their
//! default value are left out of the file.

use std::fmt;

use merpati_core::model::{HttpRequest, Id, Scripts};
use merpati_core::protocols::AiProvider;
use merpati_core::util::new_id;
use serde::de::Error as _;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Current flow file format version.
pub const FLOW_VERSION: u32 = 1;

/// The input port every block except Start has.
pub const PORT_IN: &str = "in";
/// Loop input port that stops the loop.
pub const PORT_BREAK: &str = "break";
pub const PORT_NEXT: &str = "next";
pub const PORT_SUCCESS: &str = "success";
pub const PORT_FAILURE: &str = "failure";
pub const PORT_TRUE: &str = "true";
pub const PORT_FALSE: &str = "false";
pub const PORT_ITEM: &str = "item";
pub const PORT_DONE: &str = "done";
/// Parallel output port: every edge leaving it starts a concurrent branch.
pub const PORT_BRANCHES: &str = "branches";

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

fn default_version() -> u32 {
    FLOW_VERSION
}

/// A flow: a small program made of blocks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Flow {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default = "new_id")]
    pub id: Id,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Values the caller passes in (`--input name=value`), shown on the
    /// Start block and referenced as `{{inputs.name}}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<FlowInput>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub settings: FlowSettings,
    #[serde(default)]
    pub blocks: Vec<Block>,
    #[serde(default)]
    pub edges: Vec<Edge>,
}

impl Flow {
    /// A new flow with a single Start block.
    pub fn new(name: impl Into<String>) -> Self {
        Flow {
            version: FLOW_VERSION,
            id: new_id(),
            name: name.into(),
            description: String::new(),
            inputs: Vec::new(),
            settings: FlowSettings::default(),
            blocks: vec![Block::new(BlockConfig::Start(StartConfig {}), "Start", (0.0, 0.0))],
            edges: Vec::new(),
        }
    }

    pub fn block(&self, id: &str) -> Option<&Block> {
        self.blocks.iter().find(|b| b.id == id)
    }

    pub fn block_mut(&mut self, id: &str) -> Option<&mut Block> {
        self.blocks.iter_mut().find(|b| b.id == id)
    }

    /// The (first) Start block.
    pub fn start(&self) -> Option<&Block> {
        self.blocks.iter().find(|b| b.kind() == BlockKind::Start)
    }

    /// Finds a block the way templates do: by id, then label (exact, then
    /// case-insensitive), then slugified label (`get-users` for "Get users").
    pub fn find_block(&self, key: &str) -> Option<&Block> {
        let key = key.trim();
        if let Some(b) = self.block(key) {
            return Some(b);
        }
        if let Some(b) = self.blocks.iter().find(|b| b.label == key) {
            return Some(b);
        }
        if let Some(b) = self.blocks.iter().find(|b| b.label.eq_ignore_ascii_case(key)) {
            return Some(b);
        }
        let slug = merpati_core::util::slugify(key);
        self.blocks.iter().find(|b| !b.label.is_empty() && merpati_core::util::slugify(&b.label) == slug)
    }

    /// Edges leaving `block` from `port`, in file order.
    pub fn edges_from<'a>(&'a self, block: &'a str, port: &'a str) -> impl Iterator<Item = (usize, &'a Edge)> + 'a {
        self.edges.iter().enumerate().filter(move |(_, e)| e.from_block == block && e.from_port == port)
    }

    /// Edges entering `block`.
    pub fn edges_to<'a>(&'a self, block: &'a str) -> impl Iterator<Item = (usize, &'a Edge)> + 'a {
        self.edges.iter().enumerate().filter(move |(_, e)| e.to_block == block)
    }

    /// Adds a block and returns its id.
    pub fn add_block(&mut self, block: Block) -> Id {
        let id = block.id.clone();
        self.blocks.push(block);
        id
    }

    /// Connects `from.port` to `to.in`.
    pub fn connect(&mut self, from: &str, port: &str, to: &str) {
        self.edges.push(Edge::new(from, port, to, PORT_IN));
    }

    /// Removes a block and every edge touching it.
    pub fn remove_block(&mut self, id: &str) -> Option<Block> {
        let pos = self.blocks.iter().position(|b| b.id == id)?;
        self.edges.retain(|e| e.from_block != id && e.to_block != id);
        Some(self.blocks.remove(pos))
    }
}

/// A declared flow input.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FlowInput {
    pub name: String,
    /// Used when the caller does not pass a value. `null` means no default.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub default: Value,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The run fails to start when a required input has no value.
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
}

impl FlowInput {
    pub fn new(name: impl Into<String>, default: impl Into<Value>) -> Self {
        FlowInput { name: name.into(), default: default.into(), ..Default::default() }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlowSettings {
    /// Block executions per run before the run is stopped (default 10,000).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<usize>,
    /// Keep running other paths after a failure that has no failure edge
    /// (the run still fails).
    #[serde(skip_serializing_if = "is_false")]
    pub continue_on_failure: bool,
    /// Branches of a Parallel block that run at the same time; the others
    /// start in branch order as running ones finish. 0 = all at once.
    #[serde(skip_serializing_if = "is_default")]
    pub max_concurrent: usize,
}

/// Position on the canvas, `[x, y]` in the file.
pub type Position = (f32, f32);

/// One block on the canvas. In the file:
/// `{ "id", "kind", "label", "position": [x, y], "config": { ... } }`.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub id: Id,
    /// Display name; also how templates usually refer to the block.
    pub label: String,
    pub position: Position,
    /// Kind-specific settings; [`Block::kind`] is derived from it.
    pub config: BlockConfig,
}

impl Block {
    pub fn new(config: BlockConfig, label: impl Into<String>, position: Position) -> Self {
        Block { id: new_id(), label: label.into(), position, config }
    }

    /// A block with a chosen id (handy for hand-written flows and tests).
    pub fn with_id(id: impl Into<String>, label: impl Into<String>, config: BlockConfig) -> Self {
        Block { id: id.into(), label: label.into(), position: (0.0, 0.0), config }
    }

    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.position = (x, y);
        self
    }

    pub fn kind(&self) -> BlockKind {
        self.config.kind()
    }

    /// Label, or the kind name when the label is empty.
    pub fn display_name(&self) -> &str {
        if self.label.trim().is_empty() { self.kind().label() } else { &self.label }
    }
}

impl Serialize for Block {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Block", 5)?;
        st.serialize_field("id", &self.id)?;
        st.serialize_field("kind", &self.kind())?;
        st.serialize_field("label", &self.label)?;
        st.serialize_field("position", &self.position)?;
        st.serialize_field("config", &self.config)?;
        st.end()
    }
}

impl<'de> Deserialize<'de> for Block {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default = "new_id")]
            id: Id,
            kind: BlockKind,
            #[serde(default)]
            label: String,
            #[serde(default)]
            position: Position,
            #[serde(default)]
            config: Value,
        }
        let raw = Raw::deserialize(d)?;
        let config = BlockConfig::from_value(raw.kind, raw.config)
            .map_err(|e| D::Error::custom(format!("block \"{}\" ({}): {e}", raw.id, raw.kind.as_str())))?;
        Ok(Block { id: raw.id, label: raw.label, position: raw.position, config })
    }
}

/// A connection from an output port of one block to an input port of another.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    pub from_block: Id,
    pub from_port: String,
    pub to_block: Id,
    #[serde(default = "default_in")]
    pub to_port: String,
}

fn default_in() -> String {
    PORT_IN.to_string()
}

impl Edge {
    pub fn new(from_block: &str, from_port: &str, to_block: &str, to_port: &str) -> Self {
        Edge {
            from_block: from_block.into(),
            from_port: from_port.into(),
            to_block: to_block.into(),
            to_port: to_port.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Start,
    Request,
    Condition,
    Loop,
    Script,
    Ai,
    Delay,
    SetVariable,
    Template,
    Output,
    Log,
    Parallel,
    Join,
}

impl BlockKind {
    pub const ALL: [BlockKind; 13] = [
        BlockKind::Start,
        BlockKind::Request,
        BlockKind::Condition,
        BlockKind::Loop,
        BlockKind::Parallel,
        BlockKind::Join,
        BlockKind::Script,
        BlockKind::Ai,
        BlockKind::Delay,
        BlockKind::SetVariable,
        BlockKind::Template,
        BlockKind::Output,
        BlockKind::Log,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::Start => "start",
            BlockKind::Request => "request",
            BlockKind::Condition => "condition",
            BlockKind::Loop => "loop",
            BlockKind::Script => "script",
            BlockKind::Ai => "ai",
            BlockKind::Delay => "delay",
            BlockKind::SetVariable => "set_variable",
            BlockKind::Template => "template",
            BlockKind::Output => "output",
            BlockKind::Log => "log",
            BlockKind::Parallel => "parallel",
            BlockKind::Join => "join",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BlockKind::Start => "Start",
            BlockKind::Request => "Request",
            BlockKind::Condition => "Condition",
            BlockKind::Loop => "Loop",
            BlockKind::Script => "Script",
            BlockKind::Ai => "AI",
            BlockKind::Delay => "Delay",
            BlockKind::SetVariable => "Set variable",
            BlockKind::Template => "Template",
            BlockKind::Output => "Output",
            BlockKind::Log => "Log",
            BlockKind::Parallel => "Parallel",
            BlockKind::Join => "Join",
        }
    }

    /// Ports edges can enter.
    pub fn input_ports(self) -> &'static [&'static str] {
        match self {
            BlockKind::Start => &[],
            BlockKind::Loop => &[PORT_IN, PORT_BREAK],
            _ => &[PORT_IN],
        }
    }

    /// Ports edges can leave from, in display order.
    pub fn output_ports(self) -> &'static [&'static str] {
        match self {
            BlockKind::Request | BlockKind::Script | BlockKind::Ai | BlockKind::Template => {
                &[PORT_SUCCESS, PORT_FAILURE]
            }
            BlockKind::Condition => &[PORT_TRUE, PORT_FALSE],
            BlockKind::Loop => &[PORT_ITEM, PORT_DONE],
            BlockKind::Parallel => &[PORT_BRANCHES],
            BlockKind::Start
            | BlockKind::Delay
            | BlockKind::SetVariable
            | BlockKind::Output
            | BlockKind::Log
            | BlockKind::Join => &[PORT_NEXT],
        }
    }

    /// True if the block can end on its `failure` port.
    pub fn has_failure_port(self) -> bool {
        self.output_ports().contains(&PORT_FAILURE)
    }

    /// Default configuration for a new block of this kind.
    pub fn default_config(self) -> BlockConfig {
        match self {
            BlockKind::Start => BlockConfig::Start(StartConfig {}),
            BlockKind::Request => BlockConfig::Request(RequestConfig::default()),
            BlockKind::Condition => BlockConfig::Condition(ConditionConfig::default()),
            BlockKind::Loop => BlockConfig::Loop(LoopConfig::default()),
            BlockKind::Script => BlockConfig::Script(ScriptConfig::default()),
            // New AI blocks stream (files without the field do not).
            BlockKind::Ai => BlockConfig::Ai(AiBlockConfig { stream: true, ..Default::default() }),
            BlockKind::Delay => BlockConfig::Delay(DelayConfig::default()),
            BlockKind::SetVariable => BlockConfig::SetVariable(SetVariableConfig::default()),
            BlockKind::Template => BlockConfig::Template(TemplateConfig::default()),
            BlockKind::Output => BlockConfig::Output(OutputConfig::default()),
            BlockKind::Log => BlockConfig::Log(LogConfig::default()),
            BlockKind::Parallel => BlockConfig::Parallel(ParallelConfig {}),
            BlockKind::Join => BlockConfig::Join(JoinConfig {}),
        }
    }
}

impl fmt::Display for BlockKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Kind-specific block settings.
#[derive(Clone, Debug, PartialEq)]
pub enum BlockConfig {
    Start(StartConfig),
    Request(RequestConfig),
    Condition(ConditionConfig),
    Loop(LoopConfig),
    Script(ScriptConfig),
    Ai(AiBlockConfig),
    Delay(DelayConfig),
    SetVariable(SetVariableConfig),
    Template(TemplateConfig),
    Output(OutputConfig),
    Log(LogConfig),
    Parallel(ParallelConfig),
    Join(JoinConfig),
}

impl BlockConfig {
    pub fn kind(&self) -> BlockKind {
        match self {
            BlockConfig::Start(_) => BlockKind::Start,
            BlockConfig::Request(_) => BlockKind::Request,
            BlockConfig::Condition(_) => BlockKind::Condition,
            BlockConfig::Loop(_) => BlockKind::Loop,
            BlockConfig::Script(_) => BlockKind::Script,
            BlockConfig::Ai(_) => BlockKind::Ai,
            BlockConfig::Delay(_) => BlockKind::Delay,
            BlockConfig::SetVariable(_) => BlockKind::SetVariable,
            BlockConfig::Template(_) => BlockKind::Template,
            BlockConfig::Output(_) => BlockKind::Output,
            BlockConfig::Log(_) => BlockKind::Log,
            BlockConfig::Parallel(_) => BlockKind::Parallel,
            BlockConfig::Join(_) => BlockKind::Join,
        }
    }

    /// Parses the `config` object of a block of `kind` (`null` = defaults).
    pub fn from_value(kind: BlockKind, v: Value) -> Result<BlockConfig, serde_json::Error> {
        let v = if v.is_null() { Value::Object(Default::default()) } else { v };
        Ok(match kind {
            BlockKind::Start => BlockConfig::Start(serde_json::from_value(v)?),
            BlockKind::Request => BlockConfig::Request(serde_json::from_value(v)?),
            BlockKind::Condition => BlockConfig::Condition(serde_json::from_value(v)?),
            BlockKind::Loop => BlockConfig::Loop(serde_json::from_value(v)?),
            BlockKind::Script => BlockConfig::Script(serde_json::from_value(v)?),
            BlockKind::Ai => BlockConfig::Ai(serde_json::from_value(v)?),
            BlockKind::Delay => BlockConfig::Delay(serde_json::from_value(v)?),
            BlockKind::SetVariable => BlockConfig::SetVariable(serde_json::from_value(v)?),
            BlockKind::Template => BlockConfig::Template(serde_json::from_value(v)?),
            BlockKind::Output => BlockConfig::Output(serde_json::from_value(v)?),
            BlockKind::Log => BlockConfig::Log(serde_json::from_value(v)?),
            BlockKind::Parallel => BlockConfig::Parallel(serde_json::from_value(v)?),
            BlockKind::Join => BlockConfig::Join(serde_json::from_value(v)?),
        })
    }

    /// Every user-editable template string of the block, with a field name
    /// (for diagnostics). JavaScript code is not included.
    pub fn templates(&self) -> Vec<(&'static str, &str)> {
        let mut out: Vec<(&'static str, &str)> = Vec::new();
        match self {
            BlockConfig::Start(_) | BlockConfig::Script(_) | BlockConfig::Parallel(_) | BlockConfig::Join(_) => {}
            BlockConfig::Request(c) => {
                if let RequestSource::Inline { request, .. } = &c.source {
                    out.push(("url", &request.url));
                    for h in request.headers.iter().filter(|h| h.enabled) {
                        out.push(("headers", &h.value));
                    }
                    if !request.body.raw.is_empty() {
                        out.push(("body", &request.body.raw));
                    }
                }
            }
            BlockConfig::Condition(c) => {
                if c.mode == ConditionMode::Compare {
                    out.push(("left", &c.left));
                    out.push(("right", &c.right));
                }
            }
            BlockConfig::Loop(c) => match c.mode {
                LoopMode::ForEach if c.items.contains("{{") => out.push(("items", &c.items)),
                LoopMode::ForEach => {}
                LoopMode::Count => out.push(("count", &c.count)),
            },
            BlockConfig::Ai(c) => {
                out.push(("base_url", &c.base_url));
                out.push(("model", &c.model));
                out.push(("system", &c.system));
                out.push(("prompt", &c.prompt));
            }
            BlockConfig::Delay(c) => out.push(("ms", &c.ms)),
            BlockConfig::SetVariable(c) => out.push(("value", &c.value)),
            BlockConfig::Template(c) => {
                if c.mode == TemplateMode::Template {
                    out.push(("template", &c.template));
                }
            }
            BlockConfig::Output(c) => out.push(("value", &c.value)),
            BlockConfig::Log(c) => out.push(("message", &c.message)),
        }
        out.retain(|(_, t)| !t.is_empty());
        out
    }
}

impl Serialize for BlockConfig {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            BlockConfig::Start(c) => c.serialize(s),
            BlockConfig::Request(c) => c.serialize(s),
            BlockConfig::Condition(c) => c.serialize(s),
            BlockConfig::Loop(c) => c.serialize(s),
            BlockConfig::Script(c) => c.serialize(s),
            BlockConfig::Ai(c) => c.serialize(s),
            BlockConfig::Delay(c) => c.serialize(s),
            BlockConfig::SetVariable(c) => c.serialize(s),
            BlockConfig::Template(c) => c.serialize(s),
            BlockConfig::Output(c) => c.serialize(s),
            BlockConfig::Log(c) => c.serialize(s),
            BlockConfig::Parallel(c) => c.serialize(s),
            BlockConfig::Join(c) => c.serialize(s),
        }
    }
}

/// Start has no settings; the flow's inputs are [`Flow::inputs`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StartConfig {}

/// A saved request item.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRef {
    /// Collection id. May be empty: the request is then searched in every
    /// collection.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub collection_id: Id,
    pub request_id: Id,
}

impl ItemRef {
    pub fn new(collection_id: impl Into<String>, request_id: impl Into<String>) -> Self {
        ItemRef { collection_id: collection_id.into(), request_id: request_id.into() }
    }
}

/// What a Request block sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestSource {
    /// A request saved in a collection; runs with the collection's auth,
    /// scripts and variables, exactly like the Runner.
    Saved(ItemRef),
    /// A request defined in the flow itself.
    Inline {
        request: Box<HttpRequest>,
        /// Optional collection whose auth, scripts and variables the inline
        /// request inherits (as if it were at the collection root).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        collection_id: Id,
        /// Pre-request and post-response scripts (tests) of the inline request.
        #[serde(default, skip_serializing_if = "Scripts::is_empty")]
        scripts: Scripts,
    },
}

impl Default for RequestSource {
    fn default() -> Self {
        RequestSource::Inline {
            request: Box::new(HttpRequest::new("GET", "")),
            collection_id: String::new(),
            scripts: Scripts::default(),
        }
    }
}

impl RequestSource {
    pub fn inline(request: HttpRequest) -> Self {
        RequestSource::Inline { request: Box::new(request), collection_id: String::new(), scripts: Scripts::default() }
    }
}

/// Request block. Output: see the crate docs (`status`, `headers`, `body`,
/// `json`, `time_ms`, ...). Ports: `success` / `failure`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestConfig {
    #[serde(default)]
    pub source: RequestSource,
    /// Statuses that count as success: `2xx`, `200-299`, `201`, comma
    /// separated. Empty = any status below 400.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub success_status: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionMode {
    /// `left operator right` on template values; no JavaScript involved.
    #[default]
    Compare,
    /// A JavaScript expression; truthy goes to `true`.
    Expression,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOp {
    #[default]
    #[serde(alias = "==", alias = "equals")]
    Eq,
    #[serde(alias = "!=", alias = "not_equals")]
    Ne,
    #[serde(alias = ">")]
    Gt,
    #[serde(alias = ">=")]
    Gte,
    #[serde(alias = "<")]
    Lt,
    #[serde(alias = "<=")]
    Lte,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    /// Regular expression match.
    Matches,
    /// Left is not null/empty/unresolved; right is ignored.
    Exists,
    NotExists,
    /// Null, empty string, empty array or empty object.
    Empty,
    NotEmpty,
    /// JavaScript-like truthiness (also `"false"` and `"0"` are false).
    Truthy,
    Falsy,
}

impl CompareOp {
    pub const ALL: [CompareOp; 17] = [
        CompareOp::Eq,
        CompareOp::Ne,
        CompareOp::Gt,
        CompareOp::Gte,
        CompareOp::Lt,
        CompareOp::Lte,
        CompareOp::Contains,
        CompareOp::NotContains,
        CompareOp::StartsWith,
        CompareOp::EndsWith,
        CompareOp::Matches,
        CompareOp::Exists,
        CompareOp::NotExists,
        CompareOp::Empty,
        CompareOp::NotEmpty,
        CompareOp::Truthy,
        CompareOp::Falsy,
    ];

    pub fn label(self) -> &'static str {
        match self {
            CompareOp::Eq => "equals",
            CompareOp::Ne => "does not equal",
            CompareOp::Gt => ">",
            CompareOp::Gte => ">=",
            CompareOp::Lt => "<",
            CompareOp::Lte => "<=",
            CompareOp::Contains => "contains",
            CompareOp::NotContains => "does not contain",
            CompareOp::StartsWith => "starts with",
            CompareOp::EndsWith => "ends with",
            CompareOp::Matches => "matches regex",
            CompareOp::Exists => "exists",
            CompareOp::NotExists => "does not exist",
            CompareOp::Empty => "is empty",
            CompareOp::NotEmpty => "is not empty",
            CompareOp::Truthy => "is truthy",
            CompareOp::Falsy => "is falsy",
        }
    }

    /// Operators that ignore the right-hand side.
    pub fn is_unary(self) -> bool {
        matches!(
            self,
            CompareOp::Exists
                | CompareOp::NotExists
                | CompareOp::Empty
                | CompareOp::NotEmpty
                | CompareOp::Truthy
                | CompareOp::Falsy
        )
    }
}

/// Condition block. Output: the boolean result. Ports: `true` / `false`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConditionConfig {
    pub mode: ConditionMode,
    /// Compare mode: left template (a pure `{{reference}}` keeps its type).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub left: String,
    #[serde(rename = "operator")]
    pub op: CompareOp,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub right: String,
    /// Expression mode: JavaScript expression, e.g. `input.status === 200`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub expression: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopMode {
    /// Iterate over an array.
    #[default]
    ForEach,
    /// Iterate `count` times (the item is the 0-based index).
    Count,
}

/// Default and hard cap for loop iterations (overridable per loop).
pub const DEFAULT_MAX_ITERATIONS: usize = 1000;

/// Loop block. Fires `item` once per element (the blocks reachable from
/// `item` are the loop body and run to completion before the next element),
/// then `done`. An edge into the `break` input stops the loop.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopConfig {
    pub mode: LoopMode,
    /// For-each: a template such as `{{blocks.list.output.json.items}}`
    /// (strings are parsed as JSON), or a JavaScript expression returning an
    /// array when it contains no `{{`. Objects iterate over their values.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub items: String,
    /// Count: a number or template.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub count: String,
    /// Runaway guard; the loop fails when there are more iterations
    /// (default [`DEFAULT_MAX_ITERATIONS`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<usize>,
}

/// Script block: JavaScript with `blocks`, `inputs`, `input`, `vars`, `loop`
/// and `flow` in scope plus the usual `pm` API. `return` sets the output;
/// `vars.x = ...` or `pm.variables.set("x", ...)` set flow variables,
/// `pm.environment.set` / `pm.globals.set` update the run's environment and
/// globals. Failing `pm.test`s or a thrown error go to `failure`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScriptConfig {
    pub code: String,
}

/// AI block. Either inline (provider, model, key, prompt) or based on a
/// saved AI request item, whose settings the non-empty fields here override
/// (`prompt` is appended as a final user message). Output: `text`, `json`
/// (the text parsed as JSON, or null), `model`, `finish_reason`, token
/// counts and `latency_ms`. Ports: `success` / `failure`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiBlockConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<ItemRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<AiProvider>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Sent as a Bearer token; usually `{{vault:openai}}`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub system: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Fail when the answer is not valid JSON.
    #[serde(skip_serializing_if = "is_false")]
    pub require_json: bool,
    /// Stream the answer: the run reports the text as it arrives
    /// ([`FlowEvent::AiDelta`](crate::FlowEvent::AiDelta)). The output is the
    /// same either way.
    #[serde(skip_serializing_if = "is_false")]
    pub stream: bool,
}

/// Parallel block: every edge leaving `branches` starts a branch, and the
/// branches run at the same time (their requests, AI calls, delays and
/// scripts overlap; [`FlowSettings::max_concurrent`] caps how many). Each
/// branch runs until it reaches a Join or has nothing left to do; a Join
/// fires once the branches that can reach it have finished. Output: the
/// value it received, so `{{input}}` at the start of each branch is the
/// output of the block before the Parallel.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ParallelConfig {}

/// Join block: waits for the branches of the Parallel block that can reach
/// it (all of them when every branch can), then fires once. Output:
/// `{results, items, sources, count, branches}` (see the crate docs). Port:
/// `next`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JoinConfig {}

/// Delay block. Output: `{ "ms": n }`. Port: `next`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DelayConfig {
    /// Milliseconds; a number or template.
    pub ms: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariableScope {
    /// Flow variable for the rest of the run (`{{name}}`, `{{vars.name}}`).
    #[default]
    Flow,
    /// The run's environment (the file on disk is not changed).
    Environment,
    Global,
}

/// Set Variable block. Output: the value. Port: `next`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SetVariableConfig {
    pub name: String,
    /// Template; a pure `{{reference}}` keeps its JSON type.
    pub value: String,
    #[serde(skip_serializing_if = "is_default")]
    pub scope: VariableScope,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateMode {
    #[default]
    Template,
    /// JavaScript expression.
    Expression,
}

/// Template / Evaluate block: builds a value. Output: the value.
/// Ports: `success` / `failure`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TemplateConfig {
    pub mode: TemplateMode,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub template: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub expression: String,
    /// Parse the rendered template as JSON.
    #[serde(skip_serializing_if = "is_false")]
    pub parse_json: bool,
}

/// Output block: records a named flow result. Output: the value. Port: `next`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    pub name: String,
    /// Template; a pure `{{reference}}` keeps its JSON type.
    pub value: String,
    /// Reaching this block marks the run as failed (an explicit error result).
    #[serde(skip_serializing_if = "is_false")]
    pub fail: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevelConfig {
    #[default]
    Info,
    Log,
    Warn,
    Error,
    Debug,
}

/// Log block: writes a line to the run log. Output: the message. Port: `next`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub message: String,
    #[serde(skip_serializing_if = "is_default")]
    pub level: LogLevelConfig,
}
