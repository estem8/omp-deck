//! Consecutive toasts of a TSP (chat) session with a real omp (18.15+): two model switches, the way
//! the GUI's model menu runs them (`status.model` action, then `activate` in the picker), must each show as their own toast in the
//! mirror's ring, though omp keeps one `toast` node and only changes its text.
//!
//! Private HOME/XDG tree and own `omp-deck-test-*` namespace; skipped when omp is not installed.
//! The raw TSP the session sent is recorded to `$TSP_TOAST_RECORD_OUT` (when set) for inspection.

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use deck_host::{AttachParams, HostEvent, Namespace, Palette, SessionSpec};
use deck_tsp::mirror::Mirror;
use deck_tsp::terminal::{InputEncoding, encode_event};
use deck_tsp::wire::Event;
use serde_json::Value;
use support::device::{OMP_CONFIG, Sandbox};
use support::fake_model::{self as fm, FakeModel, Reply};

const SESSION: &str = "tsptoast1";
const TURN: Duration = Duration::from_secs(90);

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

/// Copies the recording to `$TSP_TOAST_RECORD_OUT` when the test ends, also on a panic.
struct KeepRecord(PathBuf);

impl Drop for KeepRecord {
    fn drop(&mut self) {
        if let Some(out) = std::env::var_os("TSP_TOAST_RECORD_OUT") {
            let _ = std::fs::copy(&self.0, out);
        }
    }
}

/// The test binary's sandboxes point the process's HOME/XDG variables at their own tree.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Every `(seq, text)` of the mirror's unexpired toasts.
fn live_toasts(mirror: &Mutex<Mirror>) -> Vec<(u64, String)> {
    let now = Instant::now();
    let mirror = mirror.lock().unwrap_or_else(PoisonError::into_inner);
    mirror
        .document()
        .toasts(now)
        .map(|t| (t.seq, t.text.clone()))
        .collect()
}

#[test]
fn every_model_switch_shows_its_own_toast_though_omp_reuses_one_toast_node() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(omp) = which(if cfg!(windows) { "omp.exe" } else { "omp" }) else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    let config = format!(
        "{OMP_CONFIG}modelRoles:\n  default: {}\nstartup:\n  setupWizard: false\n",
        fm::MODEL
    );
    let sb = Sandbox::new("tsptoast", &config);
    ensure_host_exe();
    let model = FakeModel::start(|req: &Value| {
        let _ = req;
        Reply::text("Fake title")
    });
    let models = format!(
        "{}      - id: fake-2\n        name: Fake Two\n        contextWindow: 200000\n        \
         maxTokens: 4096\n        input: [text, image]\n",
        model.models_yml()
    );
    std::fs::write(sb.agent().join("models.yml"), models).expect("models.yml");
    let ns = Namespace::new(&sb.ns).expect("namespace");
    let _cleanup = Cleanup(Namespace::new(&sb.ns).expect("namespace"));

    let record = sb.proj().join("tsp-record.log");
    let _keep = KeepRecord(record.clone());
    let (mut env_set, env_remove) = deck_core::mux::host_env(SESSION, &[], false);
    env_set.retain(|(k, _)| k != "PI_NOTIFICATIONS");
    env_set.push(("PI_NOTIFICATIONS".to_owned(), "off".to_owned()));
    env_set.push((
        "PI_TUI_TSP_RECORD".to_owned(),
        record.to_string_lossy().into_owned(),
    ));
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
    let (sf, _editor) = wait_for("a sendable composer", TURN, || composer(&peek(&ns)));

    // A controller that stays attached, its output feeding a mirror like the app's chat view.
    let params = AttachParams {
        cols: 100,
        rows: 30,
        cell_width: 8,
        cell_height: 16,
        history_lines: 0,
        palette: Palette::default(),
    };
    let attached = deck_host::attach(&ns, SESSION, params).expect("attach");
    let (mut rx, tx) = attached.split();
    let mirror = Arc::new(Mutex::new(Mirror::new()));
    let feed = mirror.clone();
    let reader = std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            match event {
                HostEvent::Snapshot(b) | HostEvent::Output(b) => {
                    let mut m = feed.lock().unwrap_or_else(PoisonError::into_inner);
                    let _ = m.feed(&b, Instant::now());
                }
                HostEvent::Closed | HostEvent::Exited(_) => break,
                _ => {}
            }
        }
    });
    let write = |event: Event| {
        tx.input(&encode_event(&event, InputEncoding::Apc))
            .expect("send the event");
    };
    // The GUI's model menu: the composer bar's model button opens the picker (a node of the
    // `layer`), and activating the model's item switches to it and closes the picker.
    let switch = |model: &str| {
        wait_for("the picker to be closed", TURN, || {
            (!nodes(&peek(&ns)).iter().any(|(_, k, _)| k == "picker")).then_some(())
        });
        write(Event::Action {
            sf: sf.clone(),
            id: "3.bar/model".to_owned(),
            act: "status.model".to_owned(),
            value: None,
            mods: None,
        });
        let picker = wait_for("the model picker", TURN, || {
            nodes(&peek(&ns)).into_iter().find_map(|(id, k, p)| {
                let has = p["items"]
                    .as_array()?
                    .iter()
                    .any(|i| i["id"].as_str() == Some(model));
                (k == "picker" && has).then_some(id)
            })
        });
        write(Event::Activate {
            sf: sf.clone(),
            id: picker,
            item: model.to_owned(),
        });
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut next_toast = |what: &str| -> (u64, String) {
        let seen = &mut seen;
        wait_for(what, Duration::from_secs(20), || {
            let fresh = live_toasts(&mirror)
                .into_iter()
                .find(|(seq, _)| !seen.contains(seq))?;
            seen.insert(fresh.0);
            Some(fresh)
        })
    };

    switch("fake/fake-2");
    let first = next_toast("the first switch's toast");
    // Let the first toast run out the way a user's does, then switch back.
    wait_for("the first toast to end", TURN, || {
        live_toasts(&mirror).is_empty().then_some(())
    });
    switch("fake/fake-1");
    let second = next_toast("the second switch's toast");
    assert!(
        second.0 > first.0,
        "a fresh record: {first:?} then {second:?}"
    );
    assert_ne!(
        first.1, second.1,
        "the second toast says what changed: {first:?} {second:?}"
    );

    drop(tx);
    let _ = reader.join();
}
