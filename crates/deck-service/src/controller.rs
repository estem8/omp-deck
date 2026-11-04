//! The one owner of a live session's terminal and its command injection.
//!
//! A [`SessionController`] owns the session's [`Terminal`] (a client may borrow it to render) and
//! its [`Injector`], and is the only path that writes injected commands into it. Every client —
//! the desktop's status bar, palette, dialogs and panels today, protocol clients later — submits
//! [`Command`]s through [`SessionController::submit`] with an explicit [`SessionTarget`] and a
//! client-chosen [`RequestKey`]:
//!
//! - The target is the session's stable [`SessionKey`] (backend namespace + session name, or a
//!   process-unique key for a direct terminal) plus a generation that changes whenever the
//!   session behind the key may be a different conversation (another session file, a new
//!   controller for a restarted process). A submission for any other target is
//!   [`Rejected::Stale`]: a click resolved against an old view never runs on a new one.
//! - Request keys are per-client monotonic. A key already accepted returns the original
//!   [`RequestId`] (never a second injection); a key reused for a different command is
//!   [`Rejected::KeyReused`]; a key at or below the client's highest accepted sequence that is no
//!   longer remembered is [`Rejected::Expired`], never run again. The bounded history only drops
//!   settled requests, never one that is queued, being written, waiting for a draft confirmation
//!   or waiting for omp to confirm a model switch.
//! - Outcomes separate delivery from runtime effect: [`Delivery::Delivered`] means every byte
//!   of the command reached the terminal, not that omp executed it. Only model switches have a
//!   runtime verdict ([`Runtime`]), from the session file / live model.
//!
//! The controller is driven by [`SessionController::drive`] (or [`SessionController::tick`] for
//! a headless owner that also pumps the terminal's events): it builds the injector's view of the
//! terminal, applies the input hold before any byte, writes, and reports [`ControllerEvent`]s.
//! It never sleeps; `wake_in` says when to drive again.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use deck_core::mux::{Mux, MuxKind};
use deck_term::{InputMeta, ScreenText, Terminal, TerminalEvents};
use serde::{Deserialize, Serialize};

use crate::composer::TspComposer;
use crate::inject::{ComposeMode, End, Env, HOLD_TAIL, Injector, Job, JobId, Requires, Write};
use crate::model_switch::{Change, FRESH_TIMEOUT, Pending, SWITCH_TIMEOUT, plan_switch};
use crate::session_actions::{self, Action, ActionCtx};
use crate::status::TabStatus;
use crate::statusfmt::Activity;

/// Job label of a model / thinking-effort switch.
pub const MODEL_SWITCH_LABEL: &str = "Model switch";
/// Job labels of the two ways to hand a review to omp.
pub const REVIEW_INSERT_LABEL: &str = "Insert review";
pub const REVIEW_SEND_LABEL: &str = "Send review";
/// Job label of a typed prompt submitted for the user (mobile devices).
pub const PROMPT_LABEL: &str = "Send prompt";
/// Job label of the `/wt <branch>` move into a worktree.
pub const WORKTREE_MOVE_LABEL: &str = "Move to worktree";
/// Job label of the injected worktree finish prompt.
pub const WORKTREE_FINISH_LABEL: &str = "Finish worktree";
/// Job label of the `/move <dir>` relocation (a finished worktree's session into its repository).
pub const MOVE_TO_LABEL: &str = "Move to folder";
/// Job label of `/resume <name>`: the tab switches to another session file.
pub const RESUME_LABEL: &str = "Resume session";
/// Job label of `/reload-plugins` (typed after a plugin change).
pub const RELOAD_PLUGINS_LABEL: &str = "Reload plugins";
/// Job labels of composer text delivered while omp works.
pub const STEER_LABEL: &str = "Steer";
pub const FOLLOW_UP_LABEL: &str = "Follow-up";
/// Job label of a text (with attachments) inserted at omp's cursor without submitting.
pub const INSERT_LABEL: &str = "Insert text";

/// Settled requests remembered per session (pending ones are never dropped).
const HISTORY: usize = 256;
/// While a switch command is still queued or being typed its confirmation timer is held.
const SWITCH_RECHECK: Duration = Duration::from_millis(200);

/// Generations are unique in the process, so a new controller for a restarted session never
/// reuses a target an old client still holds; [`ServiceEpoch`] separates processes.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
static NEXT_DIRECT: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Random identity of this service process instance. Every [`SessionTarget`] carries it, so a
/// target (or request) from another instance — a restarted gateway reusing small generation
/// numbers — never validates here. The same value is announced by the gateway handshake.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ServiceEpoch(pub [u8; 16]);

static EPOCH: std::sync::LazyLock<ServiceEpoch> = std::sync::LazyLock::new(|| {
    let mut bytes = [0u8; 16];
    if let Err(e) = getrandom::fill(&mut bytes) {
        // No OS randomness: still distinct per process and start time.
        log::error!("no random source for the service epoch: {e}");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        bytes[..8].copy_from_slice(&(now as u64).to_le_bytes());
        bytes[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    }
    ServiceEpoch(bytes)
});

impl ServiceEpoch {
    /// This process's epoch: drawn once from the OS random source.
    pub fn current() -> ServiceEpoch {
        *EPOCH
    }

    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Option<ServiceEpoch> {
        if s.len() != 32 || !s.is_ascii() {
            return None;
        }
        let mut out = [0u8; 16];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
        }
        Some(ServiceEpoch(out))
    }
}

impl fmt::Debug for ServiceEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ServiceEpoch({})", self.to_hex())
    }
}

impl fmt::Display for ServiceEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Stable identity of a live session, independent of tabs and view indices. Cheap to clone.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionKey(Arc<str>);

impl SessionKey {
    /// A session of `mux`: its kind, namespace (tmux socket / host namespace) and session name.
    /// tmux and the session host use the same default namespace, so the kind is part of it.
    pub fn backend(mux: &Mux, name: &str) -> SessionKey {
        SessionKey::mux(mux.kind(), mux.namespace(), name)
    }

    /// A tmux session on the server `socket` (`tmux -L socket`).
    pub fn tmux(socket: &str, name: &str) -> SessionKey {
        SessionKey::mux(MuxKind::Tmux, socket, name)
    }

    /// A session-host session in `namespace`.
    pub fn host(namespace: &str, name: &str) -> SessionKey {
        SessionKey::mux(MuxKind::Host, namespace, name)
    }

    /// `kind:<namespace length>:<namespace>:<name>`: the length prefix keeps namespace and name
    /// apart whatever characters they hold.
    fn mux(kind: MuxKind, namespace: &str, name: &str) -> SessionKey {
        let kind = match kind {
            MuxKind::Tmux => "tmux",
            MuxKind::Host => "host",
        };
        SessionKey(format!("{kind}:{}:{namespace}:{name}", namespace.len()).into())
    }

    /// A terminal launched without a backend: unique for this machine while the process runs.
    pub fn direct() -> SessionKey {
        let n = NEXT_DIRECT.fetch_add(1, Ordering::Relaxed);
        SessionKey(format!("direct:{}-{n}", std::process::id()).into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a submission is aimed at: the service instance, the session and the generation the
/// client saw.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionTarget {
    pub epoch: ServiceEpoch,
    pub key: SessionKey,
    pub generation: u64,
}

/// Client-chosen identity of a request: `seq` must grow with every new request of `client`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RequestKey {
    pub client: String,
    pub seq: u64,
}

/// Hands out monotonic [`RequestKey`]s for one client.
#[derive(Clone, Debug)]
pub struct RequestKeys {
    client: String,
    next: u64,
}

impl RequestKeys {
    pub fn new(client: impl Into<String>) -> RequestKeys {
        RequestKeys {
            client: client.into(),
            next: 1,
        }
    }

    pub fn next_key(&mut self) -> RequestKey {
        let seq = self.next;
        self.next += 1;
        RequestKey {
            client: self.client.clone(),
            seq,
        }
    }
}

/// Controller-assigned identity of an accepted request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RequestId(pub u64);

impl From<JobId> for RequestId {
    fn from(id: JobId) -> RequestId {
        RequestId(id.0)
    }
}

/// Everything that runs through omp's composer or hotkeys.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Command {
    /// A session action with an injected route (see [`session_actions::job`]); fork, copy-last
    /// and export run elsewhere and are [`Rejected::NotInjected`].
    Action(Action),
    /// `/switch` (or `/effort` when `selector` is the current model).
    SwitchModel {
        selector: String,
        level: Option<String>,
    },
    /// `/effort` on the current model.
    SetEffort { level: String },
    /// A review prompt: inserted at omp's cursor (`send=false`) or sent (`send=true`).
    Review { text: String, send: bool },
    /// Composer text submitted with Enter; only while omp is idle.
    Prompt { text: String },
    /// `/wt <branch>`: omp moves the session into the worktree.
    WorktreeMove { branch: String },
    /// The worktree finish prompt.
    WorktreeFinish { prompt: String },
    /// `/move <dest>`: omp moves the session (file, artifacts, process cwd) into `dest`.
    MoveTo { dest: std::path::PathBuf },
    /// `/resume <name>`: omp switches this tab to the session `file`. omp's slash command takes
    /// a session id or file name (prefix), never a path, so the file's name without `.jsonl` is
    /// typed (omp finds it in this folder's sessions, then in all of them). It aborts a running
    /// turn, so the controller only types it while omp is idle.
    Resume { file: std::path::PathBuf },
    /// `/reload-plugins`: omp reloads its plugins in this session.
    ReloadPlugins,
    /// Text pasted while omp works, then Enter (steer the running turn) or, with `follow_up`,
    /// Ctrl+Q (omp queues it for after the turn). Busy only; a blank text is refused (an empty
    /// Enter while busy aborts the turn).
    Steer { text: String, follow_up: bool },
    /// Attachments (absolute file paths, pasted first as one file drop so omp attaches the images
    /// among them), then the text, then the submit of `mode`.
    Compose {
        attachments: Vec<std::path::PathBuf>,
        text: String,
        mode: ComposeMode,
    },
}

