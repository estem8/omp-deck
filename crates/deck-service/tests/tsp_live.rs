//! The session host as a TSP terminal for a real omp (18.15+) driven by a scripted model
//! ([`support::fake_model`]):
//! - omp's `hello` query is answered by the detached host (no GUI attached), so omp renders
//!   native surfaces: the document peeked from the host has the inline `omp.session` surface
//!   with a sendable `editor`;
//! - a `send` event written into the PTY (what the native chat view will send) starts a turn
//!   whose streamed assistant reply grows across repeated peeks, while nothing is attached;
//! - a second turn driven by a short attach-send-detach completes the same way, and omp never
//!   logs that the terminal stopped acknowledging frames (the host acks while detached).
//!
//! Private HOME/XDG tree and own `omp-deck-test-*` namespace; skipped when omp is not installed.

mod support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_host::{AttachParams, HostEvent, Namespace, Palette, SessionSpec};
use deck_tsp::terminal::{InputEncoding, encode_event};
use deck_tsp::wire::Event;
use serde_json::{Value, json};
use support::device::{OMP_CONFIG, Sandbox};
use support::fake_model::{self as fm, FakeModel, Reply};

/// How the first reply starts (a prompt never contains it, so it identifies the assistant node).
const STREAM_HEAD: &str = "Streamed answer";
const STREAM_REPLY: &str = "Streamed answer: one two three four five six seven eight nine ten \
     eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty.";
const STREAM_TIME: Duration = Duration::from_secs(6);
const SECOND_REPLY: &str = "Second answer is complete.";
const TOOL_REPLY: &str = "Tool turn is complete.";
/// How long omp may take to show a surface or finish a turn.
const TURN: Duration = Duration::from_secs(60);
const SESSION: &str = "tsp1";

fn script(notes: PathBuf) -> impl FnMut(&Value) -> Reply + Send + 'static {
    move |req| {
        if !fm::has_tools(req) {
            // A side request (e.g. the session title).
            return Reply::text("Fake title");
        }
        let user = fm::last_user_text(req);
        if user.contains("STREAM-MARK") {
            return Reply::slow_text(STREAM_REPLY, STREAM_TIME);
        }
        if user.contains("SECOND-MARK") {
            return Reply::text(SECOND_REPLY);
        }
        if user.contains("TOOL-MARK") {
            let fresh = !fm::after_last_user(req)
                .iter()
                .any(|m| fm::role(m) == "tool");
            return if fresh {
                Reply::tool_call("read", &json!({ "path": notes.to_string_lossy() }))
            } else {
                Reply::text(TOOL_REPLY)
            };
        }
        Reply::text("unscripted")
    }
}

/// Kills every session of the namespace, also when the test panics.
struct Cleanup(Namespace);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for name in self.0.list() {
            let _ = self.0.kill(&name);
        }
    }
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

/// `omp-deck-host` next to the test binaries, built when `cargo test -p deck-service` did not.
fn ensure_host_exe() {
    let exe = if cfg!(windows) {
        "omp-deck-host.exe"
    } else {
        "omp-deck-host"
    };
    let beside = PathBuf::from(support::device::GATEWAY).with_file_name(exe);
    if !beside.is_file() {
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "-p", "deck-host", "--bin", "omp-deck-host"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("run cargo build");
        assert!(status.success(), "cargo build -p deck-host");
    }
    assert!(beside.is_file(), "{} exists", beside.display());
    // SAFETY: single-threaded here (no daemon, model server or client started yet).
    unsafe { std::env::set_var(deck_host::HOST_EXE_ENV, &beside) };
}

/// Polls `f` until it returns `Some`.
fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn peek(ns: &Namespace) -> Value {
    deck_host::tsp_peek(ns, SESSION).expect("tsp_peek")
}

