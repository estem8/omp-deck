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
