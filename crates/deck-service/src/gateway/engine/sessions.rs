//! Managed sessions: launches (one at a time, the GUI's spawn pump), controllers and their
//! ticks, frontend views and their rights, closes, stops, kills and restarts, session-file
//! following, tab copies, live-control links and the manifest that lets a new owner re-attach
//! the backend sessions still alive (never launching a new omp for one that ended).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};

use deck_core::config::SessionView;
use deck_core::ctl::{
    CtlClient, CtlCommand, LiveSnapshot, ctl_dir, ctl_env, prepare_ctl_dir, with_ctl,
};
use deck_core::mux::{Launch, Mux, MuxKind};
use deck_core::status::{self as core_status, StatusTarget};
use deck_term::{ClipboardPolicy, TermOptions, Terminal, WakeCallback};
use serde::{Deserialize, Serialize};

use super::{Engine, Fin, View, ViewOp};
use crate::controller::{
    Delivery, ModelFacts, Rejected, RequestKey, SessionController, SessionKey, SessionTarget,
};
use crate::gateway::protocol::{
    Attach, BackendRef, ControllerInfo, Done, GatewayError, Hello, InputRefused, LaunchSpec,
    ModelFactsView, Outcome, Refusal, Role, ScreenView, ServerEvent, SessionRole, SwitchInfo, Tool,
    TopicValue, ViewEnd, ViewRole, Welcome,
};
use crate::gateway::server::{SideItem, ViewSide};
use crate::host_remote::HostRemote;
use crate::statusfmt::Activity;

/// The managed-session manifest in the identity's state directory.
pub(super) const MANIFEST: &str = "sessions.json";
/// A tab copy looks for its new session file this often…
const COPY_POLL: Duration = Duration::from_secs(1);
/// …for at most this long.
const COPY_TIMEOUT: Duration = Duration::from_secs(120);
/// The copy's file is at least this much younger than the launch request.
const COPY_SLACK: Duration = Duration::from_secs(2);
/// How long a restart waits for the killed backend session to disappear.
const RESTART_WAIT: Duration = Duration::from_secs(5);
/// How long a live-control command waits for the session's socket.
const CTL_CONNECT: Duration = Duration::from_secs(3);
/// Close polls while the controller still flushes.
const CLOSE_POLL: Duration = Duration::from_millis(50);
/// How often the sessions' live folders (terminal breadcrumbs) are re-read.
const LIVE_CWD_EVERY: Duration = Duration::from_secs(3);
/// A breadcrumb written this long before the pane started still belongs to its process.
const CRUMB_SLACK: Duration = Duration::from_secs(2);
/// How often a fresh session's reported (not yet written) file is looked for.
const FILE_RECHECK: Duration = Duration::from_secs(1);

/// Where a launch learns the backend that already holds its session name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Owner {
    /// The running snapshot: a live session keeps its backend.
    Snapshot,
    /// A startup re-attach keeps the backend recorded in the manifest, even while queued.
    Restored(MuxKind),
    /// A restart ended the old session (the snapshot may still list it until it refreshes):
    /// the session stays on this backend, or takes the ordinary choice for its view (`None`).
    Restarted(Option<MuxKind>),
}

/// One request to open a session.
pub(super) struct LaunchJob {
    pub(super) spec: LaunchSpec,
    /// The journaled mutation (`None`: a startup re-attach).
    pub(super) key: Option<RequestKey>,
    /// Use `spec.mux_name` as is (restart, re-attach) instead of resolving it.
    pub(super) exact: bool,
    /// Which backend the launch's name lives on.
    pub(super) owner: Owner,
    /// Startup re-attach: only if the backend session still exists.
    pub(super) restore: bool,
    pub(super) tool: Option<Tool>,
    /// A paired device's launch: its folder must still lead inside these when prepared.
    pub(super) confine: Option<crate::gateway::jobs::Confine>,
}

/// A launch prepared on a worker.
pub(crate) struct Prepared {
    job: LaunchJob,
    name: Option<String>,
    ctl_key: Option<String>,
    created: bool,
    mux: Option<Mux>,
    launch: Result<Launch, String>,
    /// The view the session runs in: what was asked for only where the backend can carry it.
    view: SessionView,
    /// A re-attach whose backend session ended meanwhile: dropped quietly.
    gone: bool,
}

pub(super) struct CopyWatch {
    src_id: String,
    since: SystemTime,
    deadline: Instant,
    next: Instant,
    busy: bool,
}

/// The live-control client of a session (while followed, or while a command waits on it).
pub(super) struct LiveLink {
    pub(super) client: CtlClient,
    queued: Vec<(RequestKey, CtlCommand, Instant)>,
    pending: HashMap<u64, RequestKey>,
    seen: (bool, Option<Arc<LiveSnapshot>>),
}

pub(super) struct Managed {
    pub(super) ctl: SessionController,
    pub(super) role: SessionRole,
    pub(super) label: String,
    pub(super) cwd: PathBuf,
    pub(super) mux: Option<Mux>,
    pub(super) mux_name: Option<String>,
    /// The view it runs in (see [`ControllerInfo::view`]); what a restart keeps.
    pub(super) view: SessionView,
    /// argv as requested (`omp` not yet resolved): what a restart runs again.
    argv: Vec<String>,
    pub(super) tool: Option<Tool>,
    pub(super) ctl_key: Option<String>,
    ctl_expect: bool,
    spawned_ms: u64,
    /// A journaled close waiting for the controller to settle.
    closing: Option<RequestKey>,
    /// The view owning focus, geometry, palette and the clipboard.
    controlling: Option<u64>,
    copy: Option<CopyWatch>,
    pub(super) live: Option<LiveLink>,
    model: Option<ModelFactsView>,
    model_at: Option<Instant>,
    /// The folder omp last reported in the session's breadcrumb (see [`read_live_cwd`]).
    live_cwd: Option<PathBuf>,
    /// The activity the Controllers topic last carried: under TSP omp's state comes from OSC
    /// 7501 reports and the document, not the title, so a change of it is its own reason to
    /// republish.
    published_activity: Activity,
}

impl Managed {
    pub(super) fn is_direct(&self) -> bool {
        self.mux.is_none()
    }

    pub(super) fn exited(&self) -> bool {
        self.ctl.terminal().is_exited()
    }

    /// The folder omp runs in now (its breadcrumb), else the one it was launched in.
    pub(super) fn current_cwd(&self) -> &Path {
        self.live_cwd.as_deref().unwrap_or(&self.cwd)
    }

    /// The folder omp last reported, if any.
    pub(super) fn live_cwd(&self) -> Option<&Path> {
        self.live_cwd.as_deref()
    }

    /// The view owning focus and geometry.
    pub(super) fn controlling(&self) -> Option<u64> {
        self.controlling
    }

    pub(super) fn display_label(&self) -> String {
        if !self.label.is_empty() {
            return self.label.clone();
        }
        self.ctl
            .terminal()
            .title()
            .filter(|t| !t.is_empty())
            .map_or_else(
                || {
                    self.cwd
                        .file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
                },
                str::to_owned,
            )
    }
}

/// A re-attachable backend session (safe metadata only: no argv, no environment).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ManifestEntry {
    kind: MuxKind,
    name: String,
    cwd: PathBuf,
    session_path: Option<PathBuf>,
    label: String,
    #[serde(default)]
    view: SessionView,
}

impl LaunchJob {
    fn restore_manifest_entry(&self) -> Option<ManifestEntry> {
        let Owner::Restored(kind) = self.owner else {
            return None;
        };
        if !self.restore {
            return None;
        }
        Some(ManifestEntry {
            kind,
            name: self.spec.mux_name.clone()?,
            cwd: self.spec.cwd.clone(),
            session_path: self.spec.session_path.clone(),
            label: self.spec.label.clone(),
            view: self.spec.view,
        })
    }
}

pub(super) struct Sessions {
    pub(super) managed: Vec<Managed>,
    launches: VecDeque<LaunchJob>,
    /// The ctl key of the launch being prepared (`Some(None)`: one without a key).
    launching: Option<Option<String>>,
    manifest: PathBuf,
    manifest_dirty: bool,
    restored: bool,
    direct_seq: u64,
    /// Accepted `Submit` mutations whose record has not reached a final delivery.
    pub(super) submits: HashMap<RequestKey, SessionKey>,
    last_running: Option<Arc<crate::gateway::running::RunningSnapshot>>,
    /// A live-folder read is running.
    cwd_busy: bool,
    cwd_next: Option<Instant>,
}

impl Sessions {
    pub(super) fn new(manifest: PathBuf) -> Sessions {
        Sessions {
            managed: Vec::new(),
            launches: VecDeque::new(),
            launching: None,
            manifest,
            manifest_dirty: false,
            restored: false,
            direct_seq: 0,
            submits: HashMap::new(),
            last_running: None,
            cwd_busy: false,
            cwd_next: None,
        }
    }

    pub(super) fn find(&self, key: &SessionKey) -> Option<usize> {
        self.managed.iter().position(|m| m.ctl.target().key == *key)
    }

    pub(super) fn launch_pending(&self) -> bool {
        self.launching.is_some() || !self.launches.is_empty()
    }

