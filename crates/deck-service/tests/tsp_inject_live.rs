//! Injected commands on a TSP (chat) session, with a real omp (18.15+) driven by a scripted model.
//!
//! Under TSP omp's composer is not on the terminal grid, so the injector reads it from the TSP
//! document and submits through the `send` event. On a TSP host session, owned the gateway's way
//! (a headless terminal joining the host, no view attached):
//! - a prompt reaches the model and the turn runs;
//! - a slash command (`/rename`) takes effect (omp's placeholder quotes the new title);
//! - an insert keeps omp's draft; a prompt over a draft asks first, and a "clear" answer sends only
//!   the prompt while declining leaves the draft alone;
//! - an idle-only command is refused while a turn runs, steering gets through, Stop ends the turn;
//! - while an `ask` question holds the composer, commands that need it are refused.
//!
//! Private HOME/XDG tree and own `omp-deck-test-*` namespace; skipped when omp is not installed.

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use deck_core::config::SessionView;
use deck_host::{Namespace, SessionSpec};
use deck_service::controller::{
    Command, Delivery, Refusal, Rejected, RequestId, RequestKey, SessionController, SessionKey,
    Submission,
};
use deck_service::gateway::client::Link;
use deck_service::gateway::protocol::{
    Answer, Done, LaunchSpec, Mutation, Query, SessionRole, Topic, TopicValue, ViewRole,
};
use deck_service::host_remote::HostRemote;
use deck_service::inject::Requires;
use deck_service::session_actions::Action;
use deck_service::statusfmt::Activity;
use deck_term::{RemoteEvent, TermOptions, Terminal};
use serde_json::{Value, json};
use support::device::{OMP_CONFIG, Sandbox, run};
use support::fake_model::{self as fm, FakeModel, Reply};

const STREAM_REPLY: &str = "Streamed answer: one two three four five six seven eight nine ten \
     eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty.";
const STREAM_TIME: Duration = Duration::from_secs(8);
const ASKED_REPLY: &str = "The question was answered.";
/// How long omp may take to show a surface, reach a state or finish a turn.
const TURN: Duration = Duration::from_secs(60);
const SESSION: &str = "tspinj1";

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

/// A [`Sandbox`] points the process's HOME/XDG variables at its own tree: the tests of this binary
/// take turns.
static SERIAL: Mutex<()> = Mutex::new(());

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
    // Both tests of the binary run in parallel: build and publish once.
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(build_host_exe);
}

fn build_host_exe() {
    let exe = if cfg!(windows) {
        "omp-deck-host.exe"
    } else {
        "omp-deck-host"
    };
    let beside = PathBuf::from(support::device::GATEWAY).with_file_name(exe);
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

/// The sendable editor node of the open inline surface.
fn editor(doc: &Value) -> Option<&Value> {
    fn find(n: &Value) -> Option<&Value> {
        if n["k"] == "editor" && n["p"]["sendable"] == true {
            return Some(n);
        }
        n["c"].as_array()?.iter().find_map(find)
    }
    doc["surfaces"]
        .as_array()?
        .iter()
        .filter(|s| s["mode"] == "inline" && s["open"] == true)
        .find_map(|s| find(&s["root"]))
}

/// What the composer holds, per the document.
fn draft(doc: &Value) -> Option<String> {
    Some(editor(doc)?["p"]["text"].as_str()?.to_owned())
}

/// The composer's placeholder as plain text (omp quotes the session title in it).
fn placeholder(doc: &Value) -> String {
    match &editor(doc).map(|e| &e["p"]["placeholder"]) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(spans)) => spans.iter().filter_map(|s| s["t"].as_str()).collect(),
        _ => String::new(),
    }
}

/// Whether the dock holds a node with `role`.
fn has_role(doc: &Value, role: &str) -> bool {
    fn find(n: &Value, role: &str) -> bool {
        n["p"]["role"] == role
            || n["c"]
                .as_array()
                .is_some_and(|c| c.iter().any(|c| find(c, role)))
    }
    doc["surfaces"].as_array().is_some_and(|s| {
        s.iter()
            .filter(|s| s["mode"] == "inline" && s["open"] == true)
            .any(|s| find(&s["root"], role))
    })
}