/// Every node of the open inline surface, depth first, as `(id, kind, props)`.
fn nodes(doc: &Value) -> Vec<(String, String, Value)> {
    fn walk(n: &Value, out: &mut Vec<(String, String, Value)>) {
        out.push((
            n["id"].as_str().unwrap_or_default().to_owned(),
            n["k"].as_str().unwrap_or_default().to_owned(),
            n["p"].clone(),
        ));
        for c in n["c"].as_array().into_iter().flatten() {
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    for s in doc["surfaces"].as_array().into_iter().flatten() {
        if s["mode"] == "inline" && s["open"] == true {
            walk(&s["root"], &mut out);
        }
    }
    out
}

/// `(surface id, editor node id)` once omp shows a sendable composer.
fn composer(doc: &Value) -> Option<(String, String)> {
    let sf = doc["surfaces"]
        .as_array()?
        .iter()
        .find(|s| s["mode"] == "inline" && s["open"] == true)?["id"]
        .as_str()?
        .to_owned();
    nodes(doc)
        .into_iter()
        .find(|(_, k, p)| k == "editor" && p["sendable"] == true)
        .map(|(id, _, _)| (sf, id))
}

/// Text of the first node whose text starts with `head`.
fn text_starting(doc: &Value, head: &str) -> Option<String> {
    nodes(doc)
        .into_iter()
        .filter_map(|(_, _, p)| p["text"].as_str().map(str::to_owned))
        .find(|t| t.starts_with(head))
}

/// Attaches, writes the `send` event for `text` and detaches again (the first send of a
/// native view does the same through its own attach).
fn send(ns: &Namespace, sf: &str, id: &str, text: &str) {
    let params = AttachParams {
        cols: 100,
        rows: 30,
        cell_width: 8,
        cell_height: 16,
        history_lines: 0,
        palette: Palette::default(),
    };
    let attached = deck_host::attach(ns, SESSION, params).expect("attach");
    let (mut rx, tx) = attached.split();
    let reader = std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            if matches!(event, HostEvent::Closed | HostEvent::Exited(_)) {
                break;
            }
        }
    });
    let bytes = encode_event(
        &Event::Send {
            sf: sf.to_owned(),
            id: id.to_owned(),
            text: text.to_owned(),
        },
        InputEncoding::Apc,
    );
    tx.input(&bytes).expect("send the prompt");
    tx.detach().expect("detach");
    reader.join().expect("reader thread");
}

fn log_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "sessions") {
                continue;
            }
            log_files(&path, out);
        } else if std::fs::metadata(&path).is_ok_and(|m| m.len() < (32 << 20)) {
            out.push(path);
        }
    }
}