    /// The manifest is written again (a session's re-launch folder changed).
    pub(super) fn touch_manifest(&mut self) {
        self.manifest_dirty = true;
    }

    pub(super) fn busy_controllers(&self) -> usize {
        self.managed.iter().filter(|m| m.ctl.is_busy()).count()
    }

    /// Direct-PTY programs still running: each holds the owner until it exits.
    pub(super) fn direct_programs(&self) -> usize {
        self.managed
            .iter()
            .filter(|m| m.is_direct() && !m.exited())
            .count()
    }

    pub(super) fn closing(&self) -> bool {
        self.managed.iter().any(|m| m.closing.is_some())
    }

    pub(super) fn set_theme(&mut self, theme: deck_term::TermTheme) {
        for m in &mut self.managed {
            m.ctl.terminal_mut().set_theme(theme);
        }
    }

    /// Drops every controller: backend clients detach, the sessions keep running.
    pub(super) fn drop_all(&mut self) {
        self.managed.clear();
    }

    pub(super) fn queue(&mut self, job: LaunchJob) {
        self.launches.push_back(job);
    }

    /// Removes the queued launches `client` asked for (not one already being prepared) and
    /// returns their keys; the others keep their order.
    pub(super) fn take_launches_of(&mut self, client: &str) -> Vec<RequestKey> {
        let mut taken = Vec::new();
        self.launches.retain_mut(|j| match &j.key {
            Some(k) if k.client == client => {
                taken.extend(j.key.take());
                false
            }
            _ => true,
        });
        taken
    }
}

/// `omp` or `omp --resume <abs path>`.
pub(super) fn omp_argv(session_path: Option<&Path>) -> Vec<String> {
    let mut argv = vec!["omp".to_owned()];
    if let Some(p) = session_path {
        argv.push("--resume".to_owned());
        argv.push(p.to_string_lossy().into_owned());
    }
    argv
}

/// The session file a session follows given the files its process has open: the first one
/// while it knows none, or the only one when omp moved on (`/new`, `/fork`, `/resume`).
pub(super) fn followed_path<'a>(
    current: Option<&Path>,
    files: &'a [PathBuf],
) -> Option<&'a PathBuf> {
    match (current, files) {
        (None, [first, ..]) => Some(first),
        (Some(cur), [only]) if only != cur => Some(only),
        _ => None,
    }
}

#[cfg(unix)]
fn direct(cwd: &Path, argv: &[String], env: Vec<(String, String)>) -> Result<Launch, String> {
    // `direct_launch` carries the terminal environment, `PI_NOTIFICATIONS=off` included.
    let mut spec = deck_core::mux::direct_launch(cwd, argv);
    spec.env.extend(env);
    Ok(Launch::Pty(spec))
}

#[cfg(not(unix))]
fn direct(_cwd: &Path, _argv: &[String], _env: Vec<(String, String)>) -> Result<Launch, String> {
    Err("the OmpDeck session host (omp-deck-host.exe) was not found".to_owned())
}

fn open_terminal(
    launch: Launch,
    mux: Option<&Mux>,
    opts: TermOptions,
    wake: WakeCallback,
) -> Result<Terminal, String> {
    match launch {
        Launch::Host(spec) => {
            let Some(Mux::Host(host)) = mux else {
                return Err("the OmpDeck session host is not available".to_owned());
            };
            let remote = HostRemote::new(host.namespace().clone(), spec);
            Ok(Terminal::connect_headless(Box::new(remote), opts, wake))
        }
        #[cfg(unix)]
        Launch::Pty(spec) => {
            let spec = deck_term::SpawnSpec {
                program: spec.program,
                args: spec.args,
                cwd: spec.cwd,
                env: spec.env,
                tmux_client: matches!(mux, Some(Mux::Tmux(_))),
            };
            Terminal::spawn_headless(spec, opts, wake).map_err(|e| e.to_string())
        }
        #[cfg(not(unix))]
        Launch::Pty(_) => Err("local terminals are not available on this platform".to_owned()),
    }
}

/// A controller event to deliver, with the record it refers to.
struct Delivered {
    target: SessionTarget,
    event: crate::controller::ControllerEvent,
    record: Option<crate::controller::RequestRecord>,
}

impl Engine {
    // ---- lookup -------------------------------------------------------------------------------

    /// The index of the controller `t` addresses, if `t` is current.
    pub(super) fn check_target(&self, t: &SessionTarget) -> Result<usize, GatewayError> {
        if t.epoch != self.epoch {
            return Err(GatewayError::StaleEpoch {
                current: self.epoch,
            });
        }
        let Some(i) = self.sess.find(&t.key) else {
            return Err(GatewayError::Refused("that session is closed".to_owned()));
        };
        let cur = self.sess.managed[i].ctl.target();
        if cur.generation != t.generation {
            return Err(GatewayError::Rejected(Rejected::Stale {
                current: cur.clone(),
            }));
        }
        Ok(i)
    }

    pub(super) fn screen(&self, t: &SessionTarget) -> Result<ScreenView, GatewayError> {
        let i = self.check_target(t)?;
        let s = self.sess.managed[i].ctl.terminal().screen_text();
        Ok(ScreenView {
            lines: s.lines,
            cursor_row: s.cursor_row,
            cursor_col: s.cursor_col,
        })
    }

    pub(super) fn worktree_sessions(&self, path: &Path) -> Vec<(SessionTarget, String)> {
        self.sess
            .managed
            .iter()
            .filter(|m| !m.exited() && m.current_cwd().starts_with(path))
            .map(|m| (m.ctl.target().clone(), m.display_label()))
            .collect()
    }

    pub(super) fn controller_infos(&self) -> Vec<ControllerInfo> {
        let snap = self.running.latest();
        self.sess
            .managed
            .iter()
            .map(|m| {
                let target = m.ctl.target().clone();
                let views = self
                    .views
                    .values()
                    .filter(|v| v.target.key == target.key)
                    .count();
                let live_control = m.live.as_ref().is_some_and(|l| l.client.connected())
                    || m.ctl_key
                        .as_deref()
                        .zip(snap.as_deref())
                        .is_some_and(|(k, s)| s.has_ctl(k));
                ControllerInfo {
                    role: m.role.clone(),
                    label: m.display_label(),
                    cwd: m.cwd.clone(),
                    session_path: m.ctl.session_path().map(Path::to_path_buf),
                    backend: match (&m.mux, &m.mux_name) {
                        (Some(mux), Some(name)) => BackendRef::Mux {
                            kind: mux.kind(),
                            name: name.clone(),
                        },
                        _ => BackendRef::Direct,
                    },
                    title: m.ctl.terminal().title().map(str::to_owned),
                    activity: m.ctl.activity(),
                    exit_code: m.ctl.terminal().exit_code(),
                    status: m.ctl.status(false),
                    busy: m.ctl.is_busy(),
                    current_model: m.ctl.current_model(),
                    pending_switch: m.ctl.pending_switch().map(SwitchInfo::from),
                    assumed_switch: m.ctl.assumed_switch().map(SwitchInfo::from),
                    ctl_expect: m.ctl_expect,
                    live_control,
                    spawned_unix_ms: m.spawned_ms,
                    ctl_key: m.ctl_key.clone(),
                    live_cwd: m.live_cwd.clone(),
                    requests: m.ctl.requests().cloned().collect(),
                    views,
                    controlled: m.controlling.is_some(),
                    view: m.view,
                    target,
                }
            })
            .collect()
    }

    pub(super) fn live_value(&self, key: &SessionKey) -> TopicValue {
        match self.sess.find(key).map(|i| &self.sess.managed[i]) {
            Some(m) => {
                let link = m.live.as_ref();
                TopicValue::Live {
                    target: Some(m.ctl.target().clone()),
                    connected: link.is_some_and(|l| l.client.connected()),
                    snapshot: link
                        .and_then(|l| l.client.latest())
                        .map(|s| Box::new((*s).clone())),
                }
            }
            None => TopicValue::Live {
                target: None,
                connected: false,
                snapshot: None,
            },
        }
    }

    // ---- views --------------------------------------------------------------------------------

    pub(super) fn on_attach(
        &mut self,
        view: u64,
        hello: Hello,
        side: Arc<ViewSide>,
    ) -> Result<Welcome, Refusal> {
        if self.stopping {
            return Err(Refusal::ShuttingDown);
        }
        if !hello.profile.same_as(&self.profile) {
            return Err(Refusal::ProfileMismatch {
                served: self.profile.clone(),
            });
        }
        if let Some(m) = &self.barrier {
            return Err(Refusal::Maintenance(m.clone()));
        }
        if hello.role == Role::Supervisor {
            return Err(Refusal::NotAllowed(
                "the supervisor cannot attach to sessions".to_owned(),
            ));
        }
        let Some(Attach { target, view: role }) = hello.attach else {
            return Err(Refusal::NotAllowed("not a view".to_owned()));
        };
        self.attach_view(view, target, role, side, None)
            .map_err(|current| Refusal::Stale { current })?;
        Ok(self.welcome())
    }

