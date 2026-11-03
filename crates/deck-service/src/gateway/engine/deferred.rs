//! Commands the gateway types into a session on its own schedule: `/reload-plugins` after a
//! plugin change (waits for omp to be idle) and `/resume <file>` with `force` (stops the running
//! turn first, then waits for omp to be idle). Both go through the session's controller like any
//! other injected command, so the idle checks, the draft confirmation and the TSP `send` apply.
//!
//! Requests typed here run under internal keys; their draft confirmations and ends reach the client
//! the work is for ([`Engine::deferred_owner`]), which may answer them. A client that cannot
//! answer any more (it left) never gets a command typed: nobody could confirm a draft, and an
//! unanswered confirmation would hold the session's injection queue.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::Engine;
use crate::controller::{
    Command, Delivery, Refusal, Rejected, RequestKey, RequestRecord, SessionKey, SessionTarget,
    Submission,
};
use crate::gateway::protocol::GatewayError;
use crate::session_actions::Action;
use crate::statusfmt::Activity;

/// The client of the requests typed on the gateway's own behalf.
const INTERNAL: &str = "~deferred";
/// How long a forced resume waits for the stopped turn to end.
pub(super) const STOP_WAIT: Duration = Duration::from_secs(10);
/// How long omp may take to report the turn an injected prompt started.
const START_GRACE: Duration = Duration::from_millis(1000);
/// A retry of something omp was not ready for.
const RETRY: Duration = Duration::from_millis(250);
/// A reload omp turned away (a turn started, a dialog opened) is offered again this many times.
const MAX_ATTEMPTS: u32 = 5;

/// What [`Mutation::ResumeHere`](crate::gateway::protocol::Mutation::ResumeHere) does for the
/// session's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResumePlan {
    /// omp is idle: type it now (the controller still checks the state when it starts).
    Send,
    /// A turn runs (or omp waits for the user) and the caller did not force it: refused, nothing
    /// journaled, so the client can ask the user and repeat with `force`.
    Refuse,
    /// Stop the turn (the running one, or the one an earlier command still being typed starts),
    /// wait for omp to be idle, then type it.
    StopThenSend,
    /// Wait for omp to be idle (it waits for the user, which a Stop does not answer), then type
    /// it.
    WaitThenSend,
}

/// `activity`: what omp is doing; `injecting`: the controller is still typing an earlier command.
pub(super) fn resume_plan(activity: Activity, injecting: bool, force: bool) -> ResumePlan {
    let working = matches!(activity, Activity::Busy | Activity::Attention) || injecting;
    match (working, force) {
        (false, _) => ResumePlan::Send,
        (true, false) => ResumePlan::Refuse,
        (true, true) if activity.is_busy() || injecting => ResumePlan::StopThenSend,
        (true, true) => ResumePlan::WaitThenSend,
    }
}

/// How a change to a session file reaches omp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// Type it into this gateway's own controller (index into the managed sessions).
    Inject(usize),
    /// Live control of this gateway's own controller.
    Ctl(usize),
    /// The control socket of a running session this gateway has no controller for.
    CtlNamed(String),
    /// omp's lease-checked headless helper (it refuses a session a running omp holds).
    Helper,
}

/// `/rename`: a controller that runs the file renames it by typing (like the status bar), any
/// other session through the helper.
pub(super) fn rename_route(controller: Option<usize>) -> Route {
    controller.map_or(Route::Helper, Route::Inject)
}

/// A label: live control of a controller that has it, else of another running session that has
/// a control socket, else the helper.
pub(super) fn label_route(controller: Option<(usize, bool)>, holder: Option<String>) -> Route {
    match (controller, holder) {
        (Some((i, true)), _) => Route::Ctl(i),
        (Some((_, false)), _) => Route::Helper,
        (None, Some(name)) => Route::CtlNamed(name),
        (None, None) => Route::Helper,
    }
}

/// A `/reload-plugins` omp turned away or never took: offered again (bounded).
fn reload_lost(delivery: &Delivery) -> bool {
    matches!(
        delivery,
        Delivery::Refused(
            Refusal::Precondition(_) | Refusal::DialogOpen | Refusal::NoBracketedPaste
        ) | Delivery::Aborted
            | Delivery::Stalled
    )
}

