//! Late-join replay: [`Document::replay`] turns a document into the program → terminal bytes
//! that rebuild an equal one in a fresh [`crate::mirror::Mirror`] (or any other passive
//! consumer of the stream).
//!
//! Order: the program's `hello` query, then for every surface (oldest first, so the effects
//! of opening one on the others replay as they happened) its `o`, `s` stylesheets, `f`
//! frames, and `x keep:true` when it is closed; finally the retained blobs that some node
//! refers to (a string prop equal to the blob's id; oldest use first, so the LRU order
//! survives). A blob no node refers to is not replayed ([`ReplayBlobs`]).
//!
//! The tree is one `add` op per node in document order (a parent before its children, each
//! appended with `before: null`), so no op outgrows the node it carries; ops are grouped
//! into frames of at most [`FRAME_BUDGET`] serialized bytes, every one carrying the surface's
//! last `s`. The frame then ends with the surface state that is not tree content: the root's
//! own props, `settle` marks (after all adds, which would clear them), `focus` and `suspend`.
//!
//! Timers stay truthful. `age` is "milliseconds already elapsed when the frame was written"
//! and consumers tick it from the receive time, so replaying node `n` at `now` advances its
//! `age` props by `now - n.changed_at()`: `elapsed` nodes without `stopped`, running `tool`
//! nodes, the `tool`/`stats` ages of running `agent` nodes and the `retry` age of any agent
//! (the renderer counts a retry down whatever the agent's status).

use serde::Serialize;
use serde_json::{Number, Value};

use super::*;
use crate::stream::encode_chunked;
use crate::wire::DEFAULT_APC_LIMIT;

/// Serialized ops per replayed frame, at most (far below the splitter's body cap).
const FRAME_BUDGET: usize = 4 << 20;

struct Writer {
    out: Vec<u8>,
    chunks: u64,
    /// Blob ids the replayed nodes refer to, collected while they are written; `None` when
    /// no blobs are replayed.
    refs: Option<HashSet<String>>,
}

impl Writer {
    fn message(&mut self, verb: Verb, params: &[(&str, &str)], body: &[u8]) {
        self.chunks += 1;
        let id = format!("r{}", self.chunks);
        self.out
            .extend(encode_chunked(verb, params, body, DEFAULT_APC_LIMIT, &id));
    }

    fn json(&mut self, verb: Verb, value: &impl Serialize) {
        match serde_json::to_vec(value) {
            Ok(body) => self.message(verb, &[], &body),
            // Every replayed value is plain data; serialization cannot fail.
            Err(e) => log::error!("encoding a TSP replay message: {e}"),
        }
    }
}

/// Adds `ms` to a numeric JSON value (integers stay integers); other values are left alone.
fn bump(v: &mut Value, ms: u64) {
    let Value::Number(n) = v else { return };
    let next = if let Some(i) = n.as_i64() {
        Some(Number::from(
            i.saturating_add(i64::try_from(ms).unwrap_or(i64::MAX)),
        ))
    } else if let Some(u) = n.as_u64() {
        Some(Number::from(u.saturating_add(ms)))
    } else {
        n.as_f64().and_then(|f| Number::from_f64(f + ms as f64))
    };
    if let Some(next) = next {
        *v = Value::Number(next);
    }
}

/// Advances the timer props of one node by `ms` (see the module docs).
fn advance_timers(kind: &str, props: &mut Map<String, Value>, ms: u64) {
    if ms == 0 {
        return;
    }
    let running = props.get("status").and_then(Value::as_str) == Some("running");
    match kind {
        "elapsed" if props.get("stopped").is_none_or(Value::is_null) => {
            if let Some(age) = props.get_mut("age") {
                bump(age, ms);
            }
        }
        "tool" if running => {
            if let Some(age) = props.get_mut("age") {
                bump(age, ms);
            }
        }
        "agent" => {
            // The renderer counts a retry down whatever the agent's status.
            for key in ["tool", "stats", "retry"] {
                if (running || key == "retry")
                    && let Some(Value::Object(inner)) = props.get_mut(key)
                    && let Some(age) = inner.get_mut("age")
                {
                    bump(age, ms);
                }
            }
        }
        _ => {}
    }
}

