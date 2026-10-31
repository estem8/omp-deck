//! A paired device deletes a session that is still running: the gateway stops it first, also when
//! no process holds the session file open. omp drops its writer handle on every atomic rewrite
//! (the title after the first turn) until its next append, so an idle session is often invisible
//! to the open-file map; it used to be left running and omp's session lease refused the delete.
//!
//! Real omp driven by a scripted model ([`support::fake_model`]) through the real relay and Noise
//! device path; private HOME/XDG tree, own `omp-deck-test-*` namespace; skipped when omp is not
//! installed.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_remote::api::{
    self, Activity, DoneView, Event, Feed, FeedValue, LiveRef, LiveSession, Op, OutcomeView,
    ReplyResult, Resp, SessionResult, Status,
};
use deck_service::gateway::client::Link;
use deck_service::gateway::protocol::{Mutation, Permissions, RelayLink, SecretText};
use support::device::{
    Device, OMP_CONFIG, Relay, SLICE, Sandbox, accepted, event, pair, run, wait_for, wait_mobile,
};
use support::fake_model::{self as fm, FakeModel, Reply};

const TURN: Duration = Duration::from_secs(90);

/// The device's view of one live session from its `live` feed.
struct Watch {
    live: LiveRef,
    latest: Option<LiveSession>,
    /// A feed value arrived without the session.
    gone: bool,
}

impl Watch {
    fn absorb(&mut self, d: &mut Device) {
        let live = self.live.clone();
        let (mut latest, mut gone) = (self.latest.take(), self.gone);
        d.records.retain(|r| match event(r) {
            Some(Event::Feed {
                value: FeedValue::Live { sessions, .. },
                ..
            }) => {
                match sessions.into_iter().find(|s| s.live == live) {
                    Some(s) => latest = Some(s),
                    None => gone = true,
                }
                false
            }
            _ => true,
        });
        self.latest = latest;
        self.gone = gone;
    }

    fn until(&mut self, d: &mut Device, what: &str, f: impl Fn(&Watch) -> bool) {
        let end = Instant::now() + TURN;
        loop {
            self.absorb(d);
            if f(self) {
                return;
            }
            assert!(
                Instant::now() < end,
                "timed out waiting for {what}; the newest live value: {:#?}",
                self.latest
            );
            d.poll(SLICE);
        }
    }

    fn idle(&self) -> bool {
        self.latest
            .as_ref()
            .is_some_and(|s| s.live_control && !s.busy && s.activity == Activity::Idle)
    }
}

/// The session file omp created in `sessions` (any but `seed`) and its header id.
fn created_session(sessions: &Path, seed: &str) -> (PathBuf, String) {
    let file = files_under(sessions)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .filter(|p| !p.to_string_lossy().contains(seed))
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .expect("omp created a session file");
    let text = std::fs::read_to_string(&file).expect("read the session file");
    let id = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "session")
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .expect("a session header");
    (file, id)
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(files_under(&p));
        }
        out.push(p);
    }
    out
}

#[test]
fn deleting_a_running_session_from_a_device_stops_it_first() {
    if which(if cfg!(windows) { "omp.exe" } else { "omp" }).is_none() {
        eprintln!("skipped: omp is not installed");
        return;
    }
    let config = format!("{OMP_CONFIG}modelRoles:\n  default: {}\n", fm::MODEL);
    let sb = Sandbox::new("rdd", &config);
    sb.report_failures();
    const SEED: &str = "seed-delete-running";
    sb.seed_session(SEED);
    let model = FakeModel::start(|req: &serde_json::Value| {
        if !fm::has_tools(req) {
            // A side request (the session title).
            return Reply::text("Fake title");
        }
        Reply::text("ok")
    });
    std::fs::write(sb.agent().join("models.yml"), model.models_yml()).expect("models.yml");
    let relay = Relay::start(sb.root.join("relay"));
    let phone = {
        let client = sb.client();
        wait_for("the gateway link", Duration::from_secs(30), || {
            matches!(client.link(), Link::Up(_)).then_some(())
        });
        run(
            &client,
            Mutation::RelaySet {
                url: relay.url(),
                room_key: SecretText::new(relay.key.encode()),
            },
        )
        .expect("relay set");
        wait_mobile(&client, "the relay link", |m| m.link == RelayLink::Up);
        pair(
            &client,
            &relay,
            "Phone",
            Permissions {
                control: true,
                manage: true,
            },
        )
    };
    let mut a = Device::connect(&relay, &phone, "Phone", true);
    let mut seq = a.welcome.next_seq;
    let project = api::project_id(sb.proj().to_string_lossy().as_bytes());

    // -- a session the device started, persisted by one turn, idle and still running --
    let launch = seq;
    seq += 1;
    assert!(!accepted(a.call(
        &mut [],
        Some(launch),
        Op::NewSession { project }
    )));
    let live = match a.outcome(&mut [], launch, TURN) {
        OutcomeView::Done {
            result: DoneView::Session { live },
        } => live,
        other => panic!("new_session: {other:?}"),
    };
    let r = a.call(
        &mut [],
        None,
        Op::Subscribe {
            feed: Feed::Live {},
        },
    );
    assert!(matches!(r, ReplyResult::Ok(Resp::Unit {})), "{r:?}");
    let mut w = Watch {
        live: live.clone(),
        latest: None,
        gone: false,
    };
    w.until(&mut a, "an idle live-control session", Watch::idle);
    let send = seq;
    seq += 1;
    assert!(!accepted(a.call(
        &mut [],
        Some(send),
        Op::Compose {
            live: live.clone(),
            text: "hello".into(),
            attachments: Vec::new(),
            mode: api::ComposeMode::Send,
        }
    )));
    a.outcome(&mut [], send, TURN);
    // The turn ends and omp rewrites the file with its title.
    w.until(&mut a, "the turn to finish", |w| {
        w.idle() && w.latest.as_ref().is_some_and(|s| s.title.is_some())
    });
    std::thread::sleep(Duration::from_secs(3));
    let sessions = sb.agent().join("sessions");
    let (file, id) = created_session(&sessions, SEED);

    // -- delete it while it runs --
    let delete = seq;
    assert!(!accepted(a.call(
        &mut [],
        Some(delete),
        Op::DeleteSessions {
            sessions: vec![id.clone()],
        }
    )));
    match a.outcome(&mut [], delete, TURN) {
        OutcomeView::Done {
            result: DoneView::Deleted { results },
        } => assert_eq!(
            results,
            vec![SessionResult {
                session: id.clone(),
                error: None
            }],
            "omp deleted the running session"
        ),
        other => panic!("delete_sessions: {other:?}"),
    }
    w.until(&mut a, "the session process to end", |w| {
        w.gone
            || w.latest
                .as_ref()
                .is_some_and(|s| matches!(s.status, Status::Exited { .. }))
    });
    let stem = file.with_extension("");
    let left: Vec<PathBuf> = files_under(&sessions)
        .into_iter()
        .filter(|p| p.starts_with(&stem) || *p == file)
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}
