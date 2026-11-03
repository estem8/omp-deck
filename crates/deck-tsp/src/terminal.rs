//! The terminal side of TSP: answers a program's `hello`/`blobs` queries, applies its surface
//! messages to a [`Document`] and produces the bytes to write to the child's input (hello
//! reply, acks, error events, view events).
//!
//! [`Responder::on_output`] is fed the child's PTY output. What it returns must be written to
//! the child's input in the order given, and ahead of any other terminal reply generated from
//! the same output (the spec requires the `hello` reply *before* the DA1 reply).
//!
//! # Hello reply and advertised features
//!
//! Kinds: all of [`KINDS`] except `json` (omp draws arguments/results as `tree` or text)
//! and `media` (omp sends blob-backed `image` nodes, also usable on remote hosts).
//! Features ([`FEATURES`]) are limited to what this crate honours:
//! - `blobs`: the `b` verb and the `blobs` query (verified by SHA-256, LRU-capped store);
//! - `settle`: tracked per node ([`crate::document::NodeRef::is_settled`]);
//! - `adopt`: `o` with `adopt:true` reopens a closed inline surface;
//! - `reduce-motion`: the reply carries `reduceMotion`, [`Responder::motion`] sends `motion`.
//!
//! Not advertised: `dock`/`aside`/`font` (renderer layout, not protocol state: this crate draws
//! nothing and omp 18.15 never branches on `dock`), `scroll` (the `scroll` op is only
//! validated), `styles` (stylesheets are stored, never applied), `flow` (flow surfaces are
//! accepted but not promised). `apc` is 65 536 and `credits` 2 (the spec defaults): the
//! responder acks every frame it applied at the end of each `on_output` call, so the window
//! never closes while the host keeps reading the PTY.
//!
//! Nothing is written before the `hello` is answered ("never write TSP events into the PTY
//! before hello", `docs/notes/tsp.md`): acks and errors raised earlier are held and sent right
//! behind the hello reply, and view events ([`Responder::resize`], [`Responder::theme`],
//! [`Responder::motion`]) only update the state the reply will carry.

use std::time::Instant;

use crate::document::Document;
pub use crate::document::Program;
use crate::stream::{Message, Splitter, Verb, encode_message};
use crate::wire::{DEFAULT_APC_LIMIT, DEFAULT_CREDITS, Event, KINDS, Query, Reply, VERSION};

/// `term` of the hello reply.
pub const TERM_NAME: &str = "omp-deck";
/// Terminal features of the hello reply (see the module docs).
pub const FEATURES: [&str; 4] = ["blobs", "settle", "adopt", "reduce-motion"];
/// Errors held before the hello is answered, at most (older ones are kept, newer dropped).
const MAX_HELD_ERRORS: usize = 256;

/// How terminal → program messages are framed on the child's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEncoding {
    /// `ESC _ tsp;… ESC \` (macOS/Linux).
    Apc,
    /// `ESC ] 877 ; tsp ; … ESC \` through a Windows ConPTY (which drops APC on input); the
    /// body's U+0080–U+009F travel as JSON escapes.
    Osc877,
}

impl InputEncoding {
    /// The framing of the platform this was compiled for.
    pub const fn native() -> InputEncoding {
        if cfg!(windows) {
            InputEncoding::Osc877
        } else {
            InputEncoding::Apc
        }
    }
}

/// What the terminal tells the program about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermInfo {
    /// Pane width in columns.
    pub cols: u32,
    pub dark: bool,
    pub reduce_motion: bool,
    /// The user's system reads a 12-hour clock.
    pub hour12: bool,
    pub input: InputEncoding,
}

/// The hello reply for `info`.
pub fn hello_reply(info: &TermInfo) -> Reply {
    Reply::Hello {
        v: VERSION,
        term: TERM_NAME.to_owned(),
        ver: Some(env!("CARGO_PKG_VERSION").to_owned()),
        kinds: KINDS
            .iter()
            .filter(|k| !matches!(**k, "json" | "media"))
            .map(|k| (*k).to_owned())
            .collect(),
        features: FEATURES.iter().map(|f| (*f).to_owned()).collect(),
        apc: Some(DEFAULT_APC_LIMIT),
        credits: Some(DEFAULT_CREDITS),
        cols: Some(info.cols),
        cell: None,
        dark: Some(info.dark),
        reduce_motion: Some(info.reduce_motion),
        hour12: Some(info.hour12),
    }
}