/// Adds to `out` every string in `props` that has the shape of a blob id (64 lowercase hex
/// digits), at any depth: a node refers to a blob by its id in some prop (`blob` of an image).
fn note_blob_ids(props: &Map<String, Value>, out: &mut HashSet<String>) {
    fn walk(v: &Value, out: &mut HashSet<String>) {
        match v {
            Value::String(s) => {
                if s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    out.insert(s.clone());
                }
            }
            Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            Value::Object(map) => map.values().for_each(|i| walk(i, out)),
            _ => {}
        }
    }
    props.values().for_each(|v| walk(v, out));
}

/// Collects ops into `f` messages of the surface, flushing past [`FRAME_BUDGET`].
struct Frames<'a> {
    sf: &'a str,
    s: u64,
    ops: Vec<u8>,
    sent: bool,
}

impl<'a> Frames<'a> {
    fn push(&mut self, w: &mut Writer, op: &Op) {
        let Ok(json) = serde_json::to_vec(op) else {
            // Ops are plain data; serialization cannot fail.
            return;
        };
        if !self.ops.is_empty() && self.ops.len() + json.len() > FRAME_BUDGET {
            self.flush(w);
        }
        if !self.ops.is_empty() {
            self.ops.push(b',');
        }
        self.ops.extend_from_slice(&json);
    }

    fn flush(&mut self, w: &mut Writer) {
        let mut body = Vec::with_capacity(self.ops.len() + 64);
        body.extend_from_slice(b"{\"sf\":");
        // A string serializes without failing.
        body.extend(serde_json::to_vec(self.sf).unwrap_or_default());
        body.extend_from_slice(format!(",\"s\":{},\"ops\":[", self.s).as_bytes());
        body.append(&mut self.ops);
        body.extend_from_slice(b"]}");
        w.message(Verb::Frame, &[], &body);
        self.sent = true;
    }

    fn finish(mut self, w: &mut Writer) {
        if !self.ops.is_empty() || !self.sent {
            self.flush(w);
        }
    }
}

impl Surface {
    fn replay(&self, now: Instant, w: &mut Writer) {
        w.json(
            Verb::Open,
            &Open {
                id: self.id.clone(),
                mode: self.mode,
                title: self.title.clone(),
                role: self.role.clone(),
                adopt: false,
                listen: self.listen,
                closable: self.closable,
                font: self.font.clone(),
            },
        );
        for style in &self.styles {
            w.json(
                Verb::Style,
                &Stylesheet {
                    sf: self.id.clone(),
                    name: style.name.clone(),
                    css: style.css.clone(),
                },
            );
        }
        if let Some(s) = self.last_s {
            let mut frames = Frames {
                sf: &self.id,
                s,
                ops: Vec::new(),
                sent: false,
            };
            let root = self.root();
            if !root.props().is_empty() {
                if let Some(refs) = &mut w.refs {
                    note_blob_ids(root.props(), refs);
                }
                frames.push(
                    w,
                    &Op::Set {
                        id: self.id.clone(),
                        props: root.props().clone(),
                    },
                );
            }
            let mut settled = Vec::new();
            self.walk(&mut |_, n| {
                if n.is_settled() {
                    settled.push(n.id());
                }
                let Some(parent) = n.entry.parent.as_deref() else {
                    return;
                };
                if let Some(refs) = &mut w.refs {
                    note_blob_ids(n.props(), refs);
                }
                let mut props = n.props().clone();
                let ms = u64::try_from(now.saturating_duration_since(n.changed_at()).as_millis())
                    .unwrap_or(u64::MAX);
                advance_timers(n.kind(), &mut props, ms);
                frames.push(
                    w,
                    &Op::Add {
                        id: n.id().to_owned(),
                        parent: parent.to_owned(),
                        before: None,
                        node: Node {
                            id: n.id().to_owned(),
                            k: n.kind().to_owned(),
                            p: props,
                            c: Vec::new(),
                        },
                    },
                );
            });
            for id in settled {
                frames.push(w, &Op::Settle { id: id.to_owned() });
            }
            if let Some(id) = &self.focus {
                frames.push(
                    w,
                    &Op::Focus {
                        id: Some(id.clone()),
                    },
                );
            }
            if self.suspended {
                frames.push(w, &Op::Suspend);
            }
            frames.finish(w);
        }
        if !self.open {
            w.json(
                Verb::Close,
                &Close {
                    id: self.id.clone(),
                    keep: true,
                },
            );
        }
    }
}