/// Which kind of command a request carried (for clients mapping outcomes back to their state).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandKind {
    Action,
    ModelSwitch,
    Review { send: bool },
    Prompt,
    WorktreeMove,
    WorktreeFinish,
    MoveTo,
    Steer { follow_up: bool },
    Resume,
    ReloadPlugins,
    Compose { mode: ComposeMode },
}

impl Command {
    fn kind(&self) -> CommandKind {
        match self {
            Command::Action(_) => CommandKind::Action,
            Command::SwitchModel { .. } | Command::SetEffort { .. } => CommandKind::ModelSwitch,
            Command::Review { send, .. } => CommandKind::Review { send: *send },
            Command::Prompt { .. } => CommandKind::Prompt,
            Command::WorktreeMove { .. } => CommandKind::WorktreeMove,
            Command::WorktreeFinish { .. } => CommandKind::WorktreeFinish,
            Command::MoveTo { .. } => CommandKind::MoveTo,
            Command::Resume { .. } => CommandKind::Resume,
            Command::ReloadPlugins => CommandKind::ReloadPlugins,
            Command::Steer { follow_up, .. } => CommandKind::Steer {
                follow_up: *follow_up,
            },
            Command::Compose { mode, .. } => CommandKind::Compose { mode: *mode },
        }
    }

    fn fingerprint(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

/// One submission.
#[derive(Clone, Debug)]
pub struct Submission {
    pub key: RequestKey,
    pub target: SessionTarget,
    pub command: Command,
}

/// An accepted submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submitted {
    pub id: RequestId,
    /// The key was accepted before; nothing new was queued.
    pub duplicate: bool,
}

/// Why a submission was not accepted (nothing was queued).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum Rejected {
    #[error("the session changed since this was requested")]
    Stale { current: SessionTarget },
    #[error("omp has exited in this session")]
    Exited,
    #[error("this view of the session is read-only")]
    ReadOnly,
    #[error("this client no longer controls the session")]
    Detached,
    #[error("omp is still handling another command; try again in a moment")]
    Busy,
    #[error("omp has not enabled bracketed paste yet; try again in a moment")]
    NoBracketedPaste,
    #[error("this action does not run through omp's composer")]
    NotInjected,
    #[error("there is nothing to send")]
    Empty,
    #[error("that is already the session's model")]
    NoChange,
    #[error("the current model is not known yet")]
    ModelUnknown,
    #[error("the request key was already used for a different command")]
    KeyReused { id: RequestId },
    #[error("the request is older than the remembered history")]
    Expired,
    #[error("omp is receiving a command; type again in a moment")]
    InputHeld,
    #[error(
        "that folder cannot be typed into omp (it must be absolute, without control characters)"
    )]
    BadPath,
}

/// Why a job was dropped before its first byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// omp was not in the state the command needs.
    Precondition(Requires),
    /// An omp dialog/overlay had the cursor.
    DialogOpen,
    /// omp had not enabled bracketed paste.
    NoBracketedPaste,
}

/// Why the controller dropped an accepted request itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cancel {
    /// omp exited.
    Exited,
    /// The terminal lost control of the session (displaced / read-only / shut down).
    Detached,
    /// The client is closing the session's view.
    Closing,
    /// Another session file took over this session before the request started.
    SessionChanged,
    /// The client lost the right to send it (a paired device revoked, or its `control`
    /// lowered) before its first byte.
    NotAllowed,
}

/// How far a request got toward the terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Delivery {
    /// Accepted; no byte written yet.
    Queued,
    /// omp's composer holds a draft: [`SessionController::answer`] decides.
    AwaitingConfirm { draft: String },
    /// Some of its bytes were written; the rest follow as omp shows each step, or all are
    /// written and the terminal's input sink has not accepted them yet.
    Writing,
    /// The terminal's input sink accepted every byte (the local PTY write or the remote
    /// transport's `input` call succeeded). Not proof that omp, or the host's child PTY, got or
    /// executed it.
    Delivered,
    /// Dropped before any byte.
    Refused(Refusal),
    /// The draft confirmation was declined; nothing was written.
    Declined,
    /// Abandoned mid-way because omp started working (no final Enter).
    Aborted,
    /// Abandoned mid-way because omp did not show a step in time; the text may sit unsent in the
    /// composer.
    Stalled,
    /// Dropped by the controller.
    Cancelled(Cancel),
    /// Every byte was handed to the terminal, but the view closed before its input sink
    /// acknowledged them: the detaching terminal may still write them. Not a success (a review
    /// stays, a switch gets no verdict) and not a failure. A switch omp was already seen to
    /// apply keeps [`Runtime::Confirmed`]: that is stronger evidence than the acknowledgement.
    Unobserved,
}

impl Delivery {
    /// No further change will happen.
    pub fn is_final(&self) -> bool {
        !matches!(
            self,
            Delivery::Queued | Delivery::AwaitingConfirm { .. } | Delivery::Writing
        )
    }
}

/// What is known about the command's effect inside omp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Runtime {
    /// The command has no observable confirmation.
    Untracked,
    /// A model switch waiting for the session file / live model to show it.
    Awaiting,
    /// omp shows the requested model / level.
    Confirmed,
    /// Nothing reports the model at all; the switch is assumed to have worked.
    Assumed,
    /// omp reports a model, but not the requested one, within the timeout.
    Unconfirmed,
    /// The switch was given up before omp could confirm it (not delivered, declined, exited).
    Abandoned,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: RequestId,
    pub key: RequestKey,
    pub target: SessionTarget,
    pub kind: CommandKind,
    pub label: String,
    pub delivery: Delivery,
    pub runtime: Runtime,
    #[serde(skip)]
    fingerprint: u64,
}

impl RequestRecord {
    fn settled(&self) -> bool {
        self.delivery.is_final() && self.runtime != Runtime::Awaiting
    }
}

/// Something a client reacts to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControllerEvent {
    /// omp's composer holds `draft`; ask, then call [`SessionController::answer`] with `id`.
    Confirm {
        id: RequestId,
        label: String,
        draft: String,
    },
    /// A request reached a final [`Delivery`].
    Ended {
        id: RequestId,
        kind: CommandKind,
        label: String,
        delivery: Delivery,
    },
}

/// Result of one [`SessionController::drive`].
#[derive(Debug, Default)]
pub struct Driven {
    pub events: Vec<ControllerEvent>,
    /// Drive again no later than this from now (`None`: nothing pending).
    pub wake_in: Option<Duration>,
}

/// Result of one [`SessionController::tick`].
#[derive(Debug, Default)]
pub struct Tick {
    pub terminal: TerminalEvents,
    pub driven: Driven,
}

/// What the client's status source reports about the session's model (see
/// [`SessionController::observe_model`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct ModelFacts<'a> {
    /// The model omp reports (session file or live control), if any.
    pub model: Option<&'a str>,
    /// The effective thinking level.
    pub thinking: Option<&'a str>,
    /// The configured thinking level.
    pub configured: Option<&'a str>,
    /// The model selector the client shows for the session.
    pub shown: Option<&'a str>,
    /// `shown` is only omp's default, not a recorded model.
    pub shown_is_default: bool,
}

/// Result of [`SessionController::observe_model`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ModelProgress {
    /// Observe again no later than this from now.
    pub wake_in: Option<Duration>,
    /// omp reported a different model within the timeout: the switch did not take.
    pub unconfirmed: Option<Pending>,
}

/// Facts the availability rules need that the controller does not own.
#[derive(Clone, Copy, Debug, Default)]
pub struct ActionFacts {
    pub last_turn_failed: bool,
    pub has_last_reply: bool,
    pub omp_found: bool,
}

/// What a read-only viewer sees of a controlled session, published by its controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub target: SessionTarget,
    pub session_path: Option<PathBuf>,
    pub title: Option<String>,
    pub activity: Activity,
    pub exit_code: Option<i32>,
    /// The visible grid as text with the cursor position.
    pub screen: ScreenText,
}

/// Called when a newer snapshot is ready for a viewer.
pub type ViewWake = Arc<dyn Fn() + Send + Sync>;

struct ViewSlot {
    latest: Mutex<(Option<Arc<SessionSnapshot>>, bool)>,
    wake: ViewWake,
}

/// A read-only subscription to a controlled session (see
/// [`SessionController::subscribe_view`]). It holds only the latest snapshot — a slow viewer
/// skips states, it never queues them or slows the controller — and has no way to write to or
/// take over the session. Dropping it unsubscribes.
pub struct ViewReceiver {
    slot: Arc<ViewSlot>,
}

impl ViewReceiver {
    /// The newest snapshot if it was not taken yet.
    pub fn take_changed(&self) -> Option<Arc<SessionSnapshot>> {
        let mut g = self
            .slot
            .latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if std::mem::take(&mut g.1) {
            g.0.clone()
        } else {
            None
        }
    }

    /// The newest snapshot, taken or not.
    pub fn latest(&self) -> Option<Arc<SessionSnapshot>> {
        let g = self
            .slot
            .latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        g.0.clone()
    }
}

/// While written bytes wait for the input sink's acknowledgement, the owner drives again this
/// soon, and user input stays held at least [`HOLD_TAIL`] past the next drive.
const ACK_POLL: Duration = Duration::from_millis(10);

/// A request whose last byte is input byte number `checkpoint` of the terminal (see
/// `deck_term::InputProgress::queued`).
#[derive(Clone, Copy, Debug)]
struct Unacked {
    id: RequestId,
    checkpoint: u64,
}

