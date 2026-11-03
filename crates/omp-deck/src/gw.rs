//! The desktop's view of the headless gateway: the one module that knows the shape of the client
//! cache and of request completion.
//!
//! The GUI never executes core operations. Everything it asks for is a query or a mutation sent
//! through [`Gw`]; the answer arrives as a closure run on the UI thread by [`Gw::pump`] (once per
//! frame, before the frame's own work), so a request can be fired from any UI code and its result
//! handled where it was asked for. Nothing blocks: the client's threads do the I/O and wake egui.
//!
//! Semantics kept visible to callers:
//! - A mutation's closure runs once, with the final [`Done`] or the error text. A mutation that is
//!   not accepted (offline, stale epoch, refused) fails at once; an accepted one whose link breaks
//!   is looked up again after the reconnect and is never sent twice.
//! - [`Gw::submit`] additionally reports the controller's [`Submitted`] id once the gateway has
//!   accepted the command: until then the request is only locally pending.
//! - Closing the window drops these closures; accepted work belongs to the gateway.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use deck_core::agents::{AgentsSnapshot, AgentsTarget};
use deck_core::config::{AppConfig, GatewayMode};
use deck_core::fleet::{FleetSnapshot, FleetTarget};
use deck_core::sessions::ProjectGroup;
use deck_core::status::SessionStatus;
use deck_service::controller::{Command, RequestKey, SessionTarget, Submitted};
use deck_service::gateway::client::{
    ClientEvent, GatewayClient, GatewayOptions, Link, Pending, RemoteSettings,
};
use deck_service::gateway::protocol::{
    Accepted, Answer, AppUpdateView, BackupView, ClientState, ControllerInfo, Done, GatewayError,
    GatewayStatus, InboxView, MobileView, ModelFactsView, Mutation, OmpUpdateView, Outcome,
    Permissions, Progress, Query, SecretText, Topic, TopicValue, UsageView, ViewRole,
};
use deck_service::gateway::running::RunningSnapshot;

use crate::app::App;

/// Work for the UI thread produced by a completed request.
pub type Job = Box<dyn FnOnce(&mut App)>;

/// `None` = not ready; `Some(None)` = finished without work for the UI thread.
type Poll = Box<dyn FnMut() -> Option<Option<Job>>>;
type DoneFn = Box<dyn FnOnce(&mut App, Result<Done, String>)>;
type ProgressFn = Rc<RefCell<dyn FnMut(&mut App, &Progress)>>;
type AcceptedFn = Box<dyn FnOnce(&mut App, RequestKey, Result<Submitted, GatewayError>)>;

/// What the gateway answered to opening a pairing window. The invite itself is dropped (and
/// wiped) on arrival: only the link the QR code carries is kept.
pub struct PairingStarted {
    pub pairing: String,
    pub link: SecretText,
    pub expires_unix_ms: u64,
}

/// A final outcome as the closure's result. Outcomes that kept no result say so.
fn outcome_result(o: Outcome) -> Result<Done, String> {
    match o {
        Outcome::Done(d) => Ok(*d),
        Outcome::Failed(s) => Err(s),
        Outcome::Rejected(r) => Err(r.to_string()),
        Outcome::Cancelled => Err(GatewayError::Cancelled.to_string()),
        Outcome::Uncertain => {
            Err("the gateway stopped before this finished; it may or may not have run".to_owned())
        }
        Outcome::Recorded { succeeded: true } => Err(
            "this finished under an earlier gateway instance; its result is no longer available"
                .to_owned(),
        ),
        Outcome::Recorded { succeeded: false } => {
            Err("this failed under an earlier gateway instance".to_owned())
        }
        Outcome::Running => Err("still running".to_owned()),
    }
}

struct Tracked {
    done: DoneFn,
    progress: Option<ProgressFn>,
    /// The gateway accepted (journaled) it: it may run even if this window goes away.
    accepted: bool,
}

struct AcceptPoll {
    key: RequestKey,
    pending: Pending<Result<Accepted, GatewayError>>,
    on_accepted: Option<AcceptedFn>,
}

/// What [`Gw::pump`] hands the app: closures to run, and the events only the app can interpret.
pub struct Pumped {
    pub jobs: Vec<Job>,
    pub events: Vec<ClientEvent>,
}

