//! Surface documents: the node trees a TSP program builds with `o`/`f`/`x`/`s`/`b` messages.
//!
//! A [`Document`] is the terminal's authoritative copy. Each [`Surface`] stores its nodes in an
//! id-keyed arena (ops address nodes by id, so an append deep in the scrollback costs the same
//! as one at the tail) and materializes a tree on demand ([`Surface::tree`], `Serialize`).
//!
//! Behavior beyond the spec text, decided here:
//! - Node kinds are not checked against a vocabulary: a kind this build does not know is kept
//!   (the renderer decides how to draw it). Spec-level rejections (unknown id, duplicate id,
//!   `before` not a child, move into own subtree, UTF-16 splice outside the text or inside a
//!   surrogate pair, node/depth limits) are reported per op.
//! - `text`/`splice` address the `text` prop of any node (not only the kinds of
//!   [`crate::wire::TEXT_KINDS`]); a non-string `text` prop is rejected.
//! - Unknown op *names* are ignored; a known op with bad arguments is rejected with its index.
//! - Surfaces closed with `keep:true` stay (up to [`MAX_CLOSED`], oldest dropped) so `adopt`
//!   can reopen them; frames to a closed or unknown surface are dropped with an `error`.
//! - A surface opened with `listen:false` gets no acks, errors or answers.
//! - `settle` is remembered per node ([`NodeRef::is_settled`]) and cleared on any later op on
//!   the node or a descendant, as the spec says; it never changes the tree.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::ser::{Serialize, SerializeMap, SerializeSeq, SerializeStruct, Serializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::stream::{Message, Verb};
use crate::wire::{
    Close, Event, Frame, Mode, Node, Op, Open, Query, REGION_DOCK, REGION_LAYER, RevealAt,
    ScrollBy, Stylesheet, TextMode, VERSION,
};

mod replay;

pub use replay::ReplayBlobs;

/// Nodes per surface, root included (spec limit).
pub const MAX_NODES: usize = 200_000;
/// Tree depth (spec limit).
pub const MAX_DEPTH: usize = 64;
/// One blob, decoded (spec limit).
pub const MAX_BLOB: usize = 16 << 20;
/// Blob bytes kept per document; past it the least recently used go.
pub const BLOB_BUDGET: usize = 64 << 20;
/// Stylesheet bytes per surface (spec limit).
pub const MAX_STYLE_BYTES: usize = 256 << 10;
/// Closed surfaces kept for `adopt` and replay.
pub const MAX_CLOSED: usize = 4;
/// Toasts kept per document (see [`Document::toasts`]).
pub const MAX_TOASTS: usize = 8;
/// A toast's lifetime when the program names none.
pub const DEFAULT_TOAST_TTL: Duration = Duration::from_millis(2400);
/// The longest toast lifetime honored (a program cannot pin one forever).
pub const MAX_TOAST_TTL: Duration = Duration::from_secs(60);
/// Blob ids a document remembers it lacks (see [`Document::missing_blobs`]).
pub const MAX_WANTED_BLOBS: usize = 256;

/// Why one op was rejected; its `Display` is the `msg` of the `error` event.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OpError {
    #[error("unknown id {0}")]
    UnknownId(String),
    #[error("duplicate id {0}")]
    DuplicateId(String),
    #[error("empty node id")]
    EmptyId,
    #[error("op id {op} does not match node id {node}")]
    IdMismatch { op: String, node: String },
    #[error("{before} is not a child of {parent}")]
    NotAChild { before: String, parent: String },
    #[error("cannot move {0} into its own subtree or before itself")]
    MoveIntoSelf(String),
    #[error("the surface root cannot be moved or deleted")]
    Root,
    #[error("node limit of {MAX_NODES} reached")]
    TooManyNodes,
    #[error("depth limit of {MAX_DEPTH} exceeded")]
    TooDeep,
    #[error("the text of {0} is not a string")]
    TextNotString(String),
    #[error("splice [{at}, {end}) of {id} is out of range or splits a surrogate pair")]
    BadSplice { id: String, at: u64, end: u64 },
    #[error("malformed {0} op")]
    Malformed(String),
}

#[derive(Debug, Clone)]
struct Entry {
    k: String,
    p: Map<String, Value>,
    parent: Option<String>,
    children: Vec<String>,
    settled: bool,
    /// When the props or text last changed (or the node was added).
    changed: Instant,
}

#[derive(Debug, Clone)]
struct Style {
    name: String,
    css: String,
}

/// One surface: its metadata and node tree.
#[derive(Debug, Clone)]
pub struct Surface {
    id: String,
    mode: Mode,
    title: Option<String>,
    role: Option<String>,
    font: Option<String>,
    listen: bool,
    closable: bool,
    open: bool,
    last_s: Option<u64>,
    focus: Option<String>,
    suspended: bool,
    nodes: HashMap<String, Entry>,
    styles: Vec<Style>,
}

/// A read-only view of one node.
#[derive(Clone, Copy)]
pub struct NodeRef<'a> {
    surface: &'a Surface,
    id: &'a str,
    entry: &'a Entry,
}

impl<'a> NodeRef<'a> {
    pub fn id(&self) -> &'a str {
        self.id
    }

    /// The node's kind (`k`).
    pub fn kind(&self) -> &'a str {
        &self.entry.k
    }

    pub fn props(&self) -> &'a Map<String, Value> {
        &self.entry.p
    }

    pub fn prop(&self, key: &str) -> Option<&'a Value> {
        self.entry.p.get(key)
    }

    pub fn str_prop(&self, key: &str) -> Option<&'a str> {
        self.entry.p.get(key).and_then(Value::as_str)
    }

    /// The primary text (`text` prop), if a string.
    pub fn text(&self) -> Option<&'a str> {
        self.str_prop("text")
    }

    pub fn is_settled(&self) -> bool {
        self.entry.settled
    }

    /// When the node's props or text last changed, or it was added; the receive time of that
    /// message (as passed to [`Document::apply`]).
    pub fn changed_at(&self) -> Instant {
        self.entry.changed
    }

    pub fn parent(&self) -> Option<NodeRef<'a>> {
        self.surface.node(self.entry.parent.as_deref()?)
    }

    pub fn child_ids(&self) -> &'a [String] {
        &self.entry.children
    }

    pub fn children(&self) -> impl Iterator<Item = NodeRef<'a>> + 'a {
        let surface = self.surface;
        self.entry
            .children
            .iter()
            .filter_map(move |c| surface.node(c))
    }
}

impl std::fmt::Debug for NodeRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeRef")
            .field("id", &self.id)
            .field("k", &self.entry.k)
            .finish()
    }
}

/// `at`/`end` (UTF-16 code units) as byte offsets of `s`; `None` when either is past the end
/// or falls inside a surrogate pair.
fn utf16_range(s: &str, at: usize, end: usize) -> Option<(usize, usize)> {
    let mut units = 0usize;
    let (mut b0, mut b1) = (None, None);
    for (b, ch) in s.char_indices() {
        if b0.is_none() && units == at {
            b0 = Some(b);
        }
        if b1.is_none() && units == end {
            b1 = Some(b);
        }
        if b1.is_some() {
            break;
        }
        units += ch.len_utf16();
    }
    if b0.is_none() && units == at {
        b0 = Some(s.len());
    }
    if b1.is_none() && units == end {
        b1 = Some(s.len());
    }
    Some((b0?, b1?))
}

fn strip_nulls(mut p: Map<String, Value>) -> Map<String, Value> {
    if p.values().any(Value::is_null) {
        p.retain(|_, v| !v.is_null());
    }
    p
}

