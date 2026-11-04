//! The gateway actor: the single authority over the managed sessions (controllers, terminal
//! fanout to frontend views), typed queries and journaled mutations, worker jobs and their
//! lanes, interest-driven watchers, the schedules that run without a GUI (usage, updates, backups,
//! project sync), the canonical notifications and inbox, the maintenance barrier and the owner's
//! lifetime.
//!
//! Everything runs on one thread ([`Engine::run`]). Connection threads ([`super::server`]) and
//! workers talk to it only through [`Msg`]; nothing that blocks on the network, git, omp or a
//! backend runs here (workers report back with [`Fin`]).

mod deferred;
mod device;
mod interest;
mod model_route;
mod mutate;
mod notify;
mod remote;
mod schedules;
mod session_ops;
mod sessions;
mod spelling;
pub(super) mod worktree;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use deck_core::agents::AgentsTarget;
use deck_core::backup::Service;
use deck_core::changes::Changes;
use deck_core::config::{AppConfig, GatewayMode};
use deck_core::fleet::FleetTarget;
use deck_core::inventory::{InventoryEntry, StatsCache};
use deck_core::omp::{Omp, UpdateStatus};
use deck_core::selfupdate::{Applied, InstallKind, Release};
use deck_core::sessions::{SessionWatcher, SessionsRoot};
use deck_core::status::StatusWatcher;
use deck_core::usage::UsageSnapshot;
use deck_term::{Clipboard, InputMeta};

use super::jobs::{Handles, JobCtx};
use super::journal::Journal;
use super::protocol::{
    Answer, ClientState, Done, Effective, Envelope, GatewayError, GatewayStatus, Hello, JobInfo,
    MaintenanceOp, PROTOCOL, Progress, Query, Refusal, Reply, Request, Role, ServerEvent,
    Supervision, Topic, ViewRole, Welcome,
};
use super::running::{RunningMonitor, RunningSnapshot};
use super::server::{Outbox, SideItem, ViewSide};
use super::{GATEWAY_VERSION, MaintenanceMarker, MaintenancePurpose, Profile, StartupContext};
use crate::backend::Backends;
use crate::controller::{RequestKey, ServiceEpoch, SessionKey, SessionTarget};
use crate::settings::{SettingsError, SettingsService, Snapshot};

use deck_term::OutputSubscription;

/// Poll interval of the session and running watchers (the GUI's).
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Longest sleep of the actor loop: schedules and close polls advance at this granularity.
const TICK: Duration = Duration::from_millis(500);
/// Messages handled per round before the loop steps again (fairness under a flood).
const BATCH: usize = 256;
/// How long a freshly started owner waits for its first client before an on-demand exit.
const START_GRACE: Duration = Duration::from_secs(30);
/// How long an on-demand owner lingers after its last client left (a GUI restarting into an
/// update reconnects within it).
const LINGER: Duration = Duration::from_secs(10);
/// How often the OS registration is re-read while Background is requested.
const REGISTRATION_EVERY: Duration = Duration::from_secs(60);
/// Refusal of a request key (journal lookup, job cancel, mutation) of another client.
const FOREIGN_KEY: &str = "this request key belongs to another client";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the engine receives.
pub(crate) enum Msg {
    /// A control link's hello; `reply` carries the verdict.
    Hello {
        conn: u64,
        hello: Hello,
        out: Arc<Outbox>,
        reply: Sender<Result<Welcome, Refusal>>,
    },
    /// A frontend view's hello.
    Attach {
        view: u64,
        hello: Hello,
        side: Arc<ViewSide>,
        reply: Sender<Result<Welcome, Refusal>>,
    },
    Request {
        conn: u64,
        env: Envelope,
    },
    /// A control link ended (`clean`: the client said BYE).
    Bye {
        conn: u64,
        clean: bool,
    },
    View {
        view: u64,
        op: ViewOp,
    },
    ViewGone {
        view: u64,
    },
    /// Something changed (terminal output, a watcher, the notifier): step now.
    Wake,
    /// A worker finished.
    Fin(Box<Fin>),
    /// The relay link of generation `link` reports.
    Relay {
        link: u64,
        event: super::remote::link::Event,
    },
}

/// One frontend view operation.
pub(crate) enum ViewOp {
    Input(Vec<u8>, InputMeta),
    Clipboard(Vec<u8>),
    Focus(bool),
    Resize(u16, u16, u16, u16),
    Palette([[u8; 3]; 19]),
    Kill,
}

/// A worker's result.
pub(crate) enum Fin {
    /// A journaled job ended.
    Mutation {
        key: RequestKey,
        /// Boxed: a result is large next to the other worker results.
        result: Result<Box<Done>, GatewayError>,
    },
    Progress {
        key: RequestKey,
        progress: Progress,
    },
    /// A worker query's reply.
    Query {
        conn: u64,
        id: u64,
        reply: Reply,
    },
    Settings {
        conn: u64,
        id: u64,
        key: RequestKey,
        result: Result<Snapshot, SettingsError>,
    },
    Flushed {
        conn: u64,
        id: u64,
    },
    Prepared(Box<sessions::Prepared>),
    /// A restart's backend session is gone (or could not be ended).
    Restarted {
        key: RequestKey,
        session: SessionKey,
        view: Option<deck_core::config::SessionView>,
        result: Result<(), String>,
    },
    CopyFound {
        session: SessionKey,
        generation: u64,
        path: Option<PathBuf>,
    },
    /// A schedule's (or update/usage job's side) result.
    Sched(Sched),
    Registration(Option<String>),
    ModeSet {
        key: RequestKey,
        result: Result<(), GatewayError>,
    },
    Inventory(Vec<InventoryEntry>),
    Changes {
        root: PathBuf,
        result: Result<Option<Changes>, String>,
    },
    Activated(Result<(), String>),
    /// The sessions' live folders (session, generation, folder).
    LiveCwds(Vec<(SessionKey, u64, Option<PathBuf>)>),
    /// Canonical forms of paths the device projections compared ([`spelling::Canon`]).
    Canonical(Vec<(PathBuf, Option<PathBuf>)>),
    /// Unjournaled work (a view's kill): only a failure is logged.
    Logged {
        what: &'static str,
        result: Result<(), String>,
    },
    /// A review prompt was built (or refused) for the `Review` mutation `key`.
    ReviewBuilt {
        key: RequestKey,
        target: SessionTarget,
        send: bool,
        result: Result<String, GatewayError>,
    },
    /// A desktop sign-in's Google URL: shown by a GUI, else the app opens on it.
    OpenUrl(String),
    /// A device operation's worker result ([`device::DeviceFin`]).
    Device(Box<device::DeviceFin>),
    /// A worktree flow's worker result ([`worktree::WtFin`]).
    Worktree(Box<worktree::WtFin>),
}