/// The link in the words the UI shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkView {
    Connecting,
    Up,
    /// Not connected (the client keeps trying): everything shown is stale and read-only.
    Offline(String),
    /// The running gateway serves another profile / protocol.
    Refused(String),
    /// OmpDeck is being updated or removed.
    Maintenance,
}

pub struct Gw {
    client: GatewayClient,
    ctx: egui::Context,
    polls: Vec<Poll>,
    accepts: Vec<AcceptPoll>,
    tracked: HashMap<RequestKey, Tracked>,
    /// The link came (back) up or the event queue overflowed: look up accepted requests again.
    relookup: bool,
    link: Link,
    fleet_cache: RefCell<(u64, Option<Arc<FleetSnapshot>>)>,
    agents_cache: RefCell<(u64, Option<Arc<AgentsSnapshot>>)>,
    settings: RemoteSettings,
}

impl Gw {
    pub fn start(ctx: &egui::Context, opts: GatewayOptions) -> Gw {
        let client = GatewayClient::start(opts);
        // Interest-driven topics: idle until this client sets targets.
        client.subscribe(Topic::Fleet);
        client.subscribe(Topic::Agents);
        let settings = client.settings();
        Gw {
            link: client.link(),
            client,
            ctx: ctx.clone(),
            polls: Vec::new(),
            accepts: Vec::new(),
            tracked: HashMap::new(),
            relookup: false,
            fleet_cache: RefCell::new((u64::MAX, None)),
            agents_cache: RefCell::new((u64::MAX, None)),
            settings,
        }
    }

    /// The link's state in the words the UI shows.
    pub fn link_view(&self) -> LinkView {
        match &self.link {
            Link::Connecting => LinkView::Connecting,
            Link::Up(_) => LinkView::Up,
            Link::Refused(r) => LinkView::Refused(r.to_string()),
            Link::Down { error } => LinkView::Offline(error.clone()),
            Link::Maintenance(_) => LinkView::Maintenance,
        }
    }

    pub fn is_up(&self) -> bool {
        matches!(self.link, Link::Up(_))
    }

    pub fn settings(&self) -> &RemoteSettings {
        &self.settings
    }

    /// The gateway lifetime lifecycle transaction (never a generic settings edit).
    pub fn set_gateway_mode(
        &self,
        mode: GatewayMode,
    ) -> Pending<Result<GatewayStatus, GatewayError>> {
        self.settings.set_gateway_mode(mode)
    }

    /// Settings flushed and acknowledged (before an update restart or exit).
    pub fn flush(&self) -> Pending<Result<(), GatewayError>> {
        self.client.flush()
    }

    // ------------------------------------------------------------------ requests

    /// Runs `f` with the answer of `q`.
    pub fn query(&mut self, q: Query, f: impl FnOnce(&mut App, Result<Answer, String>) + 'static) {
        let pending = self.client.query(q);
        let mut f = Some(f);
        self.polls.push(Box::new(move || {
            let r = pending.try_take()?;
            let f = f.take()?;
            let r = r.map_err(|e| e.to_string());
            Some(Some(Box::new(move |app: &mut App| f(app, r)) as Job))
        }));
        self.ctx.request_repaint();
    }

    /// Sends `op`; `done` runs once with its final result.
    pub fn mutate(
        &mut self,
        op: Mutation,
        done: impl FnOnce(&mut App, Result<Done, String>) + 'static,
    ) -> RequestKey {
        let m = self.client.mutate(op);
        self.track(m.key, m.accepted, Box::new(done), None, None)
    }

    /// Like [`Gw::mutate`], reporting progress of a long job too.
    pub fn mutate_with_progress(
        &mut self,
        op: Mutation,
        progress: impl FnMut(&mut App, &Progress) + 'static,
        done: impl FnOnce(&mut App, Result<Done, String>) + 'static,
    ) -> RequestKey {
        let m = self.client.mutate(op);
        self.track(
            m.key,
            m.accepted,
            Box::new(done),
            Some(Rc::new(RefCell::new(progress))),
            None,
        )
    }

    // ------------------------------------------------------------------ mobile access

