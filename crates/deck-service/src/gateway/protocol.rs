//! The gateway wire protocol: handshake, typed requests and results, events and the binary
//! terminal frames of a frontend link. See [`super::wire`] for the framing.
//!
//! Connection kinds, chosen by [`Hello::attach`]:
//! - control: `Hello` → `Welcome`/`Refuse`, then [`Envelope`] requests answered by chunked
//!   [`Reply`]s (same id), interleaved with [`ServerEvent`]s. Events are small; state is fetched
//!   by query after a `Changed` (a lagging client gets one `Resync`, never an unbounded queue).
//! - frontend: `Hello{attach}` → `Welcome`/`Refuse`, then the terminal frames below. One
//!   connection per view, so a slow view only slows itself.
//!
//! Distinct failure classes: unsupported protocol / other profile / maintenance (handshake
//! [`Refusal`]), stale epoch or target, a closed transport (the client's own `Disconnected`), a
//! controller rejection ([`GatewayError::Rejected`]), an execution refusal
//! ([`GatewayError::Refused`]), a failed operation ([`GatewayError::Failed`]) and an uncertain
//! outcome after an owner crash ([`GatewayError::Uncertain`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use deck_core::agent_defs::{AgentFile, Catalog as AgentCatalog, Edit as AgentEdit, NewAgent};
use deck_core::agents::{AgentsSnapshot, AgentsTarget, TranscriptItem};
use deck_core::backup::server::{
    Protocol as ServerProtocol, ServerAccount, ServerAuth, ServerConfig,
};
use deck_core::backup::{
    BackupFile, BackupOutcome, ConflictChoice, Destination, LastBackup, Parts, ProjectBackup,
    ProjectBackupOutcome, ProjectRestorePlan, ProjectRestoreReport, ProjectSizes, ProjectSyncState,
    RestoreReport,
};
use deck_core::changes::{ChangedFile, Changes, DiffSide, FileDiff};
use deck_core::config::{GatewayMode, SessionView};
use deck_core::ctl::{CtlCommand, LiveSnapshot};
use deck_core::fleet::{FleetSnapshot, FleetTarget};
use deck_core::inventory::{InventoryEntry, SessionStats};
use deck_core::mux::MuxKind;
use deck_core::omp::{
    AdvisorDoc, AdvisorScope, DeleteTarget, ForeignSession, ForeignSource, ModelInfo, MoveOutcome,
    PluginConfig, PluginFeatures, PluginList, PluginScope, ResetAccounts, SettingsCatalog,
    UpdateKind, UpdateStatus,
};
use deck_core::selfupdate::{Applied, InstallKind, Release};
use deck_core::sessions::ProjectGroup;
use deck_core::status::SessionStatus;
use deck_core::usage::{Activity as UsageActivity, UsageSnapshot};
use deck_core::worktree::{
    BranchState, MergeTarget, Removal, Removed, RepoInfo, Worktree, WorktreeStatus,
};
use deck_term::InputMeta;
use serde::{Deserialize, Serialize};

use super::running::RunningSnapshot;
use super::{MaintenanceMarker, MaintenancePurpose, Profile};
use crate::controller::{
    Command, ControllerEvent, Rejected, RequestId, RequestKey, RequestRecord, ServiceEpoch,
    SessionKey, SessionTarget, Submitted,
};
use crate::notifications::inbox::Kind as InboxKind;
use crate::settings::{Edit, SettingsError, Snapshot};
use crate::status::TabStatus;
use crate::statusfmt::Activity;

/// Version of this protocol. A different version is refused at the handshake.
pub const PROTOCOL: u32 = 1;

/// Frame kinds.
pub mod kind {
    pub const HELLO: u8 = 1;
    pub const WELCOME: u8 = 2;
    pub const REFUSE: u8 = 3;
    /// Client → gateway: chunked [`super::Envelope`] JSON.
    pub const REQUEST: u8 = 10;
    /// Gateway → client: chunked [`super::Reply`] JSON (chunk id = request id).
    pub const REPLY: u8 = 11;
    /// Gateway → client: one [`super::ServerEvent`] JSON.
    pub const EVENT: u8 = 12;
    /// Client → gateway: leaving cleanly (its unwritten confirmations may be cancelled).
    pub const BYE: u8 = 13;

    // Frontend, gateway → client.
    /// Chunked replay (kitty + grid snapshot bytes; chunk id = snapshot sequence).
    pub const F_SNAPSHOT: u8 = 20;
    /// Raw output bytes.
    pub const F_OUTPUT: u8 = 21;
    /// `cols, rows, cell_width, cell_height` as u16 LE.
    pub const F_RESIZE: u8 = 22;
    /// [`deck_term::FrontendState`] JSON.
    pub const F_STATE: u8 = 23;
    /// Exit code JSON (`Option<i32>`): the process really ended.
    pub const F_EXITED: u8 = 24;
    /// [`super::InputRefused`] JSON: one write was refused; the view goes on.
    pub const F_REFUSED: u8 = 25;
    /// [`super::ViewEnd`] JSON: the view ended without a process exit.
    pub const F_ENDED: u8 = 26;

    // Frontend, client → gateway.
    /// `ctrl_c u8 | has_mods u8 | mods u32 LE | bytes`.
    pub const U_INPUT: u8 = 30;
    /// The exact OSC 52 reply for the outstanding clipboard read.
    pub const U_CLIP: u8 = 31;
    /// `u8` focused.
    pub const U_FOCUS: u8 = 32;
    /// `cols, rows, cell_width, cell_height` as u16 LE.
    pub const U_RESIZE: u8 = 33;
    /// 19 × RGB.
    pub const U_PALETTE: u8 = 34;
    /// Kill the session's process (this session only).
    pub const U_KILL: u8 = 35;
    /// End this view only.
    pub const U_DETACH: u8 = 36;
}

/// Who connects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    /// A native desktop window: counts for the lifetime, reports focus/visibility.
    Gui,
    /// A command-line client (`omp-deck-gateway request|status|drain…`).
    Cli,
    /// The OS-started supervisor: not a GUI, no session rights, never a mutation owner.
    Supervisor,
}

/// What a frontend view may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewRole {
    /// Types, and owns focus, geometry and the clipboard (one per session; a new one takes over
    /// from the previous controlling view, which keeps typing rights).
    Controlling,
    /// Types; geometry, focus and clipboard stay with the controlling view.
    Input,
    /// Follows the output only.
    Observer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attach {
    pub target: SessionTarget,
    pub view: ViewRole,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub version: String,
    pub role: Role,
    /// Stable for the client process; the `client` of its [`RequestKey`]s. A frontend link
    /// uses its parent client's id.
    pub client_id: String,
    pub pid: u32,
    /// The profile the client expects; a different served profile is refused.
    pub profile: Profile,
    /// `Some` = a frontend link for this session view.
    pub attach: Option<Attach>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol: u32,
    pub version: String,
    pub epoch: ServiceEpoch,
    pub pid: u32,
    pub profile: Profile,
    pub mode: GatewayMode,
    pub maintenance: Option<MaintenanceMarker>,
}

/// Why a handshake was refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum Refusal {
    #[error("gateway protocol {theirs} is not supported by this client (protocol {ours})")]
    Unsupported { ours: u32, theirs: u32 },
    #[error("the running gateway serves another profile (settings {}, sessions {})", served.settings.display(), served.sessions_root.display())]
    ProfileMismatch { served: Profile },
    #[error("OmpDeck is being updated or removed")]
    Maintenance(MaintenanceMarker),
    /// A frontend attach for a target this instance does not hold (another epoch, a restarted
    /// session or a closed one).
    #[error("the session changed")]
    Stale { current: Option<SessionTarget> },
    /// This frontend may not attach (an observer-only session, a refused role).
    #[error("{0}")]
    NotAllowed(String),
    #[error("the gateway is shutting down")]
    ShuttingDown,
}

/// One request on a control link.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub id: u64,
    pub body: Request,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    /// Focus/visibility of a GUI (notification policy input). Replaces the previous state.
    ClientState(ClientState),
    Subscribe(Topic),
    Unsubscribe(Topic),
    /// Read-only; never journaled.
    Query(Query),
    /// A journaled, epoch-bound mutation.
    Mutate {
        key: RequestKey,
        epoch: ServiceEpoch,
        op: Box<Mutation>,
    },
    /// The record of an earlier mutation; never executes anything.
    Outcome(RequestKey),
    /// Wait until every earlier settings edit is committed.
    Flush,
    /// Bundle maintenance (CLI): raise the barrier and drain, or lower it.
    Maintenance(MaintenanceOp),
    /// The sessions this client's agents dashboard follows ([`Topic::Fleet`]); replaces the
    /// client's previous list. The gateway follows the union of every client's lists.
    SetFleetTargets(Vec<FleetTarget>),
    /// The session whose subagents this client shows ([`Topic::Agents`]).
    SetAgentsTarget(Option<AgentsTarget>),
    /// What this client's status bar shows about a session's model: resolves that session's
    /// optimistic model switch in the gateway ([`crate::controller::SessionController::observe_model`]).
    /// A switch that did not take is reported as [`ServerEvent::ModelUnconfirmed`].
    ObserveModel {
        target: SessionTarget,
        facts: ModelFactsView,
    },
}

