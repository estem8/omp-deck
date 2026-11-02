//! Replays a real omp 18.15.0 session through the terminal side of the crate.
//!
//! `fixtures/omp-18.15.0-session.jsonl` was recorded by omp itself (`PI_TUI_TSP_RECORD`) in a
//! sandbox with a scripted model, through the host terminal: a welcome screen, a streamed
//! multi-chunk assistant reply, a second short turn and a tool call turn (63 frames, 49 `text`
//! appends, adds with subtrees, sets, moves, deletes, settles). Absolute paths of the sandbox
//! were rewritten to `/sandbox` and timestamps made relative. Record format, one JSON object per
//! line: `{"t":ms,"dir":"out"|"in","verb","params","body"}` (`out` = program → terminal).
//!
//! The replay re-encodes every `out` message as the APC omp wrote and feeds it to a
//! [`Responder`] in different chunkings. The resulting document must be the same for every
//! chunking, equal to an independent reference applier written here on plain JSON, and answer
//! every frame with the ack omp's own record shows.

use std::time::{Duration, Instant};

use deck_tsp::stream::{Splitter, Verb, encode_chunked, encode_message};
use deck_tsp::terminal::{InputEncoding, Responder, TermInfo};
use serde_json::{Map, Value, json};

const FIXTURE: &str = include_str!("fixtures/omp-18.15.0-session.jsonl");

struct Record {
    t: u64,
    dir: String,
    verb: String,
    body: Value,
}

fn records() -> Vec<Record> {
    FIXTURE
        .lines()
        .map(|line| {
            let v: Value = serde_json::from_str(line).expect("fixture line is JSON");
            Record {
                t: v["t"].as_u64().expect("t"),
                dir: v["dir"].as_str().expect("dir").to_owned(),
                verb: v["verb"].as_str().expect("verb").to_owned(),
                body: v["body"].clone(),
            }
        })
        .collect()
}

fn verb(v: &str) -> Verb {
    match v {
        "o" => Verb::Open,
        "f" => Verb::Frame,
        "x" => Verb::Close,
        "s" => Verb::Style,
        other => panic!("fixture verb {other}"),
    }
}

fn info() -> TermInfo {
    TermInfo {
        cols: 100,
        dark: true,
        reduce_motion: false,
        hour12: false,
        input: InputEncoding::Apc,
    }
}

fn hello() -> Vec<u8> {
    encode_message(
        Verb::Query,
        &[],
        br#"{"q":"hello","v":[1],"app":"omp","ver":"18.15.0","features":["edit","undo","send"]}"#,
    )
}

/// The wire bytes of every `out` record, one entry per message; bodies over `chunk_limit` are
/// chunked the way omp does.
fn wire_messages(chunk_limit: usize) -> Vec<Vec<u8>> {
    records()
        .into_iter()
        .filter(|r| r.dir == "out")
        .enumerate()
        .map(|(i, r)| {
            encode_chunked(
                verb(&r.verb),
                &[],
                r.body.to_string().as_bytes(),
                chunk_limit,
                &format!("c{i}"),
            )
        })
        .collect()
}

/// What the program receives, split back into events.
fn events(bytes: &[u8]) -> Vec<Value> {
    Splitter::new()
        .feed(bytes, Instant::now())
        .into_iter()
        .filter(|m| m.verb == Verb::Event)
        .map(|m| serde_json::from_slice(&m.body).expect("event JSON"))
        .collect()
}

struct Replayed {
    document: Value,
    /// The responder's output after the hello reply.
    out: Vec<u8>,
}

/// Feeds the stream in pieces of `piece` bytes (`0` = one message per call).
fn replay(chunk_limit: usize, piece: usize) -> Replayed {
    let mut responder = Responder::new(info());
    let start = Instant::now();
    let mut out = Vec::new();
    let reply = responder.on_output(&hello(), start);
    assert!(!reply.is_empty() && responder.live());
    let wire = wire_messages(chunk_limit);
    let t = |i: usize| start + Duration::from_millis(i as u64);
    if piece == 0 {
        for (i, m) in wire.iter().enumerate() {
            out.extend(responder.on_output(m, t(i)));
        }
    } else {
        let all: Vec<u8> = wire.concat();
        for (i, p) in all.chunks(piece).enumerate() {
            out.extend(responder.on_output(p, t(i)));
        }
    }
    Replayed {
        document: serde_json::to_value(responder.document()).expect("document JSON"),
        out,
    }
}

