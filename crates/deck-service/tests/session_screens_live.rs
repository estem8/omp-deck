//! The gateway side of OmpDeck's own session screens, with a real omp (18.15+) driven by a
//! scripted model, on a chat session of a real gateway:
//! - `ResumeHere` types `/resume <file>` while omp is idle, is turned away (`Busy`, nothing
//!   journaled) while a turn runs, and with `force` stops the turn, waits for idle and then
//!   resumes;
//! - `SessionRename` of a running session is typed into it (like the status bar);
//! - `SessionLabel` of a running session goes through live control and lands in the file;
//! - `SessionPin` shows in `Query::SessionPins`.
//!
//! Private HOME/XDG tree and own `omp-deck-test-*` namespace; skipped when omp is not installed.

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use deck_core::config::SessionView;
use deck_core::omp::PluginScope;
use deck_host::Namespace;
use deck_service::controller::{CommandKind, Delivery, Rejected};
use deck_service::gateway::client::{GatewayClient, Link};
use deck_service::gateway::protocol::{
    Answer, ControllerInfo, Done, GatewayError, LaunchSpec, Mutation, Outcome, Query, SessionRole,
    Topic, TopicValue,
};
use deck_service::statusfmt::Activity;
use serde_json::Value;
use support::device::{OMP_CONFIG, Sandbox, wait_for};
use support::fake_model::{self as fm, FakeModel, Reply};

const TURN: Duration = Duration::from_secs(60);
const STREAM_TIME: Duration = Duration::from_secs(12);
const SERIAL_NAME: &str = "scrn";

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

fn ensure_host_exe() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
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
    });
}

fn script() -> impl FnMut(&Value) -> Reply + Send + 'static {
    move |req| {
        if !fm::has_tools(req) {
            return Reply::text("Fake title");
        }
        if fm::last_user_text(req).contains("STREAM-MARK") {
            return Reply::slow_text("Streamed answer that takes a while to arrive.", STREAM_TIME);
        }
        Reply::text("ok")
    }
}

fn controller(
    client: &GatewayClient,
    session: &deck_service::controller::SessionKey,
) -> Option<ControllerInfo> {
    match client
        .query(Query::Topic(Topic::Controllers))
        .wait(Duration::from_secs(10))
    {
        Some(Ok(Answer::Topic(v))) => match *v {
            TopicValue::Controllers(c) => c.into_iter().find(|c| c.target.key == *session),
            _ => None,
        },
        _ => None,
    }
}

/// Runs `op`: its accepted answer, then (when accepted) the outcome.
fn run(client: &GatewayClient, op: Mutation) -> Result<Outcome, GatewayError> {
    let m = client.mutate(op);
    match m.accepted.wait(Duration::from_secs(30)) {
        Some(Ok(_)) => {}
        Some(Err(e)) => return Err(e),
        None => panic!("the mutation was not answered"),
    }
    match m.outcome.wait(Duration::from_secs(60)) {
        Some(Ok(o)) => Ok(o),
        other => panic!("no outcome: {other:?}"),
    }
}

fn wait_idle(
    client: &GatewayClient,
    session: &deck_service::controller::SessionKey,
) -> ControllerInfo {
    wait_for("an idle controller", TURN, || {
        controller(client, session).filter(|c| c.activity == Activity::Idle)
    })
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

fn entry_id(file: &Path, kind: &str) -> String {
    std::fs::read_to_string(file)
        .expect("session file")
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == kind && v["id"].is_string())
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("no {kind} entry in {}", file.display()))
}