/// [`crate::controller::ModelFacts`] as sent by a client.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFactsView {
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub configured: Option<String>,
    pub shown: Option<String>,
    pub shown_is_default: bool,
}

/// A model/thinking switch ([`crate::model_switch::Pending`] without its clock).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchInfo {
    pub selector: Option<String>,
    pub level: Option<String>,
}

impl From<&crate::model_switch::Pending> for SwitchInfo {
    fn from(p: &crate::model_switch::Pending) -> SwitchInfo {
        SwitchInfo {
            selector: p.selector.clone(),
            level: p.level.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaintenanceOp {
    /// Raise the barrier (refuse new mutations and launches) and report the drain state.
    Begin(MaintenancePurpose),
    /// Current drain state (`Drained` once nothing accepted is left).
    Poll,
    /// Barrier stays (the CLI removes the marker); the owner exits once drained.
    Exit,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientState {
    pub focused: bool,
    /// Sessions a view of this client shows.
    pub visible: Vec<SessionTarget>,
    /// The session of the client's active tab ([`Topic::Status`] follows the focused client's).
    pub active: Option<SessionTarget>,
}

/// Live state a client caches. Always-on topics are kept current by the gateway; the
/// interest-driven ones run their watchers only while someone subscribes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Topic {
    Gateway,
    Settings,
    Sessions,
    Running,
    Controllers,
    Notifications,
    Status,
    Usage,
    AppUpdate,
    /// The backup destinations ([`BackupView`]).
    Backup,
    Inventory,
    Changes(PathBuf),
    Tree(PathBuf),
    /// The sessions of [`Request::SetFleetTargets`].
    Fleet,
    /// The session of [`Request::SetAgentsTarget`].
    Agents,
    /// A subagent transcript file.
    Transcript(PathBuf),
    /// The live-control state of a controlled session.
    Live(SessionKey),
    /// omp's version and update check.
    OmpUpdate,
    /// Mobile access: the relay link, the gateway's key fingerprint, paired devices and the
    /// open pairing window (never a secret).
    Mobile,
    /// The worktree state of the git repository holding this folder ([`WorktreesView`]).
    Worktrees(PathBuf),
}

impl Topic {
    /// Watchers run only while subscribed.
    pub fn interest_driven(&self) -> bool {
        matches!(
            self,
            Topic::Inventory
                | Topic::Changes(_)
                | Topic::Tree(_)
                | Topic::Fleet
                | Topic::Agents
                | Topic::Transcript(_)
                | Topic::Worktrees(_)
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Query {
    Topic(Topic),
    /// The visible screen text of a controlled session.
    Screen(SessionTarget),
    /// `deck_core::usage::activity` over `days`.
    UsageActivity {
        days: u32,
    },
    OmpVersion,
    OmpCheckUpdate,
    Models,
    /// omp's settings catalog with secret values replaced by `SECRET_PLACEHOLDER`.
    OmpSettings,
    Diff {
        root: PathBuf,
        file: ChangedFile,
        side: DiffSide,
    },
    Patch {
        root: PathBuf,
    },
    RepoInfo(PathBuf),
    CheckBranch {
        repo: PathBuf,
        name: String,
    },
    /// Where `branch` would get its worktree (`dest`, else the default location below the
    /// repository's primary checkout) and whether that location can be used now. The check is
    /// the one [`Mutation::WorktreeNew`] applies.
    WorktreeDestination {
        repo: PathBuf,
        branch: String,
        dest: Option<PathBuf>,
    },
    ResolveWorktree(PathBuf),
    WorktreeStatus {
        worktree: Worktree,
        base: String,
    },
    InferBase(Worktree),
    MergeTarget {
        primary: PathBuf,
        base: String,
    },
    /// Which controlled sessions run inside `worktree` (target and label).
    WorktreeSessions(Worktree),
    /// Whether a (parked) branch still exists, where it is checked out and whether it is merged.
    BranchState {
        primary: PathBuf,
        branch: String,
        base: String,
    },
    /// Config backups at the active destination, newest first.
    BackupList,
    /// Project backups at the active destination.
    ProjectBackups,
    /// The active destination's project sync record.
    ProjectSyncState,
    ProjectMeasure {
        project: PathBuf,
    },
    Stats(Vec<PathBuf>),
    FindFork {
        parent_id: String,
        since_unix_ms: u64,
    },
    FindCopy {
        parents: Vec<String>,
        dest_cwd: PathBuf,
        since_unix_ms: u64,
    },
    /// Every live backend session with its memory, and the tmux server's own (the session
    /// manager's Running view and RAM column).
    RunningDetail,
    /// Whether `worktree` can be removed or parked now, and why not ([`WorktreeVerdict`]).
    WorktreeVerdict(Worktree),
    /// The custom agent definitions omp reads: the user's and those of the projects these
    /// session cwds resolve to, with omp's agent settings.
    AgentDefs {
        projects: Vec<PathBuf>,
    },
    /// `omp plugin list --json`: npm and marketplace plugins with their manifests.
    PluginList,
    /// `omp plugin config list <pkg> --json`: stored values (secrets masked) and the manifest
    /// schema.
    PluginConfig {
        pkg: String,
    },
    /// The saved usage resets (`omp usage reset --json`) of every stored account, or of one
    /// provider (`anthropic` / `openai-codex`). Queries the providers: can take seconds.
    UsageResets {
        provider: Option<String>,
    },
    /// The Claude Code or Codex sessions omp finds, newest first.
    ForeignSessions {
        source: ForeignSource,
    },
    /// The `WATCHDOG.yml` of `scope` for the project of `cwd`, as omp reads it (an absent file
    /// is an empty document).
    AdvisorConfig {
        cwd: PathBuf,
        scope: AdvisorScope,
    },
    /// The ids of the sessions pinned in omp's `session-pins.json` (read-only).
    SessionPins,
}

/// A query's result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Answer {
    /// Boxed: topic values are large next to the other answers (JSON is unchanged).
    Topic(Box<TopicValue>),
    Screen(ScreenView),
    UsageActivity(UsageActivity),
    OmpVersion(String),
    OmpCheckUpdate(UpdateStatus),
    Models(Vec<ModelInfo>),
    OmpSettings(SettingsCatalog),
    Diff(FileDiff),
    Patch(String),
    RepoInfo(RepoInfo),
    CheckBranch,
    WorktreeDestination(DestinationView),
    ResolveWorktree(Option<Worktree>),
    WorktreeStatus(WorktreeStatus),
    InferBase(Option<String>),
    MergeTarget(MergeTarget),
    WorktreeSessions(Vec<(SessionTarget, String)>),
    BranchState(BranchState),
    BackupList(Vec<BackupFile>),
    ProjectBackups(Vec<ProjectBackup>),
    ProjectSyncState(ProjectSyncState),
    ProjectMeasure(ProjectSizes),
    Stats(Vec<(PathBuf, Option<SessionStats>)>),
    Path(Option<PathBuf>),
    Flushed,
    Maintenance(DrainState),
    /// A mutation was accepted (see [`Accepted`]).
    Accepted(Accepted),
    /// [`Request::Outcome`].
    Outcome(Outcome),
    /// Subscribe/Unsubscribe/ClientState done.
    Done,
    RunningDetail(RunningDetail),
    AgentDefs(AgentCatalog),
    WorktreeVerdict(WorktreeVerdict),
    PluginList(PluginList),
    PluginConfig(PluginConfig),
    UsageResets(ResetAccounts),
    ForeignSessions(Vec<ForeignSession>),
    AdvisorConfig(AdvisorDoc),
    SessionPins(BTreeSet<String>),
}

/// [`Query::WorktreeDestination`]'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestinationView {
    /// The location asked for, or the default one.
    pub dest: PathBuf,
    /// Why `dest` cannot be used (`None`: it can).
    pub error: Option<String>,
}

/// [`Query::WorktreeVerdict`]: the worktree's merge target, its git state and what
/// [`Mutation::WorktreeRetire`] would do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeVerdict {
    pub base: String,
    pub status: WorktreeStatus,
    pub verdict: Retirement,
}

/// What retiring a worktree can do now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Retirement {
    /// Clean and merged into its base: remove it (`park: false`).
    Remove,
    /// Clean and pushed, not merged yet (an open PR): park it (`park: true`).
    Park,
    /// Neither; the explanation.
    Blocked(String),
}

/// [`Topic::Worktrees`]: the repository holding `dir`, its linked worktrees with their merge
/// targets and running flows, and the branches parked from it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreesView {
    pub dir: PathBuf,
    /// `None` until the first look finished; `Err`: not a repository, or git failed.
    pub repo: Option<Result<RepoInfo, String>>,
    /// `dir`'s own linked worktree (`None`: the main checkout or no repository).
    pub worktree: Option<Worktree>,
    /// Every linked worktree of the repository.
    pub linked: Vec<WorktreeEntry>,
    /// The branches parked from this repository.
    pub parked: Vec<ParkedEntry>,
}

/// One linked worktree of [`WorktreesView`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeEntry {
    pub worktree: Worktree,
    /// Its merge target: the recorded one, else inferred; `None`: unknown.
    pub base: Option<String>,
    /// `base` was recorded when the worktree was made (not inferred).
    pub base_recorded: bool,
    /// The last look found it clean and pushed but not merged (park, not remove).
    pub parkable: bool,
    /// A finish or retire flow running on it.
    pub flow: Option<WorktreeFlow>,
}