    /// Registers view `view` of `target` (`owner`: the device link it belongs to). `Err` holds
    /// the session's current target when `target` is not current.
    pub(super) fn attach_view(
        &mut self,
        view: u64,
        target: SessionTarget,
        role: ViewRole,
        side: Arc<ViewSide>,
        owner: Option<u64>,
    ) -> Result<(), Option<SessionTarget>> {
        let found = self.sess.find(&target.key);
        let current = found.map(|i| self.sess.managed[i].ctl.target().clone());
        let Some(i) = found.filter(|_| target.epoch == self.epoch) else {
            return Err(current);
        };
        if current.as_ref() != Some(&target) {
            return Err(current);
        }
        let notify: deck_term::OutputNotify = {
            let weak: Weak<ViewSide> = Arc::downgrade(&side);
            Arc::new(move || {
                if let Some(s) = weak.upgrade() {
                    s.signal();
                }
            })
        };
        let m = &mut self.sess.managed[i];
        let sub = Arc::new(m.ctl.terminal().subscribe_output(notify));
        if role == ViewRole::Controlling {
            // The previous controlling view keeps typing; focus, geometry and the clipboard
            // move to the new one (a device has no clipboard: it gets focus and geometry).
            if let Some(prev) = m.controlling.replace(view)
                && let Some(pv) = self.views.get_mut(&prev)
            {
                pv.role = ViewRole::Input;
                pv.sub.set_clipboard_owner(false);
            }
            if owner.is_none() {
                sub.set_clipboard_owner(true);
            }
        }
        m.ctl.terminal().set_visible(true);
        side.set_subscription(sub.clone());
        self.views.insert(
            view,
            View {
                side,
                target,
                role,
                sub,
                owner,
            },
        );
        self.dirty.controllers = true;
        Ok(())
    }

    pub(super) fn on_view_op(&mut self, view: u64, op: ViewOp) {
        let Some(v) = self.views.get(&view) else {
            return;
        };
        let (target, role, side) = (v.target.clone(), v.role, v.side.clone());
        let Some(i) = self.sess.find(&target.key) else {
            return;
        };
        let controlling = self.sess.managed[i].controlling == Some(view);
        let m = &mut self.sess.managed[i];
        match op {
            ViewOp::Input(bytes, meta) => {
                if role == ViewRole::Observer {
                    side.push(SideItem::Refused(InputRefused::NotAllowed));
                    return;
                }
                // Validated against the view's target: a restarted or re-pathed session refuses
                // it (InputHeld is soft: the view goes on).
                if let Err(r) = m.ctl.write_input(&target, &bytes, meta) {
                    side.push(SideItem::Refused(InputRefused::Rejected(r)));
                }
            }
            ViewOp::Clipboard(bytes) => {
                if !controlling || m.ctl.terminal().write_frontend_reply(&bytes).is_err() {
                    side.push(SideItem::Refused(InputRefused::NoClipboardRead));
                }
            }
            ViewOp::Focus(focused) => {
                if controlling {
                    m.ctl.terminal_mut().set_focused(focused);
                }
            }
            ViewOp::Resize(cols, rows, cw, ch) => {
                if controlling {
                    m.ctl.terminal_mut().resize(cols, rows, cw, ch);
                }
            }
            ViewOp::Palette(p) => {
                if controlling {
                    m.ctl.terminal_mut().set_palette(p);
                }
            }
            ViewOp::Kill => {
                if role == ViewRole::Observer {
                    side.push(SideItem::Refused(InputRefused::NotAllowed));
                    return;
                }
                self.kill_session(i, None);
            }
        }
    }

    /// A view ended: it never closes the controller or cancels its commands.
    pub(super) fn on_view_gone(&mut self, view: u64) {
        let Some(v) = self.views.remove(&view) else {
            return;
        };
        if let Some(i) = self.sess.find(&v.target.key) {
            let others = self.views.values().any(|o| o.target.key == v.target.key);
            let m = &mut self.sess.managed[i];
            if m.controlling == Some(view) {
                m.controlling = None;
                m.ctl.terminal_mut().set_focused(false);
            }
            if !others {
                m.ctl.terminal().set_visible(false);
            }
        }
        self.dirty.controllers = true;
    }

    /// Ends every view of `session` with `end`.
    pub(super) fn end_views(&mut self, session: &SessionKey, end: ViewEnd) {
        self.views.retain(|_, v| {
            if v.target.key == *session {
                v.side.push(SideItem::Ended(end));
                false
            } else {
                true
            }
        });
        if let Some(i) = self.sess.find(session) {
            self.sess.managed[i].controlling = None;
        }
    }

    /// A client that left will never answer: its draft confirmations are declined.
    pub(super) fn decline_confirmations(&mut self, client: &str) {
        // Commands a worktree flow typed for this client are its to confirm too.
        let flows = self.worktree_owned(client);
        for m in &mut self.sess.managed {
            let target = m.ctl.target().clone();
            let ids: Vec<_> = m
                .ctl
                .requests()
                .filter(|r| {
                    (r.key.client == client || flows.contains(&r.key))
                        && matches!(r.delivery, Delivery::AwaitingConfirm { .. })
                })
                .map(|r| r.id)
                .collect();
            for id in ids {
                m.ctl.answer(&target, id, false);
            }
        }
        self.dirty.controllers = true;
    }

    /// Client `client` lost the right to run commands (a paired device revoked, or its
    /// `control` lowered): its controller requests that have not written a byte yet (draft
    /// confirmations included) and its queued launches are cancelled and journaled so. Work
    /// already writing, or a launch already starting, finishes.
    pub(super) fn cancel_client_work(&mut self, client: &str) {
        let mut cancelled = false;
        for m in &mut self.sess.managed {
            cancelled |= m.ctl.cancel_client(client);
        }
        let gone = self.sess.take_launches_of(client);
        if !gone.is_empty() {
            self.sync_ctl_keep();
        }
        for key in gone {
            self.job_finished(key, Err::<Box<Done>, _>(GatewayError::Cancelled));
        }
        self.cancel_worktree_work(client);
        if cancelled {
            // Their ends reach the journal and the clients on the next tick.
            self.dirty.controllers = true;
            self.waker.now();
        }
    }

    // ---- model observations -------------------------------------------------------------------

    pub(super) fn observe_model(
        &mut self,
        target: &SessionTarget,
        facts: ModelFactsView,
    ) -> Result<(), GatewayError> {
        let i = self.check_target(target)?;
        self.sess.managed[i].model = Some(facts);
        self.run_observe(i, Instant::now());
        Ok(())
    }

    fn run_observe(&mut self, i: usize, now: Instant) {
        let m = &mut self.sess.managed[i];
        let Some(f) = m.model.as_ref() else {
            return;
        };
        let progress = m.ctl.observe_model(
            now,
            ModelFacts {
                model: f.model.as_deref(),
                thinking: f.thinking.as_deref(),
                configured: f.configured.as_deref(),
                shown: f.shown.as_deref(),
                shown_is_default: f.shown_is_default,
            },
        );
        m.model_at = progress.wake_in.map(|d| now + d);
        let target = m.ctl.target().clone();
        if let Some(d) = progress.wake_in {
            self.wake_within(d);
        }
        if let Some(sw) = progress.unconfirmed {
            self.to_guis(&ServerEvent::ModelUnconfirmed {
                target,
                switch: SwitchInfo::from(&sw),
            });
        }
        self.dirty.controllers = true;
    }

    // ---- per step -----------------------------------------------------------------------------

    pub(super) fn step_sessions(&mut self, now: Instant) {
        self.restore_once();
        self.pump_launches();
        self.tick_controllers(now);
        self.poll_closes(now);
        self.follow_session_files();
        self.poll_copies(now);
        self.drive_live(now);
        self.update_status_target();
        self.poll_live_cwds(now);
        if self.sess.manifest_dirty {
            self.save_manifest();
        }
    }

    fn tick_controllers(&mut self, now: Instant) {
        let mut delivered: Vec<Delivered> = Vec::new();
        let mut exited: Vec<SessionKey> = Vec::new();
        let mut observe: Vec<usize> = Vec::new();
        let mut wake: Option<Duration> = None;
        let mut changed = false;
        for (i, m) in self.sess.managed.iter_mut().enumerate() {
            let tick = m.ctl.tick(now);
            let activity = m.ctl.activity();
            if tick.terminal.title_changed.is_some() || activity != m.published_activity {
                m.published_activity = activity;
                changed = true;
            }
            if tick.terminal.exited.is_some() {
                changed = true;
                exited.push(m.ctl.target().key.clone());
            }
            let target = m.ctl.target().clone();
            for event in tick.driven.events {
                changed = true;
                let id = match &event {
                    crate::controller::ControllerEvent::Confirm { id, .. }
                    | crate::controller::ControllerEvent::Ended { id, .. } => *id,
                };
                delivered.push(Delivered {
                    target: target.clone(),
                    record: m.ctl.request(id).cloned(),
                    event,
                });
            }
            if let Some(d) = tick.driven.wake_in {
                wake = Some(wake.map_or(d, |w| w.min(d)));
            }
            if m.model_at.is_some_and(|t| t <= now) {
                observe.push(i);
            } else if let Some(t) = m.model_at {
                let d = t.saturating_duration_since(now);
                wake = Some(wake.map_or(d, |w| w.min(d)));
            }
        }
        if let Some(d) = wake {
            self.wake_within(d);
        }
        if changed {
            self.dirty.controllers = true;
        }
        for i in observe {
            self.run_observe(i, now);
        }
        self.deliver(delivered);
        for key in exited {
            self.on_exited(&key);
        }
    }

