//! omp's run state under TSP, with a real omp (18.15+) driven by a scripted model.
//!
//! Under TSP omp drops the classic `π > label` title, so what a terminal can tell of omp's state
//! comes from its OSC 7501 run-status reports (and the TSP document). On a TSP host session:
//! - the controller's activity (a gateway-style owner terminal) is `Busy` during a turn and
//!   `Idle` after it, although the title says nothing;
//! - the running poller's snapshot (which only has the host's `info`: title + run state) shows
//!   the same states, also for a session nobody is attached to;
//! - an `ask` question puts both into `Attention`, and answering it resumes the turn;
//! - a held `done` does not wedge the next turn: it goes `Busy` again.
//!
//! Private HOME/XDG tree and own `omp-deck-test-*` namespace; skipped when omp is not installed.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deck_core::mux::{HostMux, Mux};
use deck_host::{Namespace, RunPhase, SessionSpec};
use deck_service::controller::{SessionController, SessionKey};
use deck_service::gateway::running::RunningMonitor;
use deck_service::host_remote::HostRemote;
use deck_service::statusfmt::Activity;
use deck_term::{TermOptions, Terminal};
use deck_tsp::wire::Event;
use serde_json::{Value, json};
use support::device::{OMP_CONFIG, Sandbox};
use support::fake_model::{self as fm, FakeModel, Reply};

const STREAM_REPLY: &str = "Streamed answer: one two three four five six seven eight nine ten \
     eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty.";
const STREAM_TIME: Duration = Duration::from_secs(6);
const ASKED_REPLY: &str = "The question was answered.";
/// How long omp may take to show a surface, reach a state or finish a turn.
const TURN: Duration = Duration::from_secs(60);
const SESSION: &str = "tsprs1";