/// A flow the gateway runs on a worktree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorktreeFlow {
    /// [`Mutation::WorktreeFinish`] `job`; `stage`: its latest progress text.
    Finishing {
        job: RequestKey,
        stage: Option<String>,
    },
    /// [`Mutation::WorktreeRetire`] (or a finish that reached the removal).
    Retiring { job: RequestKey, park: bool },
}

/// A parked branch and where it stands (`state`: `None` until looked at).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedEntry {
    pub record: deck_core::config::ParkedBranch,
    pub state: Option<BranchState>,
}

/// Where [`Mutation::WorktreeContinue`] continues a parked branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ContinueIn {
    /// A new worktree of the branch at the managed default location; the branch's sessions
    /// move into it.
    Worktree,
    /// The main checkout switches to the branch.
    Main,
    /// The main checkout switches back to the branch's base; the branch stays parked.
    SwitchBack,
}

/// [`Query::RunningDetail`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningDetail {
    /// A name listed by several backends belongs to the first.
    pub sessions: Vec<deck_core::mux::RunningSession>,
    /// The tmux backend is available.
    pub tmux: bool,
    /// Memory of the tmux server process alone.
    pub tmux_server: Option<deck_core::platform::TreeMemory>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenView {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

/// The state of one topic.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TopicValue {
    Gateway(GatewayStatus),
    Settings(Snapshot),
    Sessions(SessionsView),
    Running(RunningSnapshot),
    Controllers(Vec<ControllerInfo>),
    Notifications(InboxView),
    /// The status of the focused client's active session (boxed: it carries the full
    /// [`SessionStats`] with the per-source cost detail).
    Status(Box<Option<(SessionTarget, SessionStatus)>>),
    Usage(UsageView),
    AppUpdate(AppUpdateView),
    Backup(BackupView),
    Inventory(Vec<InventoryEntry>),
    Changes(Result<Option<Changes>, String>),
    Tree {
        revision: u64,
        tree: Option<deck_core::tree::SessionTree>,
        error: Option<String>,
    },
    Fleet(Option<FleetSnapshot>),
    Agents(Option<AgentsSnapshot>),
    Transcript {
        revision: u64,
        items: Vec<TranscriptItem>,
    },
    /// Boxed: the snapshot carries the agents, the context breakdown and the session's accounts.
    Live {
        target: Option<SessionTarget>,
        connected: bool,
        snapshot: Option<Box<LiveSnapshot>>,
    },
    OmpUpdate(OmpUpdateView),
    Mobile(MobileView),
    Worktrees(Box<WorktreesView>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OmpUpdateView {
    pub version: Option<String>,
    pub status: Option<Result<UpdateStatus, String>>,
    pub checked_unix_ms: Option<u64>,
    pub checking: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effective {
    /// Runs while a GUI or accepted work needs it.
    OnDemand,
    /// Supervised and kept alive across logins.
    Background,
    /// Maintenance barrier up: draining, refusing new work.
    Maintenance(MaintenancePurpose),
    /// No GUI and no work left: exiting.
    Stopping,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Supervision {
    /// Not requested (mode is not Background).
    None,
    /// Registered with the OS and its supervisor is connected.
    Active { registration: String },
    /// Registered, but no supervisor has connected (yet).
    Registered { registration: String },
    /// Background requested but registering or starting failed: the mode was rolled back.
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobInfo {
    pub key: RequestKey,
    pub label: String,
    pub progress: Option<Progress>,
    pub cancellable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayStatus {
    pub epoch: ServiceEpoch,
    pub pid: u32,
    pub version: String,
    pub profile: Profile,
    pub mode: GatewayMode,
    pub effective: Effective,
    pub supervision: Supervision,
    pub gui_clients: usize,
    pub cli_clients: usize,
    pub supervisor: bool,
    pub jobs: Vec<JobInfo>,
    /// Direct-PTY programs still running (they hold an owner until they exit).
    pub direct_programs: usize,
    pub backend_banner: Option<String>,
    /// Every backend kind found.
    pub backends: Vec<MuxKind>,
    /// The kind new sessions use.
    pub primary_backend: Option<MuxKind>,
    pub omp_found: bool,
    pub live_control: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionsView {
    pub generation: u64,
    pub root: PathBuf,
    pub root_exists: bool,
    pub groups: std::sync::Arc<Vec<ProjectGroup>>,
}

/// Where a controlled session runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendRef {
    Mux {
        kind: MuxKind,
        name: String,
    },
    /// A direct child of the gateway (ends with it): a draining job until it exits.
    Direct,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRole {
    /// An omp session.
    Session,
    /// An omp/plugin update or install run.
    Tool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControllerInfo {
    pub target: SessionTarget,
    pub role: SessionRole,
    pub label: String,
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,
    pub backend: BackendRef,
    pub title: Option<String>,
    pub activity: Activity,
    pub exit_code: Option<i32>,
    /// The indicator without the client's own bell marker ([`ServerEvent::Bell`]).
    pub status: TabStatus,
    pub busy: bool,
    pub current_model: Option<String>,
    pub pending_switch: Option<SwitchInfo>,
    pub assumed_switch: Option<SwitchInfo>,
    /// The launch created the session with the live-control extension: its socket is expected.
    pub ctl_expect: bool,
    /// Live control socket connected.
    pub live_control: bool,
    pub spawned_unix_ms: u64,
    pub ctl_key: Option<String>,
    /// The folder omp last reported for the session (its terminal breadcrumb), when it belongs
    /// to the session's current process; `None` when unknown or direct.
    pub live_cwd: Option<PathBuf>,
    /// Newest last.
    pub requests: Vec<RequestRecord>,
    /// Views attached now.
    pub views: usize,
    /// A controlling view owns geometry.
    pub controlled: bool,
    /// The view the session actually runs in: `Chat` only for a host session started in TSP
    /// mode (a Chat request on a backend that cannot carry TSP runs as `Terminal`).
    #[serde(default)]
    pub view: SessionView,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxEntry {
    pub target: SessionTarget,
    pub kind: InboxKind,
    pub title: String,
    pub project: String,
    pub body: String,
    pub at_unix_ms: u64,
    pub armed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxView {
    /// Newest first.
    pub entries: Vec<InboxEntry>,
    pub warns: bool,
    pub badge: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageView {
    pub snapshot: Option<UsageSnapshot>,
    pub fetched_unix_ms: Option<u64>,
    pub error: Option<String>,
    pub fetching: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdatePhase {
    Idle,
    Checking,
    Downloading {
        done: u64,
        total: Option<u64>,
    },
    /// Installed; the client restarts into it after flushing.
    Applied(Applied),
    /// Could not be installed automatically (e.g. a package without pkexec): the user runs
    /// `command`. The download is kept.
    NeedsManual {
        command: String,
    },
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppUpdateView {
    pub install: InstallKind,
    pub current: String,
    /// A newer release (and its asset for this install) when one was found.
    pub latest: Option<Release>,
    pub asset: Option<String>,
    pub checked_unix_ms: Option<u64>,
    pub phase: UpdatePhase,
}

/// Where backups go and how that destination stands. Secrets (Google tokens, the server's
/// password or key passphrase) never appear here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupView {
    /// The active destination (`AppConfig::backup_destination`).
    pub destination: Destination,
    /// Google client credentials are built in / configured.
    pub drive_available: bool,
    pub drive_signed_in: bool,
    pub drive_email: Option<String>,
    /// The saved backup server.
    pub server: Option<ServerView>,
    /// The active destination's last upload from this machine.
    pub last_backup: Option<LastBackup>,
    /// A backup-lane job is running (only one at a time, any destination).
    pub busy: Option<String>,
    /// The last automatic backup's failure at the active destination.
    pub last_error: Option<String>,
    pub last_auto_unix_ms: Option<u64>,
}

/// [`Topic::Mobile`]: everything the desktop shows about mobile access. Never carries a secret
/// (room secret, admission tokens, PSK or invite).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileView {
    pub relay: Option<RelayView>,
    pub link: RelayLink,
    /// The gateway's static key fingerprint (empty when its key is unavailable).
    pub fingerprint: String,
    pub devices: Vec<DeviceView>,
    pub pairing: Option<PairingView>,
}

/// The configured relay: its base URL and printable room id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayView {
    pub url: String,
    pub room_id: String,
}

/// State of the gateway's relay connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelayLink {
    /// Not running (mode `Off` or no relay configured).
    Off,
    /// Connecting (`attempt` counts from 1 since the last time it was up); `error` is why the
    /// previous attempt ended.
    Connecting {
        attempt: u32,
        error: Option<String>,
    },
    Up,
    /// The relay (or this gateway's own state) refuses the link; retried slowly.
    Refused {
        reason: String,
    },
}

/// A paired device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub platform: String,
    /// The device's static key fingerprint.
    pub fingerprint: String,
    pub permissions: Permissions,
    pub created_unix_ms: u64,
    pub last_seen_unix_ms: u64,
    /// An encrypted session with it is open now.
    pub online: bool,
    /// It has a Web Push subscription.
    #[serde(default)]
    pub push: bool,
}

/// What a device may do beyond viewing (view is implied).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Permissions {
    /// Prompts, runtime commands, terminal input, stop.
    pub control: bool,
    /// Copy/move/delete, git discard, worktree remove, restores, omp config.
    pub manage: bool,
}

impl Default for Permissions {
    /// The approval proposal: View + Control, Manage opt-in.
    fn default() -> Permissions {
        Permissions {
            control: true,
            manage: false,
        }
    }
}

impl BackupView {
    /// The active destination is set up (signed in / a server saved).
    pub fn linked(&self) -> bool {
        match self.destination {
            Destination::Drive => self.drive_available && self.drive_signed_in,
            Destination::Server => self.server.is_some(),
        }
    }

    /// The active destination's name for the UI: "Google Drive", `sftp://host` or "server".
    pub fn place(&self) -> String {
        match (self.destination, &self.server) {
            (Destination::Drive, _) => "Google Drive".to_owned(),
            (Destination::Server, Some(s)) => s.label.clone(),
            (Destination::Server, None) => "server".to_owned(),
        }
    }
}

/// The open pairing window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingView {
    /// Identifies this window in [`Mutation::PairingApprove`]/[`Mutation::PairingDeny`] (not a
    /// secret).
    pub pairing: String,
    /// When the current stage times out: the window while waiting, the approval wait once a
    /// device asks.
    pub expires_unix_ms: u64,
    pub state: PairingState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingState {
    /// Waiting for a device to scan the invite.
    Waiting,
    /// A device proved the invite's PSK and waits for the desktop's decision; compare
    /// `fingerprint` with the one the phone shows.
    Approval {
        name: String,
        platform: String,
        fingerprint: String,
    },
}

/// Text that must never reach a log: serialized as a plain string, but `Debug`/`Display` print
/// `<redacted>`, and the memory is wiped on drop.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretText(String);

impl SecretText {
    pub fn new(text: impl Into<String>) -> SecretText {
        SecretText(text.into())
    }

    /// The secret itself (for the one place that needs it).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretText {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

impl std::fmt::Debug for SecretText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl std::fmt::Display for SecretText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The most backups one [`Mutation::DeleteBackups`] / [`Mutation::ProjectDeleteBackups`]
/// deletes.
pub const MAX_BULK_DELETE: usize = 100;

/// A saved backup server without its password or key passphrase.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerView {
    pub protocol: ServerProtocol,
    pub host: String,
    /// The port dialled.
    pub port: u16,
    /// `None` = anonymous.
    pub user: Option<String>,
    /// SFTP key authentication: the private key file.
    pub key_path: Option<PathBuf>,
    pub folder: String,
    /// A trusted host key / certificate fingerprint is pinned.
    pub pinned: bool,
    /// `sftp://host[:port]`.
    pub label: String,
}

impl ServerView {
    /// The same server, login and folder as `other`: what [`ServerConfig::same_place`] decides
    /// for the saved configurations these views were made from (what is there is what cached
    /// listings and the saved sync state describe).
    pub fn same_place(&self, other: &ServerView) -> bool {
        self.protocol == other.protocol
            && self.host.eq_ignore_ascii_case(&other.host)
            && self.port == other.port
            && self.user == other.user
            && self.folder == other.folder
    }

    pub fn of(account: &ServerAccount) -> ServerView {
        let c = &account.config;
        ServerView {
            protocol: c.protocol,
            host: c.host.clone(),
            port: c.effective_port(),
            user: c.auth.user().map(str::to_owned),
            key_path: match &c.auth {
                ServerAuth::Key { key_path, .. } => Some(key_path.clone()),
                _ => None,
            },
            folder: c.folder.clone(),
            pinned: c.pinned.is_some(),
            label: c.label(),
        }
    }
}

/// [`Mutation::ServerConnect`]'s answer. Other failures are [`GatewayError::Failed`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerConnectOutcome {
    /// Checked and saved.
    Connected(ServerView),
    /// Trust on first use: nothing was saved. Show `fingerprint`; connect again with it as
    /// `pinned` once the user trusts it.
    Untrusted { fingerprint: String },
    /// The server presented another key or certificate than the pinned one: refused. The user
    /// removes the server and adds it again if the change is expected.
    Changed { pinned: String, presented: String },
}
/// A mutation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Mutation {
    /// Open (attach or create) a session; the result is its target.
    Launch(LaunchSpec),
    /// Kill and relaunch the session with the same identity request, in `view` (`None`: the
    /// view it runs in now). `Some(other)` is "switch view": the same session file resumed in
    /// the other mode.
    Restart {
        target: SessionTarget,
        view: Option<SessionView>,
    },
    /// Graceful stop of this session's backend process only.
    Stop(SessionTarget),
    /// Kill this session's process only.
    Kill(SessionTarget),
    /// Kill exactly these backend sessions (`wait`: graceful stop, then wait until gone); any
    /// controller of one ends like [`Mutation::Kill`].
    KillNames {
        names: Vec<String>,
        wait: bool,
    },
    /// Close the gateway's controller (backend sessions keep running; a direct program is
    /// killed only if `kill_direct`).
    Close {
        target: SessionTarget,
        kill_direct: bool,
    },
    /// An injected command through the controller.
    Submit {
        target: SessionTarget,
        command: Command,
    },
    /// The answer to a draft confirmation.
    Answer {
        target: SessionTarget,
        id: RequestId,
        clear: bool,
    },
    /// Set (or clear) the session file of a controlled session the client resolved.
    SetSessionPath {
        target: SessionTarget,
        path: Option<PathBuf>,
    },
    ForkAt {
        file: PathBuf,
        entry_id: String,
    },
    Copy {
        file: PathBuf,
        dest_cwd: PathBuf,
    },
    /// Stop the listed backend sessions, then move.
    Move {
        file: PathBuf,
        dest_cwd: PathBuf,
        stop: Vec<String>,
    },
    /// Stop the listed backend sessions, then delete.
    Delete {
        targets: Vec<DeleteTarget>,
        stop: Vec<String>,
    },
    ExportHtml {
        file: PathBuf,
        dest: PathBuf,
    },
    Ctl {
        target: SessionTarget,
        command: CtlCommand,
    },
    Stage {
        root: PathBuf,
        files: Vec<ChangedFile>,
    },
    Unstage {
        root: PathBuf,
        files: Vec<ChangedFile>,
    },
    Discard {
        root: PathBuf,
        files: Vec<ChangedFile>,
    },
    DeleteUntracked {
        root: PathBuf,
        files: Vec<ChangedFile>,
    },
    WorktreeCreate {
        repo: PathBuf,
        branch: String,
        base_ref: String,
        dest: PathBuf,
    },
    /// Stops the listed sessions running in it first, then moves every session file of the
    /// worktree (a fresh scan of the store, stubs included) into the worktree's primary
    /// repository with omp's `/move`, then removes it: merged (`Removal::Merged`) or parked with
    /// its branch kept (`Removal::Park`, clean + pushed). `moved`: running sessions the client
    /// already moved out with [`Command::MoveTo`](crate::controller::Command::MoveTo); they keep
    /// running. Refused while another session runs in the worktree.
    WorktreeRemove {
        worktree: Worktree,
        base: String,
        how: Removal,
        stop: Vec<SessionTarget>,
        moved: Vec<SessionTarget>,
    },
    /// Moves closed sessions into `dest` with omp's own `/move` (headless).
    RelocateSessions {
        files: Vec<PathBuf>,
        dest: PathBuf,
    },
    /// A worktree at `dest` on the existing `branch` (a parked one).
    WorktreeAddExisting {
        repo: PathBuf,
        branch: String,
        dest: PathBuf,
    },
    /// `git switch <branch>` in `dir` (refused when the checkout is dirty).
    SwitchBranch {
        dir: PathBuf,
        branch: String,
    },
    OmpSet {
        key: String,
        value: serde_json::Value,
    },
    OmpReset {
        key: String,
    },
    /// Replace `provider`'s entries of omp's `auth.accountPolicies` with `policies` (empty: back to
    /// omp's automatic order); other providers' entries are kept as they are.
    AccountPolicies {
        provider: String,
        policies: Vec<deck_core::usage::AccountPolicy>,
    },
    /// Edit one agent definition file if it still has `revision` (see `agent_defs::save`).
    AgentEdit {
        path: PathBuf,
        revision: String,
        edit: AgentEdit,
    },
    /// Write a new definition `<dir>/<name>.md` (see `agent_defs::create`); never replaces one.
    AgentCreate {
        dir: PathBuf,
        agent: NewAgent,
    },
    /// Copy omp's built-in `name` into `dir` as an editable definition that replaces it.
    AgentCustomize {
        dir: PathBuf,
        name: String,
    },
    /// Delete one agent definition file if it still has `revision`.
    AgentDelete {
        path: PathBuf,
        revision: String,
    },
    /// An omp update/install run in a session of its own; the result is its target.
    RunTool(Tool),
    Settings(Edit),
    SetGatewayMode(GatewayMode),
    DriveLoginBegin,
    DriveLoginFinish(u64),
    DriveLoginCancel(u64),
    DriveSignOut,
    /// Back up the omp config to the active destination.
    Backup {
        force: bool,
    },
    PrepareRestore(BackupFile),
    ApplyRestore(u64),
    DiscardRestore(u64),
    /// Permanently deletes a config backup (only config backups are accepted).
    DeleteBackup(BackupFile),
    /// Permanently deletes config backups one after another (at most [`MAX_BULK_DELETE`]),
    /// stopping at the first failure; the failure names how many were deleted before it.
    DeleteBackups(Vec<BackupFile>),
    /// Check and save the backup server. `keep_secret`: an empty password / missing passphrase
    /// takes the saved server's (same login), so editing never needs the secret again.
    ServerConnect {
        config: ServerConfig,
        keep_secret: bool,
    },
    /// Forget the saved server (local settings only; nothing on the server is touched).
    ServerForget,
    ProjectBackup {
        project: PathBuf,
        parts: Parts,
        excluded: BTreeSet<String>,
        force: bool,
    },
    ProjectDeleteBackup(ProjectBackup),
    /// Project backups deleted one after another, as [`Mutation::DeleteBackups`].
    ProjectDeleteBackups(Vec<ProjectBackup>),
    ProjectPrepareRestore {
        backup: ProjectBackup,
        target: PathBuf,
        parts: Parts,
    },
    /// Apply a prepared restore (only by the client that prepared it). `conflicts` decides each
    /// backed-up session whose id exists locally (missing = keep the local one); `excluded` =
    /// backed-up sessions left out of the restore. The gateway stops the sessions running the
    /// local copies it overwrites itself; `stop` is ignored.
    ProjectApplyRestore {
        handle: u64,
        conflicts: BTreeMap<String, ConflictChoice>,
        excluded: BTreeSet<String>,
        #[serde(default)]
        stop: Vec<String>,
    },
    ProjectDiscardRestore(u64),
    AppUpdateCheck,
    AppUpdateDownload,
    AppUpdateApply,
    InboxOpen(SessionTarget),
    InboxDismiss(SessionTarget),
    InboxClear,
    /// A sample desktop notification.
    InboxTest,
    /// Cancel a running job.
    CancelJob(RequestKey),
    /// Fetch usage now (through omp's report cache, like omp's own `/usage`).
    UsageRefresh,
    /// Check omp for an update now.
    OmpUpdateCheck,
    /// Rescan the sessions and poll the backends now.
    RefreshSessions,
    /// Configure the relay: its base URL (`wss://`, or `ws://` for a loopback host) and the
    /// room key printed by `omp-deck-relay enroll`. Stored only in the gateway's state dir.
    RelaySet {
        url: String,
        room_key: SecretText,
    },
    /// Forget the relay (stops the link and closes an open pairing window; devices stay).
    RelayClear,
    /// Reconnect to the configured relay now: after another gateway took the room over (the
    /// link then stays down until asked), or to skip a long backoff. Needs a relay and mobile
    /// access on.
    RelayReconnect,
    /// Open the pairing window; answers [`Done::PairingStarted`].
    PairingBegin,
    /// Close the open pairing window (and any device connection on it).
    PairingCancel,
    /// Let the device waiting in `pairing` in, with these permissions.
    PairingApprove {
        pairing: String,
        permissions: Permissions,
    },
    /// Turn the device waiting in `pairing` away and close the window.
    PairingDeny {
        pairing: String,
    },
    /// Delete a paired device: its relay token is withdrawn and its live session closed.
    DeviceRevoke {
        id: String,
    },
    DeviceRename {
        id: String,
        name: String,
    },
    DeviceSetPermissions {
        id: String,
        permissions: Permissions,
    },
    /// Review comments on `root`'s changes as one prompt, inserted at omp's cursor or sent
    /// ([`Command::Review`](crate::controller::Command::Review)); answers like `Submit`.
    Review {
        target: SessionTarget,
        root: PathBuf,
        comments: Vec<ReviewItem>,
        general: Option<String>,
        send: bool,
    },
    /// Answers the session's pending dialog `request` ([`LiveSnapshot::pending_ui`]):
    /// [`Done::Answered`] (`false`: it is gone).
    AnswerUi {
        target: SessionTarget,
        request: u64,
        answer: crate::ui_answer::UiAnswer,
    },
    /// Opens (or focuses) the session file's tab in the desktop app, starting it when no window
    /// is open.
    OpenOnDesktop {
        file: PathBuf,
        split: Split,
    },
    /// Closes the session file's tab in the desktop app (nothing to do without a window).
    CloseOnDesktop {
        file: PathBuf,
    },
    /// Sign in to the active destination on the desktop: the Google sign-in opens in the desktop
    /// browser and finishes there ([`Done::DriveAccount`]). Refused "needs desktop" without a
    /// desktop session, and for a backup server (set up in the desktop app's settings).
    BackupConnectOnDesktop,
    /// A worktree on the new `branch` made from `base_ref` (`None`: the repository's current
    /// branch, else `HEAD`) at `dest` (`None`: the managed default location); its merge target
    /// is recorded. `open`: a session starts in it ([`Done::Session`]), else [`Done::Path`].
    WorktreeNew {
        repo: PathBuf,
        branch: String,
        base_ref: Option<String>,
        dest: Option<PathBuf>,
        open: bool,
    },
    /// omp's `/wt <branch>` in the session: it moves into a new worktree; the branch it is on
    /// now is recorded as the merge target once it arrived. [`Done::Submitted`].
    WorktreeMoveSession {
        target: SessionTarget,
        branch: String,
    },
    /// The finish flow: the worktree's session (opened when none runs) is asked to commit and
    /// merge; after each of its turns the worktree is checked, and once clean and merged it is
    /// retired like [`Mutation::WorktreeRetire`] (`park: false`): [`Done::Retired`]. Progress
    /// texts explain the wait; cancellable until the removal starts.
    WorktreeFinish {
        worktree: Worktree,
    },
    /// Removes a clean, merged worktree (`park: false`) or parks a clean, pushed, unmerged one
    /// (`park: true`, the branch is kept): running sessions in it move into the main checkout
    /// (omp's `/move`, up to 30 s), the rest are stopped, every session file moves there, the
    /// folder goes and the records are kept. [`Done::Retired`].
    WorktreeRetire {
        worktree: Worktree,
        park: bool,
    },
    /// Continues the branch parked from `primary`. `session`: the parked session to open
    /// (`None`: its newest); opened for `Worktree` and `Main` ([`Done::Session`]); `SwitchBack`
    /// answers [`Done::Unit`].
    WorktreeContinue {
        primary: PathBuf,
        branch: String,
        session: Option<String>,
        place: ContinueIn,
    },
    /// `omp plugin enable|disable <name> --scope <scope>`. `cwd`: the project a `Project` scope
    /// belongs to (omp's own working directory otherwise). Once it succeeded, `reload` (the
    /// session the user is looking at) gets `/reload-plugins` typed as soon as it is idle (queued
    /// until then, never typed over a running turn; dropped silently when the tab is gone).
    PluginEnable {
        name: String,
        enabled: bool,
        scope: PluginScope,
        cwd: Option<PathBuf>,
        reload: Option<SessionTarget>,
    },
    /// The enabled features of `pkg` become exactly `features`; [`Done::PluginFeatures`] is the
    /// state omp reports afterwards. `reload` as for [`Mutation::PluginEnable`].
    PluginFeatures {
        pkg: String,
        features: Vec<String>,
        reload: Option<SessionTarget>,
    },
    /// Sets (`Some`: a string, number or boolean) or deletes (`None`: back to the default) one
    /// plugin setting. `reload` as for [`Mutation::PluginEnable`].
    PluginConfigSet {
        pkg: String,
        key: String,
        value: Option<serde_json::Value>,
        reload: Option<SessionTarget>,
    },
    /// Spends one saved reset of the stored account `credential` of `provider`
    /// (`omp usage reset <provider>/<credential>`; not idempotent, never retried). [`Done::Message`]
    /// is omp's confirmation; usage is fetched again afterwards (omp invalidates its own report
    /// cache when the spend succeeds, so no `usage invalidate` is run).
    UsageResetSpend {
        provider: String,
        credential: String,
    },
    /// Pins or unpins session `id` in omp's `session-pins.json`.
    SessionPin {
        id: String,
        pinned: bool,
    },
    /// Renames the session `file`. A session a controller of this gateway runs is renamed by
    /// typing `/rename <title>` into it (the status bar's Rename; [`Done::Submitted`]); any other
    /// session through omp's lease-checked helper ([`Done::Message`]: the title as stored). A
    /// session open in an omp this gateway does not control is refused.
    SessionRename {
        file: PathBuf,
        title: String,
    },
    /// Sets (`Some`) or removes (`None`/blank) the label of entry `entry_id` in session `file`:
    /// through live control when a running session holds it ([`Done::Ctl`]), else through omp's
    /// lease-checked helper ([`Done::Unit`]).
    SessionLabel {
        file: PathBuf,
        entry_id: String,
        label: Option<String>,
    },
    /// Imports the Claude Code / Codex session `id` as a new omp session ([`Done::Path`]: the new
    /// file). `cwd` replaces the foreign project folder when that no longer exists.
    ForeignImport {
        source: ForeignSource,
        id: String,
        cwd: PathBuf,
    },
    /// Saves the advisor document `doc` of `scope` (an empty document deletes the file).
    /// `apply`: a session whose omp should use it now: with live control the save and the
    /// apply go through it, else the file is written headless. [`Done::Message`] is omp's text.
    AdvisorSave {
        cwd: PathBuf,
        scope: AdvisorScope,
        doc: AdvisorDoc,
        apply: Option<SessionTarget>,
    },
    /// Types `/resume <file name>` into the session `target` (omp switches that tab to `file`; its
    /// slash command takes a session id or file name, not a path).
    /// Sent at once when omp is idle. While a turn runs it is refused before it is journaled
    /// ([`Rejected::Busy`](crate::controller::Rejected::Busy)) so the client can ask the user;
    /// `force` repeats it: the turn is stopped first (the status bar's Stop), then the command
    /// waits for omp to be idle (at most 10 s) and goes in. Like [`Mutation::Submit`] it ends
    /// with [`Done::Submitted`] once the command's delivery is final.
    ResumeHere {
        target: SessionTarget,
        file: PathBuf,
        force: bool,
    },
}

/// Where [`Mutation::OpenOnDesktop`] places a newly opened tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Split {
    /// A tab of its own.
    None,
    Right,
    Down,
}

impl Split {
    /// The value of the desktop app's `--split` flag.
    pub fn as_str(self) -> &'static str {
        match self {
            Split::None => "none",
            Split::Right => "right",
            Split::Down => "down",
        }
    }
}

/// One comment of [`Mutation::Review`]: lines `start..=end` of `file`'s diff on `side`
/// (numbered in the file after the change; a range of deleted lines only is quoted from the
/// file before it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewItem {
    pub file: ChangedFile,
    pub side: DiffSide,
    pub start: u32,
    pub end: u32,
    pub text: String,
    /// The exact side of the diff to quote; `None` infers it from the diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_side: Option<crate::review::Side>,
}

impl Mutation {
    /// Short label for job lists and the journal (no payload values).
    pub fn label(&self) -> &'static str {
        match self {
            Mutation::Launch(_) => "launch",
            Mutation::Restart { .. } => "restart",
            Mutation::Stop(_) => "stop",
            Mutation::Kill(_) => "kill",
            Mutation::KillNames { .. } => "kill-names",
            Mutation::Close { .. } => "close",
            Mutation::Submit { .. } => "submit",
            Mutation::Answer { .. } => "answer",
            Mutation::SetSessionPath { .. } => "set-session-path",
            Mutation::ForkAt { .. } => "fork",
            Mutation::Copy { .. } => "copy",
            Mutation::Move { .. } => "move",
            Mutation::Delete { .. } => "delete",
            Mutation::ExportHtml { .. } => "export-html",
            Mutation::Ctl { .. } => "ctl",
            Mutation::Stage { .. } => "git-stage",
            Mutation::Unstage { .. } => "git-unstage",
            Mutation::Discard { .. } => "git-discard",
            Mutation::DeleteUntracked { .. } => "git-delete-untracked",
            Mutation::WorktreeCreate { .. } => "worktree-create",
            Mutation::WorktreeRemove { .. } => "worktree-remove",
            Mutation::RelocateSessions { .. } => "relocate-sessions",
            Mutation::WorktreeAddExisting { .. } => "worktree-add-existing",
            Mutation::SwitchBranch { .. } => "git-switch",
            Mutation::OmpSet { .. } => "omp-set",
            Mutation::OmpReset { .. } => "omp-reset",
            Mutation::AccountPolicies { .. } => "account-policies",
            Mutation::AgentEdit { .. } => "agent-edit",
            Mutation::AgentCreate { .. } => "agent-create",
            Mutation::AgentCustomize { .. } => "agent-customize",
            Mutation::AgentDelete { .. } => "agent-delete",
            Mutation::RunTool(_) => "run-tool",
            Mutation::Settings(_) => "settings",
            Mutation::SetGatewayMode(_) => "gateway-mode",
            Mutation::DriveLoginBegin => "drive-login",
            Mutation::DriveLoginFinish(_) => "drive-login-finish",
            Mutation::DriveLoginCancel(_) => "drive-login-cancel",
            Mutation::DriveSignOut => "drive-sign-out",
            Mutation::Backup { .. } => "backup",
            Mutation::PrepareRestore(_) => "prepare-restore",
            Mutation::ApplyRestore(_) => "restore",
            Mutation::DiscardRestore(_) => "discard-restore",
            Mutation::DeleteBackup(_) => "delete-backup",
            Mutation::DeleteBackups(_) => "delete-backups",
            Mutation::ServerConnect { .. } => "server-connect",
            Mutation::ServerForget => "server-forget",
            Mutation::ProjectBackup { .. } => "project-backup",
            Mutation::ProjectDeleteBackup(_) => "project-delete-backup",
            Mutation::ProjectDeleteBackups(_) => "project-delete-backups",
            Mutation::ProjectPrepareRestore { .. } => "project-prepare-restore",
            Mutation::ProjectApplyRestore { .. } => "project-restore",
            Mutation::ProjectDiscardRestore(_) => "project-discard-restore",
            Mutation::AppUpdateCheck => "app-update-check",
            Mutation::AppUpdateDownload => "app-update-download",
            Mutation::AppUpdateApply => "app-update-apply",
            Mutation::InboxOpen(_) => "inbox-open",
            Mutation::InboxDismiss(_) => "inbox-dismiss",
            Mutation::InboxClear => "inbox-clear",
            Mutation::InboxTest => "inbox-test",
            Mutation::CancelJob(_) => "cancel-job",
            Mutation::UsageRefresh => "usage-refresh",
            Mutation::OmpUpdateCheck => "omp-update-check",
            Mutation::RefreshSessions => "refresh-sessions",
            Mutation::RelaySet { .. } => "relay-set",
            Mutation::RelayClear => "relay-clear",
            Mutation::RelayReconnect => "relay-reconnect",
            Mutation::PairingBegin => "pairing-begin",
            Mutation::PairingCancel => "pairing-cancel",
            Mutation::PairingApprove { .. } => "pairing-approve",
            Mutation::PairingDeny { .. } => "pairing-deny",
            Mutation::DeviceRevoke { .. } => "device-revoke",
            Mutation::DeviceRename { .. } => "device-rename",
            Mutation::DeviceSetPermissions { .. } => "device-permissions",
            Mutation::Review { .. } => "review",
            Mutation::AnswerUi { .. } => "answer-ui",
            Mutation::OpenOnDesktop { .. } => "open-on-desktop",
            Mutation::CloseOnDesktop { .. } => "close-on-desktop",
            Mutation::BackupConnectOnDesktop => "backup-connect-on-desktop",
            Mutation::WorktreeNew { .. } => "worktree-new",
            Mutation::WorktreeMoveSession { .. } => "worktree-move-session",
            Mutation::WorktreeFinish { .. } => "worktree-finish",
            Mutation::WorktreeRetire { .. } => "worktree-retire",
            Mutation::WorktreeContinue { .. } => "worktree-continue",
            Mutation::PluginEnable { .. } => "plugin-enable",
            Mutation::PluginFeatures { .. } => "plugin-features",
            Mutation::PluginConfigSet { .. } => "plugin-config-set",
            Mutation::UsageResetSpend { .. } => "usage-reset-spend",
            Mutation::SessionPin { .. } => "session-pin",
            Mutation::SessionRename { .. } => "session-rename",
            Mutation::SessionLabel { .. } => "session-label",
            Mutation::ForeignImport { .. } => "foreign-import",
            Mutation::AdvisorSave { .. } => "advisor-save",
            Mutation::ResumeHere { .. } => "resume-here",
        }
    }

    /// The session it addresses, if any (journal target; stale checks).
    pub fn target(&self) -> Option<&SessionTarget> {
        match self {
            Mutation::Restart { target: t, .. }
            | Mutation::Stop(t)
            | Mutation::Kill(t)
            | Mutation::Close { target: t, .. }
            | Mutation::Submit { target: t, .. }
            | Mutation::Answer { target: t, .. }
            | Mutation::SetSessionPath { target: t, .. }
            | Mutation::Ctl { target: t, .. }
            | Mutation::InboxOpen(t)
            | Mutation::InboxDismiss(t)
            | Mutation::Review { target: t, .. }
            | Mutation::AnswerUi { target: t, .. }
            | Mutation::WorktreeMoveSession { target: t, .. }
            | Mutation::ResumeHere { target: t, .. } => Some(t),
            _ => None,
        }
    }

    /// Allowed while the maintenance barrier is up (they end or inspect work, start none).
    pub fn allowed_in_maintenance(&self) -> bool {
        matches!(
            self,
            Mutation::Answer { .. }
                | Mutation::Close { .. }
                | Mutation::Stop(_)
                | Mutation::Kill(_)
                | Mutation::KillNames { .. }
                | Mutation::Settings(_)
                | Mutation::InboxOpen(_)
                | Mutation::InboxDismiss(_)
                | Mutation::InboxClear
                | Mutation::CancelJob(_)
                | Mutation::RefreshSessions
                | Mutation::DriveLoginCancel(_)
                | Mutation::DiscardRestore(_)
                | Mutation::ProjectDiscardRestore(_)
                | Mutation::PairingCancel
                | Mutation::PairingDeny { .. }
                | Mutation::DeviceRevoke { .. }
                | Mutation::RelayClear
                | Mutation::AnswerUi { .. }
                | Mutation::CloseOnDesktop { .. }
        )
    }
}

/// What a launch opens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSpec {
    /// Backend session name (tmux/host); `None` = a direct child (Unix) or a fresh host session.
    pub mux_name: Option<String>,
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,
    pub label: String,
    /// `argv[0] == "omp"` runs the detected omp (with the live-control extension).
    pub argv: Vec<String>,
    pub role: SessionRole,
    /// `omp --fork` copy of the session with this id: the gateway finds the new file and
    /// reports [`ServerEvent::CopyResult`].
    pub copy_of: Option<String>,
    /// The view to open the session in. `Chat` runs omp in TSP mode on the session host; a
    /// session that is already alive keeps the mode it was created with. The effective view
    /// is reported by [`Done::Session`] and [`ControllerInfo::view`].
    #[serde(default)]
    pub view: SessionView,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tool {
    Update(UpdateKind),
    Install,
}

/// A mutation was accepted (journaled) — not that it ran.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepted {
    pub key: RequestKey,
    /// The key was accepted before (the earlier record is returned).
    pub duplicate: bool,
    /// `Submit` only: the controller's identity of the request.
    pub submitted: Option<Submitted>,
}

