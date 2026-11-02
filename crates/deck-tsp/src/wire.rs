//! TSP wire shapes, following omp `v18.15.0` `packages/wire/src/tsp.ts`
//! (`https://raw.githubusercontent.com/can1357/oh-my-pi/v18.15.0/packages/wire/src/tsp.ts`) and the
//! Tern spec (`https://docs.stencil.so/tern/protocol/`).
//!
//! Unknown fields are ignored, unknown ops become [`Op::Unknown`], unknown queries
//! [`Query::Unknown`]; nothing here fails on a vocabulary it does not know. Node kinds and
//! props stay free-form strings and JSON values: the renderer decides what it draws.

use serde::de::Deserializer;
use serde::ser::{SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Protocol version this build speaks.
pub const VERSION: u32 = 1;
/// APC identifier: every TSP message body starts with `tsp;`.
pub const APC_ID: &str = "tsp";
/// Largest APC body a sender emits before chunking unless the hello reply says otherwise.
pub const DEFAULT_APC_LIMIT: usize = 65_536;
/// Unacknowledged frames a sender may have in flight unless the hello reply says otherwise.
pub const DEFAULT_CREDITS: u32 = 2;

/// Every component kind of the v1 vocabulary (`TSP_KINDS`, 45 entries at v18.15.0), in the
/// tag's order.
pub const KINDS: [&str; 45] = [
    "col",
    "row",
    "card",
    "section",
    "rule",
    "spacer",
    "text",
    "md",
    "code",
    "diff",
    "ansi",
    "math",
    "image",
    "media",
    "kv",
    "table",
    "tree",
    "json",
    "badge",
    "kbd",
    "icon",
    "spinner",
    "shimmer",
    "elapsed",
    "progress",
    "rate",
    "list",
    "item",
    "tabs",
    "editor",
    "input",
    "button",
    "status",
    "seg",
    "overlay",
    "toast",
    "rows",
    "picker",
    "prefs",
    "tool",
    "checklist",
    "agent",
    "chart",
    "meter",
    "effort",
];

/// Kinds whose primary text the `text` and `splice` ops address (`TSP_TEXT_KINDS`).
pub const TEXT_KINDS: [&str; 8] = [
    "text", "md", "code", "ansi", "math", "editor", "input", "shimmer",
];

/// Fixed ids of a surface's three regions (`TSP_REGION_IDS`).
pub const REGION_MAIN: &str = "main";
pub const REGION_DOCK: &str = "dock";
pub const REGION_LAYER: &str = "layer";

/// `null` (or absent) deserializes to the default, as the spec drops null props.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A node on the wire: `{"id","k","p"?,"c"?}`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub k: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Map::is_empty"
    )]
    pub p: Map<String, Value>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub c: Vec<Node>,
}

impl Node {
    pub fn new(id: impl Into<String>, k: impl Into<String>) -> Node {
        Node {
            id: id.into(),
            k: k.into(),
            p: Map::new(),
            c: Vec::new(),
        }
    }

    /// Sets one prop (builder style).
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Node {
        self.p.insert(key.to_owned(), value.into());
        self
    }

    /// Sets the children (builder style).
    #[must_use]
    pub fn children(mut self, children: Vec<Node>) -> Node {
        self.c = children;
        self
    }
}

/// `text` op mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextMode {
    Append,
    Replace,
}

/// `reveal` op position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealAt {
    Start,
    End,
    Nearest,
}

/// `scroll` op distance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollBy {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
    Start,
    End,
}

impl TextMode {
    fn parse(s: &str) -> Option<TextMode> {
        match s {
            "append" => Some(TextMode::Append),
            "replace" => Some(TextMode::Replace),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            TextMode::Append => "append",
            TextMode::Replace => "replace",
        }
    }
}

impl RevealAt {
    fn parse(s: &str) -> Option<RevealAt> {
        match s {
            "start" => Some(RevealAt::Start),
            "end" => Some(RevealAt::End),
            "nearest" => Some(RevealAt::Nearest),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            RevealAt::Start => "start",
            RevealAt::End => "end",
            RevealAt::Nearest => "nearest",
        }
    }
}