    /// Confirmations go only to the client that submitted the request; a final delivery also
    /// finishes its journaled `Submit`.
    fn deliver(&mut self, delivered: Vec<Delivered>) {
        for d in delivered {
            let Some(record) = d.record else {
                continue;
            };
            // A worktree flow's typed command reports to the client that started the flow.
            let client = self
                .worktree
                .owner(&record.key)
                .unwrap_or(&record.key.client)
                .to_owned();
            self.to_client(
                &client,
                &ServerEvent::Controller {
                    target: d.target.clone(),
                    event: d.event.clone(),
                },
            );
            if !matches!(d.event, crate::controller::ControllerEvent::Ended { .. }) {
                continue;
            }
            self.worktree_request_ended(&record);
            if self.sess.submits.remove(&record.key).is_some() {
                let key = record.key.clone();
                self.finish_journal(&key, Outcome::Done(Box::new(Done::Submitted(record))));
            }
        }
    }

    fn on_exited(&mut self, key: &SessionKey) {
        let Some(i) = self.sess.find(key) else {
            return;
        };
        let m = &mut self.sess.managed[i];
        let target = m.ctl.target().clone();
        let tool = m.tool.clone();
        let copy_failed = m.copy.take().is_some();
        self.sess.manifest_dirty = true;
        if copy_failed {
            self.to_guis(&ServerEvent::CopyResult {
                target,
                result: Err("the copy ended before its session file appeared".to_owned()),
            });
        }
        if tool.is_some() {
            // An update or install run ended: pick up the (new) omp and its version.
            self.redetect_omp();
        }
    }

    fn poll_closes(&mut self, now: Instant) {
        let mut done: Vec<usize> = Vec::new();
        for (i, m) in self.sess.managed.iter_mut().enumerate() {
            if m.closing.is_some() {
                if m.ctl.can_close(now) {
                    done.push(i);
                } else {
                    self.soon = Some(
                        self.soon
                            .map_or(now + CLOSE_POLL, |s| s.min(now + CLOSE_POLL)),
                    );
                }
            }
        }
        for i in done.into_iter().rev() {
            let key = self.sess.managed[i].closing.clone();
            let session = self.remove_session(i, ViewEnd::Closed, now);
            if let Some(key) = key {
                self.job_finished(key, Ok(Done::Unit));
            }
            log::debug!("closed {session}");
        }
    }

    /// Removes controller `i`: its last events are delivered, its unfinished submits end with
    /// their record (or Cancelled), its views end with `end`.
    fn remove_session(&mut self, i: usize, end: ViewEnd, now: Instant) -> SessionKey {
        let mut m = self.sess.managed.remove(i);
        let target = m.ctl.target().clone();
        let driven = m.ctl.drive(now);
        let delivered = driven
            .events
            .into_iter()
            .map(|event| {
                let id = match &event {
                    crate::controller::ControllerEvent::Confirm { id, .. }
                    | crate::controller::ControllerEvent::Ended { id, .. } => *id,
                };
                Delivered {
                    target: target.clone(),
                    record: m.ctl.request(id).cloned(),
                    event,
                }
            })
            .collect();
        self.deliver(delivered);
        let pending: Vec<RequestKey> = self
            .sess
            .submits
            .iter()
            .filter(|(_, s)| **s == target.key)
            .map(|(k, _)| k.clone())
            .collect();
        for key in pending {
            self.sess.submits.remove(&key);
            let outcome = match m.ctl.request_by_key(&key) {
                Some(r) if r.delivery.is_final() => {
                    Outcome::Done(Box::new(Done::Submitted(r.clone())))
                }
                _ => Outcome::Cancelled,
            };
            self.finish_journal(&key, outcome);
        }
        // Views of the removed controller.
        self.views.retain(|_, v| {
            if v.target.key == target.key {
                v.side.push(SideItem::Ended(end));
                false
            } else {
                true
            }
        });
        // In-flight live-control commands of the session will never be answered.
        if let Some(link) = m.live.take() {
            for (key, _, _) in link.queued {
                self.job_failed(
                    key,
                    GatewayError::Failed("the session was closed".to_owned()),
                );
            }
            for (_, key) in link.pending {
                self.job_failed(
                    key,
                    GatewayError::Failed("the session was closed".to_owned()),
                );
            }
        }
        drop(m);
        self.sess.manifest_dirty = true;
        self.dirty.controllers = true;
        self.running.poke();
        self.sync_ctl_keep();
        target.key
    }

    // ---- session controls ---------------------------------------------------------------------

    /// Begins a journaled close: polled until the controller settled. A direct program is
    /// killed first when asked to.
    pub(super) fn begin_close(&mut self, i: usize, key: RequestKey, kill_direct: bool) {
        let m = &mut self.sess.managed[i];
        if kill_direct && m.is_direct() && !m.exited() {
            m.ctl.terminal_mut().kill();
        }
        m.closing = Some(key);
    }

    /// Ends this session's process only (`key`: the journaled Kill; `None`: a view's kill).
    pub(super) fn kill_session(&mut self, i: usize, key: Option<RequestKey>) {
        let m = &mut self.sess.managed[i];
        match (&m.mux, &m.mux_name) {
            (Some(mux), Some(name)) => {
                let (mux, name) = (mux.clone(), name.clone());
                self.spawn_worker("kill", move || {
                    let result = mux.kill(&name).map_err(|e| e.to_string());
                    match key {
                        Some(key) => Fin::Mutation {
                            key,
                            result: result
                                .map(|()| Box::new(Done::Unit))
                                .map_err(GatewayError::Failed),
                        },
                        None => Fin::Logged {
                            what: "kill",
                            result,
                        },
                    }
                });
            }
            _ => {
                m.ctl.terminal_mut().kill();
                if let Some(key) = key {
                    self.job_finished(key, Ok(Done::Unit));
                }
            }
        }
    }

    /// Graceful stop of this session's backend process only.
    pub(super) fn stop_session(&mut self, i: usize, key: RequestKey) {
        let m = &mut self.sess.managed[i];
        match &m.mux_name {
            Some(name) if m.mux.is_some() => {
                let (backends, name) = (self.backends.clone(), name.clone());
                self.spawn_worker("stop", move || {
                    let result = backends.stop(&name);
                    Fin::Mutation {
                        key,
                        result: result
                            .map(|()| Box::new(Done::Unit))
                            .map_err(GatewayError::Failed),
                    }
                });
            }
            // A direct child has no backend to stop gracefully: its PTY is hung up.
            _ => {
                m.ctl.terminal_mut().kill();
                self.job_finished(key, Ok(Done::Unit));
            }
        }
    }