pub struct SessionController {
    terminal: Terminal,
    target: SessionTarget,
    session_path: Option<PathBuf>,
    injector: Injector,
    /// The job whose draft confirmation is outstanding.
    awaiting: Option<JobId>,
    /// What the live (not settled, not dropped) switch requests add up to, see
    /// [`Self::recompute_switch`].
    pending_switch: Option<Pending>,
    assumed_switch: Option<Pending>,
    /// The live switch requests whose runtime verdict follows `pending_switch`, oldest first.
    switch_requests: Vec<(RequestId, Pending)>,
    /// Requests whose bytes are all written but not yet accepted by the terminal's input sink.
    unacked: Vec<Unacked>,
    shown_model: Option<String>,
    shown_is_default: bool,
    /// Oldest first.
    history: VecDeque<RequestRecord>,
    /// Highest accepted sequence per client.
    watermarks: HashMap<String, u64>,
    /// Events produced outside `drive` (answers, closing, session changes).
    events: Vec<ControllerEvent>,
    viewers: Vec<Arc<ViewSlot>>,
    /// The snapshot last published to `viewers`.
    published: Option<Arc<SessionSnapshot>>,
}

impl SessionController {
    /// Takes ownership of the session's terminal. `session_path` is the session file when known.
    pub fn new(key: SessionKey, terminal: Terminal, session_path: Option<PathBuf>) -> Self {
        SessionController {
            terminal,
            target: SessionTarget {
                epoch: ServiceEpoch::current(),
                key,
                generation: next_generation(),
            },
            session_path,
            injector: Injector::default(),
            awaiting: None,
            pending_switch: None,
            assumed_switch: None,
            switch_requests: Vec::new(),
            unacked: Vec::new(),
            shown_model: None,
            shown_is_default: false,
            history: VecDeque::new(),
            watermarks: HashMap::new(),
            events: Vec::new(),
            viewers: Vec::new(),
            published: None,
        }
    }

    /// For the trusted native widget, which renders the terminal and must itself honor the
    /// controller's input hold for user input (`deck_term::TerminalView` does);
    /// [`Terminal::write`] and [`Terminal::send_key`] ignore the hold. Protocol clients type
    /// through [`write_input`](Self::write_input).
    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal {
        &mut self.terminal
    }

    /// User input from a client (already encoded for the terminal's modes): written at once,
    /// or refused — never queued or replayed. Refused for another instance, session or
    /// generation, an exited, read-only or detached terminal, and while the controller holds
    /// input around an injected command ([`Rejected::InputHeld`]). `meta` (semantic Ctrl+C, win32
    /// modifier state) is recorded only for accepted input, so a refused Ctrl+C never moves the
    /// injector's quiet-time guard.
    pub fn write_input(
        &mut self,
        target: &SessionTarget,
        bytes: &[u8],
        meta: InputMeta,
    ) -> Result<(), Rejected> {
        if *target != self.target {
            return Err(Rejected::Stale {
                current: self.target.clone(),
            });
        }
        if self.terminal.is_exited() {
            return Err(Rejected::Exited);
        }
        if self.terminal.is_read_only() {
            return Err(Rejected::ReadOnly);
        }
        if !self.terminal.accepts_input() {
            return Err(Rejected::Detached);
        }
        if self.terminal.input_held() {
            return Err(Rejected::InputHeld);
        }
        self.terminal.record_input_meta(meta);
        self.terminal.write(bytes);
        Ok(())
    }

