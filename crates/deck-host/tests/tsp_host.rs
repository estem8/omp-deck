//! The host as a TSP terminal (Linux): a scripted child speaks TSP through the real daemon and
//! PTY, and records every byte the daemon writes to its input. Every test uses its own
//! namespace `omp-deck-test-<pid>-<n>`; daemons are killed afterwards.
#![cfg(target_os = "linux")]

mod common;

use common::{Client, TestNs, env, scratch, wait_until};
use deck_host::{Error, Palette, SessionSpec};
use serde_json::{Value, json};

const HELLO_QUERY: &str =
    r#"\033_tsp;q;{"q":"hello","v":[1],"app":"fake","ver":"0","features":["send"]}\033\\"#;
const OPEN: &str = r#"\033_tsp;o;{"id":"s1","mode":"inline","role":"omp.session"}\033\\"#;
const FRAME: &str = r#"\033_tsp;f;{"sf":"s1","s":1,"ops":[["add","main","s1",null,{"id":"main","k":"col","c":[{"id":"t","k":"md","p":{"text":"hello from the child"}}]}]]}\033\\"#;
const FRAME2: &str =
    r#"\033_tsp;f;{"sf":"s1","s":2,"ops":[["text","t","append"," and more"]]}\033\\"#;

/// A shell child: raw tty, hello query + DA1 in one write, then `lines` of TSP output, then it
/// records everything the host writes to its input into `rec`.
fn child(name: &str, tsp: bool, rec: &std::path::Path, lines: &[&str]) -> SessionSpec {
    let mut script = String::from("stty raw -echo\n");
    script.push_str(&format!("printf '{HELLO_QUERY}\\033[c'\n"));
    for line in lines {
        script.push_str(&format!("printf '{line}'\n"));
    }
    script.push_str(&format!("exec cat > '{}'\n", rec.display()));
    SessionSpec {
        name: name.into(),
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script],
        cwd: env().root.clone(),
        env_set: vec![("TERM".into(), "xterm-256color".into())],
        env_remove: Vec::new(),
        tsp,
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn read(rec: &std::path::Path) -> Vec<u8> {
    std::fs::read(rec).unwrap_or_default()
}

/// The `tsp;e` events in `bytes` as JSON values, in order.
fn events(bytes: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(bytes).into_owned();
    text.split("\x1b_tsp;e;")
        .skip(1)
        .map(|part| {
            let json = part.split("\x1b\\").next().unwrap();
            serde_json::from_str(json).unwrap()
        })
        .collect()
}

#[test]
fn the_hello_reply_precedes_the_da1_reply_and_frames_are_acked_while_detached() {
    let ns = TestNs::new();
    let rec = scratch("tsp-order.rec");
    let _ = std::fs::remove_file(&rec);
    // Never attached: the host alone answers.
    deck_host::start(
        &ns.0,
        &child("ord", true, &rec, &[OPEN, FRAME, FRAME2]),
        100,
        30,
    )
    .unwrap();
    wait_until("the ack of frame 2", || {
        events(&read(&rec)).contains(&json!({"ev":"ack","sf":"s1","s":2}))
    });
    let bytes = read(&rec);
    let hello = find(&bytes, b"\x1b_tsp;r;{\"r\":\"hello\"").expect("hello reply");
    let da1 = find(&bytes, b"\x1b[?6c").expect("DA1 reply");
    assert_eq!(
        hello, 0,
        "nothing is written before the hello reply: {bytes:?}"
    );
    assert!(hello < da1, "hello reply first, DA1 after: {bytes:?}");
    // The reply is a complete, valid APC with the terminal's own columns.
    let end = find(&bytes[hello..], b"\x1b\\").unwrap() + hello;
    let reply: Value = serde_json::from_slice(&bytes[hello + b"\x1b_tsp;r;".len()..end]).unwrap();
    assert_eq!(
        (reply["v"].clone(), reply["cols"].clone()),
        (json!(1), json!(100))
    );
    assert_eq!(reply["term"], "omp-deck");
    assert!(end < da1);
    // Acks cover the frames, in order, and every event is one whole APC.
    let acks: Vec<_> = events(&bytes)
        .into_iter()
        .filter(|e| e["ev"] == "ack")
        .map(|e| e["s"].as_u64().unwrap())
        .collect();
    assert_eq!(acks.last(), Some(&2));
    assert!(acks.windows(2).all(|w| w[0] < w[1]), "{acks:?}");
}

#[test]
fn a_plain_session_gets_only_the_terminal_replies() {
    let ns = TestNs::new();
    let rec = scratch("tsp-off.rec");
    let _ = std::fs::remove_file(&rec);
    deck_host::start(&ns.0, &child("off", false, &rec, &[OPEN, FRAME]), 100, 30).unwrap();
    wait_until("the DA1 reply", || find(&read(&rec), b"\x1b[?6c").is_some());
    std::thread::sleep(std::time::Duration::from_millis(300));
    let bytes = read(&rec);
    assert_eq!(bytes, b"\x1b[?6c", "no TSP bytes without the spec flag");
}

#[test]
fn tsp_peek_returns_the_document_and_rejects_plain_sessions() {
    let ns = TestNs::new();
    let rec = scratch("tsp-peek.rec");
    let _ = std::fs::remove_file(&rec);
    deck_host::start(
        &ns.0,
        &child("peek", true, &rec, &[OPEN, FRAME, FRAME2]),
        100,
        30,
    )
    .unwrap();
    let mut doc = Value::Null;
    wait_until("the document", || {
        doc = deck_host::tsp_peek(&ns.0, "peek").unwrap();
        doc["surfaces"][0]["seq"] == 2
    });
    let s = &doc["surfaces"][0];
    assert_eq!(
        (s["id"].clone(), s["mode"].clone(), s["role"].clone()),
        (json!("s1"), json!("inline"), json!("omp.session"))
    );
    assert_eq!(
        s["root"],
        json!({"id":"s1","k":"col","c":[{"id":"main","k":"col","c":[
            {"id":"t","k":"md","p":{"text":"hello from the child and more"}}]}]})
    );
    assert_eq!(doc["blobs"], json!([]));

    let plain_rec = scratch("tsp-peek-plain.rec");
    let _ = std::fs::remove_file(&plain_rec);
    deck_host::start(&ns.0, &child("plain", false, &plain_rec, &[OPEN]), 100, 30).unwrap();
    match deck_host::tsp_peek(&ns.0, "plain") {
        Err(Error::Remote(msg)) => assert_eq!(msg, "not a TSP session"),
        other => panic!("{other:?}"),
    }
    // The host did not close the connection for it: a TSP peek still works afterwards.
    assert!(deck_host::tsp_peek(&ns.0, "peek").is_ok());
}

#[test]
fn resize_and_theme_changes_reach_the_program_only_once_live_and_when_changed() {
    let ns = TestNs::new();
    let rec = scratch("tsp-view.rec");
    let _ = std::fs::remove_file(&rec);
    let c = Client::create(&ns, &child("view", true, &rec, &[OPEN, FRAME]), 100, 30);
    wait_until("the hello reply", || {
        find(&read(&rec), b"\x1b_tsp;r;{\"r\":\"hello\"").is_some()
    });
    // Same size and same theme: nothing.
    c.tx.resize(100, 30, 8, 16).unwrap();
    c.tx.palette(&Palette::default()).unwrap();
    c.tx.resize(72, 30, 8, 16).unwrap();
    let mut light = Palette::default();
    light.0[Palette::BACKGROUND] = [0xff, 0xff, 0xff];
    c.tx.palette(&light).unwrap();
    c.tx.palette(&light).unwrap();
    wait_until("both view events", || {
        let ev = events(&read(&rec));
        ev.contains(&json!({"ev":"resize","cols":72}))
            && ev.contains(&json!({"ev":"theme","dark":false}))
    });
    let ev: Vec<_> = events(&read(&rec))
        .into_iter()
        .filter(|e| e["ev"] != "ack")
        .collect();
    assert_eq!(
        ev,
        [
            json!({"ev":"resize","cols":72}),
            json!({"ev":"theme","dark":false})
        ]
    );
}

// ---- snapshots carry the document ---------------------------------------------------------------

use deck_host::HostEvent;
use deck_tsp::mirror::Mirror;
use std::time::{Duration, Instant};

/// A child with a hand-written script (raw tty, then `body`, then it records its input).
fn scripted(name: &str, rec: &std::path::Path, body: &str) -> SessionSpec {
    let mut spec = child(name, true, rec, &[]);
    spec.args = vec![
        "-c".into(),
        format!(
            "stty raw -echo\nprintf '{HELLO_QUERY}\\033[c'\n{body}\nexec cat > '{}'\n",
            rec.display()
        ),
    ];
    spec
}

fn mirror_of(snapshot: &[u8]) -> Mirror {
    let mut m = Mirror::new();
    m.feed(snapshot, Instant::now());
    m
}

fn doc_json(m: &Mirror) -> Value {
    serde_json::to_value(m.document()).unwrap()
}

#[test]
fn attach_observe_and_peek_snapshots_rebuild_the_document_a_late_client_missed() {
    let ns = TestNs::new();
    let rec = scratch("tsp-snap.rec");
    let _ = std::fs::remove_file(&rec);
    // The program speaks before any client exists.
    deck_host::start(
        &ns.0,
        &child("snap", true, &rec, &[OPEN, FRAME, FRAME2]),
        100,
        30,
    )
    .unwrap();
    let mut host_doc = Value::Null;
    wait_until("the document", || {
        host_doc = deck_host::tsp_peek(&ns.0, "snap").unwrap();
        host_doc["surfaces"][0]["seq"] == 2
    });

    // attach
    let attached = deck_host::attach(&ns.0, "snap", common::params(100, 30)).unwrap();
    let mut c = Client::new(attached, 100, 30);
    let HostEvent::Snapshot(bytes) = c.next() else {
        panic!("the first event is the snapshot")
    };
    let m = mirror_of(&bytes);
    assert!(m.is_idle() && m.live());
    assert_eq!(doc_json(&m), host_doc, "attach snapshot");
    // The hello travels first so a mirror knows what the program supports; APC strings sit
    // after the VT replay.
    let hello = find(&bytes, b"\x1b_tsp;q;{\"q\":\"hello\"").expect("hello query");
    let open = find(&bytes, b"\x1b_tsp;o;").expect("open");
    assert!(hello < open);
    assert_eq!(
        m.document().program().map(|p| p.features.clone()),
        Some(vec!["send".to_owned()])
    );

    // peek and observe (the controller stays attached)
    let (_, peeked) = deck_host::peek(&ns.0, "snap", 100).unwrap();
    assert_eq!(doc_json(&mirror_of(&peeked)), host_doc, "peek snapshot");
    let (_, observed) = deck_host::observe(&ns.0, "snap", 100).unwrap();
    let (mut rx, _tx) = observed.split();
    let deck_host::ObserverEvent::Snapshot { bytes, .. } = rx.recv().unwrap() else {
        panic!("the first observer event is the snapshot")
    };
    assert_eq!(doc_json(&mirror_of(&bytes)), host_doc, "observe snapshot");

    // The screen replay is untouched by the APC strings: a plain terminal fed the same
    // snapshot shows the same grid as one fed only its VT part.
    let vt_only = &bytes[..find(&bytes, b"\x1b_tsp;q;").unwrap()];
    let mut with = common::fresh_term(100, 30);
    let mut without = common::fresh_term(100, 30);
    let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
    parser.advance(&mut with, &bytes);
    let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
    parser.advance(&mut without, vt_only);
    assert_eq!(common::screen_text(&with), common::screen_text(&without));
}

/// Two retained blobs: `pixel-a`, which an image node refers to, and `pixel-b`, which no node does.
const BLOB_A: &str = "6fe2d5846e515bc9f48577f0d0a5c72e8935655af5e138b26face383b3d2174b";
const BLOB_B: &str = "442c3c38a30e1025e4c4fde760ac50c6cbb55147cdbb9497c3e706ee6dbaf13e";

#[test]
fn attach_and_observe_carry_only_referenced_blobs_and_peek_carries_none() {
    let ns = TestNs::new();
    let rec = scratch("tsp-blobs.rec");
    let _ = std::fs::remove_file(&rec);
    let body = format!(
        "printf '{OPEN}'\n\
         printf '\\033_tsp;b;mime=image/png;cGl4ZWwtYQ==\\033\\\\'\n\
         printf '\\033_tsp;b;cGl4ZWwtYg==\\033\\\\'\n\
         printf '\\033_tsp;f;{{\"sf\":\"s1\",\"s\":1,\"ops\":[[\"add\",\"main\",\"s1\",null,{{\"id\":\"main\",\"k\":\"col\",\"c\":[{{\"id\":\"img\",\"k\":\"image\",\"p\":{{\"blob\":\"{BLOB_A}\"}}}}]}}]]}}\\033\\\\'"
    );
    deck_host::start(&ns.0, &scripted("blobs", &rec, &body), 100, 30).unwrap();
    let mut host_doc = Value::Null;
    wait_until("the frame and both blobs", || {
        host_doc = deck_host::tsp_peek(&ns.0, "blobs").unwrap();
        host_doc["surfaces"][0]["seq"] == 1
            && host_doc["blobs"].as_array().is_some_and(|b| b.len() == 2)
    });
    let has = |m: &Mirror, id: &str| m.document().blob(id).is_some();
    let has_node = |m: &Mirror| {
        m.document()
            .surface("s1")
            .and_then(|s| s.node("img"))
            .is_some()
    };

    let attached = deck_host::attach(&ns.0, "blobs", common::params(100, 30)).unwrap();
    let mut c = Client::new(attached, 100, 30);
    let HostEvent::Snapshot(bytes) = c.next() else {
        panic!("the first event is the snapshot")
    };
    let m = mirror_of(&bytes);
    assert!(
        has_node(&m) && has(&m, BLOB_A),
        "the referenced blob comes with its node"
    );
    assert!(!has(&m, BLOB_B), "an unreferenced blob is not replayed");

    let (_, observed) = deck_host::observe(&ns.0, "blobs", 100).unwrap();
    let (mut rx, _tx) = observed.split();
    let deck_host::ObserverEvent::Snapshot { bytes, .. } = rx.recv().unwrap() else {
        panic!("the first observer event is the snapshot")
    };
    let m = mirror_of(&bytes);
    assert!(has(&m, BLOB_A) && !has(&m, BLOB_B), "observe");

    let (_, peeked) = deck_host::peek(&ns.0, "blobs", 100).unwrap();
    let m = mirror_of(&peeked);
    assert!(has_node(&m), "a peek still has the document");
    assert!(
        !has(&m, BLOB_A) && !has(&m, BLOB_B),
        "a peek carries no blobs"
    );
    assert!(
        find(&peeked, b"\x1b_tsp;b;").is_none(),
        "no blob message at all"
    );
    // The host itself keeps both.
    assert_eq!(host_doc["blobs"].as_array().map(Vec::len), Some(2));
}

#[test]
fn a_plain_or_not_yet_live_session_snapshot_has_no_tsp_bytes() {
    let ns = TestNs::new();
    let rec = scratch("tsp-snap-plain.rec");
    let _ = std::fs::remove_file(&rec);
    deck_host::start(
        &ns.0,
        &child("plainsnap", false, &rec, &[OPEN, FRAME]),
        100,
        30,
    )
    .unwrap();
    wait_until("the DA1 reply", || find(&read(&rec), b"\x1b[?6c").is_some());
    let (_, bytes) = deck_host::peek(&ns.0, "plainsnap", 100).unwrap();
    assert!(find(&bytes, b"\x1b_").is_none(), "{bytes:?}");

    // TSP session whose program never said hello: not live, nothing to replay.
    let quiet = scratch("tsp-snap-quiet.rec");
    let _ = std::fs::remove_file(&quiet);
    let mut spec = child("quiet", true, &quiet, &[]);
    spec.args = vec![
        "-c".into(),
        format!(
            "stty raw -echo\nprintf '{OPEN}'\nexec cat > '{}'\n",
            quiet.display()
        ),
    ];
    deck_host::start(&ns.0, &spec, 100, 30).unwrap();
    let mut doc = Value::Null;
    wait_until("the open", || {
        doc = deck_host::tsp_peek(&ns.0, "quiet").unwrap();
        doc["surfaces"][0]["id"] == "s1"
    });
    let (_, bytes) = deck_host::peek(&ns.0, "quiet", 100).unwrap();
    assert!(find(&bytes, b"\x1b_").is_none(), "{bytes:?}");
}

#[test]
fn a_client_attaching_between_two_chunks_of_a_frame_still_gets_the_whole_frame() {
    let ns = TestNs::new();
    let rec = scratch("tsp-mid.rec");
    let _ = std::fs::remove_file(&rec);
    let mid = scratch("tsp-mid.flag");
    let go = scratch("tsp-mid.go");
    let _ = std::fs::remove_file(&mid);
    let _ = std::fs::remove_file(&go);
    // One frame in two chunks (`c=k1`), the child pausing between them until `go` exists.
    let first = r#"\033_tsp;f;c=k1;m=1;{"sf":"s1","s":3,"ops":[["text","t","append",", pend"#;
    let second = r#"\033_tsp;f;c=k1;ing tail"]]}\033\\"#;
    let first = format!("{first}\\033\\\\");
    let body = format!(
        "printf '{OPEN}'\nprintf '{FRAME}'\nprintf '{first}'\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.02; done\nprintf '{second}'",
        mid.display(),
        go.display()
    );
    deck_host::start(&ns.0, &scripted("mid", &rec, &body), 100, 30).unwrap();
    wait_until("the first chunk was sent", || mid.exists());
    // The daemon reads the chunk right away; a joiner now misses its start.
    std::thread::sleep(Duration::from_millis(150));
    let attached = deck_host::attach(&ns.0, "mid", common::params(100, 30)).unwrap();
    let mut c = Client::new(attached, 100, 30);
    let HostEvent::Snapshot(bytes) = c.next() else {
        panic!("the first event is the snapshot")
    };
    let mut m = mirror_of(&bytes);
    assert!(!m.is_idle(), "the half-received frame is resumed, not lost");
    assert_eq!(
        m.document()
            .surface("s1")
            .unwrap()
            .node("t")
            .unwrap()
            .text(),
        Some("hello from the child"),
        "nothing of the half frame applied yet"
    );
    std::fs::write(&go, b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while m.document().surface("s1").and_then(|s| s.last_seq()) != Some(3) {
        assert!(
            Instant::now() < deadline,
            "the rest of the frame never arrived"
        );
        if let HostEvent::Output(out) = c.next() {
            m.feed(&out, Instant::now());
        }
    }
    assert!(m.is_idle());
    let text = m
        .document()
        .surface("s1")
        .unwrap()
        .node("t")
        .unwrap()
        .text();
    assert_eq!(text, Some("hello from the child, pending tail"));
    let host_doc = deck_host::tsp_peek(&ns.0, "mid").unwrap();
    assert_eq!(doc_json(&m), host_doc);
}

#[test]
fn an_attached_client_fetches_blobs_the_program_sent_before_it_attached() {
    use sha2::{Digest, Sha256};

    let ns = TestNs::new();
    let rec = scratch("tsp-fetch.rec");
    let frame_file = scratch("tsp-fetch.frame");
    let go = scratch("tsp-fetch.go");
    for f in [&rec, &frame_file, &go] {
        let _ = std::fs::remove_file(f);
    }
    // `pixel-a` (small) and a 319 992-byte blob sent as eight chunks (two BlobData frames
    // on the way back); the frame that refers to them is sent only after the client attached.
    let big_chunk =
        r#"printf '\033_tsp;b;c=big;m=1;'; head -c 39999 /dev/zero | base64 -w0; printf '\033\\'"#;
    let big_last =
        r#"printf '\033_tsp;b;c=big;'; head -c 39999 /dev/zero | base64 -w0; printf '\033\\'"#;
    let body = format!(
        "printf '{OPEN}'\n\
         printf '\\033_tsp;b;mime=image/png;cGl4ZWwtYQ==\\033\\\\'\n\
         {}\n{big_last}\n\
         while [ ! -e '{}' ]; do sleep 0.02; done\n\
         cat '{}'",
        [big_chunk; 7].join("\n"),
        go.display(),
        frame_file.display()
    );
    deck_host::start(&ns.0, &scripted("fetch", &rec, &body), 100, 30).unwrap();
    let big_id = deck_tsp::document::hex(&Sha256::digest(vec![0u8; 39999 * 8]));
    wait_until("both blobs held by the host", || {
        let doc = deck_host::tsp_peek(&ns.0, "fetch").unwrap();
        doc["blobs"].as_array().is_some_and(|b| b.len() == 2)
            && doc["blobs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["id"] == big_id.as_str())
    });

    let attached = deck_host::attach(&ns.0, "fetch", common::params(100, 30)).unwrap();
    let mut c = Client::new(attached, 100, 30);
    assert!(c.tx.blob_fetch(), "the daemon announces blob fetching");
    let HostEvent::Snapshot(bytes) = c.next() else {
        panic!("the first event is the snapshot")
    };
    let mut m = mirror_of(&bytes);
    assert!(
        m.document().blob(BLOB_A).is_none() && m.document().blob(&big_id).is_none(),
        "nothing refers to the blobs yet, so the snapshot carries none"
    );

    let frame = format!(
        r#"{{"sf":"s1","s":1,"ops":[["add","main","s1",null,{{"id":"main","k":"col","c":[{{"id":"a","k":"image","p":{{"blob":"{BLOB_A}"}}}},{{"id":"b","k":"image","p":{{"blob":"{big_id}"}}}}]}}]]}}"#
    );
    std::fs::write(&frame_file, format!("\x1b_tsp;f;{frame}\x1b\\")).unwrap();
    std::fs::write(&go, b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while m
        .document()
        .surface("s1")
        .and_then(|s| s.node("b"))
        .is_none()
    {
        assert!(Instant::now() < deadline, "the frame never arrived");
        if let HostEvent::Output(out) = c.next() {
            m.feed(&out, Instant::now());
        }
    }
    let mut missing = m.document().missing_blobs();
    missing.sort();
    let mut expected = vec![BLOB_A.to_owned(), big_id.clone()];
    expected.sort();
    assert_eq!(missing, expected, "the mirror knows what it lacks");

    let absent = "0".repeat(64);
    for id in [BLOB_A, big_id.as_str(), absent.as_str()] {
        c.tx.blob(id).unwrap();
    }
    let mut answers = Vec::new();
    while answers.len() < 3 {
        if let HostEvent::Blob { id, mime, bytes } = c.next() {
            answers.push((id, mime, bytes));
        }
    }
    answers.sort();
    let by = |id: &str| answers.iter().find(|a| a.0 == id).unwrap();
    assert_eq!(
        (by(BLOB_A).1.as_deref(), by(BLOB_A).2.as_deref()),
        (Some("image/png"), Some(&b"pixel-a"[..]))
    );
    assert_eq!(by(&big_id).2.as_deref(), Some(&vec![0u8; 39999 * 8][..]));
    assert_eq!(
        by(&absent).2,
        None,
        "no such blob is an answer, not silence"
    );

    for (id, mime, bytes) in answers {
        if let Some(bytes) = bytes {
            assert!(m.put_blob(&id, mime, bytes));
        }
    }
    assert!(m.document().missing_blobs().is_empty());
    assert!(
        !m.put_blob(BLOB_A, None, b"forged".to_vec()),
        "hash checked"
    );
}