impl ScrollBy {
    fn parse(s: &str) -> Option<ScrollBy> {
        match s {
            "line-up" => Some(ScrollBy::LineUp),
            "line-down" => Some(ScrollBy::LineDown),
            "page-up" => Some(ScrollBy::PageUp),
            "page-down" => Some(ScrollBy::PageDown),
            "start" => Some(ScrollBy::Start),
            "end" => Some(ScrollBy::End),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            ScrollBy::LineUp => "line-up",
            ScrollBy::LineDown => "line-down",
            ScrollBy::PageUp => "page-up",
            ScrollBy::PageDown => "page-down",
            ScrollBy::Start => "start",
            ScrollBy::End => "end",
        }
    }
}

/// Names of the ops of the tag. A frame op with one of these names that does not parse is
/// malformed (rejected alone); any other name is a newer op and is ignored.
pub const OP_NAMES: [&str; 12] = [
    "add", "set", "text", "splice", "move", "del", "settle", "focus", "reveal", "scroll",
    "suspend", "resume",
];

/// One frame op (`TspOp`). On the wire an op is a JSON array `[name, args…]`.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// `["add", id, parent, before|null, node]`
    Add {
        id: String,
        parent: String,
        before: Option<String>,
        node: Node,
    },
    /// `["set", id, props]`
    Set {
        id: String,
        props: Map<String, Value>,
    },
    /// `["text", id, "append"|"replace", text]`
    Text {
        id: String,
        mode: TextMode,
        text: String,
    },
    /// `["splice", id, at, del, text]` (UTF-16 code unit offsets)
    Splice {
        id: String,
        at: u64,
        del: u64,
        text: String,
    },
    /// `["move", id, parent, before|null]`
    Move {
        id: String,
        parent: String,
        before: Option<String>,
    },
    /// `["del", id]`
    Del {
        id: String,
    },
    /// `["settle", id]`
    Settle {
        id: String,
    },
    /// `["focus", id|null]`
    Focus {
        id: Option<String>,
    },
    /// `["reveal", id, "start"|"end"|"nearest"]`
    Reveal {
        id: String,
        at: RevealAt,
    },
    /// `["scroll", id, by]`
    Scroll {
        id: String,
        by: ScrollBy,
    },
    Suspend,
    Resume,
    /// An op this build does not know, or a known op whose arguments do not fit (the whole
    /// array as received; see [`OP_NAMES`]).
    Unknown(Value),
}

fn string_at(a: &[Value], i: usize) -> Option<String> {
    a.get(i)?.as_str().map(str::to_owned)
}

/// `Some(None)` for `null`, `Some(Some(s))` for a string, `None` for anything else or absent.
fn nullable_string_at(a: &[Value], i: usize) -> Option<Option<String>> {
    match a.get(i)? {
        Value::Null => Some(None),
        Value::String(s) => Some(Some(s.clone())),
        _ => None,
    }
}

impl Op {
    /// Parses an op array; anything unrecognised or malformed is [`Op::Unknown`].
    pub fn from_value(value: Value) -> Op {
        match Op::parse(&value) {
            Some(op) => op,
            None => Op::Unknown(value),
        }
    }

    fn parse(value: &Value) -> Option<Op> {
        let a = value.as_array()?;
        let name = a.first()?.as_str()?;
        let none_extra = |n: usize| a.len() == n;
        Some(match name {
            "add" if none_extra(5) => Op::Add {
                id: string_at(a, 1)?,
                parent: string_at(a, 2)?,
                before: nullable_string_at(a, 3)?,
                node: Node::deserialize(&a[4]).ok()?,
            },
            "set" if none_extra(3) => Op::Set {
                id: string_at(a, 1)?,
                props: a[2].as_object()?.clone(),
            },
            "text" if none_extra(4) => Op::Text {
                id: string_at(a, 1)?,
                mode: TextMode::parse(a[2].as_str()?)?,
                text: string_at(a, 3)?,
            },
            "splice" if none_extra(5) => Op::Splice {
                id: string_at(a, 1)?,
                at: a[2].as_u64()?,
                del: a[3].as_u64()?,
                text: string_at(a, 4)?,
            },
            "move" if none_extra(4) => Op::Move {
                id: string_at(a, 1)?,
                parent: string_at(a, 2)?,
                before: nullable_string_at(a, 3)?,
            },
            "del" if none_extra(2) => Op::Del {
                id: string_at(a, 1)?,
            },
            "settle" if none_extra(2) => Op::Settle {
                id: string_at(a, 1)?,
            },
            "focus" if none_extra(2) => Op::Focus {
                id: nullable_string_at(a, 1)?,
            },
            "reveal" if none_extra(3) => Op::Reveal {
                id: string_at(a, 1)?,
                at: RevealAt::parse(a[2].as_str()?)?,
            },
            "scroll" if none_extra(3) => Op::Scroll {
                id: string_at(a, 1)?,
                by: ScrollBy::parse(a[2].as_str()?)?,
            },
            "suspend" if none_extra(1) => Op::Suspend,
            "resume" if none_extra(1) => Op::Resume,
            _ => return None,
        })
    }

