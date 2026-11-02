//! `Document::replay` + `Mirror` against the real omp 18.15.0 recording
//! (`fixtures/omp-18.15.0-session.jsonl`, see `replay.rs`).
//!
//! A mirror fed `replay()` must hold the document it came from at every message boundary, and
//! one that then follows the live stream must end where the source ends.

use std::time::{Duration, Instant};

use deck_tsp::document::{Document, NodeRef};
use deck_tsp::mirror::Mirror;
use deck_tsp::stream::{Verb, encode_chunked, encode_message};
use serde_json::{Map, Value, json};

const FIXTURE: &str = include_str!("fixtures/omp-18.15.0-session.jsonl");

fn hello() -> Vec<u8> {
    encode_message(
        Verb::Query,
        &[],
        br#"{"q":"hello","v":[1],"app":"omp","ver":"18.15.0","features":["edit","undo","send"]}"#,
    )
}

/// The wire bytes of every program → terminal record: one entry per message, the way omp
/// chunks them (512-byte bodies: the larger frames of the recording arrive as chunk sequences).
fn wire_messages() -> Vec<Vec<u8>> {
    FIXTURE
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("fixture line is JSON"))
        .filter(|v| v["dir"] == "out")
        .enumerate()
        .map(|(i, v)| {
            let verb = match v["verb"].as_str().expect("verb") {
                "o" => Verb::Open,
                "f" => Verb::Frame,
                "x" => Verb::Close,
                "s" => Verb::Style,
                other => panic!("fixture verb {other}"),
            };
            encode_chunked(
                verb,
                &[],
                v["body"].to_string().as_bytes(),
                512,
                &format!("c{i}"),
            )
        })
        .collect()
}

/// Whether `n`'s `age` is a ticking one (spec: `elapsed` without `stopped`, running `tool`).
fn ticking(n: NodeRef<'_>) -> bool {
    match n.kind() {
        "elapsed" => n.prop("stopped").is_none_or(Value::is_null),
        "tool" => n.str_prop("status") == Some("running"),
        _ => false,
    }
}

/// Everything the public API exposes of a document, timers' `age` left out.
fn full(doc: &Document) -> Value {
    let surfaces: Vec<Value> = doc
        .surfaces()
        .iter()
        .map(|s| {
            let mut nodes = Vec::new();
            s.walk(&mut |depth, n| {
                let mut p: Map<String, Value> = n.props().clone();
                if ticking(n) {
                    p.remove("age");
                }
                nodes.push(json!({
                    "depth": depth, "id": n.id(), "k": n.kind(), "p": p, "settled": n.is_settled(),
                }));
            });
            json!({
                "id": s.id(), "mode": format!("{:?}", s.mode()), "title": s.title(),
                "role": s.role(), "font": s.font(), "listens": s.listens(),
                "closable": s.is_closable(), "open": s.is_open(), "seq": s.last_seq(),
                "focus": s.focus(), "suspended": s.is_suspended(),
                "styles": s.styles().collect::<Vec<_>>(), "nodes": nodes,
            })
        })
        .collect();
    let program = doc.program().map(|p| json!([p.app, p.ver, p.features]));
    json!({"surfaces": surfaces, "program": program,
           "blobs": serde_json::to_value(doc).expect("document JSON")["blobs"]})
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).expect("small")
}

/// `age` of every ticking node of `src` at `now`, by the spec rule: the age it had plus the
/// time since the node last changed; paired with the id.
fn expected_ages(src: &Document, now: Instant) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    for s in src.surfaces() {
        s.walk(&mut |_, n| {
            if ticking(n)
                && let Some(age) = n.prop("age").and_then(Value::as_i64)
            {
                out.push((
                    n.id().to_owned(),
                    age + ms(now.saturating_duration_since(n.changed_at())),
                ));
            }
        });
    }
    out
}

fn age_of(doc: &Document, id: &str) -> Option<i64> {
    doc.surfaces()
        .iter()
        .find_map(|s| s.node(id))
        .and_then(|n| n.prop("age").and_then(Value::as_i64))
}

