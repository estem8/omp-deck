//! Launching a session in the chat view through a real gateway process with a real omp, no GUI.
//!
//! - tmux is the primary backend (`Auto`), and a chat launch still lands on the session host,
//!   whose session spec carries `tsp: true` (read back through the host's `info` and `tsp_peek`);
//! - a restart into the terminal view keeps the tab (same session key), the session file and the
//!   label, and runs a plain (non-TSP) terminal session on the backend the session was on;
//! - a restart into the chat view moves a tmux terminal session to the host, on the same file.
//!
//! Private HOME/XDG tree under a unique `omp-deck-test-*` namespace (host and tmux socket
//! included); the real omp store and the real `omp-deck` tmux server are never touched. Skipped
//! (with a message) when omp or tmux is not installed.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use deck_core::config::SessionView;
use deck_core::mux::MuxKind;
use deck_host::Namespace;
use deck_service::controller::SessionKey;
use deck_service::gateway::client::{GatewayClient, Link};
use deck_service::gateway::protocol::{
    Answer, BackendRef, ControllerInfo, Done, LaunchSpec, Mutation, Query, SessionRole, Topic,
    TopicValue,
};
use serde_json::{Value, json};
use support::device::{OMP_CONFIG, Sandbox, run, wait_for};
use support::fake_model::{self as fm, FakeModel, Reply};

const SEEDED: &str = "11111111-aaaa-bbbb-cccc-000000000001";
const SEEDED_B: &str = "11111111-aaaa-bbbb-cccc-000000000002";
/// How long omp may take to start and draw its first surface.
const START: Duration = Duration::from_secs(60);

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

/// Kills every host session of the namespace, also when the test panics.
struct Cleanup(Namespace);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for name in self.0.list() {
            let _ = self.0.kill(&name);
        }
    }
}

fn controllers(client: &GatewayClient) -> Vec<ControllerInfo> {
    match client
        .query(Query::Topic(Topic::Controllers))
        .wait(Duration::from_secs(10))
    {
        Some(Ok(Answer::Topic(v))) => match *v {
            TopicValue::Controllers(c) => c,
            other => panic!("controllers value: {other:?}"),
        },
        other => panic!("controllers query: {other:?}"),
    }
}

fn launch(client: &GatewayClient, name: &str, file: &Path, cwd: &Path, view: SessionView) -> Done {
    run(
        client,
        Mutation::Launch(LaunchSpec {
            mux_name: Some(name.to_owned()),
            cwd: cwd.to_path_buf(),
            session_path: Some(file.to_path_buf()),
            label: "Seeded".into(),
            argv: vec!["omp".into(), "--resume".into(), file.display().to_string()],
            role: SessionRole::Session,
            copy_of: None,
            view,
        }),
    )
    .expect("launch")
}

/// The record of the session `key` once omp runs (not exited).
fn running(
    client: &GatewayClient,
    target: &deck_service::controller::SessionTarget,
) -> ControllerInfo {
    wait_for("the session running", START, || {
        controllers(client)
            .into_iter()
            .find(|c| c.target.key == target.key && c.exit_code.is_none())
    })
}

fn backend(info: &ControllerInfo) -> (MuxKind, String) {
    match &info.backend {
        BackendRef::Mux { kind, name } => (*kind, name.clone()),
        other => panic!("a backend session was launched, got {other:?}"),
    }
}

/// The host's document, once omp drew an inline surface through it.
fn drawn(ns: &Namespace, name: &str) -> Value {
    wait_for("omp drawing native surfaces", START, || {
        let doc = deck_host::tsp_peek(ns, name).ok()?;
        doc["surfaces"]
            .as_array()
            .is_some_and(|s| !s.is_empty())
            .then_some(doc)
    })
}

/// The host's record of the session `name`, once the daemon runs it (the gateway's launch
/// answer does not wait for the daemon).
fn host_info(ns: &Namespace, name: &str) -> deck_host::HostInfo {
    wait_for("the host session", START, || ns.info(name).ok())
}

/// How many sessions the gateway holds on the session file `file`.
fn sessions_on(client: &GatewayClient, file: &Path) -> usize {
    controllers(client)
        .iter()
        .filter(|c| c.session_path.as_deref() == Some(file))
        .count()
}

fn session_target(done: Done, view: SessionView) -> deck_service::controller::SessionTarget {
    match done {
        Done::Session(target, got) => {
            assert_eq!(got, view, "the launch reports the view it runs in");
            target
        }
        other => panic!("session result: {other:?}"),
    }
}