/// Progress of a running job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub stage: String,
    pub done: u64,
    pub total: Option<u64>,
}

/// The record of a mutation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Outcome {
    /// Accepted; still executing.
    Running,
    /// Boxed: the result is large next to the other outcomes (JSON is unchanged).
    Done(Box<Done>),
    Failed(String),
    Rejected(Rejected),
    Cancelled,
    /// Accepted before an owner crash: it may or may not have run. Never replayed.
    Uncertain,
    /// Finished under an earlier gateway instance; only whether it succeeded is kept.
    Recorded {
        succeeded: bool,
    },
}

impl Outcome {
    pub fn is_final(&self) -> bool {
        !matches!(self, Outcome::Running)
    }
}

/// A successful mutation's result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Done {
    Unit,
    /// A session opened (or restarted): its target and the view it runs in (see
    /// [`ControllerInfo::view`]; `Terminal` when `Chat` was asked for but unavailable).
    Session(SessionTarget, SessionView),
    /// The injected request reached its final delivery (see the controller record).
    Submitted(RequestRecord),
    /// `answer` applied (`false`: no such outstanding confirmation).
    Answered(bool),
    Path(PathBuf),
    Moved(MoveOutcome),
    /// Per file: `None` = deleted, else the error text.
    Deleted(Vec<(PathBuf, Option<String>)>),
    Setting(serde_json::Value),
    /// [`Mutation::WorktreeRemove`]: the git result and the ids of the sessions it moved into the
    /// primary repository.
    Removed {
        removed: Removed,
        sessions: Vec<String>,
    },
    /// [`Mutation::RelocateSessions`]: per file, its new path or why it was not moved.
    Relocated(Vec<(PathBuf, Result<PathBuf, String>)>),
    /// [`Mutation::WorktreeRetire`] / [`Mutation::WorktreeFinish`]: the git result, the ids of
    /// the session files moved into the main checkout, how many running sessions moved there
    /// and kept running, and whether it was parked.
    Retired {
        removed: Removed,
        sessions: Vec<String>,
        moved: u32,
        parked: bool,
    },
    Settings(Snapshot),
    Gateway(GatewayStatus),
    DriveLogin {
        handle: u64,
        url: String,
    },
    DriveAccount {
        email: Option<String>,
    },
    Backup(Option<BackupOutcome>),
    Prepared {
        handle: u64,
        plan: RestorePlanView,
    },
    Restored(RestoreReport),
    ServerConnect(ServerConnectOutcome),
    ProjectBackup(Option<ProjectBackupOutcome>),
    ProjectPrepared {
        handle: u64,
        plan: ProjectRestorePlan,
    },
    ProjectRestored(ProjectRestoreReport),
    AppUpdate(AppUpdateView),
    Ctl(deck_core::ctl::CtlReply),
    /// [`Mutation::KillNames`]: the sessions that failed, with why.
    Killed(Vec<(String, String)>),
    /// [`Mutation::PairingBegin`]: the window's id, the invite (`v1.…`) and the link the QR code
    /// shows (`<pwa origin>/#pair=<invite>`). The only place the PSK leaves the gateway, and
    /// only to the requesting client.
    PairingStarted {
        pairing: String,
        invite: SecretText,
        link: SecretText,
        expires_unix_ms: u64,
    },
    /// [`Mutation::AgentEdit`]: the definition as now on disk.
    AgentFile(Box<AgentFile>),
    /// A text omp (or the helper) answered with: [`Mutation::UsageResetSpend`],
    /// [`Mutation::AdvisorSave`], a helper-made [`Mutation::SessionRename`].
    Message(String),
    /// [`Mutation::PluginFeatures`]: the features state omp reports after the change.
    PluginFeatures(PluginFeatures),
}