#[derive(Default)]
pub(super) struct Deferred {
    items: Vec<Item>,
    /// Internal requests in flight: the client they are for.
    owners: HashMap<RequestKey, String>,
    /// Reload requests in flight: what to offer again when omp turned them away.
    reloads: HashMap<RequestKey, (SessionKey, String, u32)>,
    /// Forced resumes handed to a controller: the internal key it runs under -> the journaled
    /// mutation it finishes.
    resumes: HashMap<RequestKey, RequestKey>,
    seq: u64,
}

struct Item {
    session: SessionKey,
    /// The client the command is for.
    owner: String,
    what: What,
}

enum What {
    Reload {
        attempts: u32,
    },
    /// A forced resume. `key`: its journaled mutation, tracked as a job until the command is
    /// handed to the controller.
    Resume {
        key: RequestKey,
        /// The session as admitted: a changed generation (restart, another file) ends the
        /// request as stale instead of resuming the replacement.
        target: SessionTarget,
        file: PathBuf,
        stage: Stage,
        deadline: Instant,
    },
}

enum Stage {
    /// The turn is still to be stopped. `grace`: an earlier command was being typed; the turn
    /// it starts may not be reported yet, so omp counts as idle only once this has passed.
    Stop { grace: Option<Instant> },
    /// The Stop request is being typed.
    Stopping(RequestKey),
    /// Waiting for omp to be idle.
    Idle,
}

impl Deferred {
    /// The client an internal request is for.
    pub(super) fn owner(&self, key: &RequestKey) -> Option<&str> {
        self.owners.get(key).map(String::as_str)
    }

    /// The internal requests typed for `client`.
    pub(super) fn owned(&self, client: &str) -> HashSet<RequestKey> {
        self.owners
            .iter()
            .filter(|(_, c)| c.as_str() == client)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// A reload for `session` is already waiting or being typed.
    fn reload_pending(&self, session: &SessionKey) -> bool {
        self.items
            .iter()
            .any(|i| i.session == *session && matches!(i.what, What::Reload { .. }))
            || self.reloads.values().any(|(s, _, _)| s == session)
    }
}

impl Engine {
    /// Types `/reload-plugins` into `target`'s session once it is idle, for `owner`'s
    /// confirmations. A session that is gone is dropped silently; one reload per session waits.
    pub(super) fn queue_reload(&mut self, target: &SessionTarget, owner: &str) {
        if target.epoch != self.epoch || self.sess.find(&target.key).is_none() {
            return;
        }
        if self.defer.reload_pending(&target.key) {
            return;
        }
        self.defer.items.push(Item {
            session: target.key.clone(),
            owner: owner.to_owned(),
            what: What::Reload { attempts: 0 },
        });
        self.waker.now();
    }

    /// A forced resume (the turn is stopped first, or an earlier command is awaited): `key` is
    /// the accepted mutation, tracked as a job until its command reaches the controller.
    pub(super) fn queue_forced_resume(
        &mut self,
        key: RequestKey,
        target: &SessionTarget,
        file: PathBuf,
        plan: ResumePlan,
    ) {
        let stage = match plan {
            ResumePlan::StopThenSend => Stage::Stop {
                grace: self
                    .sess
                    .find(&target.key)
                    .filter(|&i| self.sess.managed[i].ctl.is_busy())
                    .map(|_| Instant::now() + START_GRACE),
            },
            _ => Stage::Idle,
        };
        self.defer.items.push(Item {
            session: target.key.clone(),
            owner: key.client.clone(),
            what: What::Resume {
                key,
                target: target.clone(),
                file,
                stage,
                deadline: Instant::now() + STOP_WAIT,
            },
        });
        self.waker.now();
    }

    /// The client has a connection.
    fn client_connected(&self, client: &str) -> bool {
        self.conns.values().any(|c| c.client_id == client)
    }

    /// Types `command` into session `i` under a fresh internal key for `owner`.
    fn internal_submit(
        &mut self,
        i: usize,
        owner: &str,
        target: SessionTarget,
        command: Command,
    ) -> Result<RequestKey, Rejected> {
        self.defer.seq += 1;
        let key = RequestKey {
            client: INTERNAL.to_owned(),
            seq: self.defer.seq,
        };
        self.sess.managed[i].ctl.submit(Submission {
            key: key.clone(),
            target,
            command,
        })?;
        self.defer.owners.insert(key.clone(), owner.to_owned());
        self.dirty.controllers = true;
        self.waker.now();
        Ok(key)
    }