#[test]
fn a_chat_launch_runs_on_the_host_and_a_restart_switches_the_view_in_place() {
    let Some(_omp) = which(if cfg!(windows) { "omp.exe" } else { "omp" }) else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    if which("tmux").is_none() {
        eprintln!("skipped: tmux is not installed (the primary backend must differ from the host)");
        return;
    }
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("chat", &config);
    sb.report_failures();
    ensure_host_exe();
    let model = FakeModel::start(|req: &Value| {
        if fm::has_tools(req) {
            Reply::text("unscripted")
        } else {
            Reply::text("Fake title")
        }
    });
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    sb.seed_session(SEEDED);
    let file = sb
        .profile
        .sessions_root
        .join("--proj--")
        .join(format!("{SEEDED}.jsonl"));
    let before = std::fs::read_to_string(&file).expect("seeded file");
    sb.seed_session(SEEDED_B);
    let file_b = file.with_file_name(format!("{SEEDED_B}.jsonl"));
    let ns = Namespace::new(&sb.ns).expect("namespace");
    let _cleanup = Cleanup(Namespace::new(&sb.ns).expect("namespace"));

    let client = sb.client();
    wait_for("the gateway link", Duration::from_secs(30), || {
        matches!(client.link(), Link::Up(_)).then_some(())
    });

    // (a) A chat launch: tmux is the primary backend, the session lives on the host anyway,
    // as a TSP session (its omp was handed the terminal's hello reply: it draws surfaces).
    let target = session_target(
        launch(&client, "omp-chat-a", &file, &sb.proj(), SessionView::Chat),
        SessionView::Chat,
    );
    let info = running(&client, &target);
    assert_eq!(info.view, SessionView::Chat);
    let (kind, name) = backend(&info);
    assert_eq!(
        kind,
        MuxKind::Host,
        "a chat launch takes the host, not tmux"
    );
    assert!(
        host_info(&ns, &name).tsp,
        "the host session is a TSP session"
    );
    let doc = drawn(&ns, &name);
    assert_eq!(doc["surfaces"][0]["mode"], "inline", "{doc}");
    let (label, path) = (info.label.clone(), info.session_path.clone());
    assert_eq!(path.as_deref(), Some(file.as_path()));

    // (b) Restart into the terminal view: same tab (session key), same file and label. The
    // session stays on the host (the backend it runs on) as a plain terminal session.
    let t2 = session_target(
        run(
            &client,
            Mutation::Restart {
                target: info.target.clone(),
                view: Some(SessionView::Terminal),
            },
        )
        .expect("restart into the terminal"),
        SessionView::Terminal,
    );
    assert_eq!(t2.key, target.key, "the tab keeps its session identity");
    assert!(
        t2.generation > target.generation,
        "a new generation: the view is replaced"
    );
    assert_eq!(sessions_on(&client, &file), 1);
    let info = running(&client, &t2);
    assert_eq!(info.view, SessionView::Terminal);
    assert_eq!(info.label, label);
    assert_eq!(info.session_path, path);
    let (kind, name) = backend(&info);
    assert_eq!(kind, MuxKind::Host);
    assert!(
        !host_info(&ns, &name).tsp,
        "no TSP session behind a terminal view"
    );
    assert!(deck_host::tsp_peek(&ns, &name).is_err());

    // (c) And back into the chat view: the host again, with the same session file.
    let t3 = session_target(
        run(
            &client,
            Mutation::Restart {
                target: info.target.clone(),
                view: Some(SessionView::Chat),
            },
        )
        .expect("restart into the chat"),
        SessionView::Chat,
    );
    assert_eq!(t3.key, target.key);
    assert!(t3.generation > t2.generation);
    assert_eq!(sessions_on(&client, &file), 1);
    let info = running(&client, &t3);
    assert_eq!(info.view, SessionView::Chat);
    assert_eq!(info.label, label);
    assert_eq!(info.session_path, path);
    let (kind, name) = backend(&info);
    assert_eq!(kind, MuxKind::Host);
    assert!(host_info(&ns, &name).tsp);
    drawn(&ns, &name);

    // (d) A restart that names no view keeps the one the session runs in.
    let t4 = session_target(
        run(
            &client,
            Mutation::Restart {
                target: info.target.clone(),
                view: None,
            },
        )
        .expect("plain restart"),
        SessionView::Chat,
    );
    assert_eq!(t4.key, t3.key, "the same backend keeps the identity");
    let info = running(&client, &t4);
    assert_eq!(info.view, SessionView::Chat);
    assert!(host_info(&ns, &backend(&info).1).tsp);

    // (e) A terminal session on the primary backend (tmux) restarted into the chat view moves
    // to the host: another backend session, so another key, on the same file and label.
    let target = session_target(
        launch(
            &client,
            "omp-chat-b",
            &file_b,
            &sb.proj(),
            SessionView::Terminal,
        ),
        SessionView::Terminal,
    );
    assert_eq!(target.key, SessionKey::tmux(&sb.ns, "omp-chat-b"));
    let info = running(&client, &target);
    assert_eq!(info.view, SessionView::Terminal);
    assert_eq!(backend(&info), (MuxKind::Tmux, "omp-chat-b".to_owned()));
    // The launch answer does not wait for the backend session: restart only once tmux has it.
    wait_for("the tmux session", START, || {
        std::process::Command::new("tmux")
            .args(["-L", &sb.ns, "has-session", "-t", "omp-chat-b"])
            .output()
            .ok()
            .filter(|o| o.status.success())
    });
    let moved = session_target(
        run(
            &client,
            Mutation::Restart {
                target: info.target.clone(),
                view: Some(SessionView::Chat),
            },
        )
        .expect("restart tmux session into the chat"),
        SessionView::Chat,
    );
    assert_eq!(moved.key, SessionKey::host(&sb.ns, "omp-chat-b"));
    assert_eq!(sessions_on(&client, &file_b), 1);
    let info = running(&client, &moved);
    assert_eq!(info.view, SessionView::Chat);
    assert_eq!(info.label, "Seeded");
    assert_eq!(info.session_path.as_deref(), Some(file_b.as_path()));
    assert!(host_info(&ns, "omp-chat-b").tsp);
    drawn(&ns, "omp-chat-b");

    // The session file was only ever resumed, never replaced.
    let after = std::fs::read_to_string(&file).expect("session file still there");
    assert!(
        after.starts_with(&before),
        "the resumed file keeps its content: {after:?}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(after.lines().next().unwrap_or_default())
            .map(|h| h["id"].clone())
            .ok(),
        Some(json!(SEEDED))
    );
}