/// Results the schedules own ([`Engine::on_schedule_fin`]).
pub(crate) enum Sched {
    Usage(Result<UsageSnapshot, String>),
    AppDetected(InstallKind),
    AppChecked(Result<Release, String>),
    AppDownloaded(Result<(PathBuf, PathBuf), String>),
    /// The failure is the resulting phase (manual install needed, or failed).
    AppApplied(Result<Applied, super::protocol::UpdatePhase>),
    OmpFound(Option<Omp>),
    OmpVersion(Result<String, String>),
    OmpChecked(Result<UpdateStatus, String>),
    Accounts(Result<schedules::Accounts, String>),
    BackupAuto {
        destination: deck_core::backup::Destination,
        result: Result<(), String>,
    },
    ProjectAuto {
        path: PathBuf,
        result: Result<(), String>,
    },
}

impl From<Sched> for Fin {
    fn from(s: Sched) -> Fin {
        Fin::Sched(s)
    }
}

/// Coalesced wakeups from other threads (terminal output, watchers, the notifier).
pub(crate) struct Waker {
    tx: Sender<Msg>,
    queued: AtomicBool,
    deadline: Mutex<Option<Instant>>,
}

impl Waker {
    fn new(tx: Sender<Msg>) -> Arc<Waker> {
        Arc::new(Waker {
            tx,
            queued: AtomicBool::new(false),
            deadline: Mutex::new(None),
        })
    }

    pub(crate) fn now(&self) {
        if !self.queued.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(Msg::Wake);
        }
    }

    /// Step no later than `delay` from now.
    pub(crate) fn after(&self, delay: Duration) {
        if delay.is_zero() {
            self.now();
            return;
        }
        let at = Instant::now() + delay;
        let mut g = lock(&self.deadline);
        if g.is_none_or(|d| at < d) {
            *g = Some(at);
            drop(g);
            // The loop may sleep past it: let it recompute its timeout.
            self.now();
        }
    }

    fn deadline(&self) -> Option<Instant> {
        *lock(&self.deadline)
    }

    /// Called right before a step: wakeups from now on queue a new one.
    fn begin_step(&self, now: Instant) {
        self.queued.store(false, Ordering::Release);
        let mut g = lock(&self.deadline);
        if g.is_some_and(|d| d <= now) {
            *g = None;
        }
    }

    fn callback(self: &Arc<Self>) -> Arc<dyn Fn() + Send + Sync> {
        let w = self.clone();
        Arc::new(move || w.now())
    }
}

/// Where a link's replies and events go.
enum Sink {
    Local(Arc<Outbox>),
    /// A paired device's application session: replies and events are translated to the
    /// device protocol ([`device`]).
    Device(Box<device::DeviceConn>),
}

/// One control link: a local client, or a paired device's session over the relay.
struct Conn {
    sink: Sink,
    /// `None`: a paired device (never a client for the lifetime, never a GUI).
    role: Option<Role>,
    client_id: String,
    /// Order of arrival (the most recent GUI receives activations when none is focused).
    seq: u64,
    state: ClientState,
    subs: BTreeSet<Topic>,
    fleet: Vec<FleetTarget>,
    agents: Option<AgentsTarget>,
    /// When `agents` was set (the most recent wins when several follow one).
    agents_seq: u64,
}

impl Conn {
    fn is(&self, role: Role) -> bool {
        self.role == Some(role)
    }

    /// A GUI or CLI: keeps an on-demand owner alive.
    fn is_client(&self) -> bool {
        matches!(self.role, Some(Role::Gui | Role::Cli))
    }

    /// The outbox of a local link.
    fn local(&self) -> Option<&Arc<Outbox>> {
        match &self.sink {
            Sink::Local(out) => Some(out),
            Sink::Device(_) => None,
        }
    }

    fn close(&self) {
        match &self.sink {
            Sink::Local(out) => out.close(),
            Sink::Device(d) => d.out.close(),
        }
    }
}

/// One frontend view.
struct View {
    side: Arc<ViewSide>,
    target: SessionTarget,
    role: ViewRole,
    sub: Arc<OutputSubscription>,
    /// The device link whose view it is (`None`: a local frontend).
    owner: Option<u64>,
}

/// A journaled job that has not finished.
struct JobState {
    label: &'static str,
    progress: Option<Progress>,
    cancel: Option<Arc<AtomicBool>>,
    /// Holds the one-at-a-time backup lane.
    lane: bool,
    post: mutate::Post,
}