#[test]
fn the_screens_mutations_reach_a_running_session() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    if which(if cfg!(windows) { "omp.exe" } else { "omp" }).is_none() {
        eprintln!("skipped: omp is not installed");
        return;
    }
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new(SERIAL_NAME, &config);
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
    let Ok(Outcome::Done(done)) = run(
        &client,
        Mutation::Launch(LaunchSpec {
            mux_name: Some("omp-screens".to_owned()),
            cwd: sb.proj(),
            session_path: None,
            label: "Chat".into(),
            argv: vec!["omp".into()],
            role: SessionRole::Session,
            copy_of: None,
            view: SessionView::Chat,
        }),
    ) else {
        panic!("launch")
    };
    let Done::Session(target, _) = *done else {
        panic!("launch result")
    };
    let name = wait_for("the host session", TURN, || ns.list().into_iter().next());
    wait_for("a sendable composer", TURN, || {
        deck_host::tsp_peek(&ns, &name)
            .ok()
            .filter(|doc| editor(doc).is_some())
            .map(|_| ())
    });
    wait_idle(&client, &target.key);

    // A prompt makes omp write the session file (and gives an entry to label).
    let Ok(Outcome::Done(done)) = run(
        &client,
        Mutation::Submit {
            target: target.clone(),
            command: deck_service::controller::Command::Prompt {
                text: "Reply with ok. first".to_owned(),
            },
        },
    ) else {
        panic!("prompt")
    };
    assert!(
        matches!(&*done, Done::Submitted(r) if r.delivery == Delivery::Delivered),
        "{done:?}"
    );
    let info = wait_for("the session file", TURN, || {
        controller(&client, &target.key).filter(|c| {
            c.activity == Activity::Idle && c.session_path.as_ref().is_some_and(|p| p.is_file())
        })
    });
    let file = info.session_path.clone().expect("session file");

    // Pins: the id of the session, in omp's own pins file.
    let id = file
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.rsplit('_').next())
        .expect("session id")
        .to_owned();
    let pins = |client: &GatewayClient| match client
        .query(Query::SessionPins)
        .wait(Duration::from_secs(20))
    {
        Some(Ok(Answer::SessionPins(p))) => p,
        other => panic!("pins: {other:?}"),
    };
    assert!(pins(&client).is_empty());
    let Ok(Outcome::Done(_)) = run(
        &client,
        Mutation::SessionPin {
            id: id.clone(),
            pinned: true,
        },
    ) else {
        panic!("pin")
    };
    assert!(pins(&client).contains(&id));
    let Ok(Outcome::Done(_)) = run(
        &client,
        Mutation::SessionPin {
            id: id.clone(),
            pinned: false,
        },
    ) else {
        panic!("unpin")
    };
    assert!(pins(&client).is_empty());

    // A running session's label goes through live control and lands in its file.
    let entry = entry_id(&file, "message");
    let Ok(Outcome::Done(done)) = run(
        &client,
        Mutation::SessionLabel {
            file: file.clone(),
            entry_id: entry.clone(),
            label: Some("Checkpoint".to_owned()),
        },
    ) else {
        panic!("label")
    };
    assert!(matches!(&*done, Done::Ctl(r) if r.ok), "{done:?}");
    wait_for("the label in the file", TURN, || {
        std::fs::read_to_string(&file)
            .ok()
            .filter(|t| t.contains("Checkpoint"))
            .map(|_| ())
    });
    // An entry omp does not know is omp's own refusal.
    let refused = run(
        &client,
        Mutation::SessionLabel {
            file: file.clone(),
            entry_id: "nosuchent".to_owned(),
            label: Some("x".to_owned()),
        },
    );
    assert!(
        matches!(refused, Ok(Outcome::Failed(_))) || refused.is_err(),
        "an unknown entry fails: {refused:?}"
    );

    // A blank title is turned away before anything runs; a real one is typed into the session.
    assert!(matches!(
        run(
            &client,
            Mutation::SessionRename {
                file: file.clone(),
                title: "  ".to_owned()
            }
        ),
        Err(GatewayError::Refused(_))
    ));
    let Ok(Outcome::Done(done)) = run(
        &client,
        Mutation::SessionRename {
            file: file.clone(),
            title: "Screens Renamed".to_owned(),
        },
    ) else {
        panic!("rename")
    };
    assert!(
        matches!(&*done, Done::Submitted(r) if r.delivery == Delivery::Delivered),
        "{done:?}"
    );

    // A second session file to resume into: this one with another id.
    let other = file.with_file_name("2026-01-01T00-00-00-000Z_resumedst.jsonl");
    let copy: Vec<String> = std::fs::read_to_string(&file)
        .expect("session file")
        .lines()
        .map(|l| match serde_json::from_str::<Value>(l) {
            Ok(mut v) if v["type"] == "session" => {
                v["id"] = Value::from("resumedst");
                v.to_string()
            }
            _ => l.to_owned(),
        })
        .collect();
    std::fs::write(&other, copy.join("\n") + "\n").expect("second session file");

    // A turn runs: resume is turned away before it is journaled, nothing was typed.
    let Ok(Outcome::Done(_)) = run(
        &client,
        Mutation::Submit {
            target: target.clone(),
            command: deck_service::controller::Command::Prompt {
                text: "STREAM-MARK one".to_owned(),
            },
        },
    ) else {
        panic!("stream prompt")
    };
    wait_for("a running turn", TURN, || {
        controller(&client, &target.key).filter(|c| c.activity == Activity::Busy)
    });
    let busy = run(
        &client,
        Mutation::ResumeHere {
            target: target.clone(),
            file: other.clone(),
            force: false,
        },
    );
    assert!(
        matches!(busy, Err(GatewayError::Rejected(Rejected::Busy))),
        "{busy:?}"
    );
    assert_eq!(
        controller(&client, &target.key).and_then(|c| c.session_path),
        Some(file.clone()),
        "nothing resumed while the turn runs"
    );
    // A plugin change while the turn runs: the change is made at once, `/reload-plugins` is
    // typed only once omp is idle.
    let plug = sb.root.join("plugin-src");
    std::fs::create_dir_all(plug.join("tools")).expect("plugin dir");
    std::fs::write(
        plug.join("package.json"),
        r#"{"name":"fakeplug","version":"1.0.0","omp":{"tools":"tools"}}"#,
    )
    .expect("plugin manifest");
    let omp_in_sandbox = |args: &[&str]| {
        std::process::Command::new(
            which(if cfg!(windows) { "omp.exe" } else { "omp" }).expect("omp"),
        )
        .args(args)
        .current_dir(sb.proj())
        .env("HOME", sb.root.join("home"))
        .env("PI_CODING_AGENT_DIR", sb.agent())
        .output()
        .expect("run omp")
    };
    let linked = omp_in_sandbox(&["plugin", "link", &plug.to_string_lossy()]);
    assert!(linked.status.success(), "{linked:?}");
    let reloads = |client: &GatewayClient| {
        controller(client, &target.key)
            .map(|c| {
                c.requests
                    .into_iter()
                    .filter(|r| r.kind == CommandKind::ReloadPlugins)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let Ok(Outcome::Done(_)) = run(
        &client,
        Mutation::PluginEnable {
            name: "fakeplug".to_owned(),
            enabled: false,
            scope: PluginScope::User,
            cwd: None,
            reload: Some(target.clone()),
        },
    ) else {
        panic!("plugin disable")
    };
    let listed = omp_in_sandbox(&["plugin", "list", "--json"]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("\"enabled\": false"),
        "the plugin is disabled now: {listed:?}"
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        controller(&client, &target.key).map(|c| c.activity),
        Some(Activity::Busy)
    );
    assert!(
        reloads(&client).is_empty(),
        "nothing is typed over a running turn"
    );
    let reload = wait_for("the reload once omp is idle", TURN, || {
        reloads(&client).into_iter().find(|r| r.delivery.is_final())
    });
    assert_eq!(reload.delivery, Delivery::Delivered, "{reload:?}");
    wait_idle(&client, &target.key);
    // A second turn for the forced resume.
    let Ok(Outcome::Done(_)) = run(
        &client,
        Mutation::Submit {
            target: target.clone(),
            command: deck_service::controller::Command::Prompt {
                text: "STREAM-MARK two".to_owned(),
            },
        },
    ) else {
        panic!("second stream prompt")
    };
    wait_for("a running turn", TURN, || {
        controller(&client, &target.key).filter(|c| c.activity == Activity::Busy)
    });
    // Forced: the turn is stopped, omp goes idle, then the resume is typed.
    let Ok(Outcome::Done(done)) = run(
        &client,
        Mutation::ResumeHere {
            target: target.clone(),
            file: other.clone(),
            force: true,
        },
    ) else {
        panic!("forced resume")
    };
    assert!(
        matches!(&*done, Done::Submitted(r) if r.delivery == Delivery::Delivered),
        "{done:?}"
    );
    let started = Instant::now();
    let moved = wait_for("the tab on the other session", TURN, || {
        controller(&client, &target.key)
            .filter(|c| c.session_path.as_deref() == Some(other.as_path()))
    });
    assert!(started.elapsed() < TURN);
    assert_eq!(moved.session_path.as_deref(), Some(other.as_path()));

    // A forced resume that arrives while an earlier prompt is still being delivered stops the
    // turn that prompt starts, and a later command of the same client (a higher sequence number
    // than the resume's) does not make the accepted resume expire.
    wait_idle(&client, &target.key);
    let now = moved.target.clone();
    let prompt = client.mutate(Mutation::Submit {
        target: now.clone(),
        command: deck_service::controller::Command::Prompt {
            text: "STREAM-MARK three".to_owned(),
        },
    });
    let resume = client.mutate(Mutation::ResumeHere {
        target: now.clone(),
        file: file.clone(),
        force: true,
    });
    let rename = client.mutate(Mutation::Submit {
        target: now.clone(),
        command: deck_service::controller::Command::Action(
            deck_service::session_actions::Action::Rename("After Resume".to_owned()),
        ),
    });
    assert!(matches!(
        resume.accepted.wait(Duration::from_secs(30)),
        Some(Ok(_))
    ));
    let outcome = resume.outcome.wait(TURN);
    assert!(
        matches!(&outcome, Some(Ok(Outcome::Done(d))) if matches!(&**d, Done::Submitted(r) if r.delivery == Delivery::Delivered)),
        "{outcome:?}"
    );
    wait_for("the tab back on the first session", TURN, || {
        controller(&client, &target.key)
            .filter(|c| c.session_path.as_deref() == Some(file.as_path()))
    });
    let _ = (prompt.outcome.wait(TURN), rename.outcome.wait(TURN));
}

/// What the composer holds, per the document.
fn draft(doc: &Value) -> Option<String> {
    Some(editor(doc)?["p"]["text"].as_str()?.to_owned())
}

/// The `model_change` and `thinking_level_change` entries of a session file, in order.
fn changes(file: &Path) -> Vec<Value> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["type"] == "model_change" || v["type"] == "thinking_level_change")
        .collect()
}