fn script() -> impl FnMut(&Value) -> Reply + Send + 'static {
    move |req| {
        if !fm::has_tools(req) {
            // A side request (e.g. the session title).
            return Reply::text("Fake title");
        }
        let user = fm::last_user_text(req);
        if user.contains("STREAM-MARK") {
            return Reply::slow_text(STREAM_REPLY, STREAM_TIME);
        }
        if user.contains("ASK-MARK") {
            let fresh = !fm::after_last_user(req)
                .iter()
                .any(|m| fm::role(m) == "tool");
            return if fresh {
                Reply::tool_call(
                    "ask",
                    &json!({"questions": [{
                        "id": "storage",
                        "question": "Which database?",
                        "options": [{"label": "SQLite"}, {"label": "Postgres"}],
                        "recommended": 0,
                    }]}),
                )
            } else {
                Reply::text(ASKED_REPLY)
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
    // Always: a stale binary beside the tests would run the previous host daemon, which does
    // not report run state.
    {
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
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// `(surface id, editor node id)` once omp shows a sendable composer.
fn composer(doc: &Value) -> Option<(String, String)> {
    fn find(n: &Value) -> Option<String> {
        if n["k"] == "editor" && n["p"]["sendable"] == true {
            return n["id"].as_str().map(str::to_owned);
        }
        n["c"].as_array()?.iter().find_map(find)
    }
    doc["surfaces"]
        .as_array()?
        .iter()
        .filter(|s| s["mode"] == "inline" && s["open"] == true)
        .find_map(|s| Some((s["id"].as_str()?.to_owned(), find(&s["root"])?)))
}

/// What the three views of omp's state say right now.
struct Views<'a> {
    ctl: &'a SessionController,
    monitor: &'a RunningMonitor,
    ns: &'a Namespace,
}

impl Views<'_> {
    fn controller(&self) -> Activity {
        self.ctl.activity()
    }

    fn running(&self) -> Activity {
        self.monitor
            .latest()
            .map_or(Activity::Unknown, |s| s.activity_of(SESSION))
    }

    fn host_run_state(&self) -> Option<RunPhase> {
        self.ns.info(SESSION).ok()?.run_state.map(|r| r.state)
    }

    /// Both consumers agree on `want`.
    fn both(&self, want: Activity) -> bool {
        self.controller() == want && self.running() == want
    }

    fn describe(&self) -> String {
        format!(
            "controller {:?}, running {:?}, host run state {:?}, terminal run state {:?}, title {:?}",
            self.controller(),
            self.running(),
            self.ns
                .info(SESSION)
                .ok()
                .map(|i| (i.run_state, i.title, i.tsp_facts)),
            self.ctl.terminal().run_state(),
            self.ctl.terminal().title(),
        )
    }

    /// Waits until both consumers say `want`, reporting what they said otherwise.
    fn wait_both(&self, what: &str, want: Activity) {
        let end = Instant::now() + TURN;
        while !self.both(want) {
            assert!(
                Instant::now() < end,
                "timed out waiting for {what} ({want:?}): {}",
                self.describe()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[test]
fn omp_run_state_drives_activity_under_tsp() {
    let Some(omp) = which(if cfg!(windows) { "omp.exe" } else { "omp" }) else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("tsprs", &config);
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
    let peek = || deck_host::tsp_peek(&ns, SESSION).expect("tsp_peek");
    wait_for("a sendable composer", TURN, || composer(&peek()));

    // The running poller, with nobody attached yet.
    let host = HostMux::new(&sb.ns).expect("host mux");
    let monitor = RunningMonitor::spawn(
        vec![Mux::Host(host)],
        sb.ns.clone(),
        sb.root.join("sessions"),
        Duration::from_millis(100),
        || {},
    );

    // The gateway's way of owning a session's terminal, joining late.
    let term = Terminal::connect_headless(
        Box::new(HostRemote::new(
            Namespace::new(&sb.ns).expect("namespace"),
            spec,
        )),
        TermOptions::default(),
        Arc::new(|_| {}),
    );
    let ctl = SessionController::new(SessionKey::host(&sb.ns, SESSION), term, None);
    let views = Views {
        ctl: &ctl,
        monitor: &monitor,
        ns: &ns,
    };

    // (a) At the prompt: idle for the controller (a late joiner of the replay) and the poller.
    wait_for("the replayed composer", TURN, || {
        ctl.terminal().tsp().live().then_some(())
    });
    views.wait_both("idle in both views", Activity::Idle);

    // (b) A turn: busy in both views although omp's title is not a `π` title (the bug).
    let (sf, editor) = composer(&peek()).expect("composer");
    ctl.terminal()
        .send_tsp(&Event::Send {
            sf,
            id: editor,
            text: "STREAM-MARK say something long".to_owned(),
        })
        .expect("send the prompt");
    views.wait_both("busy in both views", Activity::Busy);
    assert_eq!(
        Activity::from_title(ctl.terminal().title()),
        Activity::Unknown,
        "under TSP the title does not tell: {:?}",
        ctl.terminal().title()
    );
    assert_eq!(views.host_run_state(), Some(RunPhase::Working));
    assert!(
        ctl.terminal()
            .run_state()
            .is_some_and(|r| r.state == RunPhase::Working)
    );
    // It stays busy for the streamed reply's duration, never flickering through idle.
    let busy_until = Instant::now() + STREAM_TIME / 3;
    while Instant::now() < busy_until {
        assert_eq!(views.controller(), Activity::Busy);
        std::thread::sleep(Duration::from_millis(20));
    }

    // (c) The turn ends: idle again, in both views (done or idle, both finished turns).
    views.wait_both("idle after the turn", Activity::Idle);
    assert!(matches!(
        views.host_run_state(),
        Some(RunPhase::Idle | RunPhase::Done)
    ));

    // (d) A question blocks the run: attention in both views.
    let (sf, editor) = composer(&peek()).expect("composer");
    ctl.terminal()
        .send_tsp(&Event::Send {
            sf,
            id: editor,
            text: "ASK-MARK ask me".to_owned(),
        })
        .expect("send the prompt");
    views.wait_both("attention in both views", Activity::Attention);
    assert!(matches!(
        views.host_run_state(),
        Some(RunPhase::Blocked { .. })
    ));

    // Answering (Enter takes the recommended option) resumes the turn and finishes it.
    ctl.terminal().write(b"\r");
    wait_for("the model to see the answer", TURN, || {
        model
            .since(0)
            .iter()
            .any(|r| fm::after_last_user(r).iter().any(|m| fm::role(m) == "tool"))
            .then_some(())
    });
    views.wait_both("idle after the answered question", Activity::Idle);

    // (e) A held `done`/`idle` never wedges the state: the next turn is busy again.
    let (sf, editor) = composer(&peek()).expect("composer");
    ctl.terminal()
        .send_tsp(&Event::Send {
            sf,
            id: editor,
            text: "STREAM-MARK once more".to_owned(),
        })
        .expect("send the prompt");
    views.wait_both("busy again", Activity::Busy);
    views.wait_both("idle at the end", Activity::Idle);
    let _ = ns.kill(SESSION);
    drop(ctl);
    deck_term::wait_detached(Duration::from_secs(10));
}