    /// The gateway's mobile-access record: relay, link state, devices, open pairing.
    pub fn mobile(&self) -> Option<Arc<MobileView>> {
        self.client.state().mobile.get()
    }

    /// A mutation whose answer carries nothing: `done` gets the error text, if any.
    fn mutate_unit(
        &mut self,
        op: Mutation,
        done: impl FnOnce(&mut App, Result<(), String>) + 'static,
    ) {
        self.mutate(op, move |app, r| done(app, r.map(|_| ())));
    }

    /// Configures the relay (the gateway validates the URL and the room key).
    pub fn relay_set(
        &mut self,
        url: String,
        room_key: SecretText,
        done: impl FnOnce(&mut App, Result<(), String>) + 'static,
    ) {
        self.mutate_unit(Mutation::RelaySet { url, room_key }, done);
    }

    pub fn relay_clear(&mut self, done: impl FnOnce(&mut App, Result<(), String>) + 'static) {
        self.mutate_unit(Mutation::RelayClear, done);
    }

    /// Restarts the relay link now (also takes the room back after another gateway replaced it).
    pub fn relay_reconnect(&mut self, done: impl FnOnce(&mut App, Result<(), String>) + 'static) {
        self.mutate_unit(Mutation::RelayReconnect, done);
    }

    /// Opens the pairing window. The invite never leaves this call: only the link (the QR code's
    /// payload) is handed on, as [`SecretText`] so it is wiped when dropped.
    pub fn pairing_begin(
        &mut self,
        done: impl FnOnce(&mut App, Result<PairingStarted, String>) + 'static,
    ) {
        self.mutate(Mutation::PairingBegin, move |app, r| {
            let r = match r {
                Ok(Done::PairingStarted {
                    pairing,
                    invite: _,
                    link,
                    expires_unix_ms,
                }) => Ok(PairingStarted {
                    pairing,
                    link,
                    expires_unix_ms,
                }),
                Ok(_) => Err("unexpected reply from the gateway".to_owned()),
                Err(e) => Err(e),
            };
            done(app, r);
        });
    }

    pub fn pairing_cancel(&mut self, done: impl FnOnce(&mut App, Result<(), String>) + 'static) {
        self.mutate_unit(Mutation::PairingCancel, done);
    }

    pub fn pairing_approve(
        &mut self,
        pairing: String,
        permissions: Permissions,
        done: impl FnOnce(&mut App, Result<(), String>) + 'static,
    ) {
        self.mutate_unit(
            Mutation::PairingApprove {
                pairing,
                permissions,
            },
            done,
        );
    }

    pub fn pairing_deny(
        &mut self,
        pairing: String,
        done: impl FnOnce(&mut App, Result<(), String>) + 'static,
    ) {
        self.mutate_unit(Mutation::PairingDeny { pairing }, done);
    }

    pub fn device_revoke(
        &mut self,
        id: String,
        done: impl FnOnce(&mut App, Result<(), String>) + 'static,
    ) {
        self.mutate_unit(Mutation::DeviceRevoke { id }, done);
    }

    pub fn device_rename(
        &mut self,
        id: String,
        name: String,
        done: impl FnOnce(&mut App, Result<(), String>) + 'static,
    ) {
        self.mutate_unit(Mutation::DeviceRename { id, name }, done);
    }

    pub fn device_set_permissions(
        &mut self,
        id: String,
        permissions: Permissions,
        done: impl FnOnce(&mut App, Result<(), String>) + 'static,
    ) {
        self.mutate_unit(Mutation::DeviceSetPermissions { id, permissions }, done);
    }

    /// Sends a command to a session's controller. `accepted` runs when the gateway accepted it
    /// (with the controller's request id) or refused it; `done` when it reached its final result.
    /// Between the call and `accepted` the request is only locally pending.
    pub fn submit(
        &mut self,
        target: SessionTarget,
        command: Command,
        accepted: impl FnOnce(&mut App, RequestKey, Result<Submitted, GatewayError>) + 'static,
        done: impl FnOnce(&mut App, Result<Done, String>) + 'static,
    ) -> RequestKey {
        let m = self.client.submit(target, command);
        self.track(
            m.key,
            m.accepted,
            Box::new(done),
            None,
            Some(Box::new(accepted)),
        )
    }