#[test]
fn a_mirror_fed_the_replay_holds_the_source_document_at_every_message_boundary() {
    let wire = wire_messages();
    let t0 = Instant::now();
    let at = |i: usize| t0 + Duration::from_millis(40 * i as u64);

    let mut source = Mirror::new();
    assert!(source.feed(&hello(), t0));
    let mut ticking_checked = 0;
    let mut with_elapsed = 0;
    for (i, m) in wire.iter().enumerate() {
        assert!(source.feed(m, at(i)), "message {i} changed the document");
        let now = at(i) + Duration::from_millis(7_500);
        let replay = source.document().replay(now);

        let mut whole = Mirror::new();
        assert!(whole.feed(&replay, now));
        // The same bytes in odd pieces (replay chunks cut anywhere).
        let mut pieces = Mirror::new();
        for p in replay.chunks(997) {
            pieces.feed(p, now);
        }
        for (what, m) in [("whole", &whole), ("pieces", &pieces)] {
            assert!(
                m.is_idle(),
                "boundary {i} {what}: replay ends between messages"
            );
            assert_eq!(m.live(), source.live(), "boundary {i} {what}");
            assert_eq!(
                full(m.document()),
                full(source.document()),
                "boundary {i} {what}: documents differ"
            );
        }

        // Timers advanced by the time since the node last changed, and only those.
        let expected = expected_ages(source.document(), now);
        for (id, age) in &expected {
            assert_eq!(
                age_of(whole.document(), id),
                Some(*age),
                "boundary {i}: age of {id}"
            );
            ticking_checked += 1;
        }
        with_elapsed += usize::from(!expected.is_empty());

        // The mirror stamps its nodes with the replay's time, so it replays to the very same
        // bytes as the source (timers included).
        assert!(
            whole.document().replay(now) == replay,
            "boundary {i}: a replay of the replay differs"
        );
    }
    assert!(source.live());
    assert!(
        ticking_checked > 0 && with_elapsed > 10,
        "the recording must carry running timers ({ticking_checked} checked in {with_elapsed} boundaries)"
    );
}

#[test]
fn a_mirror_that_joins_at_any_boundary_and_follows_the_stream_ends_equal() {
    let wire = wire_messages();
    let t0 = Instant::now();
    let at = |i: usize| t0 + Duration::from_millis(40 * i as u64);

    let mut full_run = Mirror::new();
    full_run.feed(&hello(), t0);
    for (i, m) in wire.iter().enumerate() {
        full_run.feed(m, at(i));
    }

    for join in 0..wire.len() {
        let mut head = Mirror::new();
        head.feed(&hello(), t0);
        for (i, m) in wire.iter().enumerate().take(join + 1) {
            head.feed(m, at(i));
        }
        let now = at(join);
        let mut late = Mirror::new();
        late.feed(&head.document().replay(now), now);
        for (i, m) in wire.iter().enumerate().skip(join + 1) {
            late.feed(m, at(i));
        }
        assert!(late.is_idle());
        assert_eq!(
            full(late.document()),
            full(full_run.document()),
            "joined after message {join}"
        );
    }
}

#[test]
fn joining_inside_a_chunk_sequence_loses_the_frame_and_idle_says_so() {
    // The reason `Mirror::is_idle` exists: a joiner that starts between two chunks of one
    // message cannot rebuild it.
    let wire = wire_messages();
    let (idx, big) = wire
        .iter()
        .enumerate()
        .find(|(_, m)| m.windows(5).any(|w| w == b";m=1;"))
        .expect("a chunked message");
    let t0 = Instant::now();
    let mut source = Mirror::new();
    source.feed(&hello(), t0);
    for m in &wire[..idx] {
        source.feed(m, t0);
    }
    assert!(source.is_idle());
    // Feed all of the chunked message but its last APC.
    let last_apc = big
        .windows(2)
        .enumerate()
        .filter(|(_, w)| *w == b"\x1b_")
        .map(|(i, _)| i)
        .next_back()
        .expect("an APC");
    assert!(last_apc > 0, "the message is split into several chunks");
    source.feed(&big[..last_apc], t0);
    assert!(!source.is_idle(), "between chunks the mirror is not idle");
    source.feed(&big[last_apc..], t0);
    assert!(source.is_idle());
    // A joiner that took the replay at that point misses the frame the rest completes.
    let mut joiner = Mirror::new();
    let mut head = Mirror::new();
    head.feed(&hello(), t0);
    for m in &wire[..idx] {
        head.feed(m, t0);
    }
    head.feed(&big[..last_apc], t0);
    assert!(!head.is_idle());
    joiner.feed(&head.document().replay(t0), t0);
    joiner.feed(&big[last_apc..], t0);
    assert_ne!(
        full(joiner.document()),
        full(source.document()),
        "the orphaned chunk must not rebuild the frame"
    );
    assert_eq!(
        full(joiner.document()),
        full(head.document()),
        "and changes nothing else"
    );
}