    /// The op's wire name (for [`Op::Unknown`], the received name, empty when absent).
    pub fn name(&self) -> &str {
        match self {
            Op::Add { .. } => "add",
            Op::Set { .. } => "set",
            Op::Text { .. } => "text",
            Op::Splice { .. } => "splice",
            Op::Move { .. } => "move",
            Op::Del { .. } => "del",
            Op::Settle { .. } => "settle",
            Op::Focus { .. } => "focus",
            Op::Reveal { .. } => "reveal",
            Op::Scroll { .. } => "scroll",
            Op::Suspend => "suspend",
            Op::Resume => "resume",
            Op::Unknown(v) => v
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .unwrap_or(""),
        }
    }

    /// Whether this is a known op that failed to parse (as opposed to a newer op name).
    pub fn is_malformed(&self) -> bool {
        match self {
            Op::Unknown(v) => match v.as_array().and_then(|a| a.first()).and_then(Value::as_str) {
                Some(name) => OP_NAMES.contains(&name),
                None => true,
            },
            _ => false,
        }
    }
}

impl Serialize for Op {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let opt = |o: &Option<String>| o.as_deref().map_or(Value::Null, Value::from);
        match self {
            Op::Unknown(v) => v.serialize(s),
            Op::Add {
                id,
                parent,
                before,
                node,
            } => {
                let mut seq = s.serialize_seq(Some(5))?;
                seq.serialize_element("add")?;
                seq.serialize_element(id)?;
                seq.serialize_element(parent)?;
                seq.serialize_element(&opt(before))?;
                seq.serialize_element(node)?;
                seq.end()
            }
            Op::Set { id, props } => {
                let mut seq = s.serialize_seq(Some(3))?;
                seq.serialize_element("set")?;
                seq.serialize_element(id)?;
                seq.serialize_element(props)?;
                seq.end()
            }
            Op::Text { id, mode, text } => {
                let mut seq = s.serialize_seq(Some(4))?;
                seq.serialize_element("text")?;
                seq.serialize_element(id)?;
                seq.serialize_element(mode.as_str())?;
                seq.serialize_element(text)?;
                seq.end()
            }
            Op::Splice { id, at, del, text } => {
                let mut seq = s.serialize_seq(Some(5))?;
                seq.serialize_element("splice")?;
                seq.serialize_element(id)?;
                seq.serialize_element(at)?;
                seq.serialize_element(del)?;
                seq.serialize_element(text)?;
                seq.end()
            }
            Op::Move { id, parent, before } => {
                let mut seq = s.serialize_seq(Some(4))?;
                seq.serialize_element("move")?;
                seq.serialize_element(id)?;
                seq.serialize_element(parent)?;
                seq.serialize_element(&opt(before))?;
                seq.end()
            }
            Op::Del { id } | Op::Settle { id } => {
                let mut seq = s.serialize_seq(Some(2))?;
                seq.serialize_element(self.name())?;
                seq.serialize_element(id)?;
                seq.end()
            }
            Op::Focus { id } => {
                let mut seq = s.serialize_seq(Some(2))?;
                seq.serialize_element("focus")?;
                seq.serialize_element(&opt(id))?;
                seq.end()
            }
            Op::Reveal { id, at } => {
                let mut seq = s.serialize_seq(Some(3))?;
                seq.serialize_element("reveal")?;
                seq.serialize_element(id)?;
                seq.serialize_element(at.as_str())?;
                seq.end()
            }
            Op::Scroll { id, by } => {
                let mut seq = s.serialize_seq(Some(3))?;
                seq.serialize_element("scroll")?;
                seq.serialize_element(id)?;
                seq.serialize_element(by.as_str())?;
                seq.end()
            }
            Op::Suspend | Op::Resume => {
                let mut seq = s.serialize_seq(Some(1))?;
                seq.serialize_element(self.name())?;
                seq.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Op {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Op, D::Error> {
        Ok(Op::from_value(Value::deserialize(d)?))
    }
}

/// Verb `f`: an atomic batch of ops for one surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    /// Surface id.
    pub sf: String,
    /// Monotonic per-surface sequence number.
    pub s: u64,
    pub ops: Vec<Op>,
}

/// Surface mode (`o` `mode`). A mode this build does not know deserializes to
/// [`Mode::Unknown`] (the spec makes it an error of that `o`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Inline,
    Screen,
    Flow,
    #[serde(other)]
    Unknown,
}

fn default_true() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

/// Verb `o`: open (or adopt) a surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Open {
    pub id: String,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub adopt: bool,
    /// `false`: the program never reads its input (no acks, events or errors).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub listen: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
}