/// Which retained blobs [`Document::replay_with`] appends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayBlobs {
    /// The ones some node of the document refers to (a string prop equal to the blob's id).
    Referenced,
    /// None: for a reader that only wants the structure.
    None,
}

impl Document {
    /// The program → terminal bytes that rebuild this document in a fresh consumer (see the
    /// module docs), with the blobs its nodes refer to; `now` is the time the replay is read,
    /// for the timer props. Empty for an empty document.
    pub fn replay(&self, now: Instant) -> Vec<u8> {
        self.replay_with(now, ReplayBlobs::Referenced)
    }

    /// [`Document::replay`] with a choice of blobs. A retained blob no node refers to is never
    /// replayed: the consumer would only hold it for a node that is gone.
    pub fn replay_with(&self, now: Instant, blobs: ReplayBlobs) -> Vec<u8> {
        let mut w = Writer {
            out: Vec::new(),
            chunks: 0,
            refs: (blobs == ReplayBlobs::Referenced).then(HashSet::new),
        };
        if let Some(p) = &self.program {
            w.json(
                Verb::Query,
                &Query::Hello {
                    v: vec![VERSION],
                    app: p.app.clone(),
                    ver: p.ver.clone(),
                    features: p.features.clone(),
                },
            );
        }
        for surface in &self.surfaces {
            surface.replay(now, &mut w);
        }
        if let Some(refs) = w.refs.take() {
            let mut blobs: Vec<(&String, &(Blob, u64))> = self
                .blobs
                .map
                .iter()
                .filter(|(id, _)| refs.contains(id.as_str()))
                .collect();
            blobs.sort_by_key(|(_, (_, tick))| *tick);
            for (_, (blob, _)) in blobs {
                let body = STANDARD.encode(&blob.bytes);
                let params: Vec<(&str, &str)> = blob
                    .mime
                    .as_deref()
                    .map(|m| ("mime", m))
                    .into_iter()
                    .collect();
                w.message(Verb::Blob, &params, body.as_bytes());
            }
        }
        w.out
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::mirror::Mirror;
    use crate::stream::Splitter;

    fn msg(verb: Verb, body: &Value) -> Message {
        Message {
            verb,
            params: Vec::new(),
            body: serde_json::to_vec(body).expect("json"),
        }
    }

    fn blob(bytes: &[u8], mime: Option<&str>) -> Message {
        Message {
            verb: Verb::Blob,
            params: mime
                .map(|m| vec![("mime".to_owned(), m.to_owned())])
                .unwrap_or_default(),
            body: STANDARD.encode(bytes).into_bytes(),
        }
    }

    fn age(doc: &Document, sf: &str, id: &str) -> Option<i64> {
        doc.surfaces()
            .iter()
            .find(|s| s.id() == sf)?
            .node(id)?
            .prop("age")?
            .as_i64()
    }

    fn nested_age(doc: &Document, sf: &str, id: &str, key: &str) -> Option<i64> {
        doc.surfaces()
            .iter()
            .find(|s| s.id() == sf)?
            .node(id)?
            .prop(key)?
            .get("age")?
            .as_i64()
    }

    /// A document with everything the replay must carry: hello, styles, closable + font,
    /// root props, timers of every kind, settle, focus, suspend; a closed surface, an open
    /// one, a `listen:false` flow surface; two blobs.
    fn rich(t0: Instant) -> Document {
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut doc = Document::new();
        let mut apply = |ms: u64, m: Message| {
            doc.apply(&m, at(ms));
        };
        apply(
            0,
            msg(
                Verb::Query,
                &json!({"q": "hello", "v": [1], "app": "omp", "ver": "18.15.0",
                        "features": ["edit", "send"]}),
            ),
        );
        apply(
            0,
            msg(
                Verb::Open,
                &json!({"id": "s1", "mode": "inline", "role": "omp.session", "title": "Chat",
                        "font": "mono", "closable": true}),
            ),
        );
        apply(
            0,
            msg(
                Verb::Style,
                &json!({"sf": "s1", "name": "base", "css": ".a{color:red}"}),
            ),
        );
        apply(
            0,
            msg(
                Verb::Style,
                &json!({"sf": "s1", "name": "extra", "css": ".b{}"}),
            ),
        );
        apply(
            0,
            msg(
                Verb::Frame,
                &json!({"sf": "s1", "s": 7, "ops": [
                    ["set", "s1", {"theme": "dark"}],
                    ["add", "main", "s1", null, {"id": "main", "k": "col", "c": [
                        {"id": "e1", "k": "elapsed", "p": {"age": 100, "format": "short"}},
                        {"id": "e2", "k": "elapsed", "p": {"age": 5, "stopped": 5}},
                        {"id": "e3", "k": "elapsed", "p": {"age": 1}},
                        {"id": "e4", "k": "elapsed", "p": {"age": -3000}},
                        {"id": "t1", "k": "tool", "p": {"status": "running", "age": 10}},
                        {"id": "t2", "k": "tool", "p": {"status": "done", "age": 3, "took": 3}},
                        {"id": "a1", "k": "agent", "p": {"status": "running",
                            "tool": {"name": "read", "age": 20}, "stats": {"age": 30, "tools": 2},
                            "retry": null}},
                        {"id": "a2", "k": "agent", "p": {"status": "done", "stats": {"age": 30}}},
                        {"id": "x", "k": "text", "p": {"text": "caf\u{e9} \u{1F600}"}},
                        {"id": "img1", "k": "image",
                            "p": {"blob": hex(&Sha256::digest(b"one")), "alt": "a"}},
                    ]}],
                    ["add", "dock", "s1", null, {"id": "dock", "k": "col", "c": [
                        {"id": "ed", "k": "editor", "p": {"text": "hi"}}]}],
                    ["add", "layer", "s1", null, {"id": "layer", "k": "col"}],
                    ["settle", "t2"], ["settle", "main"],
                    ["focus", "ed"], ["suspend"],
                ]}),
            ),
        );
        // e3 restated later: only the time since this set counts.
        apply(
            4_000,
            msg(
                Verb::Frame,
                &json!({"sf": "s1", "s": 8, "ops": [["set", "e3", {"age": 50}]]}),
            ),
        );
        // Opening a second inline surface closes s1 (dock, layer and focus go).
        apply(
            5_000,
            msg(
                Verb::Open,
                &json!({"id": "s2", "mode": "inline", "role": "omp.session"}),
            ),
        );
        apply(
            5_000,
            msg(
                Verb::Frame,
                &json!({"sf": "s2", "s": 3, "ops": [
                    ["add", "main", "s2", null, {"id": "main", "k": "col", "c": [
                        {"id": "t", "k": "text", "p": {"text": "second"}},
                        {"id": "img2", "k": "image", "p": {"blob": hex(&Sha256::digest(b"two"))}}]}],
                    ["settle", "t"],
                ]}),
            ),
        );
        apply(
            5_000,
            msg(
                Verb::Open,
                &json!({"id": "fl", "mode": "flow", "listen": false}),
            ),
        );
        apply(
            5_000,
            msg(
                Verb::Frame,
                &json!({"sf": "fl", "s": 1, "ops": [
                    ["add", "main", "fl", null, {"id": "main", "k": "col"}]]}),
            ),
        );
        apply(6_000, blob(b"one", Some("image/png")));
        apply(6_000, blob(b"two", None));
        apply(6_000, blob(b"three", None));
        // `one` is the most recently used now.
        let one = hex(&Sha256::digest(b"one"));
        assert_eq!(doc.blobs_have(&[one]).len(), 1);
        doc
    }

    /// A serialized document with the `age` props of timer nodes removed.
    fn untimed(v: &Value) -> Value {
        let mut v = v.clone();
        fn walk(node: &mut Value) {
            let kind = node["k"].as_str().unwrap_or("").to_owned();
            if let Some(p) = node.get_mut("p").and_then(Value::as_object_mut) {
                match kind.as_str() {
                    "elapsed" | "tool" => {
                        p.remove("age");
                    }
                    "agent" => {
                        for key in ["tool", "stats", "retry"] {
                            if let Some(o) = p.get_mut(key).and_then(Value::as_object_mut) {
                                o.remove("age");
                            }
                        }
                    }
                    _ => {}
                }
            }
            for c in node["c"].as_array_mut().into_iter().flatten() {
                walk(c);
            }
        }
        for s in v["surfaces"].as_array_mut().into_iter().flatten() {
            walk(&mut s["root"]);
        }
        v
    }

    #[test]
    fn replay_rebuilds_every_part_of_a_rich_document_and_ticks_the_timers() {
        let t0 = Instant::now();
        let doc = rich(t0);
        let now = t0 + Duration::from_millis(10_000);
        let bytes = doc.replay(now);

        let mut mirror = Mirror::new();
        assert!(mirror.feed(&bytes, now));
        assert!(mirror.is_idle());
        let m = mirror.document();
        // Equal serialization, timer ages aside (checked below) and the blob nobody refers
        // to (checked in its own test).
        let mut expected = untimed(&serde_json::to_value(&doc).unwrap());
        let three = hex(&Sha256::digest(b"three"));
        let listed = expected["blobs"].as_array_mut().unwrap();
        let before = listed.len();
        listed.retain(|b| b["id"] != json!(three));
        assert_eq!(listed.len() + 1, before);
        assert_eq!(untimed(&serde_json::to_value(m).unwrap()), expected);

        // What the serialization leaves out.
        let p = m.program().expect("program");
        assert_eq!(
            (p.app.as_deref(), p.ver.as_deref(), &p.features[..]),
            (
                Some("omp"),
                Some("18.15.0"),
                &["edit".to_owned(), "send".to_owned()][..]
            )
        );
        let s1 = &m.surfaces()[0];
        assert!(!s1.is_open(), "s1 was closed by the second inline surface");
        assert_eq!(
            (s1.title(), s1.font(), s1.is_closable()),
            (Some("Chat"), Some("mono"), true)
        );
        assert_eq!(
            s1.styles().collect::<Vec<_>>(),
            [("base", ".a{color:red}"), ("extra", ".b{}")]
        );
        assert_eq!(s1.last_seq(), Some(8), "the seq of the last frame");
        assert!(s1.node("dock").is_none() && s1.node("layer").is_none());
        assert_eq!(s1.focus(), None);
        assert!(!s1.is_suspended());
        assert_eq!(s1.root().prop("theme"), Some(&json!("dark")));
        // `main` was settled, then a later `set` on its descendant e3 cleared that.
        assert!(s1.node("t2").unwrap().is_settled() && !s1.node("main").unwrap().is_settled());
        assert_eq!(s1.node("x").unwrap().text(), Some("caf\u{e9} \u{1F600}"));
        let s2 = &m.surfaces()[1];
        assert!(s2.is_open() && s2.node("t").unwrap().is_settled());
        let fl = &m.surfaces()[2];
        assert!(fl.is_open() && !fl.listens());
        assert_eq!(m.surfaces().len(), 3);

        // The same surface while still open keeps dock, focus and suspend.
        let mut live = Document::new();
        for msg_ in [
            msg(
                Verb::Open,
                &json!({"id": "s1", "mode": "inline", "role": "omp.session"}),
            ),
            msg(
                Verb::Frame,
                &json!({"sf": "s1", "s": 2, "ops": [
                    ["add", "main", "s1", null, {"id": "main", "k": "col"}],
                    ["add", "dock", "s1", null, {"id": "dock", "k": "col", "c": [
                        {"id": "ed", "k": "editor"}]}],
                    ["settle", "main"], ["focus", "ed"], ["suspend"],
                ]}),
            ),
        ] {
            live.apply(&msg_, t0);
        }
        let mut twin = Mirror::new();
        twin.feed(&live.replay(t0), t0);
        let s = &twin.document().surfaces()[0];
        assert_eq!(s.focus(), Some("ed"));
        assert!(s.is_suspended() && s.node("main").unwrap().is_settled());
        assert!(
            !s.node("dock").unwrap().is_settled(),
            "settle is only on what was settled"
        );

        // Timers: advanced by the time since each node last changed; nothing else moves.
        let d = |id: &str| age(m, "s1", id);
        assert_eq!(d("e1"), Some(10_100), "ticking elapsed: +10 s");
        assert_eq!(d("e2"), Some(5), "stopped elapsed is frozen");
        assert_eq!(
            d("e3"),
            Some(50 + 6_000),
            "restated at 4 s: only the 6 s since count"
        );
        assert_eq!(
            d("e4"),
            Some(-3_000 + 10_000),
            "countdowns tick up from negative"
        );
        assert_eq!(d("t1"), Some(10_010), "running tool");
        assert_eq!(d("t2"), Some(3), "done tool is frozen");
        assert_eq!(nested_age(m, "s1", "a1", "tool"), Some(10_020));
        assert_eq!(nested_age(m, "s1", "a1", "stats"), Some(10_030));
        assert_eq!(
            nested_age(m, "s1", "a2", "stats"),
            Some(30),
            "done agent is frozen"
        );

        // Replaying the mirror at the same instant yields the very same bytes: the replay
        // loses nothing, in order or content (blob LRU order included).
        assert!(mirror.document().replay(now) == bytes);
        // Blobs: two then one (one was used last), with and without mime; `three` is not
        // referenced by any node.
        let mut order = Vec::new();
        for msg_ in Splitter::new().feed(&bytes, now) {
            if msg_.verb == Verb::Blob {
                order.push((msg_.param("mime").map(str::to_owned), msg_.body.clone()));
            }
        }
        assert_eq!(
            order,
            [
                (None, STANDARD.encode(b"two").into_bytes()),
                (
                    Some("image/png".to_owned()),
                    STANDARD.encode(b"one").into_bytes()
                ),
            ]
        );
    }

    #[test]
    fn an_empty_document_replays_to_nothing() {
        assert!(Document::new().replay(Instant::now()).is_empty());
    }

    #[test]
    fn a_retry_countdown_advances_whatever_the_agent_status() {
        let t0 = Instant::now();
        let mut doc = Document::new();
        doc.apply(
            &msg(
                Verb::Open,
                &json!({"id": "s1", "mode": "inline", "role": "omp.session"}),
            ),
            t0,
        );
        doc.apply(
            &msg(
                Verb::Frame,
                &json!({"sf": "s1", "s": 1, "ops": [
                    ["add", "main", "s1", null, {"id": "main", "k": "col", "c": [
                        {"id": "run", "k": "agent", "p": {"status": "running",
                            "retry": {"age": 100, "attempt": 1}, "stats": {"age": 7}}},
                        {"id": "failed", "k": "agent", "p": {"status": "failed",
                            "retry": {"age": 200, "attempt": 2}, "stats": {"age": 7},
                            "tool": {"name": "read", "age": 9}}},
                        {"id": "noretry", "k": "agent", "p": {"status": "done"}},
                    ]}],
                ]}),
            ),
            t0,
        );
        let now = t0 + Duration::from_millis(2_500);
        let mut mirror = Mirror::new();
        mirror.feed(&doc.replay(now), now);
        let m = mirror.document();
        assert_eq!(nested_age(m, "s1", "run", "retry"), Some(2_600));
        assert_eq!(nested_age(m, "s1", "run", "stats"), Some(2_507));
        assert_eq!(
            nested_age(m, "s1", "failed", "retry"),
            Some(2_700),
            "the renderer counts the retry down for any status"
        );
        assert_eq!(nested_age(m, "s1", "failed", "stats"), Some(7));
        assert_eq!(nested_age(m, "s1", "failed", "tool"), Some(9));
        assert!(
            m.surface("s1").unwrap().node("noretry").is_some(),
            "an agent without a retry is replayed as it is"
        );
    }

    /// The blob messages of a replay, in order, as `(id, bytes)`.
    fn replayed_blobs(bytes: &[u8], now: Instant) -> Vec<(String, Vec<u8>)> {
        Splitter::new()
            .feed(bytes, now)
            .into_iter()
            .filter(|m| m.verb == Verb::Blob)
            .map(|m| {
                let decoded = STANDARD.decode(&m.body).expect("base64");
                (hex(&Sha256::digest(&decoded)), decoded)
            })
            .collect()
    }

    #[test]
    fn only_blobs_some_node_refers_to_are_replayed_and_a_peek_carries_none() {
        let t0 = Instant::now();
        let doc = rich(t0);
        let id = |b: &[u8]| hex(&Sha256::digest(b));
        assert!(
            doc.blob(&id(b"three")).is_some(),
            "retained but unreferenced"
        );

        let bytes = doc.replay(t0);
        let blobs = replayed_blobs(&bytes, t0);
        assert_eq!(
            blobs.iter().map(|(_, b)| b.as_slice()).collect::<Vec<_>>(),
            [b"two".as_slice(), b"one".as_slice()],
            "the unreferenced `three` stays out; the rest in LRU order"
        );
        let mut mirror = Mirror::new();
        mirror.feed(&bytes, t0);
        assert!(mirror.document().blob(&id(b"one")).is_some());
        assert!(mirror.document().blob(&id(b"two")).is_some());
        assert!(mirror.document().blob(&id(b"three")).is_none());

        // A reference at any depth of any prop counts, by exact id only.
        let mut deep = Document::new();
        deep.apply(&blob(b"deep", None), t0);
        deep.apply(&blob(b"unused", None), t0);
        deep.apply(
            &msg(
                Verb::Open,
                &json!({"id": "s1", "mode": "inline", "role": "omp.session"}),
            ),
            t0,
        );
        deep.apply(
            &msg(
                Verb::Frame,
                &json!({"sf": "s1", "s": 1, "ops": [
                    ["add", "main", "s1", null, {"id": "main", "k": "col", "c": [
                        {"id": "n", "k": "text", "p": {"items": [{"icon": id(b"deep")}],
                            "text": id(b"unused").to_uppercase()}}]}],
                ]}),
            ),
            t0,
        );
        let blobs = replayed_blobs(&deep.replay(t0), t0);
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].1, b"deep");