    fn track(
        &mut self,
        key: RequestKey,
        pending: Pending<Result<Accepted, GatewayError>>,
        done: DoneFn,
        progress: Option<ProgressFn>,
        on_accepted: Option<AcceptedFn>,
    ) -> RequestKey {
        self.tracked.insert(
            key.clone(),
            Tracked {
                done,
                progress,
                accepted: false,
            },
        );
        self.accepts.push(AcceptPoll {
            key: key.clone(),
            pending,
            on_accepted,
        });
        self.ctx.request_repaint();
        key
    }

    /// Asks the gateway to cancel an accepted job; `done` runs once the gateway answered
    /// (an error when it refused, e.g. a job of another client, or the job already ended).
    pub fn cancel(
        &mut self,
        key: RequestKey,
        done: impl FnOnce(&mut App, Result<(), String>) + 'static,
    ) {
        self.mutate_unit(Mutation::CancelJob(key), done);
    }

    // ------------------------------------------------------------------ per frame

    /// Completed requests and the events the app interprets itself.
    pub fn pump(&mut self) -> Pumped {
        let mut jobs: Vec<Job> = Vec::new();
        let mut events = Vec::new();
        self.link = self.client.link();

        // Acceptance first: an outcome event for a request is only meaningful after it.
        let mut i = 0;
        while i < self.accepts.len() {
            let Some(r) = self.accepts[i].pending.try_take() else {
                i += 1;
                continue;
            };
            let a = self.accepts.swap_remove(i);
            match r {
                Ok(acc) => {
                    if let Some(t) = self.tracked.get_mut(&a.key) {
                        t.accepted = true;
                    }
                    if let Some(cb) = a.on_accepted {
                        let id = acc.submitted.ok_or_else(|| {
                            GatewayError::Failed("the gateway returned no request id".to_owned())
                        });
                        let key = a.key.clone();
                        jobs.push(Box::new(move |app: &mut App| cb(app, key, id)));
                    }
                }
                Err(e) => {
                    // Refused: it never ran.
                    let text = e.to_string();
                    if let Some(cb) = a.on_accepted {
                        let key = a.key.clone();
                        jobs.push(Box::new(move |app: &mut App| cb(app, key, Err(e))));
                    }
                    if let Some(t) = self.tracked.remove(&a.key) {
                        jobs.push(Box::new(move |app: &mut App| (t.done)(app, Err(text))));
                    }
                }
            }
        }

        for ev in self.client.events() {
            match ev {
                ClientEvent::Outcome { key, outcome } => {
                    if outcome.is_final()
                        && let Some(t) = self.tracked.remove(&key)
                    {
                        let r = outcome_result(outcome);
                        jobs.push(Box::new(move |app: &mut App| (t.done)(app, r)));
                    }
                }
                ClientEvent::Progress { key, progress } => {
                    if let Some(p) = self.tracked.get(&key).and_then(|t| t.progress.clone()) {
                        jobs.push(Box::new(move |app: &mut App| {
                            (p.borrow_mut())(app, &progress)
                        }));
                    }
                }
                ClientEvent::Link(_) | ClientEvent::Resync => {
                    self.relookup = true;
                    events.push(ev);
                }
                other => events.push(other),
            }
        }

        if self.relookup && matches!(self.link, Link::Up(_)) {
            self.relookup = false;
            let keys: Vec<RequestKey> = self
                .tracked
                .iter()
                .filter(|(_, t)| t.accepted)
                .map(|(k, _)| k.clone())
                .collect();
            for key in keys {
                let lookup = self.client.outcome(key.clone());
                self.polls.push(Box::new(move || {
                    let r = lookup.try_take()?;
                    let key = key.clone();
                    Some(Some(
                        Box::new(move |app: &mut App| app.gw_lookup(&key, r)) as Job
                    ))
                }));
            }
        }

        let mut i = 0;
        while i < self.polls.len() {
            match (self.polls[i])() {
                Some(job) => {
                    jobs.extend(job);
                    drop(self.polls.swap_remove(i));
                }
                None => i += 1,
            }
        }
        Pumped { jobs, events }
    }