/// Verb `x`: close a surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Close {
    pub id: String,
    /// Keep `main` in scrollback (default) or remove the surface.
    #[serde(default = "default_true")]
    pub keep: bool,
}

/// Verb `s`: a stylesheet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stylesheet {
    pub sf: String,
    pub name: String,
    pub css: String,
}

/// Pixel size of one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub w: u32,
    pub h: u32,
}

/// Verb `q` (program → terminal).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "q", rename_all = "lowercase")]
pub enum Query {
    Hello {
        #[serde(default)]
        v: Vec<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ver: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
    },
    Blobs {
        #[serde(default)]
        ids: Vec<String>,
    },
    /// A query this build does not know: no reply (spec).
    #[serde(other)]
    Unknown,
}

/// Verb `r` (terminal → program).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "r", rename_all = "lowercase")]
pub enum Reply {
    Hello {
        v: u32,
        term: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ver: Option<String>,
        kinds: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        apc: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credits: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cols: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell: Option<Cell>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dark: Option<bool>,
        #[serde(
            default,
            rename = "reduceMotion",
            skip_serializing_if = "Option::is_none"
        )]
        reduce_motion: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hour12: Option<bool>,
    },
    Blobs {
        have: Vec<String>,
    },
}

/// Verb `e` (terminal → program): every event of `TspEvent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "lowercase")]
pub enum Event {
    Ack {
        sf: String,
        s: u64,
    },
    Resize {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sf: Option<String>,
        cols: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cell: Option<Cell>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        visible: Option<bool>,
    },
    Theme {
        dark: bool,
    },
    Motion {
        reduce: bool,
    },
    Visible {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sf: Option<String>,
        visible: bool,
    },
    Toggle {
        sf: String,
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        collapsed: bool,
    },
    Select {
        sf: String,
        id: String,
        item: String,
    },
    Activate {
        sf: String,
        id: String,
        item: String,
    },
    Action {
        sf: String,
        id: String,
        act: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mods: Option<Vec<String>>,
    },
    /// A typed value changed by pointer; `null` resets to the default.
    Change {
        sf: String,
        id: String,
        item: String,
        #[serde(default)]
        value: Value,
    },
    /// Replace UTF-16 `[from, to)` of an `editor`/`input` with `text`, caret to `cursor`;
    /// `len` is the text length the terminal saw (a mismatch makes the edit stale).
    Edit {
        sf: String,
        id: String,
        from: u32,
        to: u32,
        text: String,
        cursor: u32,
        len: u32,
    },
    Undo {
        sf: String,
        id: String,
    },
    /// Submit `text` through the composer's ordinary submission path (only to a live editor
    /// whose `sendable` is exactly `true`, and only when the program's hello lists `send`).
    Send {
        sf: String,
        id: String,
        text: String,
    },
    Focus {
        sf: String,
        id: String,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sf: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        s: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        op: Option<u32>,
        msg: String,
    },
    Gone {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sf: Option<String>,
        ids: Vec<String>,
    },
}