/// Last published values, for change detection.
#[derive(Default)]
struct Published {
    gateway: Option<GatewayStatus>,
    settings_rev: Option<u64>,
    sessions_gen: Option<u64>,
    running: Option<Arc<RunningSnapshot>>,
    status: Option<(
        Option<SessionTarget>,
        Option<Arc<deck_core::status::SessionStatus>>,
    )>,
}

/// Flags of topics whose sources report changes by event.
#[derive(Default)]
struct Dirty {
    controllers: bool,
    usage: bool,
    app: bool,
    backup: bool,
    omp: bool,
    gateway: bool,
    mobile: bool,
}

pub(crate) struct Engine {
    profile: Profile,
    context: Arc<StartupContext>,
    epoch: ServiceEpoch,
    tx: Sender<Msg>,
    waker: Arc<Waker>,

    settings: SettingsService,
    cfg: Arc<AppConfig>,
    journal: Journal,
    root: SessionsRoot,
    backends: Backends,
    ctl_ns: String,
    ctl_ext: Option<PathBuf>,
    omp: Option<Omp>,
    /// Backups to every destination (the Drive side is unavailable without OAuth credentials).
    service: Arc<Service>,
    handles: Arc<Handles>,
    stats: Arc<Mutex<StatsCache>>,
    inventory_shared: Arc<Mutex<Arc<Vec<InventoryEntry>>>>,
    clipboard: Clipboard,

    sessions_watch: SessionWatcher,
    running: RunningMonitor,
    status: StatusWatcher,
    status_target: Option<SessionTarget>,

    conns: HashMap<u64, Conn>,
    views: HashMap<u64, View>,
    next_seq: u64,

    sess: sessions::Sessions,
    jobs: HashMap<RequestKey, JobState>,
    backup_lane: Option<String>,
    mode_queue: VecDeque<(RequestKey, GatewayMode)>,
    mode_busy: bool,
    mode_failed: Option<String>,
    registration: Option<String>,
    registration_at: Option<Instant>,
    registration_busy: bool,

    revisions: HashMap<Topic, u64>,
    published: Published,
    dirty: Dirty,

    notify: notify::Notify,
    sched: schedules::Schedules,
    interest: interest::Interest,
    mobile: remote::Mobile,
    /// Device-protocol state shared by every device session.
    devices: device::Devices,
    /// The worktree flows ([`worktree`]).
    worktree: worktree::Worktrees,
    /// Commands typed into sessions later ([`deferred`]).
    defer: deferred::Deferred,

    barrier: Option<MaintenanceMarker>,
    exit_requested: bool,
    stopping: bool,
    linger_until: Instant,
    /// Earliest deadline a controller or model observation asked for.
    soon: Option<Instant>,
}