/// Replaces U+0080–U+009F (C1 controls, which a terminal layer may take as string
/// terminators) by their JSON escapes. Only valid inside JSON strings, where they occur.
fn escape_c1(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for ch in json.chars() {
        if ('\u{80}'..='\u{9f}').contains(&ch) {
            out.push_str(&format!("\\u{:04x}", u32::from(ch)));
        } else {
            out.push(ch);
        }
    }
    out
}

fn frame_input(verb: Verb, json: &[u8], enc: InputEncoding) -> Vec<u8> {
    match enc {
        InputEncoding::Apc => encode_message(verb, &[], json),
        InputEncoding::Osc877 => {
            // `serde_json` writes valid UTF-8.
            let text = String::from_utf8_lossy(json);
            let body = escape_c1(&text);
            let mut out = Vec::with_capacity(body.len() + 16);
            out.extend_from_slice(b"\x1b]877;tsp;");
            out.push(verb.as_byte());
            out.push(b';');
            out.extend_from_slice(body.as_bytes());
            out.extend_from_slice(b"\x1b\\");
            out
        }
    }
}

/// An event as the bytes to write to the program's input (`tsp;e`, never chunked).
pub fn encode_event(event: &Event, enc: InputEncoding) -> Vec<u8> {
    match serde_json::to_vec(event) {
        Ok(json) => frame_input(Verb::Event, &json, enc),
        Err(e) => {
            // Every event is plain data; serialization cannot fail.
            log::error!("encoding a TSP event: {e}");
            Vec::new()
        }
    }
}

/// A reply as the bytes to write to the program's input (`tsp;r`).
pub fn encode_reply(reply: &Reply, enc: InputEncoding) -> Vec<u8> {
    match serde_json::to_vec(reply) {
        Ok(json) => frame_input(Verb::Reply, &json, enc),
        Err(e) => {
            log::error!("encoding a TSP reply: {e}");
            Vec::new()
        }
    }
}

/// The terminal role for one pane; see the module docs.
pub struct Responder {
    splitter: Splitter,
    document: Document,
    info: TermInfo,
    live: bool,
    held_acks: Vec<(String, u64)>,
    held_errors: Vec<Event>,
}

impl Responder {
    pub fn new(info: TermInfo) -> Responder {
        Responder {
            splitter: Splitter::new(),
            document: Document::new(),
            info,
            live: false,
            held_acks: Vec::new(),
            held_errors: Vec::new(),
        }
    }

    /// The pane's surfaces and blobs.
    pub fn document(&self) -> &Document {
        &self.document
    }

    /// Whether the `hello` was answered (the program speaks TSP with us).
    pub fn live(&self) -> bool {
        self.live
    }

    /// What the terminal currently reports (cols, theme, motion).
    pub fn info(&self) -> &TermInfo {
        &self.info
    }

    /// The program's last `hello` (`None` before one); kept by the document.
    pub fn program(&self) -> Option<&Program> {
        self.document.program()
    }

    /// Bytes that finish what a replay of [`Responder::document`] cannot carry: the chunked
    /// message and APC string still arriving (see [`Splitter::resume_bytes`]), at `now`.
    pub fn resume_bytes(&self, now: Instant) -> Vec<u8> {
        let mut out = Vec::new();
        self.splitter.resume_bytes(now, &mut out);
        out
    }