#[test]
fn omp_renders_native_surfaces_through_the_host_terminal() {
    let Some(omp) = which(if cfg!(windows) { "omp.exe" } else { "omp" }) else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("tsp", &config);
    ensure_host_exe();
    let notes = sb.proj().join("notes.txt");
    std::fs::write(&notes, "tool target\n").expect("notes");
    let model = FakeModel::start(script(notes));
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    let ns = Namespace::new(&sb.ns).expect("namespace");
    let _cleanup = Cleanup(Namespace::new(&sb.ns).expect("namespace"));
    let record = sb.root.join("tsp-record.jsonl");

    // The environment a real launch gives omp (identity of the terminal, foreign multiplexer
    // and terminal variables removed: omp does not probe TSP inside tmux).
    let (mut env_set, env_remove) = deck_core::mux::host_env(
        SESSION,
        &[("PI_TUI_TSP_RECORD".to_owned(), record.display().to_string())],
        false,
    );
    env_set.retain(|(k, _)| k != "PI_NOTIFICATIONS");
    env_set.push(("PI_NOTIFICATIONS".to_owned(), "off".to_owned()));
    let spec = SessionSpec {
        name: SESSION.to_owned(),
        program: omp,
        args: Vec::new(),
        cwd: sb.proj(),
        env_set,
        env_remove,
        tsp: true,
    };
    deck_host::start(&ns, &spec, 100, 30).expect("start omp");

    // (a) Nobody attached: omp still got its hello reply and drew native surfaces.
    let (sf, editor) = wait_for("a sendable composer", TURN, || composer(&peek(&ns)));
    let doc = peek(&ns);
    let surface = &doc["surfaces"][0];
    assert_eq!(surface["mode"], "inline");
    assert_eq!(surface["role"], "omp.session");
    assert_eq!(surface["id"], sf);

    // (b) A prompt written as a TSP `send` event streams a reply that grows between peeks.
    send(&ns, &sf, &editor, "STREAM-MARK say something long");
    let mut lengths = BTreeSet::new();
    let full = wait_for("the streamed reply", TURN, || {
        let text = text_starting(&peek(&ns), STREAM_HEAD)?;
        assert!(
            STREAM_REPLY.starts_with(&text),
            "the node only ever holds a prefix of the reply: {text:?}"
        );
        lengths.insert(text.len());
        (text == STREAM_REPLY).then_some(text)
    });
    assert_eq!(full, STREAM_REPLY);
    assert!(
        lengths.len() >= 3,
        "the reply was seen growing, not all at once: {lengths:?}"
    );

    // (c) A second turn through a fresh short attach; the composer is usable again.
    let (sf, editor) = wait_for("the composer after the turn", TURN, || composer(&peek(&ns)));
    send(&ns, &sf, &editor, "SECOND-MARK short one");
    wait_for("the second reply", TURN, || {
        text_starting(&peek(&ns), SECOND_REPLY)
    });
    // The first turn's text is still in the document.
    assert_eq!(
        text_starting(&peek(&ns), STREAM_HEAD).as_deref(),
        Some(STREAM_REPLY)
    );

    // (d) A tool call: omp shows the call as a node of its own, then the closing text.
    let (sf, editor) = wait_for("the composer after turn 2", TURN, || composer(&peek(&ns)));
    send(&ns, &sf, &editor, "TOOL-MARK read the notes");
    wait_for("the tool turn's reply", TURN, || {
        text_starting(&peek(&ns), TOOL_REPLY)
    });
    assert!(
        nodes(&peek(&ns))
            .iter()
            .any(|(_, k, p)| k == "tool" && p.to_string().contains("notes.txt")),
        "a tool node names the file it read"
    );
    let prompts = model.since(0);
    assert!(
        prompts
            .iter()
            .any(|r| fm::last_user_text(r).contains("STREAM-MARK say something long")),
        "the model received the sent prompt text"
    );
    assert!(
        prompts
            .iter()
            .any(|r| fm::last_user_text(r).contains("SECOND-MARK short one"))
    );

    // omp's logs never complained about missing acks.
    let mut files = Vec::new();
    log_files(&sb.root.join("home/.omp"), &mut files);
    log_files(&sb.root.join("state"), &mut files);
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        assert!(
            !text.contains("stopped acknowledging"),
            "omp reported unacknowledged frames: {}",
            file.display()
        );
    }
    // `omp-deck-host tsp` prints the same document as an outline (composer and replies).
    let out =
        std::process::Command::new(std::env::var_os(deck_host::HOST_EXE_ENV).expect("host exe"))
            .args(["tsp", SESSION, "--ns", &sb.ns])
            .output()
            .expect("run omp-deck-host tsp");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let outline = String::from_utf8_lossy(&out.stdout);
    eprintln!("{outline}");
    assert!(outline.starts_with("surface "), "{outline}");
    assert!(outline.contains(" role=omp.session open "), "{outline}");
    assert!(
        outline.contains("editor #") && outline.contains("sendable=true"),
        "{outline}"
    );
    assert!(
        outline.contains(SECOND_REPLY) && outline.contains(TOOL_REPLY),
        "{outline}"
    );
    // The recording omp wrote is what the replay test feeds (kept on request).
    if let Some(keep) = std::env::var_os("TSP_LIVE_KEEP_RECORD") {
        std::fs::copy(&record, keep).expect("keep the recording");
    }
    let _ = ns.kill(SESSION);
}