impl Surface {
    fn new(open: &Open, now: Instant) -> Surface {
        let mut nodes = HashMap::new();
        nodes.insert(
            open.id.clone(),
            Entry {
                k: "col".to_owned(),
                p: Map::new(),
                parent: None,
                children: Vec::new(),
                settled: false,
                changed: now,
            },
        );
        Surface {
            id: open.id.clone(),
            mode: open.mode,
            title: open.title.clone(),
            role: open.role.clone(),
            font: open.font.clone(),
            listen: open.listen,
            closable: open.closable,
            open: true,
            last_s: None,
            focus: None,
            suspended: false,
            nodes,
            styles: Vec::new(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The `o` `role` (`omp.session`, …).
    pub fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }

    pub fn font(&self) -> Option<&str> {
        self.font.as_deref()
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether the program reads input (`listen`, default true).
    pub fn listens(&self) -> bool {
        self.listen
    }

    pub fn is_closable(&self) -> bool {
        self.closable
    }

    /// `s` of the last frame applied.
    pub fn last_seq(&self) -> Option<u64> {
        self.last_s
    }

    /// The focused node (`focus` op), if any.
    pub fn focus(&self) -> Option<&str> {
        self.focus.as_deref()
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    /// Node count, root included.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Installed stylesheets `(name, css)` in installation order.
    pub fn styles(&self) -> impl Iterator<Item = (&str, &str)> {
        self.styles
            .iter()
            .map(|s| (s.name.as_str(), s.css.as_str()))
    }

    pub fn node(&self, id: &str) -> Option<NodeRef<'_>> {
        let (key, entry) = self.nodes.get_key_value(id)?;
        Some(NodeRef {
            surface: self,
            id: key,
            entry,
        })
    }

    /// The root `col` (its id is the surface id).
    pub fn root(&self) -> NodeRef<'_> {
        // The root entry is created with the surface and `del`/`move` refuse it.
        self.node(&self.id).expect("surface root exists")
    }

    /// Visits the tree in document order (parent before children) with each node's depth.
    pub fn walk<'a>(&'a self, f: &mut impl FnMut(usize, NodeRef<'a>)) {
        fn go<'a>(n: NodeRef<'a>, depth: usize, f: &mut impl FnMut(usize, NodeRef<'a>)) {
            f(depth, n);
            for c in n.children() {
                go(c, depth + 1, f);
            }
        }
        go(self.root(), 0, f);
    }

    /// Every node (document order) accepted by `pred`.
    pub fn find_all<'a>(&'a self, pred: impl Fn(NodeRef<'a>) -> bool) -> Vec<NodeRef<'a>> {
        let mut found = Vec::new();
        self.walk(&mut |_, n| {
            if pred(n) {
                found.push(n);
            }
        });
        found
    }

    /// The first node (document order) accepted by `pred`.
    pub fn find<'a>(&'a self, pred: impl Fn(NodeRef<'a>) -> bool) -> Option<NodeRef<'a>> {
        self.find_all(pred).into_iter().next()
    }

    /// The tree as owned wire nodes.
    pub fn tree(&self) -> Node {
        fn build(n: NodeRef<'_>) -> Node {
            Node {
                id: n.id().to_owned(),
                k: n.kind().to_owned(),
                p: n.props().clone(),
                c: n.children().map(build).collect(),
            }
        }
        build(self.root())
    }

    fn depth_of(&self, id: &str) -> Option<usize> {
        let mut depth = 0;
        let mut cur = self.nodes.get(id)?;
        while let Some(parent) = &cur.parent {
            depth += 1;
            cur = self.nodes.get(parent)?;
        }
        Some(depth)
    }

    fn height_of(&self, id: &str) -> usize {
        self.nodes.get(id).map_or(0, |e| {
            e.children
                .iter()
                .map(|c| 1 + self.height_of(c))
                .max()
                .unwrap_or(0)
        })
    }

    /// Clears the settle hint on `id` and every ancestor.
    fn touch(&mut self, id: &str) {
        let mut cur = id.to_owned();
        while let Some(e) = self.nodes.get_mut(&cur) {
            e.settled = false;
            match &e.parent {
                Some(p) => cur = p.clone(),
                None => break,
            }
        }
    }

    fn entry_mut(&mut self, id: &str) -> Result<&mut Entry, OpError> {
        self.nodes
            .get_mut(id)
            .ok_or_else(|| OpError::UnknownId(id.to_owned()))
    }

    fn validate_subtree<'n>(
        &self,
        node: &'n Node,
        depth: usize,
        seen: &mut HashSet<&'n str>,
    ) -> Result<(), OpError> {
        if node.id.is_empty() {
            return Err(OpError::EmptyId);
        }
        if depth > MAX_DEPTH {
            return Err(OpError::TooDeep);
        }
        if self.nodes.contains_key(&node.id) || !seen.insert(node.id.as_str()) {
            return Err(OpError::DuplicateId(node.id.clone()));
        }
        if self.nodes.len() + seen.len() > MAX_NODES {
            return Err(OpError::TooManyNodes);
        }
        for child in &node.c {
            self.validate_subtree(child, depth + 1, seen)?;
        }
        Ok(())
    }

    fn insert_tree(&mut self, node: Node, parent: &str, now: Instant) {
        let Node { id, k, p, c } = node;
        let children = c.iter().map(|n| n.id.clone()).collect();
        self.nodes.insert(
            id.clone(),
            Entry {
                k,
                p: strip_nulls(p),
                parent: Some(parent.to_owned()),
                children,
                settled: false,
                changed: now,
            },
        );
        for child in c {
            self.insert_tree(child, &id, now);
        }
    }

    /// Removes `id` and its descendants from the arena (not from the parent's child list).
    fn drop_subtree(&mut self, id: &str) {
        let mut stack = vec![id.to_owned()];
        while let Some(cur) = stack.pop() {
            if let Some(e) = self.nodes.remove(&cur) {
                stack.extend(e.children);
            }
            if self.focus.as_deref() == Some(cur.as_str()) {
                self.focus = None;
            }
        }
    }

    fn unlink(&mut self, id: &str, parent: &str) {
        if let Some(p) = self.nodes.get_mut(parent)
            && let Some(pos) = p.children.iter().rposition(|c| c == id)
        {
            p.children.remove(pos);
        }
    }

    fn link(&mut self, id: &str, parent: &str, before: Option<&str>) {
        if let Some(p) = self.nodes.get_mut(parent) {
            match before.and_then(|b| p.children.iter().position(|c| c == b)) {
                Some(pos) => p.children.insert(pos, id.to_owned()),
                None => p.children.push(id.to_owned()),
            }
        }
    }

    /// Applies one op received at `now`. Public for the renderer's own mirrors and tests.
    pub fn apply_op(&mut self, op: Op, now: Instant) -> Result<(), OpError> {
        match op {
            Op::Add {
                id,
                parent,
                before,
                node,
            } => {
                if id != node.id {
                    return Err(OpError::IdMismatch {
                        op: id,
                        node: node.id,
                    });
                }
                let parent_depth = self
                    .depth_of(&parent)
                    .ok_or_else(|| OpError::UnknownId(parent.clone()))?;
                if let Some(b) = &before
                    && !self.nodes[&parent].children.iter().any(|c| c == b)
                {
                    return Err(OpError::NotAChild {
                        before: b.clone(),
                        parent,
                    });
                }
                self.validate_subtree(&node, parent_depth + 1, &mut HashSet::new())?;
                self.touch(&parent);
                self.insert_tree(node, &parent, now);
                self.link(&id, &parent, before.as_deref());
                Ok(())
            }
            Op::Set { id, props } => {
                let e = self.entry_mut(&id)?;
                for (k, v) in props {
                    if v.is_null() {
                        e.p.remove(&k);
                    } else {
                        e.p.insert(k, v);
                    }
                }
                e.changed = now;
                self.touch(&id);
                Ok(())
            }
            Op::Text { id, mode, text } => {
                let e = self.entry_mut(&id)?;
                match (mode, e.p.get_mut("text")) {
                    (TextMode::Append, Some(Value::String(s))) => s.push_str(&text),
                    (TextMode::Append, Some(Value::Null) | None) | (TextMode::Replace, _) => {
                        e.p.insert("text".to_owned(), Value::String(text));
                    }
                    (TextMode::Append, Some(_)) => return Err(OpError::TextNotString(id)),
                }
                e.changed = now;
                self.touch(&id);
                Ok(())
            }
            Op::Splice { id, at, del, text } => {
                let e = self.entry_mut(&id)?;
                let end = at.saturating_add(del);
                let bad = || OpError::BadSplice {
                    id: id.clone(),
                    at,
                    end,
                };
                match e.p.get_mut("text") {
                    Some(Value::String(s)) => {
                        let (a, b) = utf16_range(
                            s,
                            usize::try_from(at).map_err(|_| bad())?,
                            usize::try_from(end).map_err(|_| bad())?,
                        )
                        .ok_or_else(bad)?;
                        s.replace_range(a..b, &text);
                    }
                    Some(Value::Null) | None => {
                        if at != 0 || del != 0 {
                            return Err(bad());
                        }
                        e.p.insert("text".to_owned(), Value::String(text));
                    }
                    Some(_) => return Err(OpError::TextNotString(id)),
                }
                e.changed = now;
                self.touch(&id);
                Ok(())
            }
            Op::Move { id, parent, before } => {
                if id == self.id {
                    return Err(OpError::Root);
                }
                if !self.nodes.contains_key(&id) {
                    return Err(OpError::UnknownId(id));
                }
                let parent_depth = self
                    .depth_of(&parent)
                    .ok_or_else(|| OpError::UnknownId(parent.clone()))?;
                if before.as_deref() == Some(id.as_str()) {
                    return Err(OpError::MoveIntoSelf(id));
                }
                if let Some(b) = &before
                    && !self.nodes[&parent].children.iter().any(|c| c == b)
                {
                    return Err(OpError::NotAChild {
                        before: b.clone(),
                        parent,
                    });
                }
                // Not into its own subtree: `id` must not be `parent` or one of its ancestors.
                let mut cur = Some(parent.as_str());
                while let Some(c) = cur {
                    if c == id {
                        return Err(OpError::MoveIntoSelf(id));
                    }
                    cur = self.nodes.get(c).and_then(|e| e.parent.as_deref());
                }
                if parent_depth + 1 + self.height_of(&id) > MAX_DEPTH {
                    return Err(OpError::TooDeep);
                }
                let old_parent = self.nodes[&id].parent.clone().unwrap_or_default();
                self.touch(&old_parent);
                self.touch(&parent);
                self.unlink(&id, &old_parent);
                self.link(&id, &parent, before.as_deref());
                if let Some(e) = self.nodes.get_mut(&id) {
                    e.parent = Some(parent);
                }
                self.touch(&id);
                Ok(())
            }
            Op::Del { id } => {
                if id == self.id {
                    return Err(OpError::Root);
                }
                let parent = self
                    .nodes
                    .get(&id)
                    .ok_or_else(|| OpError::UnknownId(id.clone()))?
                    .parent
                    .clone()
                    .unwrap_or_default();
                self.touch(&parent);
                self.unlink(&id, &parent);
                self.drop_subtree(&id);
                Ok(())
            }
            Op::Settle { id } => {
                self.entry_mut(&id)?.settled = true;
                Ok(())
            }
            Op::Focus { id } => {
                if let Some(id) = &id
                    && !self.nodes.contains_key(id)
                {
                    return Err(OpError::UnknownId(id.clone()));
                }
                self.focus = id;
                Ok(())
            }
            Op::Reveal {
                id,
                at: RevealAt::Start | RevealAt::End | RevealAt::Nearest,
            } => self.entry_mut(&id).map(|_| ()),
            Op::Scroll {
                id,
                by:
                    ScrollBy::LineUp
                    | ScrollBy::LineDown
                    | ScrollBy::PageUp
                    | ScrollBy::PageDown
                    | ScrollBy::Start
                    | ScrollBy::End,
            } => self.entry_mut(&id).map(|_| ()),
            Op::Suspend => {
                self.suspended = true;
                Ok(())
            }
            Op::Resume => {
                self.suspended = false;
                Ok(())
            }
            op @ Op::Unknown(_) => {
                if op.is_malformed() {
                    Err(OpError::Malformed(op.name().to_owned()))
                } else {
                    Ok(())
                }
            }
        }
    }

    /// `x keep:true`: live-only regions (`dock`, `layer`, focus) go, `main` stays.
    fn close(&mut self) {
        for region in [REGION_DOCK, REGION_LAYER] {
            if self
                .nodes
                .get(region)
                .is_some_and(|e| e.parent.as_deref() == Some(self.id.as_str()))
            {
                let root = self.id.clone();
                self.unlink(region, &root);
                self.drop_subtree(region);
            }
        }
        self.focus = None;
        self.suspended = false;
        self.open = false;
    }
}

struct NodeSer<'a>(NodeRef<'a>);

impl Serialize for NodeSer<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let n = self.0;
        let has_p = !n.props().is_empty();
        let has_c = !n.child_ids().is_empty();
        let mut map = s.serialize_map(Some(2 + usize::from(has_p) + usize::from(has_c)))?;
        map.serialize_entry("id", n.id())?;
        map.serialize_entry("k", n.kind())?;
        if has_p {
            map.serialize_entry("p", n.props())?;
        }
        if has_c {
            map.serialize_entry("c", &ChildrenSer(n))?;
        }
        map.end()
    }
}

struct ChildrenSer<'a>(NodeRef<'a>);

impl Serialize for ChildrenSer<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.child_ids().len()))?;
        for c in self.0.children() {
            seq.serialize_element(&NodeSer(c))?;
        }
        seq.end()
    }
}

impl Serialize for Surface {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Surface", 11)?;
        st.serialize_field("id", &self.id)?;
        st.serialize_field("mode", &self.mode)?;
        st.serialize_field("title", &self.title)?;
        st.serialize_field("role", &self.role)?;
        st.serialize_field("open", &self.open)?;
        st.serialize_field("listen", &self.listen)?;
        st.serialize_field("seq", &self.last_s)?;
        st.serialize_field("focus", &self.focus)?;
        st.serialize_field("suspended", &self.suspended)?;
        st.serialize_field("nodes", &self.nodes.len())?;
        st.serialize_field("root", &NodeSer(self.root()))?;
        st.end()
    }
}