    /// One step of the waiting commands.
    pub(super) fn step_deferred(&mut self, _now: Instant) {
        if self.defer.items.is_empty() {
            return;
        }
        let items = std::mem::take(&mut self.defer.items);
        let mut keep: Vec<Item> = Vec::new();
        for item in items {
            if let Some(item) = self.step_item(item) {
                keep.push(item);
            }
        }
        // Items queued meanwhile (none today) stay behind the kept ones.
        keep.append(&mut self.defer.items);
        self.defer.items = keep;
    }

    /// `Some`: still waiting.
    fn step_item(&mut self, item: Item) -> Option<Item> {
        let Some(i) = self.sess.find(&item.session) else {
            // The tab went away: a reload is dropped silently, a resume reports it.
            if let What::Resume { key, .. } = item.what {
                self.job_failed(
                    key,
                    GatewayError::Refused("that session is closed".to_owned()),
                );
            }
            return None;
        };
        let gone = self.sess.managed[i].exited();
        let owner_here = self.client_connected(&item.owner);
        match item.what {
            What::Reload { attempts } => {
                if gone || !owner_here {
                    return None;
                }
                let m = &self.sess.managed[i];
                if m.ctl.activity() != Activity::Idle || m.ctl.is_busy() {
                    return Some(Item {
                        what: What::Reload { attempts },
                        ..item
                    });
                }
                let target = self.sess.managed[i].ctl.target().clone();
                match self.internal_submit(i, &item.owner, target, Command::ReloadPlugins) {
                    Ok(key) => {
                        self.defer
                            .reloads
                            .insert(key, (item.session, item.owner, attempts));
                        None
                    }
                    Err(Rejected::Busy | Rejected::NoBracketedPaste | Rejected::InputHeld) => {
                        self.wake_within(RETRY);
                        Some(Item {
                            what: What::Reload { attempts },
                            ..item
                        })
                    }
                    Err(_) => None,
                }
            }
            What::Resume {
                key,
                target,
                file,
                stage,
                deadline,
            } => {
                if gone {
                    self.job_failed(
                        key,
                        GatewayError::Failed("omp has exited in this session".to_owned()),
                    );
                    return None;
                }
                if !owner_here {
                    self.job_failed(key, GatewayError::Cancelled);
                    return None;
                }
                // The admitted session only: a restart or another file since then is stale.
                let current = self.sess.managed[i].ctl.target().clone();
                if current != target {
                    self.job_failed(key, GatewayError::Rejected(Rejected::Stale { current }));
                    return None;
                }
                let now = Instant::now();
                if now >= deadline {
                    self.job_failed(
                        key,
                        GatewayError::Failed(
                            "omp did not stop its turn in time: nothing was resumed".to_owned(),
                        ),
                    );
                    return None;
                }
                let m = &self.sess.managed[i];
                let (activity, injecting) = (m.ctl.activity(), m.ctl.is_busy());
                let stage = match stage {
                    Stage::Stop { .. } if injecting => Stage::Stop {
                        grace: Some(now + START_GRACE),
                    },
                    Stage::Stop { .. } if activity.is_busy() => {
                        match self.internal_submit(
                            i,
                            &item.owner,
                            target.clone(),
                            Command::Action(Action::Stop),
                        ) {
                            Ok(stop) => Stage::Stopping(stop),
                            Err(Rejected::Busy | Rejected::InputHeld) => {
                                Stage::Stop { grace: None }
                            }
                            Err(_) => Stage::Idle,
                        }
                    }
                    Stage::Stop { grace: Some(g) } if activity == Activity::Idle && now < g => {
                        Stage::Stop { grace: Some(g) }
                    }
                    Stage::Stop { .. } => Stage::Idle,
                    Stage::Stopping(stop) => {
                        let ended = m
                            .ctl
                            .request_by_key(&stop)
                            .is_none_or(|r| r.delivery.is_final());
                        if ended {
                            Stage::Idle
                        } else {
                            Stage::Stopping(stop)
                        }
                    }
                    Stage::Idle => Stage::Idle,
                };
                let m = &self.sess.managed[i];
                if matches!(stage, Stage::Idle)
                    && m.ctl.activity() == Activity::Idle
                    && !m.ctl.is_busy()
                {
                    // A fresh internal key: the client's own sequence number may be behind the
                    // controller's watermark by now. Its end finishes the journaled mutation.
                    match self.internal_submit(
                        i,
                        &item.owner,
                        target.clone(),
                        Command::Resume { file: file.clone() },
                    ) {
                        Ok(internal) => {
                            self.jobs.remove(&key);
                            self.dirty.gateway = true;
                            self.sess.submits.insert(internal.clone(), item.session);
                            self.defer.resumes.insert(internal, key);
                            return None;
                        }
                        Err(Rejected::Busy | Rejected::NoBracketedPaste | Rejected::InputHeld) => {}
                        Err(r) => {
                            self.job_failed(key, GatewayError::Rejected(r));
                            return None;
                        }
                    }
                }
                self.wake_within(RETRY);
                Some(Item {
                    what: What::Resume {
                        key,
                        target,
                        file,
                        stage,
                        deadline,
                    },
                    ..item
                })
            }
        }
    }