    /// Scans `bytes` of child output. Returns the bytes for the child's input, in order: the
    /// hello reply (and `blobs` replies) as they are met, then one ack per surface covering
    /// every frame applied by this call, then error events. Empty (and allocation-free) when
    /// the output holds no TSP message.
    pub fn on_output(&mut self, bytes: &[u8], now: Instant) -> Vec<u8> {
        let messages = self.splitter.feed(bytes, now);
        if messages.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for msg in &messages {
            if msg.verb == Verb::Query {
                self.query(msg, &mut out);
                continue;
            }
            let applied = self.document.apply(msg, now);
            for (sf, s) in applied.acks {
                match self.held_acks.iter_mut().find(|(f, _)| *f == sf) {
                    Some((_, held)) => *held = (*held).max(s),
                    None => self.held_acks.push((sf, s)),
                }
            }
            self.hold_errors(applied.errors);
        }
        if self.live {
            for (sf, s) in std::mem::take(&mut self.held_acks) {
                out.extend(encode_event(&Event::Ack { sf, s }, self.info.input));
            }
            for e in std::mem::take(&mut self.held_errors) {
                out.extend(encode_event(&e, self.info.input));
            }
        }
        out
    }

    fn hold_errors(&mut self, errors: Vec<Event>) {
        for e in errors {
            if self.held_errors.len() >= MAX_HELD_ERRORS {
                break;
            }
            self.held_errors.push(e);
        }
    }

    fn query(&mut self, msg: &Message, out: &mut Vec<u8>) {
        let query: Query = match serde_json::from_slice(&msg.body) {
            Ok(q) => q,
            Err(e) => {
                self.hold_errors(vec![Event::error(format!("malformed q body: {e}"))]);
                return;
            }
        };
        match query {
            Query::Hello {
                v,
                app,
                ver,
                features,
            } => {
                // A query that does not list version 1 gets no reply: no TSP.
                if !v.contains(&VERSION) {
                    return;
                }
                self.document.set_program(Program { app, ver, features });
                self.live = true;
                out.extend(encode_reply(&hello_reply(&self.info), self.info.input));
            }
            Query::Blobs { ids } => {
                let have = self.document.blobs_have(&ids);
                out.extend(encode_reply(&Reply::Blobs { have }, self.info.input));
            }
            Query::Unknown => {}
        }
    }

    /// The pane's width changed. Returns the `resize` event once the hello was answered; before
    /// that only the state the reply carries changes. Nothing when `cols` did not change.
    pub fn resize(&mut self, cols: u32) -> Vec<u8> {
        if cols == self.info.cols {
            return Vec::new();
        }
        self.info.cols = cols;
        self.view_event(Event::Resize {
            sf: None,
            cols,
            cell: None,
            visible: None,
        })
    }

    /// The appearance switched (see [`Responder::resize`] for the return value).
    pub fn theme(&mut self, dark: bool) -> Vec<u8> {
        if dark == self.info.dark {
            return Vec::new();
        }
        self.info.dark = dark;
        self.view_event(Event::Theme { dark })
    }

    /// Reduce Motion was toggled (see [`Responder::resize`] for the return value).
    pub fn motion(&mut self, reduce: bool) -> Vec<u8> {
        if reduce == self.info.reduce_motion {
            return Vec::new();
        }
        self.info.reduce_motion = reduce;
        self.view_event(Event::Motion { reduce })
    }