    /// `view`: the view to relaunch in (`None`: the one the session runs in now).
    pub(super) fn begin_restart(&mut self, i: usize, key: RequestKey, view: Option<SessionView>) {
        let m = &mut self.sess.managed[i];
        let session = m.ctl.target().key.clone();
        match (&m.mux, &m.mux_name) {
            (Some(mux), Some(name)) => {
                let (mux, name) = (mux.clone(), name.clone());
                self.spawn_worker("restart", move || {
                    let result = mux.kill(&name).map_err(|e| e.to_string()).and_then(|()| {
                        let start = Instant::now();
                        while mux.has_session(&name) {
                            if start.elapsed() > RESTART_WAIT {
                                return Err(format!("{name} did not end"));
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        Ok(())
                    });
                    Fin::Restarted {
                        key,
                        session,
                        view,
                        result,
                    }
                });
            }
            _ => {
                m.ctl.terminal_mut().kill();
                self.on_restarted(key, session, view, Ok(()));
            }
        }
    }

    /// The old process is gone: the controller is replaced by a launch of the same identity.
    pub(super) fn on_restarted(
        &mut self,
        key: RequestKey,
        session: SessionKey,
        view: Option<SessionView>,
        result: Result<(), String>,
    ) {
        if let Err(e) = result {
            self.job_failed(key, GatewayError::Failed(format!("restart: {e}")));
            return;
        }
        let Some(i) = self.sess.find(&session) else {
            self.job_failed(
                key,
                GatewayError::Failed("the session was closed".to_owned()),
            );
            return;
        };
        let m = &self.sess.managed[i];
        let argv = match m.role {
            SessionRole::Session => omp_argv(m.ctl.session_path()),
            SessionRole::Tool => m.argv.clone(),
        };
        let launch_view = match m.role {
            SessionRole::Session => view.unwrap_or(m.view),
            SessionRole::Tool => SessionView::Terminal,
        };
        let job = LaunchJob {
            spec: LaunchSpec {
                mux_name: m.mux_name.clone(),
                cwd: m.cwd.clone(),
                session_path: m.ctl.session_path().map(Path::to_path_buf),
                label: m.label.clone(),
                argv,
                role: m.role.clone(),
                copy_of: None,
                view: launch_view,
            },
            key: Some(key),
            exact: true,
            owner: Owner::Restarted(match launch_view {
                SessionView::Chat => None,
                SessionView::Terminal => m.mux.as_ref().map(Mux::kind),
            }),
            restore: false,
            tool: m.tool.clone(),
            confine: None,
        };
        self.remove_session(i, ViewEnd::Stale, Instant::now());
        self.sess.queue(job);
        self.pump_launches();
    }

    pub(super) fn set_session_path(&mut self, i: usize, path: Option<PathBuf>) -> SessionTarget {
        let m = &mut self.sess.managed[i];
        let key = m.ctl.target().key.clone();
        if m.ctl.set_session_path(path) {
            self.end_views(&key, ViewEnd::Stale);
        }
        self.sess.manifest_dirty = true;
        self.dirty.controllers = true;
        self.sess.managed[i].ctl.target().clone()
    }

    /// Queues a live-control command (it waits up to [`CTL_CONNECT`] for the socket).
    pub(super) fn queue_ctl(&mut self, i: usize, key: RequestKey, cmd: CtlCommand) {
        self.ensure_live(i);
        if let Some(link) = &mut self.sess.managed[i].live {
            link.queued.push((key, cmd, Instant::now() + CTL_CONNECT));
        }
        self.waker.now();
    }

    fn ensure_live(&mut self, i: usize) {
        let m = &mut self.sess.managed[i];
        if m.live.is_some() {
            return;
        }
        let Some(ctl_key) = m.ctl_key.as_deref() else {
            return;
        };
        let w = self.waker.clone();
        let client = CtlClient::spawn(ctl_dir(&self.ctl_ns, ctl_key), move || w.now());
        m.live = Some(LiveLink {
            client,
            queued: Vec::new(),
            pending: HashMap::new(),
            seen: (false, None),
        });
    }

    fn drive_live(&mut self, now: Instant) {
        let wanted: HashSet<SessionKey> = self
            .conns
            .values()
            .flat_map(|c| c.subs.iter())
            .filter_map(|t| match t {
                crate::gateway::protocol::Topic::Live(k) => Some(k.clone()),
                _ => None,
            })
            .collect();
        let mut finished: Vec<(RequestKey, Result<Done, GatewayError>)> = Vec::new();
        let mut changed: Vec<SessionKey> = Vec::new();
        let mut resolved = false;
        for i in 0..self.sess.managed.len() {
            let key = self.sess.managed[i].ctl.target().key.clone();
            // A session whose file is still unknown is followed over live control until omp
            // reports a written one: omp holds its jsonl open only between appends and drops the
            // handle on every atomic rewrite (the title, in the first turn), so the open-file poll
            // in `follow_session_files` can miss a fresh session for good — and its pushes and
            // live entries would carry no session.
            let unresolved = {
                let m = &self.sess.managed[i];
                m.role == SessionRole::Session
                    && m.copy.is_none()
                    && !m.exited()
                    && m.ctl.session_path().is_none()
            };
            if wanted.contains(&key) || unresolved {
                self.ensure_live(i);
            }
            let m = &mut self.sess.managed[i];
            let busy = m
                .live
                .as_ref()
                .is_some_and(|l| !l.queued.is_empty() || !l.pending.is_empty());
            if !wanted.contains(&key) && !busy && !unresolved {
                if m.live.take().is_some() {
                    changed.push(key);
                }
                continue;
            }
            let Some(link) = &mut m.live else {
                continue;
            };
            let connected = link.client.connected();
            if connected {
                for (rk, cmd, _) in std::mem::take(&mut link.queued) {
                    let rid = link.client.send(cmd);
                    link.pending.insert(rid, rk);
                }
            } else {
                let (expired, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut link.queued)
                    .into_iter()
                    .partition(|(_, _, d)| *d <= now);
                link.queued = waiting;
                for (rk, _, _) in expired {
                    finished.push((
                        rk,
                        Err(GatewayError::Failed(
                            "could not reach the session's control socket".to_owned(),
                        )),
                    ));
                }
                if let Some(d) = link.queued.iter().map(|(_, _, d)| *d).min() {
                    self.soon = Some(self.soon.map_or(d, |s| s.min(d)));
                }
            }
            for (rid, reply) in link.client.take_replies() {
                if let Some(rk) = link.pending.remove(&rid) {
                    finished.push((rk, Ok(Done::Ctl(reply))));
                }
            }
            let latest = link.client.latest();
            // omp writes a fresh session's file only when its first reply lands; until then
            // the reported path is checked again shortly (the snapshot may not change).
            let reported = unresolved
                .then(|| latest.as_ref().and_then(|s| s.session_file.as_ref()))
                .flatten();
            if let Some(path) = reported {
                if path.is_file() {
                    // The first file of a fresh session: never a generation change.
                    m.ctl.set_session_path(Some(path.clone()));
                    resolved = true;
                } else {
                    let d = now + FILE_RECHECK;
                    self.soon = Some(self.soon.map_or(d, |s| s.min(d)));
                }
            }
            let same = link.seen.0 == connected
                && match (&link.seen.1, &latest) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    (None, None) => true,
                    _ => false,
                };
            if !same {
                link.seen = (connected, latest);
                changed.push(key);
            }
        }
        for (key, result) in finished {
            let answer_ui = self.jobs.get(&key).is_some_and(|j| j.label == "answer-ui");
            let result = if answer_ui {
                answer_ui_result(result)
            } else {
                result
            };
            self.job_finished(key, result);
        }
        if resolved {
            self.sess.manifest_dirty = true;
            self.dirty.controllers = true;
        }
        for key in changed {
            self.changed(crate::gateway::protocol::Topic::Live(key));
            self.dirty.controllers = true;
        }
    }

    // ---- session files and copies -------------------------------------------------------------

    fn follow_session_files(&mut self) {
        let Some(snap) = self.running.latest() else {
            return;
        };
        if self
            .sess
            .last_running
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, &snap))
        {
            return;
        }
        self.sess.last_running = Some(snap.clone());
        let mut stale: Vec<SessionKey> = Vec::new();
        for m in &mut self.sess.managed {
            if m.role != SessionRole::Session || m.copy.is_some() || m.exited() {
                continue;
            }
            let Some(name) = m.mux_name.as_deref() else {
                continue;
            };
            let Some(p) = followed_path(m.ctl.session_path(), snap.files_of(name)).cloned() else {
                continue;
            };
            if m.ctl.set_session_path(Some(p)) {
                stale.push(m.ctl.target().key.clone());
            }
            self.sess.manifest_dirty = true;
            self.dirty.controllers = true;
        }
        for key in stale {
            self.end_views(&key, ViewEnd::Stale);
        }
    }

    fn poll_copies(&mut self, now: Instant) {
        let mut timed_out: Vec<SessionTarget> = Vec::new();
        let mut polls: Vec<(SessionKey, u64, String, SystemTime)> = Vec::new();
        for m in &mut self.sess.managed {
            let Some(c) = &mut m.copy else {
                continue;
            };
            if c.busy {
                continue;
            }
            if now >= c.deadline {
                m.copy = None;
                timed_out.push(m.ctl.target().clone());
                continue;
            }
            if now >= c.next {
                c.busy = true;
                let t = m.ctl.target();
                polls.push((t.key.clone(), t.generation, c.src_id.clone(), c.since));
            } else {
                let next = c.next;
                self.soon = Some(self.soon.map_or(next, |s| s.min(next)));
            }
        }
        for target in timed_out {
            self.to_guis(&ServerEvent::CopyResult {
                target,
                result: Err("the copied session file did not appear in time".to_owned()),
            });
        }
        for (session, generation, src, since) in polls {
            let root = self.root.clone();
            self.spawn_worker("copy", move || Fin::CopyFound {
                path: deck_core::sessions::find_fork(&root, &src, since),
                session,
                generation,
            });
        }
    }

    pub(super) fn on_copy_found(
        &mut self,
        session: &SessionKey,
        generation: u64,
        path: Option<PathBuf>,
    ) {
        let Some(i) = self.sess.find(session) else {
            return;
        };
        let m = &mut self.sess.managed[i];
        if m.ctl.target().generation != generation {
            return;
        }
        let Some(c) = &mut m.copy else {
            return;
        };
        c.busy = false;
        c.next = Instant::now() + COPY_POLL;
        let Some(path) = path else {
            return;
        };
        m.copy = None;
        let target = self.set_session_path(i, Some(path.clone()));
        self.to_guis(&ServerEvent::CopyResult {
            target,
            result: Ok(path),
        });
    }

    fn update_status_target(&mut self) {
        let gui = self
            .conns
            .values()
            .filter(|c| c.is(Role::Gui) && c.state.active.is_some())
            .max_by_key(|c| (c.state.focused, c.seq));
        let found = gui
            .and_then(|c| c.state.active.as_ref())
            .and_then(|t| self.sess.find(&t.key));
        let target = found.map(|i| self.sess.managed[i].ctl.target().clone());
        if target == self.status_target {
            return;
        }
        let status = found.map(|i| {
            let m = &self.sess.managed[i];
            StatusTarget {
                mux: m.mux.clone().zip(m.mux_name.clone()),
                session: m.ctl.session_path().map(Path::to_path_buf),
                cwd: m.cwd.clone(),
            }
        });
        self.status.set_target(status);
        self.status_target = target;
    }

    // ---- live folders -------------------------------------------------------------------------

    /// Re-reads each backend session's breadcrumb on a worker every [`LIVE_CWD_EVERY`].
    fn poll_live_cwds(&mut self, now: Instant) {
        if self.sess.cwd_busy {
            return;
        }
        if let Some(next) = self.sess.cwd_next
            && now < next
        {
            self.soon = Some(self.soon.map_or(next, |s| s.min(next)));
            return;
        }
        self.sess.cwd_next = Some(now + LIVE_CWD_EVERY);
        let mut probes: Vec<CwdProbe> = Vec::new();
        let mut cleared = false;
        for m in &mut self.sess.managed {
            match (&m.mux, &m.mux_name) {
                (Some(mux), Some(name)) if !m.exited() => {
                    let t = m.ctl.target();
                    probes.push(CwdProbe {
                        session: t.key.clone(),
                        generation: t.generation,
                        mux: mux.clone(),
                        name: name.clone(),
                        session_path: m.ctl.session_path().map(Path::to_path_buf),
                    });
                }
                _ => cleared |= m.live_cwd.take().is_some(),
            }
        }
        if cleared {
            self.dirty.controllers = true;
        }
        if probes.is_empty() {
            return;
        }
        self.sess.cwd_busy = true;
        let agent_dir = self.root.agent_dir();
        self.spawn_worker("live-cwd", move || {
            Fin::LiveCwds(
                probes
                    .into_iter()
                    .map(|p| {
                        let cwd = read_live_cwd(&p, &agent_dir);
                        (p.session, p.generation, cwd)
                    })
                    .collect(),
            )
        });
    }

    pub(super) fn on_live_cwds(&mut self, found: Vec<(SessionKey, u64, Option<PathBuf>)>) {
        self.sess.cwd_busy = false;
        for (session, generation, cwd) in found {
            let Some(i) = self.sess.find(&session) else {
                continue;
            };
            let m = &mut self.sess.managed[i];
            if m.ctl.target().generation == generation && m.live_cwd != cwd {
                m.live_cwd = cwd;
                self.dirty.controllers = true;
            }
        }
    }

    // ---- launches -----------------------------------------------------------------------------

    /// The backend a launch of `name` in `view` runs on (see [`Backends::for_launch`]).
    fn mux_for(
        &self,
        snap: &crate::gateway::running::RunningSnapshot,
        job: &LaunchJob,
        name: &str,
    ) -> Option<Mux> {
        let owner = match job.owner {
            Owner::Snapshot => snap.owner_of(name),
            Owner::Restored(kind) => Some(kind),
            Owner::Restarted(kept) => kept,
        };
        self.backends.for_launch(owner, job.spec.view).cloned()
    }

    /// Removes the exited controllers of session file `path` (or backend session `name`), so a
    /// launch for it starts a process instead of resolving to the dead one in `existing_for`.
    pub(super) fn drop_exited_for(&mut self, path: &Path, name: Option<&str>) {
        let now = Instant::now();
        let dead: Vec<usize> = self
            .sess
            .managed
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.exited()
                    && (m.ctl.session_path() == Some(path)
                        || (name.is_some() && m.mux_name.as_deref() == name))
            })
            .map(|(i, _)| i)
            .collect();
        for i in dead.into_iter().rev() {
            let session = self.remove_session(i, ViewEnd::Closed, now);
            log::debug!("replacing the exited {session}");
        }
    }

    fn existing_for(
        &self,
        spec: &LaunchSpec,
        name: Option<&str>,
    ) -> Option<(SessionTarget, SessionView)> {
        self.sess
            .managed
            .iter()
            .find(|m| {
                (spec.session_path.is_some()
                    && m.ctl.session_path() == spec.session_path.as_deref())
                    || (name.is_some() && m.mux_name.as_deref() == name)
            })
            .map(|m| (m.ctl.target().clone(), m.view))
    }

    /// Starts the next queued launch once a running snapshot exists (names resolve against it).
    pub(super) fn pump_launches(&mut self) {
        if self.sess.launching.is_some() {
            return;
        }
        let Some(snap) = self.running.latest() else {
            return;
        };
        while let Some(mut job) = self.sess.launches.pop_front() {
            if job.spec.copy_of.is_none()
                && let Some((t, view)) = self.existing_for(&job.spec, job.spec.mux_name.as_deref())
            {
                if let Some(key) = job.key.take() {
                    self.job_finished(key, Ok(Done::Session(t, view)));
                }
                continue;
            }
            let mut name = job.spec.mux_name.clone();
            if !job.exact
                && let Some(pref) = name.as_deref()
            {
                name = Some(
                    snap.resolve_for(
                        self.running.is_blind(),
                        pref,
                        job.spec.session_path.as_deref(),
                        u128::from(crate::gateway::unix_ms()),
                    )
                    .into_name(),
                );
            }
            if name.is_none() && !cfg!(unix) {
                // No direct children here: a tool runs in a host session of its own.
                name = Some(format!("omp-tool-{}", crate::gateway::unix_ms()));
            }
            let mux = name.as_deref().and_then(|n| self.mux_for(&snap, &job, n));
            if mux.is_none() {
                name = None;
            }
            if job.restore && name.is_none() {
                continue;
            }
            if job.spec.copy_of.is_none()
                && let Some(n) = name.as_deref()
                && let Some((t, view)) = self.existing_for(&job.spec, Some(n))
            {
                if let Some(key) = job.key.take() {
                    self.job_finished(key, Ok(Done::Session(t, view)));
                }
                continue;
            }
            let wants_ctl = self.ctl_ext.is_some() && job.spec.role == SessionRole::Session;
            let ctl_key = wants_ctl.then(|| {
                name.clone().unwrap_or_else(|| {
                    self.sess.direct_seq += 1;
                    format!("direct-{}-{}", std::process::id(), self.sess.direct_seq)
                })
            });
            self.sess.launching = Some(ctl_key.clone());
            self.sync_ctl_keep();
            let ext = self.ctl_ext.clone();
            let ns = self.ctl_ns.clone();
            let omp = self.omp.as_ref().map(|o| o.program().to_path_buf());
            self.spawn_worker("launch", move || {
                Fin::Prepared(Box::new(prepare(job, name, ctl_key, mux, ns, ext, omp)))
            });
            return;
        }
    }

    pub(super) fn on_prepared(&mut self, p: Prepared) {
        self.sess.launching = None;
        let Prepared {
            mut job,
            name,
            ctl_key,
            created,
            mux,
            launch,
            view,
            gone,
        } = p;
        if gone {
            log::info!("not re-attaching {name:?}: the session ended");
            self.sess.manifest_dirty = true;
            self.sync_ctl_keep();
            self.pump_launches();
            return;
        }
        let opts = TermOptions {
            scrollback: 10_000,
            theme: crate::gateway::terminal_theme(self.cfg.theme, self.cfg.style),
            clipboard: self.clipboard.clone(),
            // ConPTY has no kitty keyboard protocol; the host answers `CSI ? u` there.
            kitty_keyboard: !cfg!(windows),
        };
        let wake: WakeCallback = {
            let w = self.waker.clone();
            Arc::new(move |d: Duration| w.after(d))
        };
        match launch.and_then(|l| open_terminal(l, mux.as_ref(), opts, wake)) {
            Err(e) => {
                let err = GatewayError::Failed(format!("could not start the terminal: {e}"));
                match job.key.take() {
                    Some(key) => self.job_failed(key, err),
                    None => log::warn!("re-attaching {name:?} failed: {e}"),
                }
            }
            Ok(mut term) => {
                // Clipboard reads and writes go to the controlling view, decided before the
                // first poll.
                term.set_clipboard_policy(ClipboardPolicy::Frontend);
                term.set_visible(false);
                let key = match (&mux, &name) {
                    (Some(m), Some(n)) => SessionKey::backend(m, n),
                    _ => SessionKey::direct(),
                };
                let ctl = SessionController::new(key, term, job.spec.session_path.clone());
                let published_activity = ctl.activity();
                let target = ctl.target().clone();
                let now = Instant::now();
                let copy = job.spec.copy_of.clone().map(|src_id| CopyWatch {
                    src_id,
                    since: SystemTime::now()
                        .checked_sub(COPY_SLACK)
                        .unwrap_or(SystemTime::UNIX_EPOCH),
                    deadline: now + COPY_TIMEOUT,
                    next: now + COPY_POLL,
                    busy: false,
                });
                let LaunchSpec {
                    cwd,
                    label,
                    argv,
                    role,
                    ..
                } = job.spec;
                self.sess.managed.push(Managed {
                    ctl,
                    role,
                    label,
                    cwd,
                    mux,
                    mux_name: name,
                    argv,
                    view,
                    tool: job.tool,
                    // Attaching ignores the argv: only a created session opens the socket.
                    ctl_expect: ctl_key.is_some() && created,
                    ctl_key,
                    spawned_ms: crate::gateway::unix_ms(),
                    closing: None,
                    controlling: None,
                    copy,
                    live: None,
                    model: None,
                    model_at: None,
                    live_cwd: None,
                    published_activity,
                });
                if let Some(key) = job.key {
                    self.job_finished(key, Ok(Done::Session(target, view)));
                }
                self.sess.manifest_dirty = true;
                self.dirty.controllers = true;
                self.running.poke();
            }
        }
        self.sync_ctl_keep();
        self.pump_launches();
    }

    /// Control dirs the running monitor's prune keeps: open sessions and pending launches.
    fn sync_ctl_keep(&self) {
        let keep: HashSet<String> = self
            .sess
            .managed
            .iter()
            .filter_map(|m| m.ctl_key.clone())
            .chain(self.sess.launching.clone().flatten())
            .chain(
                self.sess
                    .launches
                    .iter()
                    .filter_map(|j| j.spec.mux_name.clone()),
            )
            .collect();
        self.running.set_ctl_keep(keep);
    }

    // ---- manifest -----------------------------------------------------------------------------

    /// Once the first running snapshot exists: re-attach the manifest's sessions that are
    /// still alive. Attach-only: a session that ended is dropped, never relaunched.
    fn restore_once(&mut self) {
        if self.sess.restored {
            return;
        }
        let Some(snap) = self.running.latest() else {
            return;
        };
        self.sess.restored = true;
        let entries: Vec<ManifestEntry> = match std::fs::read(&self.sess.manifest) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                log::warn!("ignoring an unreadable session manifest: {e}");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        let blind = self.running.is_blind();
        for e in entries {
            let alive = blind
                || (snap.is_alive(&e.name) && snap.owner_of(&e.name).is_none_or(|k| k == e.kind));
            if !alive || self.backends.of_kind(e.kind).is_none() {
                continue;
            }
            self.sess.queue(LaunchJob {
                spec: LaunchSpec {
                    mux_name: Some(e.name),
                    argv: omp_argv(e.session_path.as_deref()),
                    cwd: e.cwd,
                    session_path: e.session_path,
                    label: e.label,
                    role: SessionRole::Session,
                    copy_of: None,
                    view: e.view,
                },
                key: None,
                exact: true,
                owner: Owner::Restored(e.kind),
                restore: true,
                tool: None,
                confine: None,
            });
        }
        self.sess.manifest_dirty = true;
    }

    pub(super) fn save_manifest(&mut self) {
        if !self.sess.restored {
            // Never overwrite what the next restore needs before it ran.
            return;
        }
        self.sess.manifest_dirty = false;
        let mut entries: Vec<ManifestEntry> = self
            .sess
            .managed
            .iter()
            .filter(|m| m.role == SessionRole::Session && !m.exited())
            .filter_map(|m| {
                Some(ManifestEntry {
                    kind: m.mux.as_ref()?.kind(),
                    name: m.mux_name.clone()?,
                    cwd: m.cwd.clone(),
                    session_path: m.ctl.session_path().map(Path::to_path_buf),
                    label: m.label.clone(),
                    view: m.view,
                })
            })
            .collect();
        // Queued re-attaches that have not run yet stay remembered.
        entries.extend(
            self.sess
                .launches
                .iter()
                .filter_map(LaunchJob::restore_manifest_entry),
        );
        match serde_json::to_vec_pretty(&entries) {
            Ok(bytes) => {
                if let Err(e) = crate::gateway::write_private(&self.sess.manifest, &bytes) {
                    log::warn!("saving the session manifest failed: {e}");
                }
            }
            Err(e) => log::warn!("encoding the session manifest failed: {e}"),
        }
    }
}