/// A stored blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub mime: Option<String>,
    /// Shared: the session host hands one out without copying it under its lock.
    pub bytes: Arc<[u8]>,
}

#[derive(Debug, Default)]
struct BlobStore {
    map: HashMap<String, (Blob, u64)>,
    total: usize,
    tick: u64,
}

impl BlobStore {
    fn insert(&mut self, id: String, blob: Blob) {
        self.tick += 1;
        self.total += blob.bytes.len();
        if let Some((old, _)) = self.map.insert(id.clone(), (blob, self.tick)) {
            self.total -= old.bytes.len();
        }
        while self.total > BLOB_BUDGET && self.map.len() > 1 {
            let Some(oldest) = self
                .map
                .iter()
                .filter(|(k, _)| **k != id)
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some((b, _)) = self.map.remove(&oldest) {
                self.total -= b.bytes.len();
            }
        }
    }
}

/// What applying one message produced for the terminal to send back.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Applied {
    /// `(surface, s)` of every frame applied to a listening surface (in message order; the
    /// highest `s` of a surface covers the earlier ones).
    pub acks: Vec<(String, u64)>,
    /// One `error` event per rejected op (with its index) or message, and the `gone` answer
    /// of an `adopt` that found nothing. Never about a `listen:false` surface.
    pub errors: Vec<Event>,
}

/// What a program said about itself in its last `hello` query.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Program {
    pub app: Option<String>,
    pub ver: Option<String>,
    /// `edit`, `undo`, `send`: what the program applies. `send` is required before sending a
    /// [`Event::Send`].
    pub features: Vec<String>,
}

impl Program {
    pub fn supports(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }
}

/// One `toast` node an inline program created (omp appends it and deletes it with the next
/// frame, so a renderer reading the tree would miss it).
#[derive(Debug, Clone)]
pub struct ToastRecord {
    /// 1, 2, 3, …: the record's identity, never reused within a document.
    pub seq: u64,
    pub text: String,
    pub sub: Option<String>,
    /// How long it shows, counted from `at`.
    pub ttl: Duration,
    /// When the creating frame was applied.
    pub at: Instant,
    sf: String,
    parent: String,
    node: Node,
}

/// Remembers into `wanted` the blob ids `props` refer to that `blobs` does not hold.
fn want_blobs(wanted: &mut HashSet<String>, blobs: &BlobStore, props: &Map<String, Value>) {
    let mut ids = HashSet::new();
    replay::note_blob_ids(props, &mut ids);
    for id in ids {
        if wanted.len() < MAX_WANTED_BLOBS && !blobs.map.contains_key(&id) {
            wanted.insert(id);
        }
    }
}

fn want_tree_blobs(wanted: &mut HashSet<String>, blobs: &BlobStore, node: &Node) {
    want_blobs(wanted, blobs, &node.p);
    for child in &node.c {
        want_tree_blobs(wanted, blobs, child);
    }
}

/// Pushes `record` onto the ring: expired entries go first, then the oldest past the bound.
fn record_toast(ring: &mut VecDeque<ToastRecord>, record: ToastRecord, now: Instant) {
    ring.retain(|t| !t.expired(now));
    ring.push_back(record);
    while ring.len() > MAX_TOASTS {
        ring.pop_front();
    }
}

impl ToastRecord {
    fn new(seq: u64, sf: &str, parent: String, node: Node, now: Instant) -> ToastRecord {
        let ttl = node
            .p
            .get("ttl")
            .and_then(Value::as_f64)
            .filter(|ms| ms.is_finite() && *ms > 0.0)
            .map_or(DEFAULT_TOAST_TTL, |ms| {
                Duration::from_secs_f64(ms / 1000.0).min(MAX_TOAST_TTL)
            });
        // A replayed toast carries its age (see `replay::advance_timers`).
        let age = node
            .p
            .get("age")
            .and_then(Value::as_f64)
            .filter(|ms| ms.is_finite() && *ms > 0.0)
            .map_or(Duration::ZERO, |ms| {
                Duration::from_secs_f64(ms / 1000.0).min(MAX_TOAST_TTL)
            });
        let (ttl, at) = if age >= ttl {
            (Duration::ZERO, now)
        } else {
            (ttl, now.checked_sub(age).unwrap_or(now))
        };
        let text = |key: &str| node.p.get(key).and_then(Value::as_str).map(str::to_owned);
        ToastRecord {
            seq,
            text: text("text").unwrap_or_default(),
            sub: text("sub"),
            ttl,
            at,
            sf: sf.to_owned(),
            parent,
            node,
        }
    }

    /// Whether the toast has run out at `now`.
    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) >= self.ttl
    }
}

/// Appends to `out` every `toast` node of the subtree `node` (attached under `parent`).
fn collect_toasts(node: &Node, parent: &str, out: &mut Vec<(String, Node)>) {
    if node.k == "toast" {
        out.push((parent.to_owned(), node.clone()));
    }
    for child in &node.c {
        collect_toasts(child, &node.id, out);
    }
}

/// The terminal's copy of every surface of one pane, plus its blobs.
#[derive(Debug, Default)]
pub struct Document {
    surfaces: Vec<Surface>,
    blobs: BlobStore,
    program: Option<Program>,
    revision: u64,
    toasts: VecDeque<ToastRecord>,
    toast_seq: u64,
    wanted: HashSet<String>,
}

impl Document {
    pub fn new() -> Document {
        Document::default()
    }

    /// Bumped by every applied message that changed anything (surface, tree, stylesheet,
    /// blob, program); a renderer repaints when it moved.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The program's last `hello` query that listed protocol version 1 (`None` before one).
    pub fn program(&self) -> Option<&Program> {
        self.program.as_ref()
    }

    /// Records the program's identity (what [`Document::apply`] does with a `hello` query).
    pub fn set_program(&mut self, program: Program) {
        self.program = Some(program);
        self.revision += 1;
    }

    /// Every surface, oldest first (closed ones included).
    pub fn surfaces(&self) -> &[Surface] {
        &self.surfaces
    }

    /// The first open surface with `id`.
    pub fn surface(&self, id: &str) -> Option<&Surface> {
        self.surfaces.iter().find(|s| s.open && s.id == id)
    }

    /// The open inline surface (an omp session).
    pub fn inline(&self) -> Option<&Surface> {
        self.surfaces
            .iter()
            .find(|s| s.open && s.mode == Mode::Inline)
    }

    /// The toasts that have not expired at `now`, oldest first (at most [`MAX_TOASTS`] are
    /// kept). A toast outlives the node omp deletes right after creating it.
    pub fn toasts(&self, now: Instant) -> impl Iterator<Item = &ToastRecord> {
        self.toasts.iter().filter(move |t| !t.expired(now))
    }

    /// Blob ids (lowercase hex SHA-256) a node refers to that this document holds no blob
    /// for: a peer that sent the blob before this document existed is the only way to get
    /// one. At most [`MAX_WANTED_BLOBS`] are remembered; an id leaves the list when its blob
    /// arrives.
    pub fn missing_blobs(&self) -> Vec<String> {
        self.wanted.iter().cloned().collect()
    }

    /// Whether [`Document::missing_blobs`] is not empty.
    pub fn wants_blobs(&self) -> bool {
        !self.wanted.is_empty()
    }
    /// Gives up on `id`: the host does not hold it either (a later node that refers to it asks
    /// again).
    pub fn forget_blob(&mut self, id: &str) {
        self.wanted.remove(id);
    }

    /// Keeps only the wanted ids some live node still refers to (a deleted node's blob is no
    /// longer missing).
    fn prune_wanted(&mut self) {
        if self.wanted.is_empty() {
            return;
        }
        let mut live = HashSet::new();
        for s in self.surfaces.iter().filter(|s| s.open) {
            for entry in s.nodes.values() {
                replay::note_blob_ids(&entry.p, &mut live);
            }
        }
        self.wanted.retain(|id| live.contains(id));
    }

    /// Stores a blob a peer supplied for `id` after checking that `bytes` hash to it; the
    /// revision moves so a renderer repaints. `false` (nothing stored) when the bytes do not
    /// match or are over [`MAX_BLOB`].
    pub fn put_blob(&mut self, id: &str, mime: Option<String>, bytes: Vec<u8>) -> bool {
        if bytes.len() > MAX_BLOB || blob_id(&bytes) != id {
            return false;
        }
        self.put_verified_blob(id, mime, bytes.into())
    }

    /// [`Document::put_blob`] for bytes the caller already hashed to `id` (outside any lock that
    /// guards this document).
    pub fn put_verified_blob(&mut self, id: &str, mime: Option<String>, bytes: Arc<[u8]>) -> bool {
        if bytes.len() > MAX_BLOB {
            return false;
        }
        self.wanted.remove(id);
        self.blobs.insert(id.to_owned(), Blob { mime, bytes });
        self.revision += 1;
        true
    }

    /// Whether any surface is open.
    pub fn has_open_surface(&self) -> bool {
        self.surfaces.iter().any(|s| s.open)
    }

    /// A stored blob by id (lowercase hex SHA-256).
    pub fn blob(&self, id: &str) -> Option<&Blob> {
        self.blobs.map.get(id).map(|(b, _)| b)
    }

    /// The ids in `ids` this document holds, in the order asked; asking counts as a use.
    pub fn blobs_have(&mut self, ids: &[String]) -> Vec<String> {
        let mut have = Vec::new();
        for id in ids {
            let key = id.to_ascii_lowercase();
            if self.blobs.map.contains_key(&key) {
                self.blobs.tick += 1;
                let tick = self.blobs.tick;
                if let Some(entry) = self.blobs.map.get_mut(&key) {
                    entry.1 = tick;
                }
                have.push(id.clone());
            }
        }
        have
    }

    /// Applies one message (`o`, `f`, `x`, `s`, `b`, and the `hello` query) received at `now`;
    /// other verbs change nothing.
    pub fn apply(&mut self, msg: &Message, now: Instant) -> Applied {
        match msg.verb {
            Verb::Open => self.open(&msg.body, now),
            Verb::Frame => self.frame(&msg.body, now),
            Verb::Close => self.close(&msg.body),
            Verb::Style => self.style(&msg.body),
            Verb::Blob => self.blob_message(msg),
            Verb::Query => self.query(&msg.body),
            Verb::Palette | Verb::Reply | Verb::Event | Verb::Unknown(_) => Applied::default(),
        }
    }