    /// A lookup of an accepted request returned: a final record settles it; a running one keeps
    /// waiting for its event; a lost link waits for the next reconnect; anything else fails it.
    pub fn settle_lookup(
        &mut self,
        key: &RequestKey,
        r: Result<Outcome, GatewayError>,
    ) -> Option<Job> {
        match r {
            Ok(o) if o.is_final() => {
                let t = self.tracked.remove(key)?;
                let r = outcome_result(o);
                Some(Box::new(move |app: &mut App| (t.done)(app, r)))
            }
            Ok(_) | Err(GatewayError::Disconnected) => None,
            Err(e) => {
                let t = self.tracked.remove(key)?;
                let e = e.to_string();
                Some(Box::new(move |app: &mut App| (t.done)(app, Err(e))))
            }
        }
    }

    // ------------------------------------------------------------------ client state

    pub fn set_client_state(&self, s: ClientState) {
        self.client.set_client_state(s);
    }

    pub fn set_agents_target(&self, t: Option<AgentsTarget>) {
        self.client.set_agents_target(t);
    }

    pub fn set_fleet_targets(&self, t: Vec<FleetTarget>) {
        self.client.set_fleet_targets(t);
    }

    /// What the status bar shows about a session's model. The gateway owns the optimistic switch
    /// state: a switch that did not take comes back as `ClientEvent::ModelUnconfirmed`.
    pub fn observe_model(&self, target: SessionTarget, facts: ModelFactsView) {
        self.client.observe_model(target, facts);
    }

    pub fn subscribe(&self, t: Topic) {
        self.client.subscribe(t);
    }

    pub fn unsubscribe(&self, t: &Topic) {
        self.client.unsubscribe(t);
    }

    /// An own view of `target` for a native terminal.
    pub fn frontend(&self, target: SessionTarget, view: ViewRole) -> Box<dyn deck_term::Frontend> {
        self.client.frontend(target, view)
    }

    // ------------------------------------------------------------------ cache reads

    pub fn gateway(&self) -> Option<Arc<GatewayStatus>> {
        self.client.state().gateway.get()
    }

    /// The committed settings and their revision.
    pub fn config(&self) -> Option<(u64, Arc<AppConfig>)> {
        let s = self.settings.snapshot()?;
        Some((s.revision, s.config))
    }

    pub fn groups(&self) -> Option<Arc<Vec<ProjectGroup>>> {
        Some(self.client.state().sessions.get()?.groups.clone())
    }

    pub fn sessions_generation(&self) -> u64 {
        self.client
            .state()
            .sessions
            .get()
            .map_or(0, |s| s.generation)
    }

    pub fn sessions_root_exists(&self) -> bool {
        self.client
            .state()
            .sessions
            .get()
            .is_none_or(|s| s.root_exists)
    }

    /// The latest snapshot of the running sessions; `None` before the first one. A blind one
    /// (the gateway's backends could not be polled) carries no information: resolve with
    /// [`RunningSnapshot::resolve_for`] and [`Gw::running_blind`].
    pub fn running(&self) -> Option<Arc<RunningSnapshot>> {
        self.client.state().running.get()
    }

    pub fn running_blind(&self) -> bool {
        self.client.state().running.get().is_some_and(|r| r.blind)
    }

    pub fn controllers(&self) -> Option<Arc<Vec<ControllerInfo>>> {
        self.client.state().controllers.get()
    }

    pub fn controller(&self, target: &SessionTarget) -> Option<ControllerInfo> {
        self.controllers()?
            .iter()
            .find(|c| &c.target == target)
            .cloned()
    }

    pub fn inbox(&self) -> Option<Arc<InboxView>> {
        self.client.state().notifications.get()
    }

    /// The status (omp's context, tokens, model …) of the session the gateway follows for this
    /// client's active tab, with that session's target.
    pub fn status(&self) -> Option<(SessionTarget, SessionStatus)> {
        self.client.state().status.get()?.as_ref().clone()
    }

    /// [`Gw::status`] when it belongs to `target`'s session.
    pub fn status_of(&self, target: &SessionTarget) -> Option<SessionStatus> {
        let (t, s) = self.status()?;
        (t.key == target.key).then_some(s)
    }