struct Rig<'a> {
    ctl: SessionController,
    ns: &'a Namespace,
    seq: u64,
}

impl Rig<'_> {
    fn peek(&self) -> Value {
        deck_host::tsp_peek(self.ns, SESSION).expect("tsp_peek")
    }

    fn try_submit(&mut self, command: Command) -> Result<RequestId, Rejected> {
        self.seq += 1;
        let target = self.ctl.target().clone();
        self.ctl
            .submit(Submission {
                key: RequestKey {
                    client: "inject-live".to_owned(),
                    seq: self.seq,
                },
                target,
                command,
            })
            .map(|s| s.id)
    }

    fn submit(&mut self, command: Command) -> RequestId {
        self.try_submit(command)
            .expect("the controller accepts the command")
    }

    fn delivery(&self, id: RequestId) -> Delivery {
        self.ctl
            .request(id)
            .expect("known request")
            .delivery
            .clone()
    }

    /// Pumps until the request reached `stop` (or any final state).
    fn pump_until(
        &mut self,
        id: RequestId,
        what: &str,
        stop: impl Fn(&Delivery) -> bool,
    ) -> Delivery {
        wait_for(what, TURN, || {
            self.ctl.tick(Instant::now());
            let d = self.delivery(id);
            (stop(&d) || d.is_final()).then_some(d)
        })
    }

    /// Submits and pumps until the request settled for good.
    fn run(&mut self, command: Command) -> Delivery {
        let id = self.submit(command);
        self.pump_until(id, "the command to settle", |_| false)
    }

    fn activity(&mut self) -> Activity {
        self.ctl.tick(Instant::now());
        self.ctl.activity()
    }

    fn wait_activity(&mut self, want: Activity) {
        wait_for(&format!("{want:?}"), TURN, || {
            (self.activity() == want).then_some(())
        });
    }

    /// Pastes `text` into omp's composer the way the user does, and waits until the document shows it.
    fn type_draft(&mut self, text: &str) {
        let bytes = self
            .ctl
            .terminal()
            .paste_bytes(text)
            .expect("omp enabled bracketed paste");
        self.ctl.terminal().write(&bytes);
        let want = text.to_owned();
        wait_for("the draft in the document", TURN, || {
            self.ctl.tick(Instant::now());
            (draft(&self.peek()).as_deref() == Some(want.as_str())).then_some(())
        });
    }
}