/// What a config restore would change ([`deck_core::backup::RestorePlan`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestorePlanView {
    pub name: String,
    pub created: String,
    pub host: String,
    pub replaced: Vec<String>,
    pub added: Vec<String>,
    pub dirs: Vec<String>,
    pub removed: Vec<String>,
    pub safety_dir: PathBuf,
}

/// Errors a request returns.
#[derive(Clone, Debug, Serialize, Deserialize, thiserror::Error)]
pub enum GatewayError {
    #[error("gateway protocol {theirs} is not supported (this build speaks {ours})")]
    Unsupported { ours: u32, theirs: u32 },
    #[error("the running gateway serves another profile")]
    ProfileMismatch { served: Profile },
    #[error("OmpDeck is being updated or removed; try again afterwards")]
    Maintenance(MaintenanceMarker),
    #[error("the gateway restarted since this was requested")]
    StaleEpoch { current: ServiceEpoch },
    #[error("not connected to the OmpDeck gateway")]
    Disconnected,
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Rejected(Rejected),
    #[error("the gateway stopped before this finished; it may or may not have run")]
    Uncertain,
    #[error("this request is too old to be looked up")]
    Expired,
    #[error("this request key was already used for different work")]
    KeyReused,
    #[error("{0}")]
    Failed(String),
    #[error("settings: {0}")]
    Settings(SettingsError),
    #[error("the reply is too large")]
    TooLarge,
    #[error("no such job")]
    NoSuchJob,
    /// The job was cancelled (its final outcome is [`Outcome::Cancelled`]).
    #[error("cancelled")]
    Cancelled,
    /// The session runs (in a backend) without a live-control link the request needs, e.g. a
    /// fork of a session the gateway cannot ask to fork itself.
    #[error(
        "Session is running in the background without live control — open it and restart with live control to fork"
    )]
    NoLiveControl,
}