// ---- an independent reference applier on plain JSON --------------------------------------------

fn strip_nulls(p: &mut Map<String, Value>) {
    p.retain(|_, v| !v.is_null());
}

/// `{id,k,p?,c?}` with nulls dropped from every `p`, recursively.
fn normalized(node: &Value) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), node["id"].clone());
    out.insert("k".into(), node["k"].clone());
    if let Some(p) = node["p"].as_object() {
        let mut p = p.clone();
        strip_nulls(&mut p);
        if !p.is_empty() {
            out.insert("p".into(), Value::Object(p));
        }
    }
    if let Some(c) = node["c"].as_array().filter(|c| !c.is_empty()) {
        out.insert("c".into(), Value::Array(c.iter().map(normalized).collect()));
    }
    Value::Object(out)
}

fn find_mut<'a>(node: &'a mut Value, id: &str) -> Option<&'a mut Value> {
    if node["id"] == id {
        return Some(node);
    }
    node["c"]
        .as_array_mut()?
        .iter_mut()
        .find_map(|c| find_mut(c, id))
}

fn detach(node: &mut Value, id: &str) -> Option<Value> {
    let children = node["c"].as_array_mut()?;
    if let Some(pos) = children.iter().position(|c| c["id"] == id) {
        return Some(children.remove(pos));
    }
    children.iter_mut().find_map(|c| detach(c, id))
}

fn attach(root: &mut Value, parent: &str, before: &Value, node: Value) {
    let target = find_mut(root, parent).expect("parent exists");
    if target["c"].is_null() {
        target["c"] = json!([]);
    }
    let children = target["c"].as_array_mut().expect("children");
    match before
        .as_str()
        .and_then(|b| children.iter().position(|c| c["id"] == b))
    {
        Some(pos) => children.insert(pos, node),
        None => children.push(node),
    }
}

fn reference_apply(root: &mut Value, op: &Value) {
    let a = op.as_array().expect("op array");
    match a[0].as_str().expect("op name") {
        "add" => attach(root, a[2].as_str().unwrap(), &a[3], normalized(&a[4])),
        "set" => {
            let node = find_mut(root, a[1].as_str().unwrap()).expect("set target");
            if node["p"].is_null() {
                node["p"] = json!({});
            }
            let p = node["p"].as_object_mut().unwrap();
            for (k, v) in a[2].as_object().expect("props") {
                if v.is_null() {
                    p.remove(k);
                } else {
                    p.insert(k.clone(), v.clone());
                }
            }
            if p.is_empty() {
                node.as_object_mut().unwrap().remove("p");
            }
        }
        "text" => {
            let node = find_mut(root, a[1].as_str().unwrap()).expect("text target");
            let add = a[3].as_str().unwrap();
            let old = if a[2] == "append" {
                node["p"]["text"].as_str().unwrap_or("").to_owned()
            } else {
                String::new()
            };
            if node["p"].is_null() {
                node["p"] = json!({});
            }
            node["p"]["text"] = json!(format!("{old}{add}"));
        }
        "del" => {
            let id = a[1].as_str().unwrap();
            detach(root, id).expect("del target");
            // Emptied child lists are not serialized.
            prune_empty(root);
        }
        "move" => {
            let node = detach(root, a[1].as_str().unwrap()).expect("move target");
            attach(root, a[2].as_str().unwrap(), &a[3], node);
        }
        "settle" | "focus" | "reveal" | "scroll" | "suspend" | "resume" => {}
        other => panic!("fixture op {other}"),
    }
}

fn prune_empty(node: &mut Value) {
    if let Some(children) = node["c"].as_array_mut() {
        for c in children.iter_mut() {
            prune_empty(c);
        }
        if children.is_empty() {
            node.as_object_mut().unwrap().remove("c");
        }
    }
}

/// The root of surface `sf` after every recorded frame, by the reference applier. A node
/// whose children were all deleted and later gets one again is handled by `attach`.
fn reference_tree() -> Value {
    let mut root = json!({"id": "s:1", "k": "col"});
    for r in records()
        .into_iter()
        .filter(|r| r.dir == "out" && r.verb == "f")
    {
        for op in r.body["ops"].as_array().expect("ops") {
            reference_apply(&mut root, op);
        }
    }
    prune_empty(&mut root);
    normalized(&root)
}