/// The composer's text once it differs from `before` (`None`: it did not change for a while, as
/// at the end of the recall history).
fn wait_for_change(rig: &mut Rig<'_>, before: &str) -> Option<String> {
    let end = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < end {
        rig.ctl.tick(Instant::now());
        if let Some(text) = draft(&rig.peek())
            && text != before
        {
            return Some(text);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    None
}

#[test]
fn injected_commands_reach_a_tsp_session() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(omp) = which(if cfg!(windows) { "omp.exe" } else { "omp" }) else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    // omp recalls drafts cleared with Ctrl+C by default (`composer.recallClearedDrafts`, true);
    // off, only a draft a `send` displaces would be in the recall history.
    let config = format!(
        "{OMP_CONFIG}composer:\n  recallClearedDrafts: false\nmodelRoles:\n  default: {}\n",
        fm::MODEL
    );
    let sb = Sandbox::new("tspinj", &config);
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
    wait_for("a sendable composer", TURN, || {
        editor(&deck_host::tsp_peek(&ns, SESSION).expect("tsp_peek")).map(|_| ())
    });

    // The gateway's way of owning a session's terminal: joining late, no view attached.
    let term = Terminal::connect_headless(
        Box::new(HostRemote::new(
            Namespace::new(&sb.ns).expect("namespace"),
            spec,
        )),
        TermOptions::default(),
        Arc::new(|_| {}),
    );
    let ctl = SessionController::new(SessionKey::host(&sb.ns, SESSION), term, None);
    let mut rig = Rig {
        ctl,
        ns: &ns,
        seq: 0,
    };
    wait_for("the replayed document", TURN, || {
        rig.ctl.terminal().tsp().live().then_some(())
    });
    rig.wait_activity(Activity::Idle);

    // (a) A prompt: a request at the model carrying the text (omp prepends its own reminder to the
    // first message); the turn runs and ends.
    let before = model.count();
    let d = rig.run(Command::Prompt {
        text: "Reply with ok. STREAM-MARK first".to_owned(),
    });
    assert_eq!(d, Delivery::Delivered, "the prompt was delivered");
    wait_for("the model to see the prompt", TURN, || {
        model
            .find(before, |r| {
                fm::last_user_text(r).ends_with("Reply with ok. STREAM-MARK first")
            })
            .map(|_| ())
    });
    rig.wait_activity(Activity::Busy);

    // (b) While the turn runs: an idle-only command is turned away without a byte; steering gets
    // through into the running turn.
    let n = model.count();
    let rejected = rig.try_submit(Command::Prompt {
        text: "second prompt".to_owned(),
    });
    assert_eq!(
        rejected,
        Err(Rejected::Busy),
        "a prompt is idle-only: the controller turns it away for a working omp"
    );
    assert!(
        has_role(&rig.peek(), "omp.working"),
        "the turn still runs after the refusal"
    );
    let steered = rig.run(Command::Steer {
        text: "steer: also say hello".to_owned(),
        follow_up: false,
    });
    assert_eq!(steered, Delivery::Delivered);
    wait_for("the steering text at the model", TURN, || {
        model
            .find(n, |r| {
                fm::last_user_text(r).contains("steer: also say hello")
            })
            .map(|_| ())
    });
    rig.wait_activity(Activity::Idle);

    // (c) Stop ends a turn long before its stream would.
    rig.run(Command::Prompt {
        text: "STREAM-MARK long one".to_owned(),
    });
    rig.wait_activity(Activity::Busy);
    let started = Instant::now();
    let stop = rig.run(Command::Action(Action::Stop));
    assert_eq!(stop, Delivery::Delivered);
    rig.wait_activity(Activity::Idle);
    assert!(
        started.elapsed() < STREAM_TIME - Duration::from_secs(2),
        "Esc interrupted the turn ({:?})",
        started.elapsed()
    );
    assert!(!has_role(&rig.peek(), "omp.working"));

    // (d) A slash command takes effect: omp quotes the new session title in the composer's
    // placeholder.
    let d = rig.run(Command::Action(Action::Rename("Renamed Chat".to_owned())));
    assert_eq!(d, Delivery::Delivered, "the rename was delivered");
    wait_for("the new title in the placeholder", TURN, || {
        rig.ctl.tick(Instant::now());
        placeholder(&rig.peek())
            .contains("Renamed Chat")
            .then_some(())
    });
    assert_eq!(draft(&rig.peek()).as_deref(), Some(""));

    // (e) Insert pastes at the cursor and keeps what omp already holds.
    rig.type_draft("my draft ");
    let d = rig.run(Command::Review {
        text: "added".to_owned(),
        send: false,
    });
    assert_eq!(d, Delivery::Delivered);
    wait_for("the inserted text after the draft", TURN, || {
        (draft(&rig.peek()).as_deref() == Some("my draft added")).then_some(())
    });

    // (f) A prompt over that draft asks first; declining writes nothing and leaves the draft.
    let n = model.count();
    let id = rig.submit(Command::Prompt {
        text: "clean prompt".to_owned(),
    });
    let d = rig.pump_until(id, "the draft confirmation", |d| {
        matches!(d, Delivery::AwaitingConfirm { .. })
    });
    assert_eq!(
        d,
        Delivery::AwaitingConfirm {
            draft: "my draft added".to_owned()
        }
    );
    let target = rig.ctl.target().clone();
    assert!(rig.ctl.answer(&target, id, false));
    assert_eq!(rig.delivery(id), Delivery::Declined);
    assert_eq!(draft(&rig.peek()).as_deref(), Some("my draft added"));
    assert_eq!(model.count(), n, "nothing reached the model");

    // Confirming clears the draft with Ctrl+C (the editor reads empty before the `send`, and the
    // draft is not displaced into omp's recall history) and sends only the prompt.
    let id = rig.submit(Command::Prompt {
        text: "clean prompt".to_owned(),
    });
    rig.pump_until(id, "the draft confirmation", |d| {
        matches!(d, Delivery::AwaitingConfirm { .. })
    });
    assert!(rig.ctl.answer(&target, id, true));
    let d = rig.pump_until(id, "the prompt to be delivered", |_| false);
    assert_eq!(d, Delivery::Delivered);
    wait_for("exactly the prompt at the model", TURN, || {
        model
            .find(n, |r| fm::last_user_text(r) == "clean prompt")
            .map(|_| ())
    });
    rig.wait_activity(Activity::Idle);
    assert_eq!(draft(&rig.peek()).as_deref(), Some(""));
    // Walk omp's recall history with Up in the empty composer: the prompt just sent is the
    // newest entry, older prompts follow, the cleared draft is nowhere in it.
    let mut recalled: Vec<String> = Vec::new();
    for _ in 0..12 {
        let before = draft(&rig.peek()).unwrap_or_default();
        rig.ctl.terminal().write(b"\x1b[A");
        let next = wait_for_change(&mut rig, &before);
        match next {
            Some(text) => recalled.push(text),
            None => break,
        }
    }
    assert_eq!(
        recalled.first().map(String::as_str),
        Some("clean prompt"),
        "Up recalls the newest entry: {recalled:?}"
    );
    assert!(
        recalled.len() >= 3,
        "older prompts are in recall too: {recalled:?}"
    );
    assert!(
        !recalled.iter().any(|t| t.contains("my draft added")),
        "the cleared draft must not be in recall: {recalled:?}"
    );
    // Ctrl+C leaves the recalled text: back to an empty composer for what follows.
    rig.ctl.terminal().write(b"\x03");
    wait_for("an empty composer", TURN, || {
        rig.ctl.tick(Instant::now());
        (draft(&rig.peek()).as_deref() == Some("")).then_some(())
    });

    // (g) An `ask` holds the composer: the question is an attention state, a prompt is refused
    // for it, and an insert (which needs the composer, not an idle omp) as a dialog.
    rig.run(Command::Prompt {
        text: "ASK-MARK ask me".to_owned(),
    });
    rig.wait_activity(Activity::Attention);
    wait_for("the ask dialog in the dock", TURN, || {
        has_role(&rig.peek(), "omp.editor.ask").then_some(())
    });
    let n = model.count();
    assert_eq!(
        rig.run(Command::Prompt {
            text: "during the ask".to_owned()
        }),
        Delivery::Refused(Refusal::Precondition(Requires::Idle))
    );
    assert_eq!(
        rig.run(Command::Review {
            text: "during the ask".to_owned(),
            send: false
        }),
        Delivery::Refused(Refusal::DialogOpen)
    );
    assert_eq!(model.count(), n, "nothing reached the model during the ask");
    // Answering (Enter takes the recommended option) resumes the turn and finishes it.
    rig.ctl.terminal().write(b"\r");
    wait_for("the model to see the answer", TURN, || {
        model
            .since(0)
            .iter()
            .any(|r| fm::after_last_user(r).iter().any(|m| fm::role(m) == "tool"))
            .then_some(())
    });
    rig.wait_activity(Activity::Idle);
    // Commands work again afterwards.
    assert_eq!(
        rig.run(Command::Prompt {
            text: "after the ask".to_owned()
        }),
        Delivery::Delivered
    );

    let _ = ns.kill(SESSION);
    drop(rig);
    deck_term::wait_detached(Duration::from_secs(10));
}

/// The gateway's controllers, as a query says now: the activity of `key`'s session.
fn gateway_activity(
    client: &deck_service::gateway::client::GatewayClient,
    key: &deck_service::controller::SessionKey,
) -> Option<Activity> {
    match client
        .query(Query::Topic(Topic::Controllers))
        .wait(Duration::from_secs(10))
    {
        Some(Ok(Answer::Topic(v))) => match *v {
            TopicValue::Controllers(c) => {
                c.iter().find(|c| c.target.key == *key).map(|c| c.activity)
            }
            _ => None,
        },
        _ => None,
    }
}

/// The same commands through a real gateway, on a chat session whose view is attached: the
/// owner's controller injects, the view receives the document updates the commands cause.
#[test]
fn gateway_commands_reach_a_chat_session_that_has_a_view_attached() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(_omp) = which(if cfg!(windows) { "omp.exe" } else { "omp" }) else {
        eprintln!("skipped: omp is not installed");
        return;
    };
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("tspinjg", &config);
    sb.report_failures();
    ensure_host_exe();
    let model = FakeModel::start(script());
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    let ns = Namespace::new(&sb.ns).expect("namespace");
    let _cleanup = Cleanup(Namespace::new(&sb.ns).expect("namespace"));

    let client = sb.client();
    wait_for("the gateway link", Duration::from_secs(30), || {
        matches!(client.link(), Link::Up(_)).then_some(())
    });
    let mux_name = "omp-chat-inj";
    let Done::Session(target, view) = run(
        &client,
        Mutation::Launch(LaunchSpec {
            mux_name: Some(mux_name.to_owned()),
            cwd: sb.proj(),
            session_path: None,
            label: "Chat".into(),
            argv: vec!["omp".into()],
            role: SessionRole::Session,
            copy_of: None,
            view: SessionView::Chat,
        }),
    )
    .expect("launch") else {
        panic!("launch result");
    };
    assert_eq!(view, SessionView::Chat);

    // The view: a controlling frontend of the session, drained into a byte log.
    let (mut rx, _tx) = client
        .frontend(target.clone(), ViewRole::Controlling)
        .split();
    let log = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = log.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = rx.recv() {
            match ev {
                RemoteEvent::Snapshot(b) | RemoteEvent::Output(b) => sink
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .extend_from_slice(&b),
                RemoteEvent::Closed => return,
                _ => {}
            }
        }
    });
    let seen = |needle: &str| {
        let log = log.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&log).contains(needle)
    };
    let name = wait_for("the host session", TURN, || ns.list().into_iter().next());
    let peek = || deck_host::tsp_peek(&ns, &name).expect("tsp_peek");
    wait_for("a sendable composer", TURN, || editor(&peek()).map(|_| ()));
    wait_for("an idle controller", TURN, || {
        (gateway_activity(&client, &target.key) == Some(Activity::Idle)).then_some(())
    });

    let submit = |command: Command| match run(
        &client,
        Mutation::Submit {
            target: target.clone(),
            command,
        },
    )
    .expect("the command ran")
    {
        Done::Submitted(rec) => rec.delivery,
        other => panic!("submit result: {other:?}"),
    };

    // A prompt: the model gets it and the attached view receives the user's card.
    let d = submit(Command::Prompt {
        text: "Reply with ok. view prompt".to_owned(),
    });
    assert_eq!(d, Delivery::Delivered);
    wait_for("the model to see the prompt", TURN, || {
        model
            .find(0, |r| {
                fm::last_user_text(r).ends_with("Reply with ok. view prompt")
            })
            .map(|_| ())
    });
    wait_for("the view to receive the prompt", TURN, || {
        seen("Reply with ok. view prompt").then_some(())
    });
    wait_for("an idle controller", TURN, || {
        (gateway_activity(&client, &target.key) == Some(Activity::Idle)).then_some(())
    });

    // A slash command: omp quotes the new title in the composer's placeholder.
    let d = submit(Command::Action(Action::Rename("View Renamed".to_owned())));
    assert_eq!(d, Delivery::Delivered);
    wait_for("the new title in the placeholder", TURN, || {
        placeholder(&peek()).contains("View Renamed").then_some(())
    });
    wait_for("the view to receive the new title", TURN, || {
        seen("View Renamed").then_some(())
    });
}