/// One reply (the payload of a `REPLY` message).
pub type Reply = Result<Answer, GatewayError>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DrainState {
    /// Work is still running.
    Draining {
        jobs: usize,
        controllers_busy: usize,
        direct_programs: usize,
    },
    /// Nothing accepted is left; settings are flushed.
    Drained,
}

/// Events on a control link.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerEvent {
    Changed {
        topic: Topic,
        revision: u64,
    },
    Controller {
        target: SessionTarget,
        event: ControllerEvent,
    },
    Outcome {
        key: RequestKey,
        outcome: Outcome,
    },
    Progress {
        key: RequestKey,
        progress: Progress,
    },
    /// A finished turn for a session (the tab's bell marker).
    Bell(SessionTarget),
    /// A desktop notification for `target` was clicked: raise and show it. `token` is the
    /// desktop's activation token (Wayland `xdg-activation`) when the notification server sent
    /// one; the window raise uses it.
    Activate {
        target: SessionTarget,
        token: Option<String>,
    },
    /// omp reported another model than a switch asked for, within the timeout.
    ModelUnconfirmed {
        target: SessionTarget,
        switch: SwitchInfo,
    },
    /// A tab copy ([`LaunchSpec::copy_of`]) found its new session file (now the session's
    /// file), or failed.
    CopyResult {
        target: SessionTarget,
        result: Result<PathBuf, String>,
    },
    /// Events were dropped (the client lagged): refetch every cached topic.
    Resync,
    /// Open (or focus) the tab of the session file `file`; a newly opened one goes beside the
    /// active tab as `split` says. Sent to the focused (else the most recent) GUI.
    OpenSession {
        file: PathBuf,
        split: Split,
    },
    /// Close the tab of the session file `file` (every GUI).
    CloseSession {
        file: PathBuf,
    },
    /// Open `url` in the desktop browser (the focused, else the most recent GUI).
    OpenUrl {
        url: String,
    },
}