    fn view_event(&self, event: Event) -> Vec<u8> {
        if self.live {
            encode_event(&event, self.info.input)
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::encode_chunked;
    use serde_json::{Value, json};

    fn info() -> TermInfo {
        TermInfo {
            cols: 120,
            dark: true,
            reduce_motion: false,
            hour12: false,
            input: InputEncoding::Apc,
        }
    }

    fn q(body: &Value) -> Vec<u8> {
        encode_message(Verb::Query, &[], body.to_string().as_bytes())
    }

    fn o(id: &str) -> Vec<u8> {
        encode_message(
            Verb::Open,
            &[],
            json!({"id": id, "mode": "inline"}).to_string().as_bytes(),
        )
    }

    fn f(sf: &str, s: u64, ops: Value) -> Vec<u8> {
        encode_message(
            Verb::Frame,
            &[],
            json!({"sf": sf, "s": s, "ops": ops}).to_string().as_bytes(),
        )
    }

    fn hello_query() -> Vec<u8> {
        q(
            &json!({"q":"hello","v":[1],"app":"omp","ver":"18.15.0","features":["edit","undo","send"]}),
        )
    }

    /// Splits terminal → program bytes back into `(verb, json)` pairs.
    fn input_messages(bytes: &[u8]) -> Vec<(Verb, Value)> {
        Splitter::new()
            .feed(bytes, Instant::now())
            .into_iter()
            .map(|m| (m.verb, serde_json::from_slice(&m.body).unwrap()))
            .collect()
    }

    #[test]
    fn hello_reply_is_exact_and_carries_the_vocabulary() {
        let mut r = Responder::new(info());
        let out = r.on_output(&hello_query(), Instant::now());
        let kinds: Vec<&str> = KINDS
            .iter()
            .copied()
            .filter(|k| !matches!(*k, "json" | "media"))
            .collect();
        let expected = json!({
            "r": "hello", "v": 1, "term": "omp-deck", "ver": env!("CARGO_PKG_VERSION"),
            "kinds": kinds,
            "features": ["blobs", "settle", "adopt", "reduce-motion"],
            "apc": 65536, "credits": 2, "cols": 120, "dark": true, "reduceMotion": false,
            "hour12": false,
        });
        let msgs = input_messages(&out);
        assert_eq!(msgs, [(Verb::Reply, expected)]);
        assert!(
            out.starts_with(b"\x1b_tsp;r;{\"r\":\"hello\",\"v\":1,") && out.ends_with(b"\x1b\\")
        );
        assert!(r.live());
        let p = r.program().unwrap();
        assert_eq!(
            (p.app.as_deref(), p.ver.as_deref()),
            (Some("omp"), Some("18.15.0"))
        );
        assert!(p.supports("send") && p.supports("edit") && !p.supports("nope"));
        assert_eq!(kinds.len(), 43);
        assert!(!kinds.contains(&"json"));
        assert!(!kinds.contains(&"media"));
    }

    #[test]
    fn a_hello_without_version_1_gets_no_reply() {
        let mut r = Responder::new(info());
        let out = r.on_output(
            &q(&json!({"q":"hello","v":[2,3],"app":"x"})),
            Instant::now(),
        );
        assert!(out.is_empty());
        assert!(!r.live());
        assert!(
            r.on_output(&q(&json!({"q":"hello"})), Instant::now())
                .is_empty()
        );
        assert!(!r.live());
    }

    #[test]
    fn nothing_is_written_before_hello_and_held_acks_follow_the_reply() {
        let mut r = Responder::new(info());
        let now = Instant::now();
        // The optimistic start: surface messages before the reply is asked for.
        assert!(r.on_output(&o("s1"), now).is_empty());
        let bad = r.on_output(&f("s1", 1, json!([["del", "ghost"]])), now);
        assert!(bad.is_empty());
        assert!(r.resize(100).is_empty() && r.theme(false).is_empty() && r.motion(true).is_empty());
        assert!(r.on_output(b"plain output \x1b[31m", now).is_empty());
        let out = r.on_output(&hello_query(), now);
        let msgs = input_messages(&out);
        assert_eq!(msgs.len(), 3, "{msgs:?}");
        // The reply carries the state changed meanwhile; acks and errors follow it.
        assert_eq!(msgs[0].1["r"], "hello");
        assert_eq!(
            (
                msgs[0].1["cols"].clone(),
                msgs[0].1["dark"].clone(),
                msgs[0].1["reduceMotion"].clone()
            ),
            (json!(100), json!(false), json!(true))
        );
        assert_eq!(msgs[1], (Verb::Event, json!({"ev":"ack","sf":"s1","s":1})));
        assert_eq!(
            msgs[2],
            (
                Verb::Event,
                json!({"ev":"error","sf":"s1","s":1,"op":0,"msg":"unknown id ghost"})
            )
        );
    }

    #[test]
    fn hello_precedes_acks_of_the_same_call_and_acks_are_coalesced_per_surface() {
        let mut r = Responder::new(info());
        let mut wire = o("s1");
        wire.extend(f(
            "s1",
            1,
            json!([["add", "main", "s1", null, {"id":"main","k":"col"}]]),
        ));
        wire.extend(hello_query());
        wire.extend(f(
            "s1",
            2,
            json!([["add", "t", "main", null, {"id":"t","k":"text"}]]),
        ));
        wire.extend(f("s1", 3, json!([["text", "t", "append", "x"]])));
        wire.extend(encode_message(
            Verb::Open,
            &[],
            br#"{"id":"s2","mode":"screen"}"#,
        ));
        wire.extend(f("s2", 7, json!([])));
        let out = r.on_output(&wire, Instant::now());
        let msgs = input_messages(&out);
        assert_eq!(
            msgs.iter()
                .map(|(v, j)| (*v, j["r"].as_str().or(j["ev"].as_str()).unwrap().to_owned()))
                .collect::<Vec<_>>(),
            [
                (Verb::Reply, "hello".to_owned()),
                (Verb::Event, "ack".to_owned()),
                (Verb::Event, "ack".to_owned())
            ]
        );
        assert_eq!(msgs[1].1, json!({"ev":"ack","sf":"s1","s":3}));
        assert_eq!(msgs[2].1, json!({"ev":"ack","sf":"s2","s":7}));
        assert_eq!(
            r.document()
                .surface("s1")
                .unwrap()
                .node("t")
                .unwrap()
                .text(),
            Some("x")
        );
        // A call with no TSP in it writes nothing, and acks are not repeated.
        assert!(r.on_output(b"just text", Instant::now()).is_empty());
        let again = r.on_output(&f("s1", 4, json!([])), Instant::now());
        assert_eq!(
            input_messages(&again),
            [(Verb::Event, json!({"ev":"ack","sf":"s1","s":4}))]
        );
    }

    #[test]
    fn rejected_ops_become_error_events_with_their_index_and_the_rest_still_apply() {
        let mut r = Responder::new(info());
        let mut wire = hello_query();
        wire.extend(o("s1"));
        wire.extend(f(
            "s1",
            1,
            json!([
                ["add", "main", "s1", null, {"id":"main","k":"col"}],
                ["set", "ghost", {"a": 1}],
                ["add", "m", "main", null, {"id":"m","k":"md","p":{"text":"ok"}}],
                ["splice", "m", 1, 5, "x"],
            ]),
        ));
        let msgs = input_messages(&r.on_output(&wire, Instant::now()));
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[1].1, json!({"ev":"ack","sf":"s1","s":1}));
        assert_eq!(
            msgs[2].1,
            json!({"ev":"error","sf":"s1","s":1,"op":1,"msg":"unknown id ghost"})
        );
        assert_eq!(msgs[3].1["op"], 3);
        assert_eq!(
            r.document()
                .surface("s1")
                .unwrap()
                .node("m")
                .unwrap()
                .text(),
            Some("ok")
        );
    }

    #[test]
    fn blobs_queries_and_blobs_are_answered() {
        let mut r = Responder::new(info());
        let bytes = b"png-ish";
        let id = crate::document::hex(&sha2::Sha256::digest(bytes));
        use base64::Engine as _;
        use sha2::Digest as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        let mut wire = hello_query();
        wire.extend(encode_message(
            Verb::Blob,
            &[("id", &id), ("mime", "image/png")],
            b64.as_bytes(),
        ));
        wire.extend(q(
            &json!({"q":"blobs","ids":["f".repeat(64), id, "0".repeat(64)]}),
        ));
        let msgs = input_messages(&r.on_output(&wire, Instant::now()));
        assert_eq!(msgs[1], (Verb::Reply, json!({"r":"blobs","have":[id]})));
        assert_eq!(
            r.document().blob(&id).unwrap().mime.as_deref(),
            Some("image/png")
        );
    }

    #[test]
    fn unknown_queries_get_no_reply_and_malformed_bodies_get_an_error() {
        let mut r = Responder::new(info());
        let mut wire = hello_query();
        wire.extend(q(&json!({"q":"teleport"})));
        wire.extend(encode_message(Verb::Query, &[], b"{nope"));
        wire.extend(encode_message(Verb::Unknown(b'z'), &[], b"{}"));
        wire.extend(encode_message(Verb::Palette, &[], b"{}"));
        let msgs = input_messages(&r.on_output(&wire, Instant::now()));
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[1].1["msg"].as_str(), Some(m) if m.starts_with("malformed q body")));
    }

    #[test]
    fn view_events_follow_hello_and_only_when_the_value_changed() {
        let mut r = Responder::new(info());
        r.on_output(&hello_query(), Instant::now());
        assert!(r.resize(120).is_empty());
        assert_eq!(
            r.resize(90),
            b"\x1b_tsp;e;{\"ev\":\"resize\",\"cols\":90}\x1b\\"
        );
        assert_eq!(
            r.theme(false),
            b"\x1b_tsp;e;{\"ev\":\"theme\",\"dark\":false}\x1b\\"
        );
        assert!(r.theme(false).is_empty());
        assert_eq!(
            r.motion(true),
            b"\x1b_tsp;e;{\"ev\":\"motion\",\"reduce\":true}\x1b\\"
        );
        assert_eq!(r.info().cols, 90);
    }

    #[test]
    fn events_encode_exactly_and_osc877_escapes_c1_controls() {
        let ev = Event::Send {
            sf: "s1".into(),
            id: "ed".into(),
            text: "hi".into(),
        };
        assert_eq!(
            encode_event(&ev, InputEncoding::Apc),
            b"\x1b_tsp;e;{\"ev\":\"send\",\"sf\":\"s1\",\"id\":\"ed\",\"text\":\"hi\"}\x1b\\"
        );
        let edit = Event::Edit {
            sf: "s".into(),
            id: "e".into(),
            from: 0,
            to: 0,
            text: "a\u{85}b\u{9c}c\u{a0}\u{7f}".into(),
            cursor: 1,
            len: 0,
        };
        let osc = encode_event(&edit, InputEncoding::Osc877);
        assert_eq!(
            String::from_utf8(osc.clone()).unwrap(),
            "\x1b]877;tsp;e;{\"ev\":\"edit\",\"sf\":\"s\",\"id\":\"e\",\"from\":0,\"to\":0,\"text\":\"a\\u0085b\\u009cc\u{a0}\u{7f}\",\"cursor\":1,\"len\":0}\x1b\\"
        );
        assert!(!osc.contains(&0x85));
        let apc = encode_event(&edit, InputEncoding::Apc);
        assert!(String::from_utf8(apc).unwrap().contains("a\u{85}b\u{9c}c"));
        // The reply through OSC 877 too.
        let mut r = Responder::new(TermInfo {
            input: InputEncoding::Osc877,
            ..info()
        });
        let out = r.on_output(&hello_query(), Instant::now());
        assert!(out.starts_with(b"\x1b]877;tsp;r;{\"r\":\"hello\""));
    }

    #[test]
    fn chunked_frames_through_the_responder_apply_whole() {
        let mut r = Responder::new(info());
        let text = "é€😀;k=v;".repeat(30_000);
        let body = json!({"sf":"s1","s":1,"ops":[["add","main","s1",null,{"id":"main","k":"col","c":[{"id":"t","k":"text","p":{"text":text}}]}]]}).to_string();
        let mut wire = hello_query();
        wire.extend(o("s1"));
        wire.extend(encode_chunked(
            Verb::Frame,
            &[],
            body.as_bytes(),
            65_536,
            "k1",
        ));
        assert!(wire.len() > 3 * 65_536, "the frame really is chunked");
        // Feed in odd-sized pieces.
        let mut out = Vec::new();
        for piece in wire.chunks(7_919) {
            out.extend(r.on_output(piece, Instant::now()));
        }
        let msgs = input_messages(&out);
        assert!(
            msgs.iter()
                .any(|(_, j)| j == &json!({"ev":"ack","sf":"s1","s":1})),
            "{msgs:?}"
        );
        assert_eq!(
            r.document()
                .surface("s1")
                .unwrap()
                .node("t")
                .unwrap()
                .text(),
            Some(text.as_str())
        );
    }
}