        // Without blobs the replay is the same structure, minus every `b` message.
        let bare = doc.replay_with(t0, ReplayBlobs::None);
        assert!(replayed_blobs(&bare, t0).is_empty());
        assert!(bare.len() < bytes.len());
        assert!(
            bytes.starts_with(&bare),
            "blobs come last, after the surfaces"
        );
        let mut structure = Mirror::new();
        structure.feed(&bare, t0);
        assert_eq!(
            serde_json::to_value(structure.document().surfaces()).unwrap(),
            serde_json::to_value(mirror.document().surfaces()).unwrap()
        );
        assert!(structure.document().blob(&id(b"one")).is_none());
    }

    #[test]
    fn a_big_tree_splits_into_frames_that_all_carry_the_surface_seq() {
        let t0 = Instant::now();
        // 40 nodes of 200 KB: 8 MB of ops, over the 4 MiB frame budget and every APC chunked.
        let unit = "é€😀x".repeat(20_000);
        assert!(unit.len() > 190_000);
        let kids: Vec<Value> = (0..40)
            .map(|i| json!({"id": format!("n{i}"), "k": "text", "p": {"text": format!("{i}{unit}")}}))
            .collect();
        let mut doc = Document::new();
        doc.apply(
            &msg(
                Verb::Open,
                &json!({"id": "s1", "mode": "inline", "role": "omp.session"}),
            ),
            t0,
        );
        // Chunks of the program's own writes are not what is tested; apply the body whole.
        doc.apply(
            &msg(
                Verb::Frame,
                &json!({"sf": "s1", "s": 9, "ops": [
                    ["add", "main", "s1", null, {"id": "main", "k": "col", "c": kids}]]}),
            ),
            t0,
        );
        let bytes = doc.replay(t0);
        let frames: Vec<Message> = Splitter::new()
            .feed(&bytes, t0)
            .into_iter()
            .filter(|m| m.verb == Verb::Frame)
            .collect();
        assert!(frames.len() >= 2, "{} frames", frames.len());
        for f in &frames {
            let v: Value = serde_json::from_slice(&f.body).unwrap();
            assert_eq!((v["sf"].as_str(), v["s"].as_u64()), (Some("s1"), Some(9)));
            assert!(f.body.len() < 5 << 20, "frame body {}", f.body.len());
        }
        let mut mirror = Mirror::new();
        mirror.feed(&bytes, t0);
        assert!(mirror.is_idle() && mirror.live());
        let n = mirror.document().surface("s1").unwrap();
        assert_eq!(n.last_seq(), Some(9));
        assert_eq!(n.len(), 42, "root, main and 40 children");
        for i in 0..40 {
            assert_eq!(
                n.node(&format!("n{i}")).unwrap().text(),
                Some(format!("{i}{unit}").as_str()),
                "n{i}"
            );
        }
        assert_eq!(n.node("main").unwrap().child_ids().len(), 40);
        assert_eq!(
            n.node("main").unwrap().child_ids()[39],
            "n39",
            "document order is kept across frames"
        );
    }
}