    /// Only the `hello` query is remembered; answering queries is the terminal's job.
    fn query(&mut self, body: &[u8]) -> Applied {
        if let Ok(Query::Hello {
            v,
            app,
            ver,
            features,
        }) = serde_json::from_slice(body)
            && v.contains(&VERSION)
        {
            self.set_program(Program { app, ver, features });
        }
        Applied::default()
    }

    fn rejected(msg: impl Into<String>) -> Applied {
        Applied {
            acks: Vec::new(),
            errors: vec![Event::error(msg)],
        }
    }

    fn open_index(&self, id: &str) -> Option<usize> {
        self.surfaces.iter().position(|s| s.open && s.id == id)
    }

    /// Ends every open surface accepted by `pred`: screen surfaces are discarded, the others
    /// closed as if `keep:true`.
    fn retire(&mut self, pred: impl Fn(&Surface) -> bool) {
        self.surfaces.retain_mut(|s| {
            if s.open && pred(s) {
                if s.mode == Mode::Screen {
                    return false;
                }
                s.close();
            }
            true
        });
    }

    fn trim_closed(&mut self) {
        let mut closed = self.surfaces.iter().filter(|s| !s.open).count();
        self.surfaces.retain(|s| {
            if !s.open && closed > MAX_CLOSED {
                closed -= 1;
                false
            } else {
                true
            }
        });
    }

    fn open(&mut self, body: &[u8], now: Instant) -> Applied {
        let open: Open = match serde_json::from_slice(body) {
            Ok(o) => o,
            Err(e) => return Document::rejected(format!("malformed o body: {e}")),
        };
        if open.id.is_empty() {
            return Document::rejected("an o message needs an id");
        }
        if open.mode == Mode::Unknown {
            return Document::rejected("unknown surface mode");
        }
        if open.adopt {
            return self.adopt(&open);
        }
        match open.mode {
            Mode::Inline => self.retire(|s| matches!(s.mode, Mode::Inline | Mode::Screen)),
            Mode::Screen => self.retire(|s| s.mode == Mode::Screen),
            Mode::Flow => self.retire(|s| s.mode == Mode::Flow),
            Mode::Unknown => {}
        }
        self.surfaces.push(Surface::new(&open, now));
        self.trim_closed();
        self.revision += 1;
        Applied::default()
    }

    /// `o` with `adopt:true`: reopens the newest closed inline surface with that id.
    fn adopt(&mut self, open: &Open) -> Applied {
        if self
            .surfaces
            .iter()
            .any(|s| s.open && s.mode == Mode::Inline && s.id == open.id)
        {
            return Applied::default();
        }
        let Some(pos) = self
            .surfaces
            .iter()
            .rposition(|s| !s.open && s.mode == Mode::Inline && s.id == open.id)
        else {
            return if open.listen {
                Applied {
                    acks: Vec::new(),
                    errors: vec![Event::Gone {
                        sf: None,
                        ids: vec![open.id.clone()],
                    }],
                }
            } else {
                Applied::default()
            };
        };
        let mut surface = self.surfaces.remove(pos);
        self.retire(|s| matches!(s.mode, Mode::Inline | Mode::Screen));
        surface.open = true;
        surface.listen = open.listen;
        surface.closable = open.closable;
        if open.title.is_some() {
            surface.title.clone_from(&open.title);
        }
        if open.role.is_some() {
            surface.role.clone_from(&open.role);
        }
        if open.font.is_some() {
            surface.font.clone_from(&open.font);
        }
        self.surfaces.push(surface);
        self.revision += 1;
        Applied::default()
    }

    fn frame(&mut self, body: &[u8], now: Instant) -> Applied {
        let frame: Frame = match serde_json::from_slice(body) {
            Ok(f) => f,
            Err(e) => return Document::rejected(format!("malformed f body: {e}")),
        };
        let Frame { sf, s, ops } = frame;
        let Some(idx) = self.open_index(&sf) else {
            return Applied {
                acks: Vec::new(),
                errors: vec![Event::Error {
                    sf: Some(sf.clone()),
                    s: Some(s),
                    op: None,
                    msg: format!("unknown or closed surface {sf}"),
                }],
            };
        };
        let surface = &mut self.surfaces[idx];
        let listen = surface.listen;
        let mut applied = Applied::default();
        let mut removes = false;
        for (i, op) in ops.into_iter().enumerate() {
            let mut created = Vec::new();
            let mut retoast: Option<String> = None;
            removes |= matches!(op, Op::Del { .. });
            match &op {
                Op::Add { parent, node, .. } => {
                    collect_toasts(node, parent, &mut created);
                    want_tree_blobs(&mut self.wanted, &self.blobs, node);
                }
                Op::Set { id, props } => {
                    want_blobs(&mut self.wanted, &self.blobs, props);
                    retoast = Some(id.clone());
                }
                Op::Text { id, .. } | Op::Splice { id, .. } => retoast = Some(id.clone()),
                _ => {}
            }
            match surface.apply_op(op, now) {
                Ok(()) => {
                    // omp keeps one `toast` node and only changes its text for the next
                    // toast: every change is a new toast to show.
                    if let Some(id) = retoast.take()
                        && let Some(e) = surface.nodes.get(&id)
                        && e.k == "toast"
                    {
                        let mut p = e.p.clone();
                        // The age of a replayed node belongs to its old text.
                        p.remove("age");
                        let node = Node {
                            id: id.clone(),
                            k: e.k.clone(),
                            p,
                            c: Vec::new(),
                        };
                        created.push((e.parent.clone().unwrap_or_default(), node));
                    }
                    for (parent, node) in created {
                        self.toast_seq += 1;
                        record_toast(
                            &mut self.toasts,
                            ToastRecord::new(self.toast_seq, &sf, parent, node, now),
                            now,
                        );
                    }
                }
                Err(e) if listen => {
                    applied.errors.push(Event::Error {
                        sf: Some(sf.clone()),
                        s: Some(s),
                        op: u32::try_from(i).ok(),
                        msg: e.to_string(),
                    });
                }
                Err(_) => {}
            }
        }
        surface.last_s = Some(s);
        if listen {
            applied.acks.push((sf, s));
        }
        if removes {
            self.prune_wanted();
        }
        self.revision += 1;
        applied
    }

    fn close(&mut self, body: &[u8]) -> Applied {
        let close: Close = match serde_json::from_slice(body) {
            Ok(c) => c,
            Err(e) => return Document::rejected(format!("malformed x body: {e}")),
        };
        let Some(idx) = self.open_index(&close.id) else {
            return Document::rejected(format!("unknown or closed surface {}", close.id));
        };
        if close.keep && self.surfaces[idx].mode != Mode::Screen {
            self.surfaces[idx].close();
            self.trim_closed();
        } else {
            self.surfaces.remove(idx);
        }
        self.revision += 1;
        Applied::default()
    }

    fn style(&mut self, body: &[u8]) -> Applied {
        let sheet: Stylesheet = match serde_json::from_slice(body) {
            Ok(s) => s,
            Err(e) => return Document::rejected(format!("malformed s body: {e}")),
        };
        let Some(idx) = self.open_index(&sheet.sf) else {
            return Document::rejected(format!("unknown or closed surface {}", sheet.sf));
        };
        let surface = &mut self.surfaces[idx];
        let others: usize = surface
            .styles
            .iter()
            .filter(|s| s.name != sheet.name)
            .map(|s| s.css.len())
            .sum();
        if others + sheet.css.len() > MAX_STYLE_BYTES {
            return Applied {
                acks: Vec::new(),
                errors: vec![Event::Error {
                    sf: Some(sheet.sf),
                    s: None,
                    op: None,
                    msg: format!("stylesheets over {MAX_STYLE_BYTES} bytes"),
                }],
            };
        }
        match surface.styles.iter_mut().find(|s| s.name == sheet.name) {
            Some(existing) => existing.css = sheet.css,
            None => surface.styles.push(Style {
                name: sheet.name,
                css: sheet.css,
            }),
        }
        self.revision += 1;
        Applied::default()
    }

    fn blob_message(&mut self, msg: &Message) -> Applied {
        // 4 base64 characters per 3 bytes, plus padding.
        if msg.body.len() > MAX_BLOB.div_ceil(3) * 4 + 4 {
            return Document::rejected("blob is larger than 16 MiB");
        }
        let bytes = match STANDARD.decode(&msg.body) {
            Ok(b) => b,
            Err(e) => return Document::rejected(format!("blob is not valid base64: {e}")),
        };
        if bytes.len() > MAX_BLOB {
            return Document::rejected("blob is larger than 16 MiB");
        }
        // The blob's id is the hash of its bytes; an `id` parameter is ignored (spec).
        let id = hex(&Sha256::digest(&bytes));
        self.wanted.remove(&id);
        self.blobs.insert(
            id,
            Blob {
                mime: msg.param("mime").map(str::to_owned),
                bytes: bytes.into(),
            },
        );
        self.revision += 1;
        Applied::default()
    }
}

/// The `b` message(s) that store `bytes` in a [`Document`] fed with them (one APC string, or a
/// `c=<chunk_id>` chunk sequence for a big blob), for a consumer that learns of a blob out of
/// band. `chunk_id` must not collide with another chunk sequence in flight.
pub fn encode_blob(mime: Option<&str>, bytes: &[u8], chunk_id: &str) -> Vec<u8> {
    let params: Vec<(&str, &str)> = mime.map(|m| ("mime", m)).into_iter().collect();
    crate::stream::encode_chunked(
        Verb::Blob,
        &params,
        STANDARD.encode(bytes).as_bytes(),
        crate::wire::DEFAULT_APC_LIMIT,
        chunk_id,
    )
}

/// The id of a blob: the lowercase hex SHA-256 of its bytes.
pub fn blob_id(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    s
}

struct BlobsSer<'a>(&'a BlobStore);

impl Serialize for BlobsSer<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut ids: Vec<&String> = self.0.map.keys().collect();
        ids.sort();
        let mut seq = s.serialize_seq(Some(ids.len()))?;
        for id in ids {
            let (blob, _) = &self.0.map[id];
            seq.serialize_element(&serde_json::json!({
                "id": id,
                "mime": blob.mime,
                "len": blob.bytes.len(),
            }))?;
        }
        seq.end()
    }
}

impl Serialize for Document {
    /// `{"surfaces":[{id,mode,title,role,open,listen,seq,focus,suspended,nodes,root:{id,k,p?,c?}}],
    /// "blobs":[{id,mime,len}]}`; blob bytes are never serialized.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Document", 2)?;
        st.serialize_field("surfaces", &self.surfaces)?;
        st.serialize_field("blobs", &BlobsSer(&self.blobs))?;
        st.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::encode_message;
    use serde_json::json;

    fn msg(verb: Verb, body: &Value) -> Message {
        Message {
            verb,
            params: Vec::new(),
            body: serde_json::to_vec(body).unwrap(),
        }
    }