impl Event {
    /// An `error` event without surface or op.
    pub fn error(msg: impl Into<String>) -> Event {
        Event::Error {
            sf: None,
            s: None,
            op: None,
            msg: msg.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kinds_match_the_tag_and_are_unique() {
        assert_eq!(KINDS.len(), 45);
        let mut sorted = KINDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), KINDS.len());
        for kind in TEXT_KINDS {
            assert!(KINDS.contains(&kind), "{kind}");
        }
    }

    #[test]
    fn every_op_round_trips_through_its_array_form() {
        let ops = json!([
            ["add", "n1", "main", null, {"id": "n1", "k": "text", "p": {"text": "hi"}}],
            ["add", "n2", "main", "n1", {"id": "n2", "k": "col", "c": [{"id": "n3", "k": "rule"}]}],
            ["set", "n1", {"tone": "info", "gone": null}],
            ["text", "n1", "append", "x"],
            ["text", "n1", "replace", "y"],
            ["splice", "n1", 1, 2, "zz"],
            ["move", "n2", "main", null],
            ["del", "n2"],
            ["settle", "n1"],
            ["focus", "n1"],
            ["focus", null],
            ["reveal", "n1", "nearest"],
            ["scroll", "n1", "page-down"],
            ["suspend"],
            ["resume"],
        ]);
        let frame: Frame =
            serde_json::from_value(json!({"sf": "s1", "s": 7, "ops": ops.clone()})).unwrap();
        assert_eq!(frame.ops.len(), 15);
        assert!(frame.ops.iter().all(|op| !matches!(op, Op::Unknown(_))));
        assert_eq!(serde_json::to_value(&frame.ops).unwrap(), ops);
    }

    #[test]
    fn malformed_and_newer_ops_are_unknown_but_distinguished() {
        for bad in [
            json!(["add", "n1", "main", null]),
            json!(["text", "n1", "prepend", "x"]),
            json!(["splice", "n1", -1, 0, ""]),
            json!(["move", "n1", 7, null]),
            json!(["del"]),
            json!([]),
            json!({"op": "del"}),
        ] {
            let op = Op::from_value(bad.clone());
            assert_eq!(op, Op::Unknown(bad.clone()), "{bad}");
            assert!(op.is_malformed(), "{bad}");
        }
        let newer = Op::from_value(json!(["pin", "n1"]));
        assert_eq!(newer.name(), "pin");
        assert!(!newer.is_malformed());
    }

    #[test]
    fn nodes_drop_nulls_and_default_missing_parts() {
        let n: Node =
            serde_json::from_str(r#"{"id":"a","k":"text","p":null,"c":null,"x":1}"#).unwrap();
        assert_eq!(n, Node::new("a", "text"));
        assert_eq!(
            serde_json::to_string(&n).unwrap(),
            r#"{"id":"a","k":"text"}"#
        );
    }

    #[test]
    fn queries_replies_and_events_use_the_tag_shapes() {
        let q: Query = serde_json::from_str(
            r#"{"q":"hello","v":[1],"app":"omp","ver":"18.15.0","features":["edit","undo","send"],"extra":1}"#,
        )
        .unwrap();
        assert_eq!(
            q,
            Query::Hello {
                v: vec![1],
                app: Some("omp".into()),
                ver: Some("18.15.0".into()),
                features: vec!["edit".into(), "undo".into(), "send".into()],
            }
        );
        assert_eq!(
            serde_json::from_str::<Query>(r#"{"q":"future"}"#).unwrap(),
            Query::Unknown
        );
        let ev = Event::Send {
            sf: "s1".into(),
            id: "e".into(),
            text: "hi".into(),
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"ev":"send","sf":"s1","id":"e","text":"hi"}"#
        );
        assert_eq!(
            serde_json::to_string(&Event::Resize {
                sf: None,
                cols: 100,
                cell: None,
                visible: None
            })
            .unwrap(),
            r#"{"ev":"resize","cols":100}"#
        );
        let open: Open = serde_json::from_str(r#"{"id":"s1","mode":"warp"}"#).unwrap();
        assert_eq!(open.mode, Mode::Unknown);
        assert!(open.listen && !open.adopt);
        let close: Close = serde_json::from_str(r#"{"id":"s1"}"#).unwrap();
        assert!(close.keep);
    }
}