    /// The session as a read-only viewer sees it now.
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            target: self.target.clone(),
            session_path: self.session_path.clone(),
            title: self.terminal.title().map(str::to_owned),
            activity: self.activity(),
            exit_code: self.terminal.exit_code(),
            screen: self.terminal.screen_text(),
        }
    }

    /// A read-only view of this session that follows it without attaching a second client to
    /// the backend (works for tmux, host and direct sessions alike). Snapshots are published by
    /// [`drive`](Self::drive) when the session changed; `wake` is called after each.
    pub fn subscribe_view(&mut self, wake: ViewWake) -> ViewReceiver {
        // Bring the existing viewers up to date first: the new one starts from the same
        // published snapshot, so a later drive compares against what everyone holds.
        self.publish_views();
        let snap = match &self.published {
            Some(snap) => snap.clone(),
            None => {
                let snap = Arc::new(self.snapshot());
                self.published = Some(snap.clone());
                snap
            }
        };
        let slot = Arc::new(ViewSlot {
            latest: Mutex::new((Some(snap), true)),
            wake,
        });
        self.viewers.push(slot.clone());
        ViewReceiver { slot }
    }

    /// Publishes a changed snapshot to the live viewers; forgets dropped ones.
    fn publish_views(&mut self) {
        self.viewers.retain(|v| Arc::strong_count(v) > 1);
        if self.viewers.is_empty() {
            self.published = None;
            return;
        }
        let snap = self.snapshot();
        if self.published.as_deref() == Some(&snap) {
            return;
        }
        let snap = Arc::new(snap);
        for v in &self.viewers {
            *v.latest.lock().unwrap_or_else(PoisonError::into_inner) = (Some(snap.clone()), true);
            (v.wake)();
        }
        self.published = Some(snap);
    }

    pub fn target(&self) -> &SessionTarget {
        &self.target
    }

    pub fn session_path(&self) -> Option<&Path> {
        self.session_path.as_deref()
    }

    /// The session file behind the session changed (`/new`, `/fork`, `/move`, `/resume`, …).
    /// The first discovery of a file for a fresh session is not a change. A change starts a new
    /// generation and cancels every request that has not written a byte yet — including one
    /// waiting for a draft confirmation, so an answer meant for the old conversation can never
    /// clear the new one's draft. A request already writing finishes its sequence (cutting it
    /// mid-way could leave a half command), guarded as always by the injector's idle checks.
    /// Returns whether the generation changed.
    pub fn set_session_path(&mut self, path: Option<PathBuf>) -> bool {
        if self.session_path == path {
            return false;
        }
        let first = self.session_path.is_none();
        self.session_path = path;
        if first {
            return false;
        }
        self.target.generation = next_generation();
        self.injector.drop_unstarted();
        self.awaiting = None;
        self.sweep(Cancel::SessionChanged);
        true
    }

    /// omp's state: its OSC 7501 report, else the `π` title, else (a TSP terminal) the document
    /// (see [`Activity::of_terminal`]).
    pub fn activity(&self) -> Activity {
        Activity::of_terminal(&self.terminal)
    }

    /// The session's indicator; `bell` is the client's own unseen-bell marker.
    pub fn status(&self, bell: bool) -> TabStatus {
        TabStatus::of(self.terminal.exit_code(), self.activity(), bell)
    }

    /// A request is queued, running, or waiting for its bytes to be accepted.
    pub fn is_busy(&self) -> bool {
        self.injector.is_busy() || !self.unacked.is_empty()
    }

    /// The availability inputs of the session actions.
    pub fn action_ctx(&self, facts: ActionFacts) -> ActionCtx {
        let activity = self.activity();
        ActionCtx {
            busy: activity.is_busy(),
            idle: activity == Activity::Idle,
            has_session_file: self.session_path.is_some(),
            last_turn_failed: facts.last_turn_failed,
            has_last_reply: facts.has_last_reply,
            exited: self.terminal.is_exited(),
            omp_found: facts.omp_found,
        }
    }

    pub fn pending_switch(&self) -> Option<&Pending> {
        self.pending_switch.as_ref()
    }

    pub fn assumed_switch(&self) -> Option<&Pending> {
        self.assumed_switch.as_ref()
    }

    /// The model the session runs as far as the deck knows: an unconfirmed switch's model (omp
    /// applies it before anything queued now), else an assumed switch's model while the client
    /// can only show omp's default, else the shown model.
    pub fn current_model(&self) -> Option<String> {
        let pending = self
            .pending_switch
            .as_ref()
            .and_then(|p| p.selector.clone());
        let assumed = self
            .assumed_switch
            .as_ref()
            .filter(|_| self.shown_model.is_none() || self.shown_is_default)
            .and_then(|a| a.selector.clone());
        pending.or(assumed).or_else(|| self.shown_model.clone())
    }

    pub fn request(&self, id: RequestId) -> Option<&RequestRecord> {
        self.history.iter().find(|r| r.id == id)
    }

    pub fn request_by_key(&self, key: &RequestKey) -> Option<&RequestRecord> {
        self.history.iter().find(|r| r.key == *key)
    }

    /// Every remembered request, oldest first (pending ones are never dropped).
    pub fn requests(&self) -> impl Iterator<Item = &RequestRecord> {
        self.history.iter()
    }

    /// The single entry point for injected commands.
    pub fn submit(&mut self, s: Submission) -> Result<Submitted, Rejected> {
        let fingerprint = s.command.fingerprint();
        if let Some(r) = self.request_by_key(&s.key) {
            return if r.fingerprint == fingerprint && r.target == s.target {
                Ok(Submitted {
                    id: r.id,
                    duplicate: true,
                })
            } else {
                Err(Rejected::KeyReused { id: r.id })
            };
        }
        if self
            .watermarks
            .get(&s.key.client)
            .is_some_and(|&w| s.key.seq <= w)
        {
            return Err(Rejected::Expired);
        }
        if s.target != self.target {
            return Err(Rejected::Stale {
                current: self.target.clone(),
            });
        }
        if self.terminal.is_exited() {
            return Err(Rejected::Exited);
        }
        if self.terminal.is_read_only() {
            return Err(Rejected::ReadOnly);
        }
        if !self.terminal.accepts_input() {
            return Err(Rejected::Detached);
        }
        let kind = s.command.kind();
        let (job, switch) = self.job_for(&s.command)?;
        let label = job.label.clone();
        let id = RequestId::from(self.injector.push(job));
        let mut runtime = Runtime::Untracked;
        if let Some(next) = switch {
            self.pending_switch = Some(match self.pending_switch.take() {
                Some(prev) => prev.then(next.clone()),
                None => next.clone(),
            });
            self.switch_requests.push((id, next));
            runtime = Runtime::Awaiting;
        }
        self.watermarks.insert(s.key.client.clone(), s.key.seq);
        self.history.push_back(RequestRecord {
            id,
            key: s.key,
            target: s.target,
            kind,
            label,
            delivery: Delivery::Queued,
            runtime,
            fingerprint,
        });
        self.trim_history();
        Ok(Submitted {
            id,
            duplicate: false,
        })
    }

    /// What a model or effort command changes, with the gating the typed path has (a no-op
    /// switch, an effort on a session whose model is unknown); `None`: not a model command.
    pub fn model_change(&self, command: &Command) -> Option<Result<Change, Rejected>> {
        match command {
            Command::SwitchModel { selector, level } => {
                let current = self.current_model();
                Some(
                    plan_switch(current.as_deref(), selector, level.as_deref())
                        .ok_or(Rejected::NoChange),
                )
            }
            Command::SetEffort { level } => Some(
                self.current_model()
                    .ok_or(Rejected::ModelUnknown)
                    .and_then(|current| {
                        plan_switch(Some(&current), &current, Some(level)).ok_or(Rejected::NoChange)
                    }),
            ),
            _ => None,
        }
    }

    fn job_for(&self, command: &Command) -> Result<(Job, Option<Pending>), Rejected> {
        match command {
            Command::Action(action) => match session_actions::job(action) {
                Some(job) => Ok((job, None)),
                None if matches!(action, Action::Fork | Action::CopyLast | Action::ExportHtml) => {
                    Err(Rejected::NotInjected)
                }
                None => Err(Rejected::Empty),
            },
            Command::SwitchModel { .. } | Command::SetEffort { .. } => {
                let change = self.model_change(command).ok_or(Rejected::Empty)??;
                let job = Job::slash(MODEL_SWITCH_LABEL, &change.command, Requires::Any);
                Ok((
                    job,
                    Some(Pending {
                        selector: change.selector,
                        level: change.level,
                        since: Instant::now(),
                    }),
                ))
            }
            Command::Review { text, send } => {
                if self.terminal.paste_bytes("x").is_none() {
                    return Err(Rejected::NoBracketedPaste);
                }
                if self.is_busy() {
                    return Err(Rejected::Busy);
                }
                let label = if *send {
                    REVIEW_SEND_LABEL
                } else {
                    REVIEW_INSERT_LABEL
                };
                Ok((Job::text(label, text, *send), None))
            }
            Command::Prompt { text } => {
                if text.trim().is_empty() {
                    return Err(Rejected::Empty);
                }
                if self.terminal.paste_bytes("x").is_none() {
                    return Err(Rejected::NoBracketedPaste);
                }
                if self.is_busy() || self.activity().is_busy() {
                    return Err(Rejected::Busy);
                }
                Ok((Job::text(PROMPT_LABEL, text, true), None))
            }
            Command::WorktreeMove { branch } => Ok((
                Job::slash(
                    WORKTREE_MOVE_LABEL,
                    &format!("/wt {branch}"),
                    Requires::Idle,
                ),
                None,
            )),
            Command::WorktreeFinish { prompt } => {
                if self.is_busy() {
                    return Err(Rejected::Busy);
                }
                Ok((
                    Job::slash(WORKTREE_FINISH_LABEL, prompt, Requires::Idle),
                    None,
                ))
            }
            Command::MoveTo { dest } => {
                if self.is_busy() {
                    return Err(Rejected::Busy);
                }
                let text = dest
                    .to_str()
                    .filter(|t| dest.is_absolute() && !t.chars().any(char::is_control))
                    .ok_or(Rejected::BadPath)?;
                Ok((
                    Job::slash(MOVE_TO_LABEL, &format!("/move {text}"), Requires::Idle),
                    None,
                ))
            }
            Command::Resume { file } => {
                if self.is_busy() {
                    return Err(Rejected::Busy);
                }
                let name = file
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .filter(|n| {
                        file.is_absolute()
                            && !n.is_empty()
                            && !n.chars().any(|c| c.is_control() || c.is_whitespace())
                    })
                    .ok_or(Rejected::BadPath)?;
                Ok((
                    Job::slash(RESUME_LABEL, &format!("/resume {name}"), Requires::Idle),
                    None,
                ))
            }
            Command::ReloadPlugins => {
                if self.is_busy() {
                    return Err(Rejected::Busy);
                }
                Ok((
                    Job::slash(RELOAD_PLUGINS_LABEL, "/reload-plugins", Requires::Idle),
                    None,
                ))
            }
            Command::Steer { text, follow_up } => {
                let mode = if *follow_up {
                    ComposeMode::FollowUp
                } else {
                    ComposeMode::Steer
                };
                self.compose_job(&[], text, mode)
            }
            Command::Compose {
                attachments,
                text,
                mode,
            } => self.compose_job(attachments, text, *mode),
        }
    }

    /// The job of [`Command::Steer`] / [`Command::Compose`].
    fn compose_job(
        &self,
        attachments: &[std::path::PathBuf],
        text: &str,
        mode: ComposeMode,
    ) -> Result<(Job, Option<Pending>), Rejected> {
        let blank = text.trim().is_empty();
        let steering = matches!(mode, ComposeMode::Steer | ComposeMode::FollowUp);
        if (steering && blank) || (blank && attachments.is_empty()) {
            return Err(Rejected::Empty);
        }
        if attachments
            .iter()
            .any(|p| !p.is_absolute() || p.to_str().is_none_or(|s| s.chars().any(char::is_control)))
        {
            return Err(Rejected::BadPath);
        }
        if self.terminal.paste_bytes("x").is_none() {
            return Err(Rejected::NoBracketedPaste);
        }
        if self.is_busy() || (mode == ComposeMode::Send && self.activity().is_busy()) {
            return Err(Rejected::Busy);
        }
        let attach = (!attachments.is_empty()).then(|| {
            deck_term::input::encode_dropped_paths(attachments.iter().map(|p| p.as_path()))
        });
        let label = match mode {
            ComposeMode::Send => PROMPT_LABEL,
            ComposeMode::Steer => STEER_LABEL,
            ComposeMode::FollowUp => FOLLOW_UP_LABEL,
            ComposeMode::Insert => INSERT_LABEL,
        };
        let text = if blank { "" } else { text };
        Ok((Job::compose(label, attach.as_deref(), text, mode), None))
    }

    /// Drops the oldest settled requests beyond [`HISTORY`]; pending ones always stay. Dropped
    /// keys stay rejected through the client's watermark.
    fn trim_history(&mut self) {
        while self.history.len() > HISTORY {
            let Some(i) = self.history.iter().position(RequestRecord::settled) else {
                break;
            };
            self.history.remove(i);
        }
    }

    fn record_mut(&mut self, id: RequestId) -> Option<&mut RequestRecord> {
        self.history.iter_mut().find(|r| r.id == id)
    }

    /// Sets a final delivery and reports it. A switch request that ends without its bytes
    /// accepted gets no runtime verdict and stops counting toward the pending switch.
    fn finish(&mut self, id: RequestId, delivery: Delivery, events: &mut Vec<ControllerEvent>) {
        let Some(r) = self.record_mut(id) else {
            return;
        };
        r.delivery = delivery.clone();
        let abandoned = r.runtime == Runtime::Awaiting && delivery != Delivery::Delivered;
        if abandoned {
            r.runtime = Runtime::Abandoned;
        }
        events.push(ControllerEvent::Ended {
            id,
            kind: r.kind,
            label: r.label.clone(),
            delivery,
        });
        if abandoned {
            self.switch_requests.retain(|(s, _)| *s != id);
            self.recompute_switch();
        }
    }

    /// `pending_switch` from the live switch requests, keeping the running timer.
    fn recompute_switch(&mut self) {
        let since = self.pending_switch.as_ref().map(|p| p.since);
        self.pending_switch = self
            .switch_requests
            .iter()
            .map(|(_, p)| p.clone())
            .reduce(Pending::then);
        if let (Some(p), Some(since)) = (&mut self.pending_switch, since) {
            p.since = since;
        }
    }

    /// Every unfinished request neither the injector nor the acknowledgement wait holds any
    /// more was dropped by the controller.
    fn sweep(&mut self, why: Cancel) {
        let live: Vec<RequestId> = self
            .injector
            .live_ids()
            .map(RequestId::from)
            .chain(self.unacked.iter().map(|u| u.id))
            .collect();
        let gone: Vec<RequestId> = self
            .history
            .iter()
            .filter(|r| !r.delivery.is_final() && !live.contains(&r.id))
            .map(|r| r.id)
            .collect();
        let mut events = std::mem::take(&mut self.events);
        for id in gone {
            self.finish(id, Delivery::Cancelled(why), &mut events);
        }
        self.events = events;
    }

    /// The terminal is gone for writing: every request still under way is cancelled and the
    /// delivered switches get no verdict.
    fn cancel_all(&mut self, why: Cancel) {
        self.injector.clear();
        self.awaiting = None;
        self.unacked.clear();
        self.sweep(why);
        self.pending_switch = None;
        self.settle_switch(Runtime::Abandoned);
    }

    /// Client `client` may no longer send commands: its requests that have not written a byte
    /// yet (queued, or waiting for a draft confirmation) are cancelled. One already writing
    /// finishes. True when any was cancelled.
    pub fn cancel_client(&mut self, client: &str) -> bool {
        let ids: Vec<JobId> = self
            .history
            .iter()
            .filter(|r| r.key.client == client && !r.delivery.is_final())
            .map(|r| JobId(r.id.0))
            .collect();
        if ids.is_empty() || !self.injector.drop_unwritten(&ids) {
            return false;
        }
        if self.awaiting.is_some_and(|a| ids.contains(&a)) {
            self.awaiting = None;
        }
        self.sweep(Cancel::NotAllowed);
        true
    }

    fn settle_switch(&mut self, verdict: Runtime) {
        for (id, _) in std::mem::take(&mut self.switch_requests) {
            if let Some(r) = self.record_mut(id)
                && r.runtime == Runtime::Awaiting
            {
                r.runtime = verdict;
            }
        }
    }

    /// Ends the requests whose bytes the input sink accepted ([`Delivery::Delivered`]) or failed
    /// to take ([`Cancel::Detached`]). There is no timeout: queued bytes can still be written
    /// (the terminal flushes them even after it is dropped), so a request stays `Writing` until
    /// the sink settles it either way.
    fn settle_acks(&mut self, events: &mut Vec<ControllerEvent>) {
        if self.unacked.is_empty() {
            return;
        }
        let progress = self.terminal.input_progress();
        let mut i = 0;
        while let Some(&u) = self.unacked.get(i) {
            let end = if progress.acknowledged >= u.checkpoint {
                Delivery::Delivered
            } else if progress.failed || progress.closed {
                Delivery::Cancelled(Cancel::Detached)
            } else {
                i += 1;
                continue;
            };
            self.unacked.remove(i);
            self.finish(u.id, end, events);
        }
    }

    /// Holds user input: through the injector's own hold (`injector_hold` from now) and, while
    /// bytes await acceptance, through [`HOLD_TAIL`] past the next drive. One deadline, the later
    /// of the two, so neither shortens the other (`hold_input_until` replaces the deadline).
    fn hold_input(&self, now: Instant, injector_hold: Option<Duration>, out: &mut Driven) {
        let acks = (!self.unacked.is_empty()).then_some(ACK_POLL + HOLD_TAIL);
        if let Some(h) = injector_hold.max(acks) {
            self.terminal.hold_input_until(now + h);
        }
        if acks.is_some() {
            out.wake_in = Some(out.wake_in.map_or(ACK_POLL, |w| w.min(ACK_POLL)));
        }
    }

    /// Pumps the terminal's events, then [`drive`](Self::drive)s: one step for an owner that has
    /// no other use for the terminal (headless).
    pub fn tick(&mut self, now: Instant) -> Tick {
        let terminal = self.terminal.poll();
        let driven = self.drive(now);
        Tick { terminal, driven }
    }

    /// Advances the injection queue (never blocks): input hold first, then the bytes due now.
    /// Then publishes a changed snapshot to read-only viewers.
    pub fn drive(&mut self, now: Instant) -> Driven {
        let out = self.drive_queue(now);
        self.publish_views();
        out
    }

    fn drive_queue(&mut self, now: Instant) -> Driven {
        let mut out = Driven {
            events: std::mem::take(&mut self.events),
            wake_in: None,
        };
        self.settle_acks(&mut out.events);
        if self.terminal.is_exited() || !self.terminal.accepts_input() {
            let why = if self.terminal.is_exited() {
                Cancel::Exited
            } else {
                Cancel::Detached
            };
            if self.is_busy() || self.pending_switch.is_some() {
                self.cancel_all(why);
                out.events.append(&mut self.events);
            }
            return out;
        }
        if !self.injector.is_busy() {
            self.hold_input(now, None, &mut out);
            return out;
        }
        let activity = self.activity();
        let term = &self.terminal;
        let screen = || term.screen_text();
        let paste = |t: &str| term.paste_bytes(t);
        let handle = term.tsp();
        let tsp = || handle.with(TspComposer::of);
        let poll = self.injector.poll(&Env {
            now,
            activity,
            last_ctrl_c: term.last_ctrl_c(),
            screen: &screen,
            paste: &paste,
            tsp: handle.live().then_some(&tsp as &dyn Fn() -> TspComposer),
        });
        // The hold comes first so no user key can slip in between our bytes.
        if let Some(h) = poll.hold_for {
            term.hold_input_until(now + h);
        }
        // A TSP event the terminal refused was never written: the job it ends must not read as
        // delivered (the injector wrote at most one job's bytes this poll).
        let mut unwritten = false;
        for w in poll.writes {
            match w {
                Write::Bytes(bytes) => term.write(&bytes),
                Write::Key(key, mods) => term.send_key(key, mods),
                Write::Send {
                    surface,
                    editor,
                    text,
                } => {
                    let event = deck_tsp::wire::Event::Send {
                        sf: surface,
                        id: editor,
                        text,
                    };
                    unwritten |= term.inject_tsp(&event).is_err();
                }
            }
        }
        for e in poll.ended {
            let id = RequestId::from(e.id);
            if self.awaiting == Some(e.id) {
                self.awaiting = None;
            }
            let delivery = match e.end {
                End::Completed if unwritten => Delivery::Aborted,
                End::Completed => {
                    // Written; delivered once the input sink accepted the last byte.
                    let checkpoint = self.terminal.input_progress().queued;
                    if let Some(r) = self.record_mut(id) {
                        r.delivery = Delivery::Writing;
                    }
                    self.unacked.push(Unacked { id, checkpoint });
                    continue;
                }
                End::Refused(r) => Delivery::Refused(Refusal::Precondition(r)),
                End::DialogOpen => Delivery::Refused(Refusal::DialogOpen),
                End::NoPaste => Delivery::Refused(Refusal::NoBracketedPaste),
                End::Aborted => Delivery::Aborted,
                End::Stalled => Delivery::Stalled,
            };
            self.finish(id, delivery, &mut out.events);
        }
        self.settle_acks(&mut out.events);
        if let Some(c) = poll.confirm {
            self.awaiting = Some(c.id);
            let id = RequestId::from(c.id);
            if let Some(r) = self.record_mut(id) {
                r.delivery = Delivery::AwaitingConfirm {
                    draft: c.draft.clone(),
                };
            }
            out.events.push(ControllerEvent::Confirm {
                id,
                label: c.label,
                draft: c.draft,
            });
        }
        let writing: Vec<RequestId> = self
            .injector
            .live_ids()
            .filter(|&j| self.injector.has_written(j))
            .map(RequestId::from)
            .collect();
        for id in writing {
            if let Some(r) = self.record_mut(id)
                && r.delivery == Delivery::Queued
            {
                r.delivery = Delivery::Writing;
            }
        }
        out.wake_in = poll.wake_in;
        self.hold_input(now, poll.hold_for, &mut out);
        out
    }

    /// The answer to [`ControllerEvent::Confirm`] `id` for `target`: `true` clears the draft and
    /// goes on, `false` drops the request. An answer for anything but the outstanding
    /// confirmation of this exact target (an old one, another instance's, another
    /// conversation's) is ignored; returns whether it applied.
    pub fn answer(&mut self, target: &SessionTarget, id: RequestId, clear: bool) -> bool {
        if *target != self.target {
            return false;
        }
        let Some(job) = self.awaiting.filter(|j| RequestId::from(*j) == id) else {
            return false;
        };
        if !self.injector.awaiting_answer() {
            self.awaiting = None;
            return false;
        }
        self.injector.answer(clear);
        self.awaiting = None;
        if clear {
            if let Some(r) = self.record_mut(id) {
                r.delivery = Delivery::Queued;
            }
        } else {
            let mut events = std::mem::take(&mut self.events);
            self.finish(RequestId::from(job), Delivery::Declined, &mut events);
            self.events = events;
        }
        true
    }

    /// The view is closing: drops every request that has not written a byte. True once nothing
    /// is left to send, no request is mid-way and the last byte is old enough to have been
    /// delivered — the terminal may go now (it keeps flushing queued input while it detaches).
    /// Requests whose bytes the input sink has not acknowledged by then end
    /// [`Delivery::Unobserved`]. On an exited terminal everything still under way is cancelled
    /// at once.
    pub fn can_close(&mut self, now: Instant) -> bool {
        let mut events = std::mem::take(&mut self.events);
        self.settle_acks(&mut events);
        if self.terminal.is_exited() {
            self.events = events;
            self.cancel_all(Cancel::Exited);
            return true;
        }
        let ready = self.injector.can_close(now);
        if !self.injector.awaiting_answer() {
            self.awaiting = None;
        }
        if ready {
            for u in std::mem::take(&mut self.unacked) {
                self.finish(u.id, Delivery::Unobserved, &mut events);
            }
        }
        self.events = events;
        // Every poll: the injector dropped the requests that had not started.
        self.sweep(Cancel::Closing);
        ready
    }

    /// Feeds what the client's status source shows about the model; resolves the optimistic
    /// switch: confirmed when omp shows it, held while the command is still being typed, after
    /// the timeout assumed (nothing reports a model) or unconfirmed (another model is reported).
    pub fn observe_model(&mut self, now: Instant, facts: ModelFacts<'_>) -> ModelProgress {
        self.shown_model = facts.shown.map(str::to_owned);
        self.shown_is_default = facts.shown_is_default;
        if facts.model.is_some() {
            self.assumed_switch = None;
        }
        let mut out = ModelProgress::default();
        let busy = self.is_busy();
        let Some(p) = &mut self.pending_switch else {
            return out;
        };
        let limit = if facts.model.is_none() {
            FRESH_TIMEOUT
        } else {
            SWITCH_TIMEOUT
        };
        if p.confirmed_by(facts.model, facts.thinking, facts.configured) {
            self.pending_switch = None;
            self.settle_switch(Runtime::Confirmed);
        } else if busy {
            // The timer starts once the command has actually been typed.
            p.since = now;
            out.wake_in = Some(SWITCH_RECHECK);
        } else if p.timed_out(now, limit) {
            if facts.model.is_none() {
                // Nothing reports the model (no file entry, no live control): assume it worked,
                // on top of an earlier assumed switch (`/effort` keeps the model).
                let prev = self.assumed_switch.take();
                self.assumed_switch = self.pending_switch.take().map(|mut p| {
                    if let Some(prev) = prev {
                        p.selector = p.selector.or(prev.selector);
                        p.level = p.level.or(prev.level);
                    }
                    p
                });
                self.settle_switch(Runtime::Assumed);
            } else {
                out.unconfirmed = self.pending_switch.take();
                self.settle_switch(Runtime::Unconfirmed);
            }
        } else {
            out.wake_in = Some(limit.saturating_sub(now.duration_since(p.since)));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::mpsc::{self, Receiver, Sender};

    use deck_term::{Remote, RemoteEvent, RemoteRx, RemoteTx, TermOptions};

    use super::*;
    use crate::session_actions::CompactMode;

    /// A scripted omp behind a remote terminal: draws a rail composer, takes pastes into it,
    /// submits on Enter and clears on Ctrl+C.
    #[derive(Default)]
    struct Omp {
        draft: String,
        submitted: Vec<String>,
        /// Drafts queued with Ctrl+Q (0x11).
        followed_up: Vec<String>,
        input: Vec<u8>,
        /// The transport fails to take a lone Enter.
        fail_enter: bool,
        /// The transport blocks on a lone Enter until this is cleared.
        block_enter: bool,
    }

    #[derive(Clone)]
    struct Fake {
        omp: Arc<Mutex<Omp>>,
        out: Sender<RemoteEvent>,
    }

    impl Fake {
        fn redraw(&self, omp: &Omp) {
            let screen = format!("\x1b[2J\x1b[H\r\n  v18.12.5\r\n▎ {}", omp.draft);
            let _ = self.out.send(RemoteEvent::Output(screen.into_bytes()));
        }

        fn omp(&self) -> std::sync::MutexGuard<'_, Omp> {
            self.omp.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    struct FakeRemote {
        fake: Fake,
        events: Receiver<RemoteEvent>,
    }

    impl Remote for FakeRemote {
        fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn RemoteTx>) {
            (
                Box::new(FakeRx(self.events)),
                Box::new(FakeTx(self.fake.clone())),
            )
        }
    }

    struct FakeRx(Receiver<RemoteEvent>);

    impl RemoteRx for FakeRx {
        fn recv(&mut self) -> io::Result<RemoteEvent> {
            Ok(self.0.recv().unwrap_or(RemoteEvent::Closed))
        }
    }

    struct FakeTx(Fake);

    impl RemoteTx for FakeTx {
        fn input(&self, bytes: &[u8]) -> io::Result<()> {
            while bytes == b"\r" && self.0.omp().block_enter {
                std::thread::sleep(Duration::from_millis(5));
            }
            let mut omp = self.0.omp();
            if omp.fail_enter && bytes == b"\r" {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            omp.input.extend_from_slice(bytes);
            if let Some(rest) = bytes.strip_prefix(b"\x1b[200~") {
                let text = rest.strip_suffix(b"\x1b[201~").unwrap_or(rest);
                omp.draft.push_str(&String::from_utf8_lossy(text));
            } else if bytes == b"\r" {
                let line = std::mem::take(&mut omp.draft);
                omp.submitted.push(line);
            } else if bytes == b"\x03" {
                omp.draft.clear();
            } else if bytes == b"\x11" {
                let line = std::mem::take(&mut omp.draft);
                omp.followed_up.push(line);
            } else {
                return Ok(());
            }
            self.0.redraw(&omp);
            Ok(())
        }
        fn resize(&self, _: u16, _: u16, _: u16, _: u16) -> io::Result<()> {
            Ok(())
        }
        fn palette(&self, _: [[u8; 3]; 19]) -> io::Result<()> {
            Ok(())
        }
        fn kill(&self) -> io::Result<()> {
            Ok(())
        }
        fn detach(&self) -> io::Result<()> {
            Ok(())
        }
    }

    fn controller(draft: &str, session: Option<&str>) -> (SessionController, Fake) {
        let (out, events) = mpsc::channel();
        let fake = Fake {
            omp: Arc::new(Mutex::new(Omp {
                draft: draft.to_owned(),
                ..Omp::default()
            })),
            out,
        };
        fake.redraw(&fake.omp());
        let terminal = Terminal::connect_headless(
            Box::new(FakeRemote {
                fake: fake.clone(),
                events,
            }),
            TermOptions::default(),
            Arc::new(|_| {}),
        );
        let mut ctl = SessionController::new(
            SessionKey::tmux("omp-deck-test", "s"),
            terminal,
            session.map(PathBuf::from),
        );
        // Wait until the terminal shows the composer.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ctl
            .terminal()
            .screen_text()
            .lines
            .iter()
            .any(|l| l.starts_with('▎'))
        {
            assert!(Instant::now() < deadline, "the composer never showed");
            ctl.terminal_mut().poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        (ctl, fake)
    }

    /// Ticks in real time until `done` or 5 s; returns every event.
    fn run(
        ctl: &mut SessionController,
        mut done: impl FnMut(&SessionController, &[ControllerEvent]) -> bool,
    ) -> Vec<ControllerEvent> {
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            events.extend(ctl.tick(Instant::now()).driven.events);
            if done(ctl, &events) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        events
    }

    fn submission(ctl: &SessionController, seq: u64, command: Command) -> Submission {
        Submission {
            key: RequestKey {
                client: "test".to_owned(),
                seq,
            },
            target: ctl.target().clone(),
            command,
        }
    }

    #[test]
    fn a_submission_for_another_generation_is_refused_and_queues_nothing() {
        let (mut ctl, _fake) = controller("", None);
        let mut s = submission(&ctl, 1, Command::Action(Action::AutoName));
        s.target.generation += 1;
        assert!(matches!(ctl.submit(s), Err(Rejected::Stale { .. })));
        assert!(!ctl.is_busy());
        let mut s = submission(&ctl, 2, Command::Action(Action::AutoName));
        s.target.key = SessionKey::tmux("omp-deck-test", "other");
        assert!(matches!(ctl.submit(s), Err(Rejected::Stale { .. })));
        assert!(!ctl.is_busy());
    }

    #[test]
    fn a_resubmitted_request_is_typed_once_and_a_reused_key_is_refused() {
        let (mut ctl, fake) = controller("", None);
        let s = submission(&ctl, 1, Command::Action(Action::AutoName));
        let first = ctl.submit(s.clone()).expect("accepted");
        assert!(!first.duplicate);
        let again = ctl.submit(s.clone()).expect("same request");
        assert_eq!((again.id, again.duplicate), (first.id, true));
        run(&mut ctl, |c, _| {
            c.request(first.id)
                .is_some_and(|r| r.delivery == Delivery::Delivered)
        });
        // A retry after delivery still answers with the original request, typing nothing.
        let late = ctl.submit(s.clone()).expect("same request");
        assert_eq!((late.id, late.duplicate), (first.id, true));
        assert!(!ctl.is_busy());
        assert_eq!(fake.omp().submitted, vec!["/rename".to_owned()]);
        let reused = Submission {
            command: Command::Action(Action::Compact(CompactMode::Soft)),
            ..s
        };
        assert_eq!(
            ctl.submit(reused),
            Err(Rejected::KeyReused { id: first.id })
        );
    }

    /// The fake omp enables bracketed paste (?2004) and shows an idle title, like omp at rest.
    fn idle_composer(ctl: &mut SessionController, fake: &Fake) {
        let _ = fake.out.send(RemoteEvent::Output(
            "\x1b[?2004h\x1b]0;π > app\x07".as_bytes().to_vec(),
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        while ctl.terminal().paste_bytes("x").is_none() || ctl.activity() != Activity::Idle {
            assert!(
                Instant::now() < deadline,
                "?2004 and the idle title never applied"
            );
            ctl.terminal_mut().poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_prompt_is_submitted_with_enter_when_omp_is_idle() {
        let (mut ctl, fake) = controller("", None);
        idle_composer(&mut ctl, &fake);
        let s = submission(
            &ctl,
            1,
            Command::Prompt {
                text: "explain main".into(),
            },
        );
        let first = ctl.submit(s).expect("accepted");
        let record = ctl.request(first.id).expect("recorded");
        assert_eq!(
            (record.kind, record.label.as_str()),
            (CommandKind::Prompt, PROMPT_LABEL)
        );
        run(&mut ctl, |c, _| {
            c.request(first.id)
                .is_some_and(|r| r.delivery == Delivery::Delivered)
        });
        assert_eq!(fake.omp().submitted, vec!["explain main".to_owned()]);
    }

    #[test]
    fn a_prompt_is_refused_while_omp_works_or_a_command_runs() {
        let (mut ctl, fake) = controller("", None);
        idle_composer(&mut ctl, &fake);
        assert_eq!(
            ctl.submit(submission(&ctl, 1, Command::Prompt { text: " \n".into() })),
            Err(Rejected::Empty)
        );
        // A queued command holds the injector.
        ctl.submit(submission(&ctl, 2, Command::Action(Action::AutoName)))
            .expect("accepted");
        assert_eq!(
            ctl.submit(submission(&ctl, 3, Command::Prompt { text: "a".into() })),
            Err(Rejected::Busy)
        );
        run(&mut ctl, |c, _| !c.is_busy());
        // omp streams a turn (its title shows a spinner).
        let _ = fake.out.send(RemoteEvent::Output(
            "\x1b]0;π ⠋ work\x07".as_bytes().to_vec(),
        ));
        run(&mut ctl, |c, _| c.activity().is_busy());
        assert_eq!(
            ctl.submit(submission(&ctl, 4, Command::Prompt { text: "a".into() })),
            Err(Rejected::Busy)
        );
        assert_eq!(fake.omp().submitted, vec!["/rename".to_owned()]);
    }

    /// omp streams a turn (its title shows a spinner).
    fn busy_composer(ctl: &mut SessionController, fake: &Fake) {
        idle_composer(ctl, fake);
        let _ = fake.out.send(RemoteEvent::Output(
            "\x1b]0;π ⠋ work\x07".as_bytes().to_vec(),
        ));
        run(ctl, |c, _| c.activity().is_busy());
    }

    #[test]
    fn steer_pastes_and_enters_while_omp_works() {
        let (mut ctl, fake) = controller("", None);
        busy_composer(&mut ctl, &fake);
        let s = submission(
            &ctl,
            1,
            Command::Steer {
                text: "go left".into(),
                follow_up: false,
            },
        );
        let first = ctl.submit(s).expect("accepted");
        let record = ctl.request(first.id).expect("recorded");
        assert_eq!(
            (record.kind, record.label.as_str()),
            (CommandKind::Steer { follow_up: false }, STEER_LABEL)
        );
        run(&mut ctl, |c, _| {
            c.request(first.id)
                .is_some_and(|r| r.delivery == Delivery::Delivered)
        });
        let omp = fake.omp();
        assert_eq!(omp.submitted, vec!["go left".to_owned()]);
        // No Esc (it would interrupt the turn): exactly the paste and one Enter.
        assert_eq!(omp.input, b"\x1b[200~go left\x1b[201~\r".to_vec());
    }

    #[test]
    fn follow_up_pastes_and_sends_ctrl_q_never_a_bare_enter() {
        let (mut ctl, fake) = controller("", None);
        busy_composer(&mut ctl, &fake);
        let s = submission(
            &ctl,
            1,
            Command::Steer {
                text: "then add tests".into(),
                follow_up: true,
            },
        );
        let first = ctl.submit(s).expect("accepted");
        run(&mut ctl, |c, _| {
            c.request(first.id)
                .is_some_and(|r| r.delivery == Delivery::Delivered)
        });
        let omp = fake.omp();
        assert_eq!(omp.followed_up, vec!["then add tests".to_owned()]);
        assert!(omp.submitted.is_empty());
        assert_eq!(omp.input, b"\x1b[200~then add tests\x1b[201~\x11".to_vec());
    }

    #[test]
    fn steering_refuses_blank_text_and_waits_for_a_turn() {
        let (mut ctl, fake) = controller("", None);
        idle_composer(&mut ctl, &fake);
        for follow_up in [false, true] {
            let blank = Command::Steer {
                text: " \n".into(),
                follow_up,
            };
            assert_eq!(ctl.submit(submission(&ctl, 1, blank)), Err(Rejected::Empty));
        }
        // Idle: accepted, then refused by the injector without a byte.
        let s = submission(
            &ctl,
            2,
            Command::Steer {
                text: "x".into(),
                follow_up: false,
            },
        );
        let id = ctl.submit(s).expect("accepted").id;
        run(&mut ctl, |c, _| {
            c.request(id).is_some_and(|r| {
                r.delivery == Delivery::Refused(Refusal::Precondition(Requires::Busy))
            })
        });
        assert!(fake.omp().input.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn compose_pastes_the_quoted_attachments_then_the_text_then_enter() {
        let (mut ctl, fake) = controller("", None);
        idle_composer(&mut ctl, &fake);
        let s = submission(
            &ctl,
            1,
            Command::Compose {
                attachments: vec!["/tmp/my shot.png".into()],
                text: "what is this?".into(),
                mode: ComposeMode::Send,
            },
        );
        let id = ctl.submit(s).expect("accepted").id;
        run(&mut ctl, |c, _| {
            c.request(id)
                .is_some_and(|r| r.delivery == Delivery::Delivered)
        });
        let omp = fake.omp();
        assert_eq!(
            omp.input,
            b"\x1b[200~/tmp/my\\ shot.png\x1b[201~\x1b[200~what is this?\x1b[201~\r".to_vec()
        );
        assert_eq!(
            omp.submitted,
            vec!["/tmp/my\\ shot.pngwhat is this?".to_owned()]
        );
    }

    #[test]
    fn compose_refuses_relative_attachments_and_nothing_to_send() {
        let (mut ctl, fake) = controller("", None);
        idle_composer(&mut ctl, &fake);
        let compose = |attachments: Vec<std::path::PathBuf>, text: &str| Command::Compose {
            attachments,
            text: text.into(),
            mode: ComposeMode::Send,
        };
        assert_eq!(
            ctl.submit(submission(&ctl, 1, compose(vec![], "  "))),
            Err(Rejected::Empty)
        );
        assert_eq!(
            ctl.submit(submission(&ctl, 2, compose(vec!["rel.png".into()], "x"))),
            Err(Rejected::BadPath)
        );
        assert!(fake.omp().input.is_empty());
    }

    #[test]
    fn a_request_dropped_from_the_bounded_history_is_never_accepted_again() {
        let (mut ctl, _fake) = controller("", None);
        // Stop needs a running turn: each is refused on the next drive and settles.
        let n = HISTORY as u64 + 10;
        for seq in 1..=n {
            ctl.submit(submission(&ctl, seq, Command::Action(Action::Stop)))
                .expect("accepted");
            ctl.drive(Instant::now());
        }
        assert!(ctl.history.len() <= HISTORY);
        assert_eq!(
            ctl.submit(submission(&ctl, 1, Command::Action(Action::Stop))),
            Err(Rejected::Expired)
        );
        let newest = ctl
            .submit(submission(&ctl, n, Command::Action(Action::Stop)))
            .expect("remembered");
        assert!(newest.duplicate);
        assert!(!ctl.is_busy(), "nothing was queued again");
    }

    #[test]
    fn a_read_only_viewer_terminal_takes_no_commands() {
        struct Follow(Receiver<RemoteEvent>);
        struct Stop;
        impl deck_term::Observer for Follow {
            fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn deck_term::ObserverTx>) {
                (Box::new(FakeRx(self.0)), Box::new(Stop))
            }
        }
        impl deck_term::ObserverTx for Stop {
            fn detach(&self) -> io::Result<()> {
                Ok(())
            }
        }
        let (_tx, rx) = mpsc::channel();
        let terminal = Terminal::observe_headless(
            Box::new(Follow(rx)),
            TermOptions::default(),
            Arc::new(|_| {}),
        );
        let mut ctl = SessionController::new(SessionKey::direct(), terminal, None);
        assert_eq!(
            ctl.submit(submission(&ctl, 1, Command::Action(Action::AutoName))),
            Err(Rejected::ReadOnly)
        );
    }

    #[test]
    fn a_viewer_joining_after_a_change_brings_earlier_viewers_up_to_date() {
        let (mut ctl, fake) = controller("", None);
        let first = ctl.subscribe_view(Arc::new(|| {}));
        let a = first.take_changed().expect("initial snapshot");
        // The screen changes; no drive publishes it before the second viewer joins.
        let _ = fake.out.send(RemoteEvent::Output(b"\r\nchanged".to_vec()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while ctl.snapshot() == *a {
            assert!(Instant::now() < deadline, "the screen never changed");
            ctl.terminal_mut().poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        let b = ctl.snapshot();
        let second = ctl.subscribe_view(Arc::new(|| {}));
        assert_eq!(second.take_changed().as_deref(), Some(&b));
        assert_eq!(first.take_changed().as_deref(), Some(&b));
        // Without further output nothing is published again, and both hold B.
        ctl.drive(Instant::now());
        assert!(first.take_changed().is_none());
        assert!(second.take_changed().is_none());
        assert_eq!(first.latest().as_deref(), Some(&b));
        assert_eq!(second.latest().as_deref(), Some(&b));
    }

    fn switch(ctl: &SessionController, seq: u64, selector: &str) -> Submission {
        submission(
            ctl,
            seq,
            Command::SwitchModel {
                selector: selector.to_owned(),
                level: None,
            },
        )
    }

    #[test]
    fn a_session_change_forgets_a_switch_that_was_never_sent() {
        let (mut ctl, _fake) = controller("half typed", Some("/s/a.jsonl"));
        let id = ctl.submit(switch(&ctl, 1, "a/new")).expect("accepted").id;
        run(&mut ctl, |_, ev| {
            ev.iter()
                .any(|e| matches!(e, ControllerEvent::Confirm { .. }))
        });
        assert_eq!(ctl.current_model().as_deref(), Some("a/new"));
        assert!(ctl.set_session_path(Some(PathBuf::from("/s/b.jsonl"))));
        assert!(ctl.pending_switch().is_none());
        assert_eq!(
            ctl.current_model(),
            None,
            "the cancelled selector is not current"
        );
        assert_eq!(
            ctl.request(id).map(|r| (r.delivery.clone(), r.runtime)),
            Some((
                Delivery::Cancelled(Cancel::SessionChanged),
                Runtime::Abandoned
            ))
        );
    }

    #[test]
    fn declining_one_draft_leaves_an_unrelated_queued_switch_pending() {
        let (mut ctl, _fake) = controller("half typed", None);
        let context = ctl
            .submit(submission(&ctl, 1, Command::Action(Action::AutoName)))
            .expect("accepted")
            .id;
        let sw = ctl.submit(switch(&ctl, 2, "a/new")).expect("accepted").id;
        run(&mut ctl, |_, ev| {
            ev.iter()
                .any(|e| matches!(e, ControllerEvent::Confirm { id, .. } if *id == context))
        });
        assert!(ctl.answer(&ctl.target().clone(), context, false));
        assert_eq!(
            ctl.request(context).map(|r| r.delivery.clone()),
            Some(Delivery::Declined)
        );
        let r = ctl.request(sw).expect("known");
        assert_eq!(r.runtime, Runtime::Awaiting, "still queued, still awaiting");
        assert_eq!(
            ctl.pending_switch().and_then(|p| p.selector.as_deref()),
            Some("a/new")
        );
        assert_eq!(ctl.current_model().as_deref(), Some("a/new"));
    }

    #[test]
    fn closing_an_exited_session_ends_every_request_still_under_way() {
        let (mut ctl, fake) = controller("half typed", None);
        let id = ctl
            .submit(submission(&ctl, 1, Command::Action(Action::AutoName)))
            .expect("accepted")
            .id;
        let _ = fake.out.send(RemoteEvent::Exited(Some(0)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ctl.terminal().is_exited() {
            assert!(Instant::now() < deadline, "the exit never arrived");
            ctl.terminal_mut().poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ctl.can_close(Instant::now()));
        assert_eq!(
            ctl.request(id).map(|r| r.delivery.clone()),
            Some(Delivery::Cancelled(Cancel::Exited))
        );
        let events = ctl.drive(Instant::now()).events;
        assert!(events.iter().any(|e| matches!(
            e,
            ControllerEvent::Ended {
                id: ended,
                delivery: Delivery::Cancelled(Cancel::Exited),
                ..
            } if *ended == id
        )));
        assert!(!ctl.is_busy());
    }

    #[test]
    fn closing_before_the_transport_took_the_bytes_reports_them_unobserved() {
        let (mut ctl, fake) = controller("", None);
        fake.omp().block_enter = true;
        let id = ctl
            .submit(submission(&ctl, 1, Command::Action(Action::AutoName)))
            .expect("accepted")
            .id;
        // All bytes written, the Enter stuck in the transport.
        run(&mut ctl, |c, _| !c.injector.is_busy());
        assert_eq!(
            ctl.request(id).map(|r| r.delivery.clone()),
            Some(Delivery::Writing)
        );
        assert!(ctl.is_busy(), "still waiting for the acknowledgement");
        // Long after the last write: the original close boundary.
        assert!(ctl.can_close(Instant::now() + Duration::from_secs(1)));
        assert_eq!(
            ctl.request(id).map(|r| r.delivery.clone()),
            Some(Delivery::Unobserved)
        );
        let events = ctl.drive(Instant::now()).events;
        assert!(events.iter().any(|e| matches!(
            e,
            ControllerEvent::Ended {
                id: ended,
                delivery: Delivery::Unobserved,
                ..
            } if *ended == id
        )));
        fake.omp().block_enter = false;
    }

    #[test]
    fn client_typing_is_refused_while_a_command_holds_input_and_resumes_after() {
        let (mut ctl, fake) = controller("", None);
        fake.omp().block_enter = true;
        let target = ctl.target().clone();
        let id = ctl
            .submit(submission(&ctl, 1, Command::Action(Action::AutoName)))
            .expect("accepted")
            .id;
        // The command is written; its Enter is still in the transport, so input stays held.
        run(&mut ctl, |c, _| !c.injector.is_busy());
        let (draft, input) = {
            let omp = fake.omp();
            (omp.draft.clone(), omp.input.clone())
        };
        let meta = InputMeta::default();
        assert_eq!(
            ctl.write_input(&target, b"x", meta),
            Err(Rejected::InputHeld)
        );
        let stale = SessionTarget {
            generation: target.generation + 1,
            ..target.clone()
        };
        assert!(matches!(
            ctl.write_input(&stale, b"x", meta),
            Err(Rejected::Stale { .. })
        ));
        // A target of another service instance with the same key and generation is stale too.
        let other_instance = SessionTarget {
            epoch: ServiceEpoch([0xa5; 16]),
            ..target.clone()
        };
        assert!(matches!(
            ctl.write_input(&other_instance, b"x", meta),
            Err(Rejected::Stale { .. })
        ));
        std::thread::sleep(Duration::from_millis(50));
        {
            let omp = fake.omp();
            assert_eq!(omp.draft, draft, "the draft was not touched");
            assert_eq!(omp.input, input, "nothing reached the transport");
        }
        // The Enter goes through; once the hold tail passed, typing is accepted again.
        fake.omp().block_enter = false;
        run(&mut ctl, |c, _| {
            c.request(id)
                .is_some_and(|r| r.delivery == Delivery::Delivered)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match ctl.write_input(&target, b"x", meta) {
                Ok(()) => break,
                Err(Rejected::InputHeld) => {
                    assert!(Instant::now() < deadline, "the hold never ended");
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("{e}"),
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fake.omp().input.ends_with(b"x") {
            assert!(Instant::now() < deadline, "the typed byte never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn closing_after_the_transport_took_the_bytes_keeps_them_delivered() {
        let (mut ctl, _fake) = controller("", None);
        let id = ctl
            .submit(submission(&ctl, 1, Command::Action(Action::AutoName)))
            .expect("accepted")
            .id;
        run(&mut ctl, |c, _| {
            c.request(id)
                .is_some_and(|r| r.delivery == Delivery::Delivered)
        });
        assert!(ctl.can_close(Instant::now() + Duration::from_secs(1)));
        assert_eq!(
            ctl.request(id).map(|r| r.delivery.clone()),
            Some(Delivery::Delivered)
        );
    }

    #[test]
    fn bytes_the_transport_refused_are_never_reported_delivered() {
        let (mut ctl, fake) = controller("", None);
        fake.omp().fail_enter = true;
        let id = ctl
            .submit(submission(&ctl, 1, Command::Action(Action::AutoName)))
            .expect("accepted")
            .id;
        let events = run(&mut ctl, |_, ev| {
            ev.iter()
                .any(|e| matches!(e, ControllerEvent::Ended { id: ended, .. } if *ended == id))
        });
        let delivery = events.iter().find_map(|e| match e {
            ControllerEvent::Ended {
                id: ended,
                delivery,
                ..
            } if *ended == id => Some(delivery.clone()),
            _ => None,
        });
        assert!(
            matches!(delivery, Some(Delivery::Cancelled(Cancel::Detached))),
            "{delivery:?}"
        );
        assert!(fake.omp().submitted.is_empty());
    }

    #[test]
    fn a_session_change_cancels_an_unanswered_draft_confirmation_and_ignores_its_answer() {
        let (mut ctl, fake) = controller("half typed", Some("/s/a.jsonl"));
        let id = ctl
            .submit(submission(&ctl, 1, Command::Action(Action::AutoName)))
            .expect("accepted")
            .id;
        let events = run(&mut ctl, |_, ev| {
            ev.iter()
                .any(|e| matches!(e, ControllerEvent::Confirm { .. }))
        });
        assert!(events.contains(&ControllerEvent::Confirm {
            id,
            label: "Auto-name".to_owned(),
            draft: "half typed".to_owned(),
        }));
        let old = ctl.target().clone();
        assert!(ctl.set_session_path(Some(PathBuf::from("/s/b.jsonl"))));
        assert!(!ctl.answer(&old, id, true), "the old conversation's answer");
        assert!(
            !ctl.answer(&ctl.target().clone(), id, true),
            "nor under the new target"
        );
        let events = ctl.drive(Instant::now()).events;
        assert!(events.iter().any(|e| matches!(
            e,
            ControllerEvent::Ended {
                delivery: Delivery::Cancelled(Cancel::SessionChanged),
                ..
            }
        )));
        for _ in 0..30 {
            ctl.tick(Instant::now());
            std::thread::sleep(Duration::from_millis(10));
        }
        let omp = fake.omp();
        assert_eq!(omp.draft, "half typed", "the new draft was not cleared");
        assert!(omp.input.is_empty(), "nothing was typed");
    }

    #[test]
    fn a_client_losing_its_rights_has_its_unwritten_requests_cancelled_and_no_one_elses() {
        let (mut ctl, fake) = controller("half typed", None);
        let phone = |ctl: &SessionController, seq: u64, command: Command| Submission {
            key: RequestKey {
                client: "d-phone".to_owned(),
                seq,
            },
            ..submission(ctl, seq, command)
        };
        // The phone's command waits for a decision on the desktop draft; another of its
        // commands and one of the desktop's queue behind it.
        let waiting = ctl
            .submit(phone(&ctl, 1, Command::Action(Action::AutoName)))
            .expect("accepted")
            .id;
        let queued = ctl
            .submit(phone(&ctl, 2, Command::Action(Action::AutoName)))
            .expect("accepted")
            .id;
        let desktop = ctl
            .submit(submission(&ctl, 3, Command::Action(Action::AutoName)))
            .expect("accepted")
            .id;
        run(&mut ctl, |_, ev| {
            ev.iter()
                .any(|e| matches!(e, ControllerEvent::Confirm { id, .. } if *id == waiting))
        });
        assert!(!ctl.cancel_client("someone-else"));
        assert!(ctl.cancel_client("d-phone"));
        for id in [waiting, queued] {
            assert_eq!(
                ctl.request(id).map(|r| r.delivery.clone()),
                Some(Delivery::Cancelled(Cancel::NotAllowed))
            );
        }
        let events = ctl.drive(Instant::now()).events;
        let ended: Vec<RequestId> = events
            .iter()
            .filter_map(|e| match e {
                ControllerEvent::Ended {
                    id,
                    delivery: Delivery::Cancelled(Cancel::NotAllowed),
                    ..
                } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(ended, [waiting, queued]);
        // The phone's draft question is gone and nothing of its was typed; the desktop's own
        // command now asks about the draft itself.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Confirm { id, .. } if *id == desktop)),
            "{events:?}"
        );
        assert!(!ctl.answer(&ctl.target().clone(), waiting, true));
        let omp = fake.omp();
        assert_eq!(omp.draft, "half typed");
        assert!(omp.input.is_empty(), "nothing was typed");
        assert!(!ctl.cancel_client("d-phone"), "nothing of it is left");
    }

    #[test]
    fn a_delivered_model_switch_is_not_a_confirmed_one() {
        let (mut ctl, fake) = controller("", None);
        let id = ctl
            .submit(submission(
                &ctl,
                1,
                Command::SwitchModel {
                    selector: "a/new".to_owned(),
                    level: None,
                },
            ))
            .expect("accepted")
            .id;
        run(&mut ctl, |c, _| {
            c.request(id)
                .is_some_and(|r| r.delivery == Delivery::Delivered)
        });
        assert_eq!(fake.omp().submitted, vec!["/switch a/new".to_owned()]);
        let r = ctl.request(id).expect("known");
        assert_eq!(r.runtime, Runtime::Awaiting);
        assert_eq!(ctl.current_model().as_deref(), Some("a/new"));
        // omp keeps reporting the old model past the timeout: the switch did not take.
        let facts = ModelFacts {
            model: Some("a/old"),
            shown: Some("a/old"),
            ..ModelFacts::default()
        };
        let progress = ctl.observe_model(Instant::now() + SWITCH_TIMEOUT * 2, facts);
        assert!(progress.unconfirmed.is_some());
        assert_eq!(
            ctl.request(id).map(|r| (r.delivery.clone(), r.runtime)),
            Some((Delivery::Delivered, Runtime::Unconfirmed))
        );
        assert_eq!(ctl.current_model().as_deref(), Some("a/old"));
    }
}