/// Why one frontend write was refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputRefused {
    Rejected(Rejected),
    /// This view may not type (observer).
    NotAllowed,
    /// The clipboard reply did not match an outstanding read.
    NoClipboardRead,
}

/// Why a view ended without a process exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewEnd {
    /// The gateway closed the session's controller.
    Closed,
    /// The session restarted / changed generation.
    Stale,
    /// The view lagged beyond its output budget: re-attach for a fresh snapshot.
    Overflow,
    /// The owner lost its backend link (host connection lost) without an exit.
    Disconnected,
    /// The gateway is shutting down.
    Shutdown,
}

/// `cols, rows, cell_width, cell_height` frames.
pub fn encode_geometry(cols: u16, rows: u16, cw: u16, ch: u16) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[..2].copy_from_slice(&cols.to_le_bytes());
    out[2..4].copy_from_slice(&rows.to_le_bytes());
    out[4..6].copy_from_slice(&cw.to_le_bytes());
    out[6..].copy_from_slice(&ch.to_le_bytes());
    out
}

pub fn decode_geometry(b: &[u8]) -> Option<(u16, u16, u16, u16)> {
    let b: &[u8; 8] = b.try_into().ok()?;
    let u = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    Some((u(0), u(2), u(4), u(6)))
}