/// What a live-folder read needs of one session.
struct CwdProbe {
    session: SessionKey,
    generation: u64,
    mux: Mux,
    name: String,
    session_path: Option<PathBuf>,
}

/// The result of an `AnswerUi` from the extension's reply: answered, already gone (the dialog
/// closed meanwhile), or refused with its reason.
fn answer_ui_result(result: Result<Done, GatewayError>) -> Result<Done, GatewayError> {
    match result {
        Ok(Done::Ctl(reply)) if reply.ok => Ok(Done::Answered(true)),
        Ok(Done::Ctl(reply)) if reply.error.as_deref() == Some("no such request") => {
            Ok(Done::Answered(false))
        }
        Ok(Done::Ctl(reply)) => {
            Err(GatewayError::Failed(reply.error.unwrap_or_else(|| {
                "the session refused the answer".to_owned()
            })))
        }
        other => other,
    }
}

/// The folder omp wrote to the session's terminal breadcrumb, when the breadcrumb belongs to
/// the pane's current process (written since it started) or names the session's own file (not
/// a previous owner of the pty). Blocking.
fn read_live_cwd(p: &CwdProbe, agent_dir: &Path) -> Option<PathBuf> {
    let pane = p.mux.pane(&p.name)?;
    let crumb = core_status::read_breadcrumb(agent_dir, &pane.breadcrumb_key)?;
    let written = std::fs::metadata(core_status::breadcrumb_path(
        agent_dir,
        &pane.breadcrumb_key,
    ))
    .and_then(|m| m.modified())
    .ok()?;
    let fresh = written + CRUMB_SLACK >= pane.started;
    (fresh || p.session_path.as_deref() == Some(crumb.session.as_path())).then_some(crumb.cwd)
}