impl Engine {
    /// Opens everything the owner serves. Blocking setup (backend probes, omp detection, the
    /// journal's recovery) runs here, before any connection is accepted.
    pub(crate) fn new(
        profile: Profile,
        startup_file: Option<PathBuf>,
        tx: Sender<Msg>,
    ) -> io::Result<Engine> {
        let context = match &startup_file {
            Some(path) => StartupContext::load(path)?,
            None => StartupContext::capture(profile.clone())?,
        };
        if !context.profile.same_as(&profile) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the startup context belongs to another profile",
            ));
        }
        let state_dir = profile.state_dir();
        super::private_dir(&state_dir)?;
        let journal = Journal::open(&state_dir.join("journal"))?;
        let settings = SettingsService::open(profile.settings.clone());
        let cfg = settings.snapshot().config;
        let root = SessionsRoot::at(profile.sessions_root.clone());
        let backends = Backends::detect(cfg.session_backend);
        backends.prepare();
        let ctl_ns = backends.ctl_namespace();
        let ctl_ext = dirs::data_dir().and_then(|d| {
            deck_core::ctl::install_extension(&d.join("omp-deck"))
                .map_err(|e| log::warn!("live control extension unavailable: {e}"))
                .ok()
        });
        if ctl_ext.is_none() && dirs::data_dir().is_none() {
            log::warn!("live control extension unavailable: no per-user data directory");
        }
        let omp = super::resolve_omp(context.omp.as_deref());
        // The backup credentials belong to the served profile: `app_root()` is `config_dir()` for
        // the default profile, and an explicit profile (`--settings`) keeps its own on every
        // platform (Windows' config dir ignores the environment).
        let app_root = profile.app_root();
        let service = Arc::new(Service {
            store: deck_core::backup::drive::AccountStore::at(app_root.join("google-drive.json")),
            server_store: deck_core::backup::server::ServerStore::at(
                app_root.join("backup-server.json"),
            ),
            ..Service::new(
                deck_core::backup::drive::Credentials::detect(),
                root.agent_dir(),
            )
        });
        let waker = Waker::new(tx.clone());
        let wake = waker.callback();
        let sessions_watch = {
            let w = wake.clone();
            SessionWatcher::spawn(root.clone(), POLL_INTERVAL, move || w())
        };
        let running = {
            let w = wake.clone();
            RunningMonitor::spawn(
                backends.all().to_vec(),
                ctl_ns.clone(),
                root.path.clone(),
                POLL_INTERVAL,
                move || w(),
            )
        };
        let status = {
            let w = wake.clone();
            StatusWatcher::spawn(root.agent_dir(), move || w())
        };
        {
            let w = wake.clone();
            settings.on_change(move || w());
        }
        let now = Instant::now();
        // Loads (or creates) the VAPID key `push.key` in the secrets dir.
        let notify = notify::Notify::new(wake.clone(), &profile.secrets_dir());
        let manifest = state_dir.join(sessions::MANIFEST);
        let omp_found = omp.is_some();
        // Creates (and protects) the secrets dir; a failure only disables mobile access.
        let mobile = remote::Mobile::open(&profile.secrets_dir());
        let mut engine = Engine {
            profile,
            context: Arc::new(context),
            epoch: ServiceEpoch::current(),
            tx,
            waker,
            settings,
            cfg,
            journal,
            root,
            backends,
            ctl_ns,
            ctl_ext,
            omp,
            service,
            handles: Arc::new(Handles::default()),
            stats: Arc::new(Mutex::new(StatsCache::default())),
            inventory_shared: Arc::new(Mutex::new(Arc::new(Vec::new()))),
            clipboard: Clipboard::new(),
            sessions_watch,
            running,
            status,
            status_target: None,
            conns: HashMap::new(),
            views: HashMap::new(),
            next_seq: 0,
            sess: sessions::Sessions::new(manifest),
            jobs: HashMap::new(),
            backup_lane: None,
            mode_queue: VecDeque::new(),
            mode_busy: false,
            mode_failed: None,
            registration: None,
            registration_at: None,
            registration_busy: false,
            revisions: HashMap::new(),
            published: Published::default(),
            dirty: Dirty::default(),
            notify,
            sched: schedules::Schedules::new(now, omp_found),
            interest: interest::Interest::default(),
            mobile,
            devices: device::Devices::default(),
            worktree: worktree::Worktrees::default(),
            defer: deferred::Deferred::default(),
            barrier: None,
            exit_requested: false,
            stopping: false,
            linger_until: now + START_GRACE,
            soon: None,
        };
        engine.barrier = super::maintenance_active(&engine.profile.app_root());
        engine.start_background_work();
        Ok(engine)
    }

    /// Runs until the lifetime ends; returns the process exit code.
    pub(crate) fn run(mut self, rx: Receiver<Msg>) -> i32 {
        loop {
            let now = Instant::now();
            let timeout = self.timeout(now);
            match rx.recv_timeout(timeout) {
                Ok(msg) => {
                    self.handle(msg);
                    for _ in 0..BATCH {
                        match rx.try_recv() {
                            Ok(m) => self.handle(m),
                            Err(_) => break,
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                // Every sender is gone: nothing can reach the owner any more.
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let now = Instant::now();
            self.waker.begin_step(now);
            self.step(now);
            if self.should_exit(now) {
                break;
            }
        }
        self.shutdown()
    }

    fn timeout(&self, now: Instant) -> Duration {
        let mut at = now + TICK;
        if let Some(d) = self.waker.deadline() {
            at = at.min(d);
        }
        if let Some(d) = self.soon {
            at = at.min(d);
        }
        at.saturating_duration_since(now)
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Hello {
                conn,
                hello,
                out,
                reply,
            } => {
                let verdict = self.on_hello(conn, hello, out);
                let _ = reply.send(verdict);
            }
            Msg::Attach {
                view,
                hello,
                side,
                reply,
            } => {
                let verdict = self.on_attach(view, hello, side);
                let _ = reply.send(verdict);
            }
            Msg::Request { conn, env } => self.on_request(conn, env),
            Msg::Bye { conn, clean } => self.on_bye(conn, clean),
            Msg::View { view, op } => self.on_view_op(view, op),
            Msg::ViewGone { view } => self.on_view_gone(view),
            Msg::Wake => {}
            Msg::Fin(fin) => self.on_fin(*fin),
            Msg::Relay { link, event } => self.on_relay(link, event),
        }
    }

    fn step(&mut self, now: Instant) {
        self.soon = None;
        self.refresh_settings();
        self.step_sessions(now);
        self.step_deferred(now);
        self.step_interest(now);
        self.step_worktrees(now);
        self.step_notifications(now);
        self.step_schedules(now);
        self.step_lifetime(now);
        self.step_mobile(now);
        self.publish();
        self.step_devices();
    }

    /// The soonest a controller or observation needs another step.
    fn wake_within(&mut self, d: Duration) {
        let at = Instant::now() + d;
        self.soon = Some(self.soon.map_or(at, |s| s.min(at)));
    }

    // ---- workers ------------------------------------------------------------------------------

    /// Runs `f` on a worker thread and delivers its [`Fin`].
    fn spawn_worker(&self, name: &str, f: impl FnOnce() -> Fin + Send + 'static) {
        let tx = self.tx.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("gateway-{name}"))
            .spawn(move || {
                let fin = f();
                let _ = tx.send(Msg::Fin(Box::new(fin)));
            });
        if let Err(e) = spawned {
            log::error!("cannot start a gateway worker ({name}): {e}");
        }
    }

    fn job_ctx(&self) -> JobCtx {
        JobCtx {
            omp: self.omp.clone(),
            root: self.root.clone(),
            backends: self.backends.clone(),
            ctl_ns: self.ctl_ns.clone(),
            service: self.service.clone(),
            destination: self.cfg.backup_destination,
            handles: self.handles.clone(),
            stats: self.stats.clone(),
            inventory: self.inventory_shared.clone(),
        }
    }

    fn on_fin(&mut self, fin: Fin) {
        match fin {
            Fin::Mutation { key, result } => self.job_finished(key, result),
            Fin::Worktree(fin) => self.on_worktree_fin(*fin),
            Fin::Progress { key, progress } => self.job_progress(key, progress),
            Fin::Query { conn, id, reply } => self.reply(conn, id, reply),
            Fin::Settings {
                conn,
                id,
                key,
                result,
            } => self.settings_finished(conn, id, key, result),
            Fin::Flushed { conn, id } => self.reply(conn, id, Ok(Answer::Flushed)),
            Fin::Prepared(p) => self.on_prepared(*p),
            Fin::Restarted {
                key,
                session,
                view,
                result,
            } => self.on_restarted(key, session, view, result),
            Fin::CopyFound {
                session,
                generation,
                path,
            } => self.on_copy_found(&session, generation, path),
            Fin::Registration(r) => {
                self.registration_busy = false;
                self.registration_at = Some(Instant::now());
                self.registration = r;
            }
            Fin::ModeSet { key, result } => self.mode_finished(key, result),
            Fin::Inventory(entries) => self.on_inventory(entries),
            Fin::Changes { root, result } => self.on_changes(root, result),
            Fin::Logged { what, result } => {
                if let Err(e) = result {
                    log::warn!("{what} failed: {e}");
                }
                self.running.poke();
            }
            Fin::Sched(s) => self.on_schedule_fin(s),
            Fin::LiveCwds(found) => self.on_live_cwds(found),
            Fin::Canonical(found) => self.on_canonical(found),
            Fin::Activated(r) => {
                if let Err(e) = r {
                    log::warn!("opening OmpDeck for a notification failed: {e}");
                }
            }
            Fin::ReviewBuilt {
                key,
                target,
                send,
                result,
            } => self.on_review_built(key, target, send, result),
            Fin::OpenUrl(url) => self.open_url(url),
            Fin::Device(f) => self.on_device_fin(*f),
        }
    }

    // ---- links --------------------------------------------------------------------------------

    fn welcome(&self) -> Welcome {
        Welcome {
            protocol: PROTOCOL,
            version: GATEWAY_VERSION.to_owned(),
            epoch: self.epoch,
            pid: std::process::id(),
            profile: self.profile.clone(),
            mode: self.cfg.gateway_mode,
            maintenance: self.barrier.clone(),
        }
    }

    fn on_hello(&mut self, conn: u64, hello: Hello, out: Arc<Outbox>) -> Result<Welcome, Refusal> {
        if self.stopping {
            return Err(Refusal::ShuttingDown);
        }
        if !hello.profile.same_as(&self.profile) {
            return Err(Refusal::ProfileMismatch {
                served: self.profile.clone(),
            });
        }
        // Paired devices' work, outcomes and confirmations are keyed by their `d-…` ids: a
        // local link may never claim one.
        if remote::is_device_id(&hello.client_id) {
            return Err(Refusal::NotAllowed(
                "client ids starting with \"d-\" belong to paired devices".to_owned(),
            ));
        }
        // The CLI drains over its link and the supervisor watches the owner through the
        // barrier; only a GUI is turned away.
        if hello.role == Role::Gui
            && let Some(m) = &self.barrier
        {
            return Err(Refusal::Maintenance(m.clone()));
        }
        self.next_seq += 1;
        self.conns.insert(
            conn,
            Conn {
                sink: Sink::Local(out),
                role: Some(hello.role),
                client_id: hello.client_id,
                seq: self.next_seq,
                state: ClientState::default(),
                subs: BTreeSet::new(),
                fleet: Vec::new(),
                agents: None,
                agents_seq: 0,
            },
        );
        self.dirty.gateway = true;
        Ok(self.welcome())
    }

    fn on_bye(&mut self, conn: u64, clean: bool) {
        let Some(c) = self.conns.remove(&conn) else {
            return;
        };
        c.close();
        if c.is_client() && !self.conns.values().any(Conn::is_client) {
            self.linger_until = Instant::now() + LINGER;
        }
        // A client that left cleanly will not answer its draft confirmations: decline them
        // (never approve), unless the same client is still connected on another link.
        if clean && !self.conns.values().any(|o| o.client_id == c.client_id) {
            self.decline_confirmations(&c.client_id);
        }
        self.dirty.gateway = true;
    }

    /// Queues `reply` on link `conn` (gone links are skipped).
    fn reply(&self, conn: u64, id: u64, reply: Reply) {
        match self.conns.get(&conn).map(|c| &c.sink) {
            Some(Sink::Local(out)) => out.reply(id, &reply),
            Some(Sink::Device(d)) => d.out.reply(id, self.device_reply(d, reply)),
            None => {}
        }
    }

    /// Sends `ev` to every control link of `client`.
    fn to_client(&self, client: &str, ev: &ServerEvent) {
        for c in self.conns.values().filter(|c| c.client_id == client) {
            match &c.sink {
                Sink::Local(out) => out.event(ev),
                Sink::Device(d) => {
                    if let Some(ev) = self.device_event(d, ev) {
                        d.out.event(&ev);
                    }
                }
            }
        }
    }

    /// Sends `ev` to every GUI link (and what a device shows of it to every device).
    fn to_guis(&self, ev: &ServerEvent) {
        for c in self.conns.values() {
            match &c.sink {
                Sink::Local(out) if c.is(Role::Gui) => out.event(ev),
                Sink::Device(d) => {
                    if let ServerEvent::ModelUnconfirmed { .. } = ev
                        && let Some(ev) = self.device_event(d, ev)
                    {
                        d.out.event(&ev);
                    }
                }
                Sink::Local(_) => {}
            }
        }
    }

    fn on_request(&mut self, conn: u64, env: Envelope) {
        let Envelope { id, body } = env;
        let Some(role) = self.conns.get(&conn).and_then(|c| c.role) else {
            return;
        };
        match body {
            Request::ClientState(state) => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.state = state;
                }
                self.reply(conn, id, Ok(Answer::Done));
            }
            Request::Subscribe(topic) => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.subs.insert(topic);
                }
                self.reply(conn, id, Ok(Answer::Done));
            }
            Request::Unsubscribe(topic) => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.subs.remove(&topic);
                }
                self.reply(conn, id, Ok(Answer::Done));
            }
            Request::Query(q) => self.on_query(conn, id, q),
            Request::Mutate { key, epoch, op } => self.on_mutate(conn, id, key, epoch, *op, None),
            Request::Outcome(key) => {
                // Only a client's own requests: another client's keys are refused whether or
                // not they exist (no probing of foreign requests).
                let own = self
                    .conns
                    .get(&conn)
                    .is_some_and(|c| c.client_id == key.client);
                let reply = if !own {
                    Err(GatewayError::Refused(FOREIGN_KEY.to_owned()))
                } else {
                    match self.journal.lookup(&key) {
                        Ok(Some(o)) => Ok(Answer::Outcome(o)),
                        Ok(None) => Err(GatewayError::NoSuchJob),
                        Err(_) => Err(GatewayError::Expired),
                    }
                };
                self.reply(conn, id, reply);
            }
            Request::Flush => {
                let settings = self.settings.clone();
                self.spawn_worker("flush", move || {
                    settings.flush();
                    Fin::Flushed { conn, id }
                });
            }
            Request::Maintenance(op) => {
                if role == Role::Supervisor {
                    self.reply(
                        conn,
                        id,
                        Err(GatewayError::Refused(
                            "the supervisor cannot raise the maintenance barrier".to_owned(),
                        )),
                    );
                    return;
                }
                let reply = self.on_maintenance(op);
                self.reply(conn, id, reply);
            }
            Request::SetFleetTargets(targets) => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.fleet = targets;
                }
                self.reply(conn, id, Ok(Answer::Done));
            }
            Request::SetAgentsTarget(target) => {
                self.next_seq += 1;
                let seq = self.next_seq;
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.agents = target;
                    c.agents_seq = seq;
                }
                self.reply(conn, id, Ok(Answer::Done));
            }
            Request::ObserveModel { target, facts } => {
                if role == Role::Supervisor {
                    self.reply(
                        conn,
                        id,
                        Err(GatewayError::Refused(
                            "the supervisor has no sessions".to_owned(),
                        )),
                    );
                    return;
                }
                let reply = self.observe_model(&target, facts).map(|()| Answer::Done);
                self.reply(conn, id, reply);
            }
        }
    }

    fn on_query(&mut self, conn: u64, id: u64, q: Query) {
        match &q {
            Query::Topic(topic) => {
                let subscribed = self
                    .conns
                    .get(&conn)
                    .is_some_and(|c| c.subs.contains(topic));
                let reply = self
                    .topic_value(topic, subscribed)
                    .map(|v| Answer::Topic(Box::new(v)));
                self.reply(conn, id, reply);
            }
            Query::Screen(target) => {
                let reply = self.screen(target).map(Answer::Screen);
                self.reply(conn, id, reply);
            }
            Query::WorktreeSessions(wt) => {
                let list = self.worktree_sessions(&wt.path);
                self.reply(conn, id, Ok(Answer::WorktreeSessions(list)));
            }
            Query::WorktreeVerdict(wt) => self.query_verdict(conn, id, wt.clone()),
            _ => {
                let ctx = self.job_ctx();
                self.spawn_worker("query", move || {
                    let reply = super::jobs::query(&ctx, &q).unwrap_or_else(|| {
                        Err(GatewayError::Failed("not a worker query".to_owned()))
                    });
                    Fin::Query { conn, id, reply }
                });
            }
        }
    }

    // ---- maintenance and lifetime -------------------------------------------------------------

    fn on_maintenance(&mut self, op: MaintenanceOp) -> Reply {
        match op {
            MaintenanceOp::Begin(purpose) => {
                if self.barrier.is_none() {
                    self.barrier = Some(
                        super::maintenance_active(&self.profile.app_root())
                            .unwrap_or_else(|| marker(purpose)),
                    );
                }
                self.cancel_pairing();
                self.dirty.gateway = true;
                Ok(Answer::Maintenance(self.drain_state()))
            }
            MaintenanceOp::Poll => Ok(Answer::Maintenance(self.drain_state())),
            MaintenanceOp::Exit => {
                if self.barrier.is_none() {
                    self.barrier = Some(
                        super::maintenance_active(&self.profile.app_root())
                            .unwrap_or_else(|| marker(MaintenancePurpose::Update)),
                    );
                }
                self.exit_requested = true;
                self.cancel_pairing();
                self.dirty.gateway = true;
                Ok(Answer::Done)
            }
        }
    }

    fn drain_state(&self) -> super::protocol::DrainState {
        let jobs = self.journal.running() + usize::from(self.sess.launch_pending());
        let controllers_busy = self.sess.busy_controllers();
        let direct_programs = self.sess.direct_programs();
        if jobs == 0 && controllers_busy == 0 && direct_programs == 0 && self.idle_work() {
            // Settings edits are committed before Drained is reported.
            self.settings.flush();
            super::protocol::DrainState::Drained
        } else {
            super::protocol::DrainState::Draining {
                jobs,
                controllers_busy,
                direct_programs,
            }
        }
    }

    /// No accepted or internal work is left (jobs, launches, closes, lanes, mode changes).
    fn idle_work(&self) -> bool {
        self.journal.running() == 0
            && self.jobs.is_empty()
            && !self.sess.launch_pending()
            && !self.sess.closing()
            && self.sess.busy_controllers() == 0
            && self.sess.direct_programs() == 0
            && self.backup_lane.is_none()
            && !self.mode_busy
            && self.mode_queue.is_empty()
    }

    fn mode(&self) -> GatewayMode {
        self.cfg.gateway_mode
    }

    fn has_clients(&self) -> bool {
        self.conns.values().any(Conn::is_client)
    }

    fn gui_count(&self) -> usize {
        self.conns.values().filter(|c| c.is(Role::Gui)).count()
    }

    /// Background work (schedules) runs for a GUI, for a supervised owner, and while the owner
    /// has to stay anyway; an on-demand owner about to exit starts nothing new.
    fn background_wanted(&self) -> bool {
        self.mode() == GatewayMode::Background || self.gui_count() > 0
    }

    fn should_exit(&mut self, now: Instant) -> bool {
        let exit = if self.exit_requested {
            self.idle_work() && self.journal.running() == 0
        } else {
            self.mode() != GatewayMode::Background
                && !self.has_clients()
                && now >= self.linger_until
                && self.idle_work()
        };
        if exit {
            self.stopping = true;
        }
        exit
    }

    fn step_lifetime(&mut self, now: Instant) {
        let background = self.mode() == GatewayMode::Background;
        let due = self
            .registration_at
            .is_none_or(|t| now.saturating_duration_since(t) >= REGISTRATION_EVERY);
        if background && due && !self.registration_busy {
            self.refresh_registration();
        }
        if !background && self.registration.is_some() && !self.registration_busy && due {
            // Mode left Background: re-read once so the status stops claiming a registration.
            self.refresh_registration();
        }
        self.pump_mode();
    }

    fn refresh_registration(&mut self) {
        self.registration_busy = true;
        let context = self.context.clone();
        self.spawn_worker("registration", move || {
            Fin::Registration(super::lifetime::registration_status(&context))
        });
    }

    fn pump_mode(&mut self) {
        if self.mode_busy {
            return;
        }
        let Some((key, desired)) = self.mode_queue.pop_front() else {
            return;
        };
        self.mode_busy = true;
        let (context, settings) = (self.context.clone(), self.settings.clone());
        self.spawn_worker("mode", move || Fin::ModeSet {
            result: super::lifetime::set_mode(&context, &settings, desired),
            key,
        });
    }

    fn mode_finished(&mut self, key: RequestKey, result: Result<(), GatewayError>) {
        self.mode_busy = false;
        self.mode_failed = result.as_ref().err().map(ToString::to_string);
        // The settings listener already woke the loop for the new mode; re-read it now so the
        // status below reports it.
        self.refresh_settings();
        self.refresh_registration();
        let result = result.map(|()| Done::Gateway(self.gateway_status()));
        self.job_finished(key, result);
        self.dirty.gateway = true;
        self.pump_mode();
    }

    fn gateway_status(&self) -> GatewayStatus {
        let mode = self.mode();
        let supervisor = self.conns.values().any(|c| c.is(Role::Supervisor));
        let supervision = if mode != GatewayMode::Background {
            match &self.mode_failed {
                Some(e) => Supervision::Failed(e.clone()),
                None => Supervision::None,
            }
        } else {
            match (&self.registration, supervisor) {
                (Some(r), true) => Supervision::Active {
                    registration: r.clone(),
                },
                (Some(r), false) => Supervision::Registered {
                    registration: r.clone(),
                },
                (None, _) => Supervision::Failed(
                    self.mode_failed
                        .clone()
                        .unwrap_or_else(|| "the background service is not registered".to_owned()),
                ),
            }
        };
        let effective = if let Some(m) = &self.barrier {
            Effective::Maintenance(m.purpose)
        } else if self.stopping {
            Effective::Stopping
        } else if matches!(supervision, Supervision::Active { .. }) {
            Effective::Background
        } else {
            Effective::OnDemand
        };
        let mut jobs: Vec<JobInfo> = self
            .jobs
            .iter()
            .map(|(key, j)| JobInfo {
                key: key.clone(),
                label: j.label.to_owned(),
                progress: j.progress.clone(),
                cancellable: j.cancel.is_some(),
            })
            .collect();
        jobs.sort_by(|a, b| a.key.cmp(&b.key));
        GatewayStatus {
            epoch: self.epoch,
            pid: std::process::id(),
            version: GATEWAY_VERSION.to_owned(),
            profile: self.profile.clone(),
            mode,
            effective,
            supervision,
            gui_clients: self.gui_count(),
            cli_clients: self.conns.values().filter(|c| c.is(Role::Cli)).count(),
            supervisor,
            jobs,
            direct_programs: self.sess.direct_programs(),
            backend_banner: self.backends.banner(),
            backends: self
                .backends
                .all()
                .iter()
                .map(deck_core::mux::Mux::kind)
                .collect(),
            primary_backend: self.backends.primary().map(deck_core::mux::Mux::kind),
            omp_found: self.omp.is_some(),
            live_control: self.ctl_ext.is_some(),
        }
    }

    /// Ends every view and link, commits settings and drops the controllers: backend sessions
    /// keep running (their clients detach); the manifest lets the next owner re-attach them.
    fn shutdown(mut self) -> i32 {
        self.stopping = true;
        self.stop_mobile();
        for (_, v) in self.views.drain() {
            v.side
                .push(SideItem::Ended(super::protocol::ViewEnd::Shutdown));
        }
        for c in self.conns.values() {
            c.close();
        }
        self.save_manifest();
        self.settings.flush();
        self.sess.drop_all();
        super::server::exit::OK
    }

    // ---- topics -------------------------------------------------------------------------------

    /// Publishes a new revision of `topic`: always-on topics go to every GUI link, the rest
    /// (and every topic for CLI/supervisor links) only to subscribers.
    fn changed(&mut self, topic: Topic) {
        let rev = self.revisions.entry(topic.clone()).or_insert(0);
        *rev += 1;
        let ev = ServerEvent::Changed {
            topic: topic.clone(),
            revision: *rev,
        };
        let always = always_on(&topic);
        for c in self.conns.values_mut() {
            match &mut c.sink {
                Sink::Local(out) => {
                    if (always && c.role == Some(Role::Gui)) || c.subs.contains(&topic) {
                        out.event(&ev);
                    }
                }
                Sink::Device(d) => d.topic_changed(&topic),
            }
        }
    }

    fn refresh_settings(&mut self) {
        let snap = self.settings.snapshot();
        if self.published.settings_rev == Some(snap.revision) {
            return;
        }
        let old = std::mem::replace(&mut self.cfg, snap.config);
        self.published.settings_rev = Some(snap.revision);
        if old.theme != self.cfg.theme || old.style != self.cfg.style {
            self.sess
                .set_theme(super::terminal_theme(self.cfg.theme, self.cfg.style));
        }
        if old.session_backend != self.cfg.session_backend {
            self.backends.set_choice(self.cfg.session_backend);
        }
        if old.gateway_mode != self.cfg.gateway_mode {
            self.dirty.gateway = true;
            self.registration_at = None;
        }
        self.sync_mobile_link();
        self.changed(Topic::Settings);
    }

    fn publish(&mut self) {
        let status = self.gateway_status();
        if self.dirty.gateway || self.published.gateway.as_ref() != Some(&status) {
            self.dirty.gateway = false;
            self.published.gateway = Some(status);
            self.changed(Topic::Gateway);
        }
        let generation = self.sessions_watch.generation();
        if self.published.sessions_gen != Some(generation) {
            self.published.sessions_gen = Some(generation);
            self.changed(Topic::Sessions);
        }
        let running = self.running.latest();
        let same_running = match (&self.published.running, &running) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same_running {
            self.published.running = running;
            self.changed(Topic::Running);
        }
        if std::mem::take(&mut self.dirty.controllers) {
            self.changed(Topic::Controllers);
        }
        let latest = self.status.latest();
        let same_status = match &self.published.status {
            Some((t, s)) => {
                *t == self.status_target
                    && match (s, &latest) {
                        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                        (None, None) => true,
                        _ => false,
                    }
            }
            None => false,
        };
        if !same_status {
            self.published.status = Some((self.status_target.clone(), latest));
            self.changed(Topic::Status);
        }
        if std::mem::take(&mut self.dirty.usage) {
            self.changed(Topic::Usage);
        }
        if std::mem::take(&mut self.dirty.app) {
            self.changed(Topic::AppUpdate);
        }
        if std::mem::take(&mut self.dirty.backup) {
            self.changed(Topic::Backup);
        }
        if std::mem::take(&mut self.dirty.omp) {
            self.changed(Topic::OmpUpdate);
        }
        if std::mem::take(&mut self.dirty.mobile) {
            self.changed(Topic::Mobile);
        }
    }

    fn topic_value(
        &self,
        topic: &Topic,
        subscribed: bool,
    ) -> Result<super::protocol::TopicValue, GatewayError> {
        use super::protocol::{SessionsView, TopicValue};
        // Interest-driven state exists only while someone follows it.
        let needs_sub = topic.interest_driven() || matches!(topic, Topic::Live(_));
        if needs_sub && !subscribed {
            return Err(GatewayError::Refused(
                "subscribe to this topic before querying it".to_owned(),
            ));
        }
        Ok(match topic {
            Topic::Gateway => TopicValue::Gateway(self.gateway_status()),
            Topic::Settings => TopicValue::Settings(self.settings.snapshot()),
            Topic::Sessions => TopicValue::Sessions(SessionsView {
                generation: self.sessions_watch.generation(),
                root: self.root.path.clone(),
                root_exists: self.sessions_watch.root_exists(),
                groups: self.sessions_watch.latest().unwrap_or_default(),
            }),
            Topic::Running => TopicValue::Running(
                self.running
                    .latest()
                    .map(|s| (*s).clone())
                    .unwrap_or_default(),
            ),
            Topic::Controllers => TopicValue::Controllers(self.controller_infos()),
            Topic::Notifications => TopicValue::Notifications(self.inbox_view()),
            Topic::Status => TopicValue::Status(Box::new(
                self.status_target
                    .clone()
                    .zip(self.status.latest().map(|s| (*s).clone())),
            )),
            Topic::Usage => TopicValue::Usage(self.sched.usage_view()),
            Topic::AppUpdate => TopicValue::AppUpdate(self.sched.app_view()),
            Topic::Backup => TopicValue::Backup(self.backup_view()),
            Topic::OmpUpdate => TopicValue::OmpUpdate(self.sched.omp_view()),
            Topic::Mobile => TopicValue::Mobile(self.mobile_view()),
            Topic::Live(key) => self.live_value(key),
            Topic::Worktrees(dir) => self.worktrees_value(dir),
            Topic::Inventory
            | Topic::Changes(_)
            | Topic::Tree(_)
            | Topic::Fleet
            | Topic::Agents
            | Topic::Transcript(_) => self.interest_value(topic)?,
        })
    }

    fn start_background_work(&mut self) {
        self.start_schedules();
        self.refresh_registration();
    }
}

/// Topics every GUI follows without subscribing.
fn always_on(t: &Topic) -> bool {
    matches!(
        t,
        Topic::Gateway
            | Topic::Settings
            | Topic::Sessions
            | Topic::Running
            | Topic::Controllers
            | Topic::Notifications
            | Topic::Status
            | Topic::Usage
            | Topic::AppUpdate
            | Topic::Backup
            | Topic::OmpUpdate
            | Topic::Mobile
    )
}

/// A barrier raised over the link while no marker file exists (the CLI writes the file).
fn marker(purpose: MaintenancePurpose) -> MaintenanceMarker {
    MaintenanceMarker {
        purpose,
        since_unix_ms: super::unix_ms(),
        by_version: GATEWAY_VERSION.to_owned(),
    }
}
