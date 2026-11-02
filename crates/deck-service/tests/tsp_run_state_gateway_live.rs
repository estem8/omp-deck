//! A chat (TSP) session's activity reaches a gateway client that only subscribes to the
//! controllers topic, with a real gateway process and a real omp (18.15+) on the scripted model.
//!
//! Under TSP omp sets no `π` title, so nothing about the terminal title changes while a turn
//! starts and ends: the gateway must republish the topic on omp's own state (OSC 7501 reports,
//! the TSP document). The session starts a turn on its argument prompt; the subscriber's cache
//! must show it `Busy` while the turn streams and `Idle` again once it finished. No view is
//! attached.
//!
//! Private HOME/XDG tree under a unique `omp-deck-test-*` namespace; skipped when omp is not
//! installed.

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use deck_core::config::SessionView;
use deck_service::controller::SessionTarget;
use deck_service::gateway::client::{GatewayClient, Link};
use deck_service::gateway::protocol::{
    Answer, ControllerInfo, Done, LaunchSpec, Mutation, Query, SessionRole, Topic, TopicValue,
};
use deck_service::statusfmt::Activity;
use serde_json::Value;
use support::device::{OMP_CONFIG, Sandbox, run, wait_for};
use support::fake_model::{self as fm, FakeModel, Reply};

const STREAM_REPLY: &str = "Streamed answer: one two three four five six seven eight nine ten \
     eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty.";
const STREAM_TIME: Duration = Duration::from_secs(6);
const START: Duration = Duration::from_secs(60);

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

/// `omp-deck-host` next to the test binaries, rebuilt: a stale one would run the previous
/// daemon, which reports no run state.
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

/// Kills every host session of the namespace, also when the test panics.
struct Cleanup(deck_host::Namespace);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for name in self.0.list() {
            let _ = self.0.kill(&name);
        }
    }
}

/// The controllers the client's subscription last received (never a query).
fn cached(client: &GatewayClient) -> Vec<ControllerInfo> {
    match client.state().topic(&Topic::Controllers) {
        Some(TopicValue::Controllers(c)) => c,
        _ => Vec::new(),
    }
}

/// What the gateway itself answers now (a query, not the subscription).
fn fresh(client: &GatewayClient) -> Vec<ControllerInfo> {
    match client
        .query(Query::Topic(Topic::Controllers))
        .wait(Duration::from_secs(10))
    {
        Some(Ok(Answer::Topic(v))) => match *v {
            TopicValue::Controllers(c) => c,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn activity_of(client: &GatewayClient, target: &SessionTarget) -> Option<Activity> {
    cached(client)
        .iter()
        .find(|c| c.target.key == target.key)
        .map(|c| c.activity)
}

/// Waits until the subscribed cache shows `want`, saying what it showed otherwise.
fn wait_activity(client: &GatewayClient, target: &SessionTarget, what: &str, want: Activity) {
    let end = Instant::now() + START;
    loop {
        let got = activity_of(client, target);
        if got == Some(want) {
            return;
        }
        assert!(
            Instant::now() < end,
            "timed out waiting for {what} ({want:?}); the subscriber has {got:?}; a query says {:?}",
            fresh(client)
                .iter()
                .find(|c| c.target.key == target.key)
                .map(|c| (c.activity, c.title.clone()))
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn a_subscriber_sees_a_chat_sessions_turn_without_any_title_change() {
    let Some(_omp) = which(if cfg!(windows) { "omp.exe" } else { "omp" }) else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("tsprg", &config);
    sb.report_failures();
    ensure_host_exe();
    let model = FakeModel::start(|req: &Value| {
        if !fm::has_tools(req) {
            return Reply::text("Fake title");
        }
        if fm::last_user_text(req).contains("STREAM-MARK") {
            Reply::slow_text(STREAM_REPLY, STREAM_TIME)
        } else {
            Reply::text("unscripted")
        }
    });
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    let _cleanup = Cleanup(deck_host::Namespace::new(&sb.ns).expect("namespace"));

    let client = sb.client();
    wait_for("the gateway link", Duration::from_secs(30), || {
        matches!(client.link(), Link::Up(_)).then_some(())
    });
    client.subscribe(Topic::Controllers);

    let done = run(
        &client,
        Mutation::Launch(LaunchSpec {
            mux_name: Some("omp-chat-rs".to_owned()),
            cwd: sb.proj(),
            session_path: None,
            label: "Chat".into(),
            argv: vec!["omp".into(), "STREAM-MARK say something long".into()],
            role: SessionRole::Session,
            copy_of: None,
            view: SessionView::Chat,
        }),
    )
    .expect("launch");
    let Done::Session(target, view) = done else {
        panic!("launch result: {done:?}");
    };
    assert_eq!(view, SessionView::Chat);

    // omp starts a turn on its argument prompt: the subscriber sees it run ...
    wait_activity(&client, &target, "the running turn", Activity::Busy);
    assert!(
        cached(&client)
            .iter()
            .find(|c| c.target.key == target.key)
            .is_some_and(|c| c.title.as_deref().is_none_or(|t| !t.starts_with('π'))),
        "no classic title tells the state under TSP"
    );
    // ... and end, with no title change to trigger the publish.
    wait_activity(&client, &target, "the finished turn", Activity::Idle);
    assert!(
        model
            .since(0)
            .iter()
            .any(|r| fm::last_user_text(r).contains("STREAM-MARK say something long")),
        "the model received the prompt"
    );
}