    pub fn usage(&self) -> Option<Arc<UsageView>> {
        self.client.state().usage.get()
    }

    pub fn app_update(&self) -> Option<Arc<AppUpdateView>> {
        self.client.state().app_update.get()
    }

    pub fn omp_update(&self) -> Option<Arc<OmpUpdateView>> {
        self.client.state().omp_update.get()
    }

    pub fn backup(&self) -> Option<Arc<BackupView>> {
        self.client.state().backup.get()
    }

    /// The dashboard's fleet snapshot (unwrapped once per gateway revision, not per frame).
    pub fn fleet(&self) -> Option<Arc<FleetSnapshot>> {
        let rev = self.revision(&Topic::Fleet);
        let mut c = self.fleet_cache.borrow_mut();
        if c.0 != rev {
            let snap = self
                .client
                .state()
                .fleet
                .get()
                .and_then(|a| (*a).clone().map(Arc::new));
            *c = (rev, snap);
        }
        c.1.clone()
    }

    /// The active session's subagents (unwrapped once per gateway revision).
    pub fn agents(&self) -> Option<Arc<AgentsSnapshot>> {
        let rev = self.revision(&Topic::Agents);
        let mut c = self.agents_cache.borrow_mut();
        if c.0 != rev {
            let snap = self
                .client
                .state()
                .agents
                .get()
                .and_then(|a| (*a).clone().map(Arc::new));
            *c = (rev, snap);
        }
        c.1.clone()
    }

    /// The cached value of an interest-driven topic (`Live`, `Transcript`, `Inventory`,
    /// `Changes`, `Tree`); the gateway pushes it while this client is subscribed.
    pub fn topic(&self, t: &Topic) -> Option<TopicValue> {
        self.client.state().topic(t)
    }

    /// The revision of a topic (a cheap "did it change" test for caches).
    pub fn revision(&self, topic: &Topic) -> u64 {
        self.client.state().revision(topic)
    }
}

impl Gw {
    /// Runs blocking native `work` (a folder picker, a filesystem probe) on its own named thread;
    /// `done` runs on the UI thread with its result. The UI thread never waits for it.
    pub fn native<R: Send + 'static>(
        &mut self,
        work: impl FnOnce() -> R + Send + 'static,
        done: impl FnOnce(&mut App, R) + 'static,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = self.ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("omp-deck-native".into())
            .spawn(move || {
                let _ = tx.send(work());
                ctx.request_repaint();
            });
        if let Err(e) = spawned {
            log::warn!("starting a native worker failed: {e}");
            return;
        }
        let mut done = Some(done);
        self.polls.push(Box::new(move || match rx.try_recv() {
            Ok(r) => {
                let d = done.take()?;
                Some(Some(Box::new(move |app: &mut App| d(app, r)) as Job))
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            // The worker panicked: nothing to deliver.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(None),
        }));
    }
}

/// Why a mutation sent through [`Gw::mutate_typed`] did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutateError {
    /// The gateway turned it away with a typed reason (e.g. `Rejected::Busy`).
    Rejected(deck_service::controller::Rejected),
    /// Any other failure, as text.
    Failed(String),
}

impl Gw {
    /// [`Gw::mutate`] for callers that act on the gateway's typed refusal.
    pub fn mutate_typed(
        &mut self,
        op: Mutation,
        done: impl FnOnce(&mut App, Result<Done, MutateError>) + 'static,
    ) -> RequestKey {
        let m = self.client.mutate(op);
        let refusal = Rc::new(std::cell::Cell::new(None));
        let seen = Rc::clone(&refusal);
        // The acceptance callback runs before the completion for the same request.
        let on_accepted = move |_: &mut App, _: RequestKey, r: Result<Submitted, GatewayError>| {
            if let Err(GatewayError::Rejected(why)) = r {
                seen.set(Some(why));
            }
        };
        self.track(
            m.key,
            m.accepted,
            Box::new(move |app, r| {
                done(
                    app,
                    r.map_err(|text| match refusal.take() {
                        Some(why) => MutateError::Rejected(why),
                        None => MutateError::Failed(text),
                    }),
                );
            }),
            None,
            Some(Box::new(on_accepted)),
        )
    }
}