    /// The journaled mutation a finished internal request stands for (`key` itself for any
    /// other), with the record shown under that key.
    pub(super) fn journaled_key(&mut self, key: RequestKey) -> RequestKey {
        self.defer.owners.remove(&key);
        self.defer.resumes.remove(&key).unwrap_or(key)
    }

    /// A request ended: forgets an internal one's owner; a reload omp turned away is offered
    /// again.
    pub(super) fn deferred_request_ended(&mut self, record: &RequestRecord) {
        self.defer.owners.remove(&record.key);
        let Some((session, owner, attempts)) = self.defer.reloads.remove(&record.key) else {
            return;
        };
        if reload_lost(&record.delivery)
            && attempts + 1 < MAX_ATTEMPTS
            && self.sess.find(&session).is_some()
        {
            self.defer.items.push(Item {
                session,
                owner,
                what: What::Reload {
                    attempts: attempts + 1,
                },
            });
            self.waker.now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_goes_in_when_omp_is_idle_and_asks_before_it_touches_a_turn() {
        use Activity::{Attention, Busy, Idle, Unknown};
        use ResumePlan::*;
        for force in [false, true] {
            assert_eq!(resume_plan(Idle, false, force), Send, "force={force}");
            // omp has not reported a state: the controller decides when it starts.
            assert_eq!(resume_plan(Unknown, false, force), Send, "force={force}");
        }
        assert_eq!(resume_plan(Busy, false, false), Refuse);
        assert_eq!(resume_plan(Attention, false, false), Refuse);
        assert_eq!(resume_plan(Idle, true, false), Refuse);
        assert_eq!(resume_plan(Busy, false, true), StopThenSend);
        // An earlier command still being typed may start the turn: it is stopped once it did.
        assert_eq!(resume_plan(Busy, true, true), StopThenSend);
        assert_eq!(resume_plan(Idle, true, true), StopThenSend);
        // A Stop does not answer what omp waits for the user on.
        assert_eq!(resume_plan(Attention, false, true), WaitThenSend);
    }

    #[test]
    fn a_reload_omp_turned_away_is_offered_again() {
        for lost in [
            Delivery::Refused(Refusal::NoBracketedPaste),
            Delivery::Refused(Refusal::DialogOpen),
            Delivery::Aborted,
            Delivery::Stalled,
        ] {
            assert!(reload_lost(&lost), "{lost:?}");
        }
        assert!(!reload_lost(&Delivery::Delivered));
    }

    #[test]
    fn a_rename_is_typed_into_the_controller_that_runs_the_file_else_the_helper() {
        assert_eq!(rename_route(Some(2)), Route::Inject(2));
        assert_eq!(rename_route(None), Route::Helper);
    }

    #[test]
    fn a_label_uses_live_control_only_when_something_running_has_it() {
        assert_eq!(label_route(Some((1, true)), None), Route::Ctl(1));
        assert_eq!(
            label_route(Some((1, true)), Some("other".into())),
            Route::Ctl(1)
        );
        // The gateway runs the file without a control socket: omp's lease refuses the helper
        // with its own explanation.
        assert_eq!(
            label_route(Some((1, false)), Some("other".into())),
            Route::Helper
        );
        assert_eq!(
            label_route(None, Some("omp-abc".into())),
            Route::CtlNamed("omp-abc".into())
        );
        assert_eq!(label_route(None, None), Route::Helper);
    }
}