pub fn encode_input(bytes: &[u8], meta: InputMeta) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + bytes.len());
    out.push(u8::from(meta.ctrl_c));
    out.push(u8::from(meta.win32_modifiers.is_some()));
    out.extend_from_slice(&meta.win32_modifiers.unwrap_or(0).to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

pub fn decode_input(b: &[u8]) -> Option<(InputMeta, &[u8])> {
    if b.len() < 6 {
        return None;
    }
    let mods = u32::from_le_bytes(b[2..6].try_into().ok()?);
    let meta = InputMeta {
        ctrl_c: b[0] != 0,
        win32_modifiers: (b[1] != 0).then_some(mods),
    };
    Some((meta, &b[6..]))
}

pub fn encode_palette(p: &[[u8; 3]; 19]) -> Vec<u8> {
    p.iter().flatten().copied().collect()
}

pub fn decode_palette(b: &[u8]) -> Option<[[u8; 3]; 19]> {
    if b.len() != 57 {
        return None;
    }
    let mut out = [[0u8; 3]; 19];
    for (i, c) in out.iter_mut().enumerate() {
        c.copy_from_slice(&b[i * 3..i * 3 + 3]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_text_is_redacted_in_debug_but_serialized_plainly() {
        let op = Mutation::RelaySet {
            url: "wss://relay.example".into(),
            room_key: SecretText::new("ompdeck-relay-v1:topsecret"),
        };
        let debug = format!("{op:?}");
        assert!(!debug.contains("topsecret"), "{debug}");
        assert!(debug.contains("<redacted>"));
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("\"ompdeck-relay-v1:topsecret\""), "{json}");
        let back: Mutation = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(back, Mutation::RelaySet { room_key, .. } if room_key.expose() == "ompdeck-relay-v1:topsecret")
        );
        let done = Done::PairingStarted {
            pairing: "p".into(),
            invite: SecretText::new("v1.invite"),
            link: SecretText::new("https://h/#pair=v1.invite"),
            expires_unix_ms: 1,
        };
        assert!(!format!("{done:?}").contains("v1.invite"));
        assert_eq!(format!("{}", SecretText::new("x")), "<redacted>");
    }

    #[test]
    fn binary_frontend_payloads_round_trip() {
        let meta = InputMeta {
            ctrl_c: true,
            win32_modifiers: Some(4),
        };
        let enc = encode_input(b"\x1b[99;5u", meta);
        assert_eq!(decode_input(&enc), Some((meta, &b"\x1b[99;5u"[..])));
        let plain = encode_input(b"a", InputMeta::default());
        assert_eq!(
            decode_input(&plain),
            Some((InputMeta::default(), &b"a"[..]))
        );
        assert_eq!(decode_input(b"short"), None);
        assert_eq!(
            decode_geometry(&encode_geometry(120, 40, 9, 18)),
            Some((120, 40, 9, 18))
        );
        let mut pal = [[0u8; 3]; 19];
        pal[18] = [1, 2, 3];
        assert_eq!(decode_palette(&encode_palette(&pal)), Some(pal));
        assert_eq!(decode_palette(&[0; 56]), None);
    }

    #[test]
    fn delete_backup_round_trips_with_its_journal_key() {
        let file = BackupFile {
            id: "1AbC-x_9".into(),
            name: "omp-backup-20261022T135201Z-host.tar.gz".into(),
            size: 12_345,
            created: "2026-10-22T13:52:01.000Z".into(),
            props: [("host".to_owned(), "host".to_owned())].into(),
        };
        let op = Mutation::DeleteBackup(file.clone());
        assert_eq!(op.label(), "delete-backup");
        assert!(op.target().is_none());
        assert!(!op.allowed_in_maintenance());
        let back: Mutation = serde_json::from_slice(&serde_json::to_vec(&op).unwrap()).unwrap();
        match back {
            Mutation::DeleteBackup(f) => assert_eq!(f, file),
            other => panic!("decoded as {}", other.label()),
        }
        // The reply mirrors ProjectDeleteBackup's.
        let done: Done = serde_json::from_slice(&serde_json::to_vec(&Done::Unit).unwrap()).unwrap();
        assert!(matches!(done, Done::Unit));
    }

    #[test]
    fn server_connect_round_trips_and_its_view_carries_no_secret() {
        let config = ServerConfig {
            protocol: ServerProtocol::Sftp,
            host: "nas.local".into(),
            port: 2222,
            auth: ServerAuth::Key {
                user: "me".into(),
                key_path: "/home/me/.ssh/id_ed25519".into(),
                passphrase: Some("hunter2-passphrase".into()),
            },
            folder: "backups".into(),
            pinned: Some("SHA256:abc".into()),
        };
        let op = Mutation::ServerConnect {
            config: config.clone(),
            keep_secret: true,
        };
        assert_eq!(op.label(), "server-connect");
        assert!(!op.allowed_in_maintenance());
        let back: Mutation = serde_json::from_slice(&serde_json::to_vec(&op).unwrap()).unwrap();
        match back {
            Mutation::ServerConnect {
                config: c,
                keep_secret: true,
            } => assert_eq!(c, config),
            other => panic!("decoded as {}", other.label()),
        }

        let account = ServerAccount {
            config,
            last_backup: None,
            synced_fingerprint: None,
        };
        let view = ServerView::of(&account);
        assert_eq!(view.port, 2222);
        assert!(view.pinned);
        let done = Done::ServerConnect(ServerConnectOutcome::Connected(view.clone()));
        let bytes = serde_json::to_vec(&done).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("hunter2-passphrase"));
        match serde_json::from_slice::<Done>(&bytes).unwrap() {
            Done::ServerConnect(ServerConnectOutcome::Connected(v)) => assert_eq!(v, view),
            _ => panic!("decoded as something else"),
        }
        let untrusted = Done::ServerConnect(ServerConnectOutcome::Untrusted {
            fingerprint: "SHA256:xyz".into(),
        });
        let back: Done = serde_json::from_slice(&serde_json::to_vec(&untrusted).unwrap()).unwrap();
        assert!(matches!(
            back,
            Done::ServerConnect(ServerConnectOutcome::Untrusted { fingerprint }) if fingerprint == "SHA256:xyz"
        ));
    }
}
