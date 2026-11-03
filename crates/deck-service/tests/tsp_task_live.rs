//! A subagent on a TSP (chat) session, with a real omp (18.15+) driven by the scripted model: the
//! parent's `task` call spawns a subagent whose own model requests the script answers with its
//! finish tool, and the document shows an `agent` node that is seen running and then done.
//!
//! Private HOME/XDG tree and own `omp-deck-test-*` namespace; skipped when omp is not installed.

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use deck_host::{AttachParams, HostEvent, Namespace, Palette, SessionSpec};
use deck_tsp::terminal::{InputEncoding, encode_event};
use deck_tsp::wire::Event;
use serde_json::Value;
use support::device::{OMP_CONFIG, Sandbox};
use support::fake_model::{self as fm, FakeModel, Reply};

const SESSION: &str = "tsptask1";
const TURN: Duration = Duration::from_secs(90);
/// How long the subagent's finish call streams: long enough to poll its `running` state.
const HOLD: Duration = Duration::from_secs(3);

fn script() -> impl FnMut(&Value) -> Reply + Send + 'static {
    move |req| {
        if !fm::has_tools(req) {
            // A side request (e.g. the session title).
            return Reply::text("Fake title");
        }
        fm::task_script(req, HOLD).unwrap_or_else(|| Reply::text("unscripted"))
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

/// `omp-deck-host` next to the test binaries, rebuilt: a stale binary would run an older daemon.
fn ensure_host_exe() {
    let exe = if cfg!(windows) {
        "omp-deck-host.exe"
    } else {
        "omp-deck-host"
    };
    let beside = PathBuf::from(support::device::GATEWAY).with_file_name(exe);
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "-p", "deck-host", "--bin", "omp-deck-host"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("run cargo build");
    assert!(status.success(), "cargo build -p deck-host");
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

fn peek(ns: &Namespace) -> Value {
    deck_host::tsp_peek(ns, SESSION).expect("tsp_peek")
}

/// Every node of the document's surfaces, depth first, as `(id, kind, props)`.
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
        walk(&s["root"], &mut out);
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

/// The `status` of every `agent` node.
fn agent_statuses(doc: &Value) -> Vec<String> {
    nodes(doc)
        .into_iter()
        .filter(|(_, k, _)| k == "agent")
        .filter_map(|(_, _, p)| p["status"].as_str().map(str::to_owned))
        .collect()
}

/// Attaches, writes the `send` event for `text` and detaches again.
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

/// The test binary's sandboxes point the process's HOME/XDG variables at their own tree: the
/// tests take turns.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs a prompt with `keyword` on a fresh omp and returns once the agent node is done and the
/// parent has closed its turn; `check` sees the model's requests.
fn run_case(label: &str, keyword: &str, settings: &str, check: impl FnOnce(&[Value])) {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(omp) = which(if cfg!(windows) { "omp.exe" } else { "omp" }) else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    let config = format!(
        "{OMP_CONFIG}modelRoles:\n  default: {}\n{settings}",
        fm::MODEL
    );
    let sb = Sandbox::new(label, &config);
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
    let (sf, editor) = wait_for("a sendable composer", TURN, || composer(&peek(&ns)));

    send(&ns, &sf, &editor, &format!("{keyword} run a helper"));

    // The subagent's request reaches the model, which holds its answer: the agent node is
    // running (or pending) on the first look after that, and later done.
    wait_for("the subagent's request", TURN, || {
        model.since(0).iter().any(fm::is_subagent).then_some(())
    });
    let statuses = agent_statuses(&peek(&ns));
    assert!(
        statuses.iter().any(|s| s == "running" || s == "pending"),
        "the agent is working while the subagent's answer is held: {statuses:?}"
    );
    wait_for("the agent node to reach done", TURN, || {
        agent_statuses(&peek(&ns))
            .iter()
            .any(|s| s == "done")
            .then_some(())
    });
    wait_for("the parent's closing text", TURN, || {
        nodes(&peek(&ns))
            .into_iter()
            .any(|(_, _, p)| p["text"].as_str() == Some(fm::TASK_PARENT_REPLY))
            .then_some(())
    });
    assert!(
        nodes(&peek(&ns))
            .iter()
            .any(|(_, k, p)| k == "tool" && p.to_string().contains("task")),
        "the parent's task call is a tool node"
    );
    check(&model.since(0));
}

#[test]
fn a_subagent_shows_as_an_agent_node_that_runs_and_ends_done() {
    run_case("tsptask", fm::TASK_MARK, "", |requests| {
        assert!(
            requests.iter().any(fm::is_subagent),
            "a request of the subagent reached the model"
        );
    });
}

#[test]
fn a_subagent_with_an_output_schema_ends_through_its_yield_tool() {
    run_case(
        "tsptasky",
        fm::TASK_YIELD_MARK,
        "task:\n  structured: true\n",
        |requests| {
            assert!(
                requests
                    .iter()
                    .any(|r| fm::tool_names(r).contains(&"yield")),
                "the subagent was offered the yield tool"
            );
        },
    );
}
