//! A late-joining terminal mirrors a real omp's TSP document (omp 18.15+, scripted model):
//! - a gateway-style owner terminal ([`Terminal::connect_headless`] over [`HostRemote`]) that
//!   attaches after omp already drew its surfaces gets the host's replay and holds a document
//!   equal to the host's;
//! - [`Terminal::send_tsp`] from it submits a prompt through omp's composer, and the streamed
//!   reply grows in the terminal's own mirror, which keeps equal to the host's document;
//! - the mirror never answers: omp logs no missing acks and the host stays its only answerer.
//!
//! Private HOME/XDG tree and own `omp-deck-test-*` namespace; skipped when omp is not installed.

mod support;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deck_host::{Namespace, SessionSpec};
use deck_service::host_remote::HostRemote;
use deck_term::{TermOptions, Terminal, TspRefused};
use deck_tsp::wire::Event;
use serde_json::Value;
use support::device::{OMP_CONFIG, Sandbox};
use support::fake_model::{self as fm, FakeModel, Reply};

const STREAM_HEAD: &str = "Streamed answer";
const STREAM_REPLY: &str = "Streamed answer: one two three four five six seven eight nine ten \
     eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty.";
const STREAM_TIME: Duration = Duration::from_secs(6);
const TURN: Duration = Duration::from_secs(60);
const SESSION: &str = "tspm1";

fn script() -> impl FnMut(&Value) -> Reply + Send + 'static {
    move |req| {
        if !fm::has_tools(req) {
            return Reply::text("Fake title");
        }
        if fm::last_user_text(req).contains("STREAM-MARK") {
            return Reply::slow_text(STREAM_REPLY, STREAM_TIME);
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

fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
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

fn text_starting(doc: &Value, head: &str) -> Option<String> {
    nodes(doc)
        .into_iter()
        .filter_map(|(_, _, p)| p["text"].as_str().map(str::to_owned))
        .find(|t| t.starts_with(head))
}

fn mirror_doc(t: &Terminal) -> Value {
    t.tsp()
        .with(|d| serde_json::to_value(d).expect("document JSON"))
}

#[test]
fn a_late_joining_terminal_mirrors_omp_and_sends_prompts_through_the_host() {
    let Some(omp) = which(if cfg!(windows) { "omp.exe" } else { "omp" }) else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("tspm", &config);
    ensure_host_exe();
    let model = FakeModel::start(script());
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    let ns = Namespace::new(&sb.ns).expect("namespace");
    let _cleanup = Cleanup(Namespace::new(&sb.ns).expect("namespace"));

    let (mut env_set, env_remove) = deck_core::mux::host_env(SESSION, &[], false);
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
    let peek = |ns: &Namespace| deck_host::tsp_peek(ns, SESSION).expect("tsp_peek");

    // omp drew its surfaces before anybody looked.
    wait_for("a sendable composer at the host", TURN, || {
        composer(&peek(&ns))
    });

    // The gateway's way of owning a session's terminal, joining late.
    let mut term = Terminal::connect_headless(
        Box::new(HostRemote::new(ns, spec)),
        TermOptions::default(),
        Arc::new(|_| {}),
    );
    let tsp = term.tsp();
    let ns = Namespace::new(&sb.ns).expect("namespace");
    let (sf, editor) = wait_for("the replayed composer", TURN, || {
        tsp.live().then(|| composer(&mirror_doc(&term))).flatten()
    });
    // Equal to the host's document once both are quiet (omp may still be repainting).
    let equal = |what: &str| {
        let mut last = (Value::Null, Value::Null);
        let ok = wait_for(what, Duration::from_secs(30), || {
            last = (mirror_doc(&term), peek(&ns));
            (last.0["surfaces"] == last.1["surfaces"]).then_some(true)
        });
        assert!(ok);
        last
    };
    let (mirror, host) = equal("the mirror to equal the host's document");
    assert_eq!(mirror["surfaces"][0]["role"], "omp.session");
    assert_eq!(mirror["surfaces"], host["surfaces"]);
    assert!(
        tsp.with(|d| d.program().is_some_and(|p| p.supports("send"))),
        "the program's hello travels in the replay"
    );

    // The prompt goes in through send_tsp; the reply streams into the mirror itself.
    let revision = tsp.revision();
    term.send_tsp(&Event::Send {
        sf,
        id: editor,
        text: "STREAM-MARK say something long".to_owned(),
    })
    .expect("a live, attached terminal sends");
    let mut lengths = BTreeSet::new();
    let full = wait_for("the streamed reply in the mirror", TURN, || {
        let text = text_starting(&mirror_doc(&term), STREAM_HEAD)?;
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
        "the reply was seen growing in the mirror: {lengths:?}"
    );
    assert!(tsp.revision() > revision);
    let (mirror, host) = equal("the mirror to equal the host's document after the turn");
    assert_eq!(
        text_starting(&mirror, STREAM_HEAD).as_deref(),
        Some(STREAM_REPLY)
    );
    assert_eq!(mirror["surfaces"], host["surfaces"]);
    assert!(
        model
            .since(0)
            .iter()
            .any(|r| fm::last_user_text(r).contains("STREAM-MARK say something long")),
        "the model received the prompt sent from the terminal"
    );

    // Once the session is gone the terminal says so instead of dropping the event.
    ns.kill(SESSION).expect("kill the session");
    let refused = wait_for(
        "the refusal after the kill",
        Duration::from_secs(30),
        || {
            term.poll();
            term.send_tsp(&Event::Theme { dark: true }).err()
        },
    );
    assert!(
        matches!(refused, TspRefused::Exited | TspRefused::Detached),
        "{refused:?}"
    );
    drop(term);
    deck_term::wait_detached(Duration::from_secs(10));
}