/// The view a launch really gets. Chat needs TSP, which only the OmpDeck session host carries:
/// on any other backend (or none) the session is a terminal. A host session that is created now
/// starts as asked; one that is already alive keeps the mode it was created with (`live_tsp`,
/// from its daemon; unknown counts as a terminal).
fn effective_view(
    wanted: SessionView,
    backend: Option<MuxKind>,
    created: bool,
    live_tsp: Option<bool>,
) -> SessionView {
    if backend != Some(MuxKind::Host) {
        return SessionView::Terminal;
    }
    match (created, live_tsp) {
        (true, _) => wanted,
        (false, Some(true)) => SessionView::Chat,
        (false, _) => SessionView::Terminal,
    }
}

/// Worker: control dir and environment, the resolved argv and the backend launch.
fn prepare(
    mut job: LaunchJob,
    name: Option<String>,
    ctl_key: Option<String>,
    mux: Option<Mux>,
    ns: String,
    ext: Option<PathBuf>,
    omp: Option<PathBuf>,
) -> Prepared {
    let created = match (&mux, &name) {
        (Some(m), Some(n)) => !m.has_session(n),
        _ => true,
    };
    // Only an omp session has a chat view; a tool run is always a terminal.
    let wanted = match job.spec.role {
        SessionRole::Session => job.spec.view,
        SessionRole::Tool => SessionView::Terminal,
    };
    let live_tsp = match (&mux, &name) {
        (Some(Mux::Host(h)), Some(n)) if !created => h.namespace().info(n).ok().map(|i| i.tsp),
        _ => None,
    };
    let view = effective_view(wanted, mux.as_ref().map(Mux::kind), created, live_tsp);
    if job.restore && created {
        return Prepared {
            job,
            name,
            ctl_key: None,
            created,
            mux,
            launch: Err("the session ended".to_owned()),
            view,
            gone: true,
        };
    }
    // A device's folder must still lead inside its roots here, where it is used.
    if let Some(c) = &job.confine {
        match c.admit(&job.spec.cwd) {
            Ok(cwd) => job.spec.cwd = cwd,
            Err(e) => {
                return Prepared {
                    job,
                    name,
                    ctl_key: None,
                    created,
                    mux,
                    launch: Err(e.to_string()),
                    view,
                    gone: false,
                };
            }
        }
    }
    let ctl_key = ctl_key.and_then(|key| match prepare_ctl_dir(&ctl_dir(&ns, &key)) {
        Ok(()) => Some(key),
        Err(e) => {
            log::warn!("live control unavailable for {key}: {e}");
            None
        }
    });
    let (ctl_key, env) = match ctl_key {
        Some(key) => match ctl_env(&ctl_dir(&ns, &key)) {
            Ok(vars) => (
                Some(key),
                vars.into_iter()
                    .map(|(k, v)| (k.to_owned(), v.to_string_lossy().into_owned()))
                    .collect(),
            ),
            Err(e) => {
                log::warn!("live control unavailable for {key}: {e}");
                (None, Vec::new())
            }
        },
        None => (None, Vec::new()),
    };
    let mut argv = job.spec.argv.clone();
    // The detected omp may live outside PATH (`~/.local/bin`).
    if let (Some(first), Some(omp)) = (argv.first_mut(), &omp)
        && *first == "omp"
    {
        *first = omp.to_string_lossy().into_owned();
    }
    let argv = match (&ctl_key, &ext) {
        (Some(_), Some(ext)) => with_ctl(&argv, ext),
        _ => argv,
    };
    let launch = match (&mux, &name) {
        (Some(m), Some(n)) => {
            let mut launch = m.launch_with_env(n, &job.spec.cwd, &argv, &env);
            if let Launch::Host(spec) = &mut launch {
                spec.tsp = view == SessionView::Chat;
            }
            Ok(launch)
        }
        _ => direct(&job.spec.cwd, &argv, env),
    };
    Prepared {
        job,
        name,
        ctl_key,
        created,
        mux,
        launch,
        view,
        gone: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ui_answer_reply_becomes_answered_gone_or_refused() {
        let reply = |ok: bool, error: Option<&str>| {
            Ok(Done::Ctl(deck_core::ctl::CtlReply {
                ok,
                error: error.map(str::to_owned),
                new_file: None,
                leaf: None,
            }))
        };
        assert!(matches!(
            answer_ui_result(reply(true, None)),
            Ok(Done::Answered(true))
        ));
        assert!(matches!(
            answer_ui_result(reply(false, Some("no such request"))),
            Ok(Done::Answered(false))
        ));
        assert!(matches!(
            answer_ui_result(reply(false, Some("bad value"))),
            Err(GatewayError::Failed(m)) if m == "bad value"
        ));
        assert!(matches!(
            answer_ui_result(reply(false, None)),
            Err(GatewayError::Failed(m)) if m == "the session refused the answer"
        ));
        assert!(matches!(
            answer_ui_result(Err(GatewayError::Cancelled)),
            Err(GatewayError::Cancelled)
        ));
    }

    #[test]
    fn a_client_losing_its_rights_loses_only_its_own_queued_launches() {
        let job = |label: &str, key: Option<(&str, u64)>| LaunchJob {
            spec: LaunchSpec {
                mux_name: None,
                cwd: PathBuf::from("/p"),
                session_path: None,
                label: label.into(),
                argv: omp_argv(None),
                role: SessionRole::Session,
                copy_of: None,
                view: SessionView::Terminal,
            },
            key: key.map(|(client, seq)| RequestKey {
                client: client.into(),
                seq,
            }),
            exact: false,
            owner: Owner::Snapshot,
            restore: false,
            tool: None,
            confine: None,
        };
        let mut sess = Sessions::new(PathBuf::from("/nowhere/sessions.json"));
        sess.queue(job("phone 1", Some(("d-phone", 1))));
        sess.queue(job("desk", Some(("gui", 1))));
        sess.queue(job("re-attach", None));
        sess.queue(job("phone 2", Some(("d-phone", 2))));
        let taken = sess.take_launches_of("d-phone");
        assert_eq!(
            taken
                .iter()
                .map(|k| (k.client.as_str(), k.seq))
                .collect::<Vec<_>>(),
            [("d-phone", 1), ("d-phone", 2)]
        );
        let left: Vec<&str> = sess
            .launches
            .iter()
            .map(|j| j.spec.label.as_str())
            .collect();
        assert_eq!(left, ["desk", "re-attach"]);
        assert!(sess.take_launches_of("d-phone").is_empty());
    }

    #[test]
    fn a_session_follows_its_only_open_file_and_adopts_the_first_when_unknown() {
        let a = PathBuf::from("/s/a.jsonl");
        let b = PathBuf::from("/s/b.jsonl");
        assert_eq!(followed_path(None, &[a.clone(), b.clone()]), Some(&a));
        assert_eq!(followed_path(Some(&a), std::slice::from_ref(&b)), Some(&b));
        assert_eq!(followed_path(Some(&a), std::slice::from_ref(&a)), None);
        assert_eq!(followed_path(Some(&a), &[a.clone(), b.clone()]), None);
        assert_eq!(followed_path(Some(&a), &[]), None);
    }

    #[test]
    fn a_resume_argv_names_the_session_file() {
        assert_eq!(omp_argv(None), ["omp"]);
        assert_eq!(
            omp_argv(Some(Path::new("/s/a.jsonl"))),
            ["omp", "--resume", "/s/a.jsonl"]
        );
    }

    #[test]
    fn the_manifest_holds_no_argv_or_environment() {
        let e = ManifestEntry {
            kind: MuxKind::Tmux,
            name: "omp-1".into(),
            cwd: PathBuf::from("/p"),
            session_path: Some(PathBuf::from("/s/a.jsonl")),
            label: "a".into(),
            view: SessionView::Chat,
        };
        let text = serde_json::to_string(&e).expect("json");
        assert!(!text.contains("argv") && !text.contains("env"));
        let back: ManifestEntry = serde_json::from_str(&text).expect("parse");
        assert_eq!(back, e);
    }

    #[test]
    fn a_manifest_written_before_views_restores_as_terminals() {
        let old = r#"[{"kind":"Tmux","name":"omp-1","cwd":"/p","session_path":null,"label":"a"}]"#;
        let entries: Vec<ManifestEntry> = serde_json::from_str(old).expect("parse");
        assert_eq!(entries[0].view, SessionView::Terminal);
    }

    #[test]
    fn chat_is_only_ever_effective_on_a_host_that_carries_it() {
        use SessionView::{Chat, Terminal};
        let host = Some(MuxKind::Host);
        // A session created now starts as asked, but only on the host.
        assert_eq!(effective_view(Chat, host, true, None), Chat);
        assert_eq!(effective_view(Terminal, host, true, None), Terminal);
        assert_eq!(
            effective_view(Chat, Some(MuxKind::Tmux), true, None),
            Terminal
        );
        assert_eq!(effective_view(Chat, None, true, None), Terminal);
        // One that is alive keeps the mode its daemon was created with.
        assert_eq!(effective_view(Terminal, host, false, Some(true)), Chat);
        assert_eq!(effective_view(Chat, host, false, Some(false)), Terminal);
        assert_eq!(effective_view(Chat, host, false, None), Terminal);
    }

    #[test]
    fn queued_restore_manifest_keeps_the_real_backend_across_restarts() {
        for kind in [MuxKind::Host, MuxKind::Tmux] {
            let mut job = prepare_job(SessionView::Terminal, SessionRole::Session);
            job.restore = true;
            job.owner = Owner::Restored(kind);
            let entry = job.restore_manifest_entry().expect("queued restore");
            let bytes = serde_json::to_vec(&entry).expect("serialize");
            let reloaded: ManifestEntry = serde_json::from_slice(&bytes).expect("reload");
            assert_eq!(reloaded.kind, kind);
            assert_eq!(reloaded.view, SessionView::Terminal);
            assert_eq!(reloaded.name, "omp-prep-chat");
            job.restore = false;
            assert_eq!(job.restore_manifest_entry(), None);
        }
    }

    fn prepare_job(view: SessionView, role: SessionRole) -> LaunchJob {
        LaunchJob {
            spec: LaunchSpec {
                mux_name: Some("omp-prep-chat".into()),
                cwd: PathBuf::from("/tmp"),
                session_path: Some(PathBuf::from("/s/a.jsonl")),
                label: "a".into(),
                argv: omp_argv(Some(Path::new("/s/a.jsonl"))),
                role,
                copy_of: None,
                view,
            },
            key: None,
            exact: true,
            owner: Owner::Snapshot,
            restore: false,
            tool: None,
            confine: None,
        }
    }

    fn host_mux() -> Mux {
        Mux::Host(deck_core::mux::HostMux::new("omp-deck-test-prepare").expect("namespace"))
    }

    #[test]
    fn a_chat_launch_asks_the_host_for_a_tsp_session_and_a_terminal_launch_does_not() {
        let chat = prepare(
            prepare_job(SessionView::Chat, SessionRole::Session),
            Some("omp-prep-chat".into()),
            None,
            Some(host_mux()),
            "omp-deck-test-prepare".into(),
            None,
            None,
        );
        assert_eq!(chat.view, SessionView::Chat);
        match chat.launch.expect("launch") {
            Launch::Host(spec) => {
                assert!(spec.tsp, "the daemon must act as the TSP terminal");
                assert_eq!(spec.args, ["--resume", "/s/a.jsonl"]);
            }
            other => panic!("not a host launch: {other:?}"),
        }
        let term = prepare(
            prepare_job(SessionView::Terminal, SessionRole::Session),
            Some("omp-prep-term".into()),
            None,
            Some(host_mux()),
            "omp-deck-test-prepare".into(),
            None,
            None,
        );
        assert_eq!(term.view, SessionView::Terminal);
        assert!(matches!(term.launch, Ok(Launch::Host(s)) if !s.tsp));
        // A tool run is a terminal whatever its spec says.
        let tool = prepare(
            prepare_job(SessionView::Chat, SessionRole::Tool),
            Some("omp-prep-tool".into()),
            None,
            Some(host_mux()),
            "omp-deck-test-prepare".into(),
            None,
            None,
        );
        assert_eq!(tool.view, SessionView::Terminal);
        assert!(matches!(tool.launch, Ok(Launch::Host(s)) if !s.tsp));
    }

    #[cfg(unix)]
    #[test]
    fn a_chat_launch_without_a_host_runs_as_a_terminal_and_says_so() {
        // tmux would be the backend here: it cannot carry TSP.
        let tmux = Mux::Tmux(deck_core::tmux::Tmux::with_socket(
            "tmux",
            "omp-deck-test-prepare",
            "/nonexistent/tmux.conf",
        ));
        let p = prepare(
            prepare_job(SessionView::Chat, SessionRole::Session),
            Some("omp-prep-tmux".into()),
            None,
            Some(tmux),
            "omp-deck-test-prepare".into(),
            None,
            None,
        );
        assert_eq!(p.view, SessionView::Terminal);
        assert!(matches!(p.launch, Ok(Launch::Pty(_))));
        // And no backend at all: a direct child.
        let p = prepare(
            prepare_job(SessionView::Chat, SessionRole::Session),
            None,
            None,
            None,
            "direct".into(),
            None,
            None,
        );
        assert_eq!(p.view, SessionView::Terminal);
        assert!(matches!(p.launch, Ok(Launch::Pty(_))));
    }
}