// ---- tests -------------------------------------------------------------------------------------

fn all_nodes<'a>(node: &'a Value, out: &mut Vec<&'a Value>) {
    out.push(node);
    for c in node["c"].as_array().into_iter().flatten() {
        all_nodes(c, out);
    }
}

#[test]
fn the_recording_really_covers_what_the_document_must_handle() {
    let rows = records();
    assert_eq!(
        rows.iter()
            .filter(|r| r.dir == "out" && r.verb == "f")
            .count(),
        63
    );
    let mut names = std::collections::BTreeSet::new();
    for r in rows.iter().filter(|r| r.verb == "f" && r.dir == "out") {
        for op in r.body["ops"].as_array().unwrap() {
            names.insert(op[0].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(
        names.into_iter().collect::<Vec<_>>(),
        ["add", "del", "focus", "move", "set", "settle", "text"]
    );
    assert!(
        rows.windows(2).all(|w| w[0].t <= w[1].t),
        "time only moves forward"
    );
}

#[test]
fn every_chunking_yields_the_reference_tree() {
    let expected = reference_tree();
    let whole = replay(65_536, 0);
    let root = &whole.document["surfaces"][0]["root"];
    assert_eq!(
        root, &expected,
        "document differs from the reference applier"
    );

    // Chunked at 2 KiB (several frames do not fit), in odd pieces, byte by byte.
    for (limit, piece) in [(2_048, 0), (65_536, 4_099), (2_048, 977), (65_536, 1)] {
        let r = replay(limit, piece);
        assert_eq!(
            r.document, whole.document,
            "chunk limit {limit}, piece {piece}: document differs"
        );
    }
}

#[test]
fn the_final_document_is_what_omp_showed() {
    let doc = replay(65_536, 0).document;
    let surface = &doc["surfaces"][0];
    assert_eq!(
        (
            &surface["id"],
            &surface["mode"],
            &surface["role"],
            &surface["open"]
        ),
        (
            &json!("s:1"),
            &json!("inline"),
            &json!("omp.session"),
            &json!(true)
        )
    );
    assert_eq!(surface["seq"], 63);
    let mut nodes = Vec::new();
    all_nodes(&surface["root"], &mut nodes);
    assert_eq!(surface["nodes"], nodes.len());

    let editor = nodes
        .iter()
        .find(|n| n["k"] == "editor")
        .expect("the composer's editor");
    assert_eq!(editor["p"]["sendable"], true);
    assert_eq!(surface["focus"], editor["id"], "omp focused the editor");

    let texts: Vec<&str> = nodes
        .iter()
        .filter(|n| n["k"] == "md")
        .filter_map(|n| n["p"]["text"].as_str())
        .collect();
    let streamed = texts
        .iter()
        .find(|t| t.starts_with("Streamed answer: one two three"))
        .expect("the streamed reply (49 appends)");
    assert!(streamed.ends_with("nineteen twenty."), "{streamed}");
    assert!(texts.contains(&"Second answer is complete."), "{texts:?}");
    assert!(texts.contains(&"Tool turn is complete."), "{texts:?}");
    assert!(
        nodes
            .iter()
            .any(|n| n["k"] == "tool" && n.to_string().contains("/sandbox/home/proj/notes.txt")),
        "the tool call names the file"
    );
}

#[test]
fn every_frame_gets_the_ack_omp_recorded() {
    let recorded: Vec<Value> = records()
        .into_iter()
        .filter(|r| r.dir == "in" && r.body["ev"] == "ack")
        .map(|r| r.body)
        .collect();
    assert_eq!(recorded.len(), 63);
    // One frame per call: one ack per frame, same `sf` and `s`.
    let r = replay(65_536, 0);
    assert_eq!(events(&r.out), recorded);
    // Everything in one call: a single ack covers the highest `s`.
    let mut responder = Responder::new(info());
    let mut out = responder.on_output(&hello(), Instant::now());
    out.clear();
    out.extend(responder.on_output(&wire_messages(65_536).concat(), Instant::now()));
    assert_eq!(events(&out), [json!({"ev":"ack","sf":"s:1","s":63})]);
}