/// A model and an effort change from the picker, with a draft in omp's composer, go over live
/// control: no confirmation, nothing typed, the draft untouched, and omp's own entries in the
/// session file. An unknown model is omp's error, not typed text.
#[test]
fn model_and_effort_changes_leave_the_composer_draft_alone() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    if which(if cfg!(windows) { "omp.exe" } else { "omp" }).is_none() {
        eprintln!("skipped: omp is not installed");
        return;
    }
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("scrm", &config);
    sb.report_failures();
    ensure_host_exe();
    let model = FakeModel::start(script());
    // A second model that can think, to switch to.
    let models = model.models_yml()
        + "      - id: fake-2\n        name: Fake Two\n        reasoning: true\n        \
           contextWindow: 200000\n        maxTokens: 4096\n        input: [text, image]\n";
    std::fs::write(sb.agent().join("models.yml"), models).expect("models.yml");
    let ns = Namespace::new(&sb.ns).expect("namespace");
    let _cleanup = Cleanup(Namespace::new(&sb.ns).expect("namespace"));

    let client = sb.client();
    wait_for("the gateway link", Duration::from_secs(30), || {
        matches!(client.link(), Link::Up(_)).then_some(())
    });
    let Ok(Outcome::Done(done)) = run(
        &client,
        Mutation::Launch(LaunchSpec {
            mux_name: Some("omp-modelsw".to_owned()),
            cwd: sb.proj(),
            session_path: None,
            label: "Chat".into(),
            argv: vec!["omp".into()],
            role: SessionRole::Session,
            copy_of: None,
            view: SessionView::Chat,
        }),
    ) else {
        panic!("launch")
    };
    let Done::Session(target, _) = *done else {
        panic!("launch result")
    };
    let name = wait_for("the host session", TURN, || ns.list().into_iter().next());
    let peek = || deck_host::tsp_peek(&ns, &name).expect("tsp_peek");
    wait_for("a sendable composer", TURN, || editor(&peek()).map(|_| ()));
    wait_idle(&client, &target.key);
    let command = |c| Mutation::Submit {
        target: target.clone(),
        command: c,
    };
    // A prompt makes omp write the session file.
    let Ok(Outcome::Done(_)) = run(
        &client,
        command(deck_service::controller::Command::Prompt {
            text: "Reply with ok. model".to_owned(),
        }),
    ) else {
        panic!("prompt")
    };
    let info = wait_for("the session file", TURN, || {
        controller(&client, &target.key).filter(|c| {
            c.activity == Activity::Idle && c.session_path.as_ref().is_some_and(|p| p.is_file())
        })
    });
    let file = info.session_path.expect("session file");
    let before = changes(&file).len();

    // A draft in the composer.
    let Ok(Outcome::Done(_)) = run(
        &client,
        command(deck_service::controller::Command::Review {
            text: "my half-typed draft".to_owned(),
            send: false,
        }),
    ) else {
        panic!("insert the draft")
    };
    wait_for("the draft in the composer", TURN, || {
        (draft(&peek()).as_deref() == Some("my half-typed draft")).then_some(())
    });
    let requests = |client: &GatewayClient| {
        controller(client, &target.key)
            .map(|c| c.requests.len())
            .unwrap_or_default()
    };
    let typed_before = requests(&client);

    // The picker's model change: omp's own answer, no confirmation, nothing typed.
    let switched = run(
        &client,
        command(deck_service::controller::Command::SwitchModel {
            selector: "fake/fake-2".to_owned(),
            level: None,
        }),
    );
    let Ok(Outcome::Done(done)) = switched else {
        panic!("model switch: {switched:?}")
    };
    assert!(
        matches!(&*done, Done::Message(m) if m.contains("fake/fake-2")),
        "{done:?}"
    );
    assert_eq!(
        requests(&client),
        typed_before,
        "nothing was typed into omp"
    );
    assert_eq!(draft(&peek()).as_deref(), Some("my half-typed draft"));

    // The thinking level (`model:level`, as the picker's level chips send it): the same.
    let effort = run(
        &client,
        command(deck_service::controller::Command::SwitchModel {
            selector: "fake/fake-2".to_owned(),
            level: Some("low".to_owned()),
        }),
    );
    let Ok(Outcome::Done(done)) = effort else {
        panic!("effort: {effort:?}")
    };
    assert!(
        matches!(&*done, Done::Message(m) if m.contains("fake/fake-2")),
        "{done:?}"
    );
    assert_eq!(
        requests(&client),
        typed_before,
        "nothing was typed into omp"
    );
    assert_eq!(draft(&peek()).as_deref(), Some("my half-typed draft"));
    let entries = wait_for("omp's entries", TURN, || {
        let all = changes(&file);
        (all.len() >= before + 2).then_some(all)
    });
    let new = &entries[before..];
    assert!(
        new.iter().any(|e| e["type"] == "model_change"
            && e["model"] == "fake/fake-2"
            && e["role"] == "temporary"),
        "{new:?}"
    );
    assert!(
        new.iter()
            .any(|e| e["type"] == "thinking_level_change" && e["thinkingLevel"] == "low"),
        "{new:?}"
    );

    // omp's refusal is the error, and nothing is typed instead.
    let unknown = run(
        &client,
        command(deck_service::controller::Command::SwitchModel {
            selector: "fake/nope-9".to_owned(),
            level: None,
        }),
    );
    assert!(
        matches!(&unknown, Ok(Outcome::Failed(t)) if t.contains("Unknown model"))
            || unknown.is_err(),
        "{unknown:?}"
    );
    assert_eq!(requests(&client), typed_before);
    assert_eq!(draft(&peek()).as_deref(), Some("my half-typed draft"));
}