    fn open_inline(doc: &mut Document, id: &str) {
        let a = doc.apply(
            &msg(
                Verb::Open,
                &json!({"id": id, "mode": "inline", "role": "omp.session"}),
            ),
            Instant::now(),
        );
        assert_eq!(a, Applied::default());
        frame(
            doc,
            id,
            1,
            json!([
                ["add", "main", id, null, {"id": "main", "k": "col"}],
                ["add", "dock", id, null, {"id": "dock", "k": "col"}],
                ["add", "layer", id, null, {"id": "layer", "k": "col"}],
            ]),
        );
    }

    fn frame(doc: &mut Document, sf: &str, s: u64, ops: Value) -> Applied {
        doc.apply(
            &msg(Verb::Frame, &json!({"sf": sf, "s": s, "ops": ops})),
            Instant::now(),
        )
    }

    fn text_of(doc: &Document, sf: &str, id: &str) -> Option<String> {
        doc.surface(sf)?.node(id)?.text().map(str::to_owned)
    }

    fn child_ids(doc: &Document, sf: &str, id: &str) -> Vec<String> {
        doc.surface(sf)
            .unwrap()
            .node(id)
            .unwrap()
            .child_ids()
            .to_vec()
    }

    #[test]
    fn open_frame_ack_and_serialize_the_whole_tree() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        let a = frame(
            &mut doc,
            "s1",
            2,
            json!([
                ["add", "m1", "main", null, {"id": "m1", "k": "md", "p": {"text": "hello", "tone": null}}],
                ["add", "ed", "dock", null, {"id": "ed", "k": "editor", "p": {"sendable": true}}],
                ["focus", "ed"],
            ]),
        );
        assert_eq!(a.acks, [("s1".to_owned(), 2)]);
        assert!(a.errors.is_empty());
        let s = doc.surface("s1").unwrap();
        assert_eq!(
            (s.role(), s.last_seq(), s.focus()),
            (Some("omp.session"), Some(2), Some("ed"))
        );
        assert!(
            s.node("m1").unwrap().prop("tone").is_none(),
            "null props are dropped"
        );
        let editor = s.find(|n| n.kind() == "editor" && n.prop("sendable") == Some(&json!(true)));
        assert_eq!(editor.unwrap().id(), "ed");
        let doc_json = serde_json::to_value(&doc).unwrap();
        assert_eq!(
            doc_json["surfaces"][0]["root"],
            json!({"id": "s1", "k": "col", "c": [
                {"id": "main", "k": "col", "c": [{"id": "m1", "k": "md", "p": {"text": "hello"}}]},
                {"id": "dock", "k": "col", "c": [{"id": "ed", "k": "editor", "p": {"sendable": true}}]},
                {"id": "layer", "k": "col"},
            ]})
        );
        assert_eq!(doc_json["surfaces"][0]["seq"], 2);
        assert_eq!(
            serde_json::to_value(s.tree()).unwrap(),
            doc_json["surfaces"][0]["root"]
        );
    }

    #[test]
    fn add_before_places_the_node_and_rejects_bad_anchors_atomically() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        frame(
            &mut doc,
            "s1",
            2,
            json!([
                ["add", "a", "main", null, {"id": "a", "k": "text"}],
                ["add", "c", "main", null, {"id": "c", "k": "text"}],
                ["add", "b", "main", "c", {"id": "b", "k": "text"}],
            ]),
        );
        assert_eq!(child_ids(&doc, "s1", "main"), ["a", "b", "c"]);
        let a = frame(
            &mut doc,
            "s1",
            3,
            json!([
                ["add", "x", "main", "nope", {"id": "x", "k": "text"}],
                ["add", "dup", "main", null, {"id": "dup", "k": "col", "c": [{"id": "a", "k": "text"}]}],
                ["add", "y", "ghost", null, {"id": "y", "k": "text"}],
                ["add", "z", "main", null, {"id": "w", "k": "text"}],
                ["add", "ok", "main", null, {"id": "ok", "k": "col", "c": [{"id": "ok2", "k": "text"}]}],
            ]),
        );
        let ops: Vec<_> = a
            .errors
            .iter()
            .map(|e| match e {
                Event::Error { op, s, sf, msg } => (*op, *s, sf.clone(), msg.clone()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            ops,
            [
                (
                    Some(0),
                    Some(3),
                    Some("s1".to_owned()),
                    "nope is not a child of main".to_owned()
                ),
                (
                    Some(1),
                    Some(3),
                    Some("s1".to_owned()),
                    "duplicate id a".to_owned()
                ),
                (
                    Some(2),
                    Some(3),
                    Some("s1".to_owned()),
                    "unknown id ghost".to_owned()
                ),
                (
                    Some(3),
                    Some(3),
                    Some("s1".to_owned()),
                    "op id z does not match node id w".to_owned()
                ),
            ]
        );
        // The rejected subtree left nothing behind (not even its fresh `dup` node); the later
        // op still applied; the frame was acked.
        let s = doc.surface("s1").unwrap();
        assert!(s.node("dup").is_none() && s.node("x").is_none());
        assert_eq!(child_ids(&doc, "s1", "main"), ["a", "b", "c", "ok"]);
        assert!(s.node("ok2").is_some());
        assert_eq!(a.acks, [("s1".to_owned(), 3)]);
    }

    #[test]
    fn set_merges_shallowly_and_null_deletes() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        frame(
            &mut doc,
            "s1",
            2,
            json!([
                ["add", "t", "main", null, {"id": "t", "k": "tool", "p": {"status": "running", "head": {"a": 1}, "tone": "info"}}],
                ["set", "t", {"status": "done", "tone": null, "head": {"b": 2}}],
            ]),
        );
        let n = doc.surface("s1").unwrap().node("t").unwrap();
        assert_eq!(n.str_prop("status"), Some("done"));
        assert!(n.prop("tone").is_none());
        assert_eq!(
            n.prop("head"),
            Some(&json!({"b": 2})),
            "the prop is replaced whole"
        );
    }

    #[test]
    fn text_append_replace_and_missing_text() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        frame(
            &mut doc,
            "s1",
            2,
            json!([
                ["add", "m", "main", null, {"id": "m", "k": "md"}],
                ["text", "m", "append", "Hel"],
                ["text", "m", "append", "lo"],
            ]),
        );
        assert_eq!(text_of(&doc, "s1", "m").as_deref(), Some("Hello"));
        frame(&mut doc, "s1", 3, json!([["text", "m", "replace", "bye"]]));
        assert_eq!(text_of(&doc, "s1", "m").as_deref(), Some("bye"));
        let a = frame(
            &mut doc,
            "s1",
            4,
            json!([
                ["set", "m", {"text": 5}],
                ["text", "m", "append", "x"],
                ["text", "ghost", "append", "x"],
            ]),
        );
        assert_eq!(a.errors.len(), 2);
        assert!(
            matches!(&a.errors[0], Event::Error { op: Some(1), msg, .. } if msg == "the text of m is not a string")
        );
        assert!(
            matches!(&a.errors[1], Event::Error { op: Some(2), msg, .. } if msg == "unknown id ghost")
        );
    }

    #[test]
    fn splice_counts_utf16_units_and_never_splits_a_surrogate_pair() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        // "a😀b": a=1 unit, 😀=2 units, b=1 unit.
        frame(
            &mut doc,
            "s1",
            2,
            json!([["add", "e", "dock", null, {"id": "e", "k": "editor", "p": {"text": "a😀b"}}]]),
        );
        let a = frame(
            &mut doc,
            "s1",
            3,
            json!([
                ["splice", "e", 2, 0, "X"], // inside the pair: rejected
                ["splice", "e", 1, 1, "X"], // half of the pair: rejected
                ["splice", "e", 0, 5, "X"], // past the end: rejected
                ["splice", "e", 4, 1, "X"], // at past the end: rejected
            ]),
        );
        assert_eq!(a.errors.len(), 4);
        assert_eq!(text_of(&doc, "s1", "e").as_deref(), Some("a😀b"));
        frame(&mut doc, "s1", 4, json!([["splice", "e", 1, 2, "é"]]));
        assert_eq!(text_of(&doc, "s1", "e").as_deref(), Some("aéb"));
        frame(
            &mut doc,
            "s1",
            5,
            json!([["splice", "e", 3, 0, "😀"], ["splice", "e", 0, 0, ">"]]),
        );
        assert_eq!(text_of(&doc, "s1", "e").as_deref(), Some(">aéb😀"));
        frame(&mut doc, "s1", 6, json!([["splice", "e", 1, 5, ""]]));
        assert_eq!(text_of(&doc, "s1", "e").as_deref(), Some(">"));
        // Splicing a node without text: only the empty range at 0 works.
        frame(
            &mut doc,
            "s1",
            7,
            json!([["add", "i", "dock", null, {"id": "i", "k": "input"}], ["splice", "i", 0, 0, "hi"]]),
        );
        assert_eq!(text_of(&doc, "s1", "i").as_deref(), Some("hi"));
    }

    #[test]
    fn utf16_range_edges() {
        assert_eq!(utf16_range("", 0, 0), Some((0, 0)));
        assert_eq!(utf16_range("ab", 2, 2), Some((2, 2)));
        assert_eq!(utf16_range("ab", 1, 2), Some((1, 2)));
        assert_eq!(utf16_range("ab", 3, 3), None);
        assert_eq!(utf16_range("😀", 0, 2), Some((0, 4)));
        assert_eq!(utf16_range("😀", 1, 2), None);
        assert_eq!(utf16_range("😀", 0, 1), None);
        assert_eq!(utf16_range("a😀", 1, 3), Some((1, 5)));
    }

    #[test]
    fn move_reorders_reparents_and_rejects_cycles() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        frame(
            &mut doc,
            "s1",
            2,
            json!([
                ["add", "a", "main", null, {"id": "a", "k": "col", "c": [{"id": "a1", "k": "text"}]}],
                ["add", "b", "main", null, {"id": "b", "k": "col"}],
                ["add", "c", "main", null, {"id": "c", "k": "col"}],
            ]),
        );
        let a = frame(
            &mut doc,
            "s1",
            3,
            json!([
                ["move", "c", "main", "a"],     // reorder to the front
                ["move", "a1", "b", null],      // reparent
                ["move", "a", "a1", null], // a into its own subtree (a1 is now under b, so fine?) -> see below
                ["move", "b", "b", null],  // into itself
                ["move", "b", "main", "b"], // before itself
                ["move", "main", "dock", null], // moves a surface region: allowed, not the root
                ["move", "s1", "main", null], // the root
                ["move", "b", "main", "zzz"], // anchor is not a child
            ]),
        );
        // a1 sits under b; moving a under a1 is legal (a1 is not inside a).
        let kinds: Vec<_> = a
            .errors
            .iter()
            .map(|e| match e {
                Event::Error { op: Some(op), .. } => *op,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(kinds, [3, 4, 6, 7]);
        // Final order of main: c, then b (holding a1 → a) in place; the region move left main
        // under dock, so assert through the structure, not main's old position.
        let s = doc.surface("s1").unwrap();
        assert_eq!(s.node("a").unwrap().parent().unwrap().id(), "a1");
        assert_eq!(s.node("a1").unwrap().parent().unwrap().id(), "b");
        assert_eq!(s.node("main").unwrap().parent().unwrap().id(), "dock");
        assert_eq!(s.depth_of("a"), Some(5));
    }

    #[test]
    fn move_into_own_subtree_is_rejected() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        frame(
            &mut doc,
            "s1",
            2,
            json!([
                ["add", "p", "main", null, {"id": "p", "k": "col", "c": [{"id": "q", "k": "col", "c": [{"id": "r", "k": "col"}]}]}],
            ]),
        );
        let a = frame(
            &mut doc,
            "s1",
            3,
            json!([["move", "p", "r", null], ["move", "q", "p", null]]),
        );
        assert_eq!(a.errors.len(), 1, "{:?}", a.errors);
        assert!(
            matches!(&a.errors[0], Event::Error { op: Some(0), msg, .. } if msg.contains("own subtree"))
        );
        // Same-parent reorder with `before` keeps all children.
        frame(
            &mut doc,
            "s1",
            4,
            json!([
                ["add", "x", "main", null, {"id": "x", "k": "text"}],
                ["move", "x", "main", "p"],
            ]),
        );
        assert_eq!(child_ids(&doc, "s1", "main"), ["x", "p"]);
    }

    #[test]
    fn del_removes_subtrees_focus_and_refuses_the_root() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        frame(
            &mut doc,
            "s1",
            2,
            json!([
                ["add", "w", "main", null, {"id": "w", "k": "col", "c": [{"id": "w1", "k": "editor"}]}],
                ["focus", "w1"],
            ]),
        );
        assert_eq!(doc.surface("s1").unwrap().focus(), Some("w1"));
        let a = frame(
            &mut doc,
            "s1",
            3,
            json!([["del", "w"], ["del", "w1"], ["del", "s1"], ["focus", "w1"]]),
        );
        let s = doc.surface("s1").unwrap();
        assert!(s.node("w").is_none() && s.node("w1").is_none());
        assert_eq!(s.focus(), None);
        assert_eq!(s.len(), 4, "root + 3 regions");
        assert_eq!(a.errors.len(), 3);
        assert!(matches!(&a.errors[0], Event::Error { op: Some(1), .. }));
        assert!(
            matches!(&a.errors[1], Event::Error { op: Some(2), msg, .. } if msg.contains("root"))
        );
        assert!(matches!(&a.errors[2], Event::Error { op: Some(3), .. }));
    }

    #[test]
    fn settle_is_cleared_by_later_ops_on_the_node_and_its_descendants() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        frame(
            &mut doc,
            "s1",
            2,
            json!([
                ["add", "t", "main", null, {"id": "t", "k": "col", "c": [{"id": "t1", "k": "text"}]}],
                ["settle", "t"],
                ["settle", "t1"],
            ]),
        );
        let s = doc.surface("s1").unwrap();
        assert!(s.node("t").unwrap().is_settled() && s.node("t1").unwrap().is_settled());
        frame(&mut doc, "s1", 3, json!([["text", "t1", "append", "!"]]));
        let s = doc.surface("s1").unwrap();
        assert!(!s.node("t1").unwrap().is_settled());
        assert!(
            !s.node("t").unwrap().is_settled(),
            "ancestors are cleared too"
        );
        assert!(
            frame(&mut doc, "s1", 4, json!([["settle", "ghost"]]))
                .errors
                .len()
                == 1
        );
    }

    #[test]
    fn unknown_ops_are_ignored_but_malformed_known_ops_are_rejected_with_their_index() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        let a = frame(
            &mut doc,
            "s1",
            2,
            json!([
                ["pin", "main"],
                ["del"],
                ["text", "main", "prepend", "x"],
                ["reveal", "main", "end"],
                ["reveal", "main", "middle"],
                ["scroll", "main", "page-up"],
                ["scroll", "main", "sideways"],
                ["suspend"],
                ["resume"],
            ]),
        );
        let idx: Vec<_> = a
            .errors
            .iter()
            .map(|e| match e {
                Event::Error { op: Some(op), .. } => *op,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(idx, [1, 2, 4, 6]);
        assert_eq!(a.acks.len(), 1);
        assert!(!doc.surface("s1").unwrap().is_suspended());
        frame(&mut doc, "s1", 3, json!([["suspend"]]));
        assert!(doc.surface("s1").unwrap().is_suspended());
    }

    #[test]
    fn limits_depth_and_node_count_reject_the_add() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        // Depth (checked on the typed op: serde_json's own 128-level recursion limit bites
        // first on the wire). `main` is at depth 1; a chain of n nodes below it reaches 1 + n.
        let chain = |prefix: &str, n: usize| {
            let mut node = Node::new(format!("{prefix}0"), "col");
            for i in 1..n {
                node = Node::new(format!("{prefix}{i}"), "col").children(vec![node]);
            }
            node
        };
        let mut surface = doc.surfaces.pop().unwrap();
        let add = |node: Node| Op::Add {
            id: node.id.clone(),
            parent: "main".into(),
            before: None,
            node,
        };
        assert_eq!(
            surface.apply_op(add(chain("d", MAX_DEPTH)), Instant::now()),
            Err(OpError::TooDeep)
        );
        assert!(
            surface.node("d0").is_none(),
            "nothing of the rejected add stays"
        );
        assert_eq!(
            surface.apply_op(add(chain("e", MAX_DEPTH - 1)), Instant::now()),
            Ok(())
        );
        assert_eq!(surface.depth_of("e0"), Some(MAX_DEPTH));
        // Moving a subtree deeper than the limit is refused too.
        surface
            .apply_op(add(chain("g", 2)), Instant::now())
            .unwrap();
        assert_eq!(
            surface.apply_op(
                Op::Move {
                    id: "g1".into(),
                    parent: "e0".into(),
                    before: None
                },
                Instant::now()
            ),
            Err(OpError::TooDeep)
        );

        // Node count: fill to the limit directly, then one more is rejected.
        let filler = MAX_NODES - surface.len();
        for i in 0..filler {
            surface
                .apply_op(
                    Op::Add {
                        id: format!("f{i}"),
                        parent: "main".into(),
                        before: None,
                        node: Node::new(format!("f{i}"), "text"),
                    },
                    Instant::now(),
                )
                .unwrap();
        }
        assert_eq!(surface.len(), MAX_NODES);
        assert_eq!(
            surface.apply_op(
                Op::Add {
                    id: "over".into(),
                    parent: "main".into(),
                    before: None,
                    node: Node::new("over", "text"),
                },
                Instant::now()
            ),
            Err(OpError::TooManyNodes)
        );
        assert!(surface.node("over").is_none());
    }

    #[test]
    fn frames_to_unknown_or_closed_surfaces_are_dropped_with_an_error_and_no_ack() {
        let mut doc = Document::new();
        let a = frame(&mut doc, "nope", 1, json!([]));
        assert!(a.acks.is_empty());
        assert!(
            matches!(&a.errors[0], Event::Error { sf: Some(sf), s: Some(1), op: None, .. } if sf == "nope")
        );
        open_inline(&mut doc, "s1");
        assert!(
            doc.apply(&msg(Verb::Close, &json!({"id": "s1"})), Instant::now())
                .errors
                .is_empty()
        );
        let a = frame(&mut doc, "s1", 9, json!([]));
        assert!(a.acks.is_empty() && a.errors.len() == 1);
        let bad = doc.apply(
            &Message {
                verb: Verb::Frame,
                params: vec![],
                body: b"{nope".to_vec(),
            },
            Instant::now(),
        );
        assert!(
            matches!(&bad.errors[0], Event::Error { msg, op: None, sf: None, .. } if msg.starts_with("malformed f body"))
        );
        let bad = doc.apply(
            &msg(Verb::Frame, &json!({"sf": "s1", "ops": []})),
            Instant::now(),
        );
        assert!(
            matches!(&bad.errors[0], Event::Error { msg, .. } if msg.starts_with("malformed f body"))
        );
    }

    #[test]
    fn listen_false_surfaces_get_no_acks_and_no_errors() {
        let mut doc = Document::new();
        doc.apply(
            &msg(
                Verb::Open,
                &json!({"id": "q", "mode": "screen", "listen": false}),
            ),
            Instant::now(),
        );
        let a = frame(
            &mut doc,
            "q",
            1,
            json!([["del", "ghost"], ["add", "main", "q", null, {"id": "main", "k": "col"}]]),
        );
        assert_eq!(a, Applied::default());
        assert!(doc.surface("q").unwrap().node("main").is_some());
        let gone = doc.apply(
            &msg(
                Verb::Open,
                &json!({"id": "zz", "adopt": true, "listen": false}),
            ),
            Instant::now(),
        );
        assert_eq!(gone, Applied::default());
    }

    #[test]
    fn close_keeps_main_drops_dock_layer_and_focus_and_screens_are_discarded() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        frame(
            &mut doc,
            "s1",
            2,
            json!([
                ["add", "m", "main", null, {"id": "m", "k": "md", "p": {"text": "kept"}}],
                ["add", "ed", "dock", null, {"id": "ed", "k": "editor"}],
                ["add", "ov", "layer", null, {"id": "ov", "k": "overlay"}],
                ["focus", "ed"],
            ]),
        );
        doc.apply(
            &msg(Verb::Close, &json!({"id": "s1", "keep": true})),
            Instant::now(),
        );
        assert!(doc.surface("s1").is_none(), "closed surfaces are not open");
        let s = &doc.surfaces()[0];
        assert!(!s.is_open());
        assert_eq!(s.node("m").unwrap().text(), Some("kept"));
        assert!(s.node("ed").is_none() && s.node("dock").is_none() && s.node("ov").is_none());
        assert_eq!(s.focus(), None);
        // keep:false removes; a screen surface is discarded whatever `keep` says.
        open_inline(&mut doc, "s2");
        doc.apply(
            &msg(Verb::Close, &json!({"id": "s2", "keep": false})),
            Instant::now(),
        );
        assert_eq!(doc.surfaces().len(), 1);
        doc.apply(
            &msg(Verb::Open, &json!({"id": "sc", "mode": "screen"})),
            Instant::now(),
        );
        doc.apply(&msg(Verb::Close, &json!({"id": "sc"})), Instant::now());
        assert_eq!(doc.surfaces().len(), 1);
        let a = doc.apply(&msg(Verb::Close, &json!({"id": "sc"})), Instant::now());
        assert!(matches!(&a.errors[0], Event::Error { .. }));
    }

    #[test]
    fn a_new_inline_open_replaces_the_live_one_and_discards_screens_but_not_flow() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        doc.apply(
            &msg(Verb::Open, &json!({"id": "sc", "mode": "screen"})),
            Instant::now(),
        );
        doc.apply(
            &msg(Verb::Open, &json!({"id": "fl", "mode": "flow"})),
            Instant::now(),
        );
        assert_eq!(doc.surfaces().len(), 3);
        // A screen open never touches the inline surface.
        doc.apply(
            &msg(Verb::Open, &json!({"id": "sc2", "mode": "screen"})),
            Instant::now(),
        );
        assert!(doc.surface("s1").unwrap().is_open());
        assert!(doc.surface("sc").is_none() && doc.surface("sc2").is_some());
        // A new inline open closes the old inline (kept), discards the screen, leaves flow.
        open_inline(&mut doc, "s2");
        assert!(doc.surface("s1").is_none() && doc.surface("sc2").is_none());
        assert!(doc.surface("fl").is_some() && doc.surface("s2").is_some());
        assert!(
            doc.surfaces()
                .iter()
                .any(|s| s.id() == "s1" && !s.is_open())
        );
        assert_eq!(doc.inline().unwrap().id(), "s2");
    }

    #[test]
    fn adopt_reopens_the_newest_closed_inline_surface_or_answers_gone() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        frame(
            &mut doc,
            "s1",
            2,
            json!([
                ["add", "m", "main", null, {"id": "m", "k": "text", "p": {"text": "history"}}],
                ["add", "ed", "dock", null, {"id": "ed", "k": "editor"}],
            ]),
        );
        doc.apply(&msg(Verb::Close, &json!({"id": "s1"})), Instant::now());
        let a = doc.apply(
            &msg(
                Verb::Open,
                &json!({"id": "s1", "adopt": true, "title": "again"}),
            ),
            Instant::now(),
        );
        assert_eq!(a, Applied::default());
        let s = doc.surface("s1").unwrap();
        assert_eq!(s.title(), Some("again"));
        assert_eq!(s.node("m").unwrap().text(), Some("history"));
        assert!(
            s.node("dock").is_none(),
            "close dropped dock; the program adds it again"
        );
        assert_eq!(doc.surfaces().len(), 1);
        let a = frame(
            &mut doc,
            "s1",
            3,
            json!([["add", "dock", "s1", null, {"id": "dock", "k": "col"}]]),
        );
        assert!(a.errors.is_empty());
        // Adopting the live id again does nothing; an unknown id is `gone`.
        assert_eq!(
            doc.apply(
                &msg(Verb::Open, &json!({"id": "s1", "adopt": true})),
                Instant::now()
            ),
            Applied::default()
        );
        let a = doc.apply(
            &msg(Verb::Open, &json!({"id": "other", "adopt": true})),
            Instant::now(),
        );
        assert_eq!(
            a.errors,
            [Event::Gone {
                sf: None,
                ids: vec!["other".into()]
            }]
        );
        assert!(doc.surface("other").is_none());
    }

    #[test]
    fn open_rejects_bad_ids_modes_and_bodies() {
        let mut doc = Document::new();
        for body in [
            json!({"mode": "inline"}),
            json!({"id": ""}),
            json!({"id": "a", "mode": "warp"}),
        ] {
            let a = doc.apply(&msg(Verb::Open, &body), Instant::now());
            assert_eq!(a.errors.len(), 1, "{body}");
        }
        assert!(doc.surfaces().is_empty());
        let a = doc.apply(
            &Message {
                verb: Verb::Open,
                params: vec![],
                body: b"[1]".to_vec(),
            },
            Instant::now(),
        );
        assert!(
            matches!(&a.errors[0], Event::Error { msg, .. } if msg.starts_with("malformed o body"))
        );
    }

    #[test]
    fn closed_surfaces_are_capped() {
        let mut doc = Document::new();
        for i in 0..(MAX_CLOSED + 3) {
            open_inline(&mut doc, &format!("s{i}"));
        }
        let closed: Vec<_> = doc
            .surfaces()
            .iter()
            .filter(|s| !s.is_open())
            .map(|s| s.id().to_owned())
            .collect();
        assert_eq!(closed.len(), MAX_CLOSED);
        assert_eq!(closed[0], "s2", "the oldest closed ones went first");
    }

    #[test]
    fn stylesheets_are_stored_replaced_and_capped() {
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        let sheet = |name: &str, css: String| {
            msg(Verb::Style, &json!({"sf": "s1", "name": name, "css": css}))
        };
        assert_eq!(
            doc.apply(&sheet("a", ".x{}".into()), Instant::now()),
            Applied::default()
        );
        doc.apply(&sheet("a", ".y{}".into()), Instant::now());
        doc.apply(&sheet("b", ".z{}".into()), Instant::now());
        let styles: Vec<_> = doc.surface("s1").unwrap().styles().collect();
        assert_eq!(styles, [("a", ".y{}"), ("b", ".z{}")]);
        let big = doc.apply(&sheet("c", "x".repeat(MAX_STYLE_BYTES)), Instant::now());
        assert_eq!(big.errors.len(), 1);
        assert_eq!(doc.surface("s1").unwrap().styles().count(), 2);
        // Replacing a sheet counts only the new size.
        assert!(
            doc.apply(&sheet("a", "x".repeat(MAX_STYLE_BYTES - 4)), Instant::now())
                .errors
                .is_empty()
        );
    }

    #[test]
    fn blobs_are_verified_by_hash_listed_and_evicted_lru() {
        let mut doc = Document::new();
        let bytes = b"hello blob".to_vec();
        let id = hex(&Sha256::digest(&bytes));
        let b64 = STANDARD.encode(&bytes);
        let blob_msg = |params: Vec<(String, String)>, body: &str| Message {
            verb: Verb::Blob,
            params,
            body: body.as_bytes().to_vec(),
        };
        // A wrong `id` parameter is ignored: Tern hashes what arrives.
        let a = doc.apply(
            &blob_msg(
                vec![
                    ("id".into(), "00".into()),
                    ("mime".into(), "text/plain".into()),
                ],
                &b64,
            ),
            Instant::now(),
        );
        assert_eq!(a, Applied::default());
        let stored = doc.blob(&id).unwrap();
        assert_eq!(
            (stored.mime.as_deref(), &stored.bytes[..]),
            (Some("text/plain"), &bytes[..])
        );
        assert_eq!(
            doc.blobs_have(&["nope".into(), id.to_uppercase(), id.clone()]),
            [id.to_uppercase(), id.clone()]
        );
        let bad = doc.apply(&blob_msg(vec![], "!!!not base64"), Instant::now());
        assert!(
            matches!(&bad.errors[0], Event::Error { msg, .. } if msg.starts_with("blob is not valid base64"))
        );
        let doc_json = serde_json::to_value(&doc).unwrap();
        assert_eq!(
            doc_json["blobs"],
            json!([{"id": id, "mime": "text/plain", "len": 10}])
        );

        // LRU: 64 MiB budget, 3 × 24 MiB blobs: the least recently used goes, the newest stays.
        let mut store = BlobStore::default();
        let big = |n: u8| Blob {
            mime: None,
            bytes: vec![n; 24 << 20].into(),
        };
        store.insert("a".into(), big(1));
        store.insert("b".into(), big(2));
        store.map.get_mut("a").unwrap().1 = 100; // `a` was used more recently than `b`
        store.tick = 100;
        store.insert("c".into(), big(3));
        assert!(store.map.contains_key("a") && store.map.contains_key("c"));
        assert!(!store.map.contains_key("b"));
        assert_eq!(store.total, 48 << 20);
    }

    #[test]
    fn oversized_blobs_are_rejected() {
        let mut doc = Document::new();
        let body = STANDARD.encode(vec![0u8; MAX_BLOB + 1]);
        let a = doc.apply(
            &Message {
                verb: Verb::Blob,
                params: vec![],
                body: body.into_bytes(),
            },
            Instant::now(),
        );
        assert!(
            matches!(&a.errors[0], Event::Error { msg, .. } if msg == "blob is larger than 16 MiB")
        );
        assert!(
            serde_json::to_value(&doc).unwrap()["blobs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    fn toast_frame(doc: &mut Document, s: u64, id: &str, props: Value, now: Instant) {
        let f = msg(
            Verb::Frame,
            &json!({"sf": "s1", "s": s, "ops": [
                ["add", id, "main", null, {"id": id, "k": "toast", "p": props}],
            ]}),
        );
        doc.apply(&f, now);
    }

    fn del_frame(doc: &mut Document, s: u64, id: &str, now: Instant) {
        let f = msg(
            Verb::Frame,
            &json!({"sf": "s1", "s": s, "ops": [["del", id]]}),
        );
        doc.apply(&f, now);
    }

    #[test]
    fn a_toast_outlives_the_node_omp_deletes_and_expires_after_its_ttl() {
        let t0 = Instant::now();
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        toast_frame(
            &mut doc,
            2,
            "t1",
            json!({"text": "Copied", "sub": "to clipboard", "ttl": 3000}),
            t0,
        );
        del_frame(&mut doc, 3, "t1", t0);
        assert!(doc.surface("s1").unwrap().node("t1").is_none());

        let got: Vec<_> = doc.toasts(t0 + Duration::from_millis(2999)).collect();
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].seq, got[0].text.as_str(), got[0].sub.as_deref()),
            (1, "Copied", Some("to clipboard"))
        );
        assert_eq!((got[0].ttl, got[0].at), (Duration::from_secs(3), t0));
        assert_eq!(doc.toasts(t0 + Duration::from_millis(3000)).count(), 0);

        // No ttl: omp's 2400 ms; nothing but a toast node is recorded.
        toast_frame(&mut doc, 4, "t2", json!({"text": "Saved"}), t0);
        let t2: Vec<_> = doc.toasts(t0 + Duration::from_millis(100)).collect();
        assert_eq!((t2.len(), t2[1].seq, t2[1].sub.as_deref()), (2, 2, None));
        assert_eq!(t2[1].ttl, DEFAULT_TOAST_TTL);
        let f = msg(
            Verb::Frame,
            &json!({"sf": "s1", "s": 5, "ops": [
                ["add", "m", "main", null, {"id": "m", "k": "md", "p": {"text": "hi"}}],
            ]}),
        );
        doc.apply(&f, t0);
        assert_eq!(doc.toasts(t0).count(), 2);
        // A program cannot pin a toast.
        toast_frame(&mut doc, 6, "t3", json!({"text": "x", "ttl": 1e9}), t0);
        assert_eq!(doc.toasts(t0).last().unwrap().ttl, MAX_TOAST_TTL);
    }

    #[test]
    fn a_toast_node_whose_text_changes_is_a_new_toast_each_time() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        toast_frame(
            &mut doc,
            2,
            "q",
            json!({"text": "fake-1", "ttl": 1000}),
            ms(0),
        );
        let op = |s: u64, ops: Value| msg(Verb::Frame, &json!({"sf": "s1", "s": s, "ops": ops}));
        doc.apply(&op(3, json!([["text", "q", "replace", "fake-2"]])), ms(500));
        // fake-1 expires at 1000; the text change at 500 is a second, fresh record.
        let texts: Vec<_> = doc.toasts(ms(600)).map(|t| t.text.clone()).collect();
        assert_eq!(texts, ["fake-1", "fake-2"]);
        doc.apply(&op(4, json!([["set", "q", {"sub": "again"}]])), ms(2000));
        let last = doc.toasts(ms(2100)).last().unwrap();
        assert_eq!((last.at, last.sub.as_deref()), (ms(2000), Some("again")));
        assert_eq!(doc.toasts(ms(2999)).count(), 1);
        assert_eq!(doc.toasts(ms(3000)).count(), 0);
    }

    #[test]
    fn the_toast_ring_keeps_the_newest_eight_and_drops_the_expired_on_the_next_one() {
        let t0 = Instant::now();
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        for i in 0..11u64 {
            let text = format!("n{i}");
            toast_frame(
                &mut doc,
                2 + i,
                &format!("t{i}"),
                json!({ "text": text }),
                t0,
            );
        }
        let seqs: Vec<u64> = doc.toasts(t0).map(|t| t.seq).collect();
        assert_eq!(seqs, (4..=11).collect::<Vec<_>>());
        // The expired ones are gone for good once a later toast arrives.
        let later = t0 + Duration::from_secs(5);
        assert_eq!(doc.toasts(later).count(), 0);
        toast_frame(&mut doc, 20, "late", json!({"text": "late"}), later);
        assert_eq!(doc.toasts(t0).count(), 1);
        assert_eq!(doc.toasts(later).next().unwrap().seq, 12);
    }

    #[test]
    fn a_toast_rejected_by_its_frame_is_not_recorded() {
        let t0 = Instant::now();
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        toast_frame(&mut doc, 2, "t1", json!({"text": "ok"}), t0);
        toast_frame(&mut doc, 3, "t1", json!({"text": "duplicate id"}), t0);
        assert_eq!(doc.toasts(t0).count(), 1);
    }

    #[test]
    fn a_wanted_blob_is_dropped_when_its_node_is_deleted_or_the_host_has_none() {
        let t0 = Instant::now();
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        let f = msg(
            Verb::Frame,
            &json!({"sf": "s1", "s": 2, "ops": [
                ["add", "ia", "main", null, {"id": "ia", "k": "image", "p": {"blob": a}}],
                ["add", "ib", "main", null, {"id": "ib", "k": "image", "p": {"blob": b}}],
            ]}),
        );
        doc.apply(&f, t0);
        assert_eq!(doc.missing_blobs().len(), 2);

        let del = msg(
            Verb::Frame,
            &json!({"sf": "s1", "s": 3, "ops": [["del", "ia"]]}),
        );
        doc.apply(&del, t0);
        assert_eq!(doc.missing_blobs(), std::slice::from_ref(&b));

        doc.forget_blob(&b);
        assert!(doc.missing_blobs().is_empty() && !doc.wants_blobs());
    }

    #[test]
    fn blobs_a_node_refers_to_but_this_document_never_got_are_missing_until_put() {
        let t0 = Instant::now();
        let bytes = b"late blob".to_vec();
        let id = hex(&Sha256::digest(&bytes));
        let mut doc = Document::new();
        open_inline(&mut doc, "s1");
        let f = msg(
            Verb::Frame,
            &json!({"sf": "s1", "s": 2, "ops": [
                ["add", "img", "main", null, {"id": "img", "k": "image", "p": {"blob": id}}],
                ["add", "txt", "main", null, {"id": "txt", "k": "text", "p": {"text": "no blob"}}],
            ]}),
        );
        doc.apply(&f, t0);
        assert_eq!(doc.missing_blobs(), std::slice::from_ref(&id));

        let revision = doc.revision();
        assert!(
            !doc.put_blob(&id, None, b"other bytes".to_vec()),
            "hash mismatch"
        );
        assert_eq!(doc.revision(), revision);
        assert!(doc.put_blob(&id, Some("image/png".into()), bytes.clone()));
        assert!(doc.revision() > revision);
        assert_eq!(&doc.blob(&id).unwrap().bytes[..], &bytes[..]);
        assert!(doc.missing_blobs().is_empty());

        // A blob that arrives before the node that refers to it was never wanted.
        let early = b"early".to_vec();
        let early_id = hex(&Sha256::digest(&early));
        doc.apply(
            &Message {
                verb: Verb::Blob,
                params: Vec::new(),
                body: STANDARD.encode(&early).into_bytes(),
            },
            t0,
        );
        let f = msg(
            Verb::Frame,
            &json!({"sf": "s1", "s": 3, "ops": [
                ["set", "img", {"blob": early_id}],
            ]}),
        );
        doc.apply(&f, t0);
        assert!(doc.missing_blobs().is_empty());
    }

    #[test]
    fn splitter_messages_apply_end_to_end() {
        let mut splitter = crate::stream::Splitter::new();
        let mut doc = Document::new();
        let mut wire = encode_message(Verb::Open, &[], br#"{"id":"s1","mode":"inline"}"#);
        wire.extend(encode_message(
            Verb::Frame,
            &[],
            br#"{"sf":"s1","s":1,"ops":[["add","main","s1",null,{"id":"main","k":"col"}],["add","t","main",null,{"id":"t","k":"text","p":{"text":"hi"}}]]}"#,
        ));
        for m in splitter.feed(&wire, std::time::Instant::now()) {
            doc.apply(&m, Instant::now());
        }
        assert_eq!(text_of(&doc, "s1", "t").as_deref(), Some("hi"));
    }

    #[test]
    fn changed_at_is_the_receive_time_of_the_last_change_to_that_node() {
        use std::time::Duration;
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let apply = |doc: &mut Document, ms: u64, verb: Verb, body: Value| {
            doc.apply(&msg(verb, &body), at(ms))
        };
        let mut doc = Document::new();
        apply(
            &mut doc,
            0,
            Verb::Open,
            json!({"id": "s1", "mode": "inline"}),
        );
        apply(
            &mut doc,
            10,
            Verb::Frame,
            json!({"sf": "s1", "s": 1, "ops": [
                ["add", "main", "s1", null, {"id": "main", "k": "col", "c": [
                    {"id": "a", "k": "text", "p": {"text": "a"}},
                    {"id": "b", "k": "text", "p": {"text": "b"}},
                    {"id": "c", "k": "text", "p": {"text": "c"}},
                ]}],
            ]}),
        );
        let stamp = |doc: &Document, id: &str| {
            doc.surface("s1")
                .unwrap()
                .node(id)
                .unwrap()
                .changed_at()
                .duration_since(t0)
                .as_millis()
        };
        assert_eq!(stamp(&doc, "s1"), 0, "the root stamps at the open");
        assert_eq!(
            (stamp(&doc, "main"), stamp(&doc, "a"), stamp(&doc, "c")),
            (10, 10, 10),
            "an add stamps the whole added subtree"
        );
        // set / text / splice stamp their target only; siblings and ancestors keep theirs.
        apply(
            &mut doc,
            20,
            Verb::Frame,
            json!({"sf": "s1", "s": 2, "ops": [["set", "a", {"tone": "info"}]]}),
        );
        apply(
            &mut doc,
            30,
            Verb::Frame,
            json!({"sf": "s1", "s": 3, "ops": [["text", "b", "append", "!"]]}),
        );
        apply(
            &mut doc,
            40,
            Verb::Frame,
            json!({"sf": "s1", "s": 4, "ops": [["splice", "c", 0, 1, "C"]]}),
        );
        assert_eq!(
            (stamp(&doc, "a"), stamp(&doc, "b"), stamp(&doc, "c")),
            (20, 30, 40)
        );
        assert_eq!((stamp(&doc, "main"), stamp(&doc, "s1")), (10, 0));
        // A rejected op stamps nothing; a move or settle changes no props.
        let a = apply(
            &mut doc,
            50,
            Verb::Frame,
            json!({"sf": "s1", "s": 5, "ops": [
                ["splice", "a", 9, 0, "x"], ["settle", "b"], ["move", "c", "main", "a"],
            ]}),
        );
        assert_eq!(a.errors.len(), 1);
        assert_eq!(
            (stamp(&doc, "a"), stamp(&doc, "b"), stamp(&doc, "c")),
            (20, 30, 40)
        );
    }

    #[test]
    fn revision_counts_only_messages_that_changed_something() {
        let t = Instant::now();
        let mut doc = Document::new();
        assert_eq!(doc.revision(), 0);
        let mut last = doc.revision();
        let mut step = |doc: &mut Document, m: Message| {
            doc.apply(&m, t);
            let moved = doc.revision() != last;
            last = doc.revision();
            moved
        };
        // Nothing to apply: malformed, unknown surface, unknown verb, reply/event, bad query.
        assert!(!step(&mut doc, msg(Verb::Open, &json!({"nope": 1}))));
        assert!(!step(
            &mut doc,
            msg(Verb::Frame, &json!({"sf": "x", "s": 1, "ops": []}))
        ));
        assert!(!step(&mut doc, msg(Verb::Unknown(b'z'), &json!({}))));
        assert!(!step(&mut doc, msg(Verb::Event, &json!({"ev": "ack"}))));
        assert!(!step(
            &mut doc,
            msg(Verb::Query, &json!({"q": "hello", "v": [2]}))
        ));
        assert!(!step(
            &mut doc,
            msg(Verb::Query, &json!({"q": "blobs", "ids": []}))
        ));
        assert!(!step(&mut doc, msg(Verb::Close, &json!({"id": "x"}))));
        // Each of these changes the document.
        assert!(step(
            &mut doc,
            msg(
                Verb::Query,
                &json!({"q": "hello", "v": [1], "app": "omp", "ver": "1", "features": ["send"]})
            )
        ));
        assert!(step(
            &mut doc,
            msg(Verb::Open, &json!({"id": "s1", "mode": "inline"}))
        ));
        assert!(step(
            &mut doc,
            msg(Verb::Frame, &json!({"sf": "s1", "s": 1, "ops": []}))
        ));
        assert!(step(
            &mut doc,
            msg(Verb::Style, &json!({"sf": "s1", "name": "a", "css": "x"}))
        ));
        assert!(step(
            &mut doc,
            Message {
                verb: Verb::Blob,
                params: vec![],
                body: STANDARD.encode(b"x").into_bytes(),
            }
        ));
        assert!(step(&mut doc, msg(Verb::Close, &json!({"id": "s1"}))));
        let program = doc.program().unwrap();
        assert_eq!(program.app.as_deref(), Some("omp"));
        assert!(program.supports("send") && !program.supports("edit"));
    }
}
