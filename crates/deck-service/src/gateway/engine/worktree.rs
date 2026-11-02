//! The worktree flows the desktop app and paired devices share: making a worktree at its default
//! location with its merge target recorded, moving a session into a new one, the finish flow
//! (the session is asked to commit and merge, the worktree is checked after each of its turns and
//! removed once clean and merged), removing or parking a worktree (its running sessions move into
//! the main checkout first), continuing a parked branch, the periodic look at parked branches and
//! the once-per-run cleanup of removed worktrees' leftover sessions.
//!
//! Commands typed into a session on a flow's behalf ([`Command::WorktreeFinish`],
//! [`Command::MoveTo`]) run under internal request keys (fresh ones per retry); their draft
//! confirmations and ends reach the client that started the flow, which may answer them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use deck_core::config::{AppConfig, ParkedBranch, RemovedWorktree};
use deck_core::omp::Omp;
use deck_core::sessions::{ProjectGroup, SessionEntry, abbreviate_home};
use deck_core::worktree::{
    self, BranchState, MergeTarget, Removal, Removed, RepoInfo, TabCwd, Worktree, WorktreeStatus,
};

use super::mutate::Post;
use super::sessions::{LaunchJob, Owner, omp_argv};
use super::{Engine, Fin, JobState};
use crate::controller::{
    Command, Delivery, Refusal as CtlRefusal, Rejected, RequestKey, RequestRecord, SessionKey,
    SessionTarget, Submission,
};
use crate::gateway::jobs;
use crate::gateway::protocol::{
    Answer, ContinueIn, DestinationView, Done, GatewayError, LaunchSpec, Mutation, ParkedEntry,
    Progress, Retirement, SessionRole, Topic, TopicValue, WorktreeEntry, WorktreeFlow,
    WorktreeVerdict, WorktreesView,
};
use crate::settings::{Edit, Setting, SettingsError, SettingsService};
use crate::statusfmt::Activity;

/// The client of the requests a flow types into a session.
const INTERNAL: &str = "~worktree";
/// The finish prompt has to go in within this long.
const FINISH_TIMEOUT: Duration = Duration::from_secs(90);
/// How long running sessions get to `/move` before the flow goes on without them.
const RELOCATE_TIMEOUT: Duration = Duration::from_secs(30);
/// Pause before a command that did not go in is typed again.
const RETRY: Duration = Duration::from_millis(1500);
/// A `/wt` move that never lands is forgotten after this long.
const MOVE_EXPIRES: Duration = Duration::from_secs(10 * 60);
/// Parked branches are looked at this often…
const PARK_CHECK: Duration = Duration::from_secs(10 * 60);
/// …and again this soon after a failed look.
const PARK_RETRY: Duration = Duration::from_secs(30);
/// A followed repository is looked at again this often (branches change outside OmpDeck).
const LOOK_EVERY: Duration = Duration::from_secs(30);
/// Steps while a flow runs.
const FLOW_TICK: Duration = Duration::from_millis(400);

const NOT_PARKED: &str = "This session is not parked anymore";

/// Per session file: its new path, or why it was not moved.
type Relocations = Vec<(PathBuf, Result<PathBuf, String>)>;
/// A parked branch: its repository's main checkout and its name.
type BranchKey = (PathBuf, String);

/// A worker's result for the worktree flows.
pub(crate) enum WtFin {
    Looked {
        dir: PathBuf,
        look: Look,
    },
    Verdict {
        conn: u64,
        id: u64,
        path: PathBuf,
        result: Result<WorktreeVerdict, String>,
    },
    Created {
        key: RequestKey,
        open: bool,
        result: Result<PathBuf, String>,
    },
    MoveBase {
        key: RequestKey,
        target: SessionTarget,
        branch: String,
        base: Option<String>,
    },
    FinishReady {
        path: PathBuf,
        result: Result<(String, MergeTarget), String>,
    },
    FinishTurn {
        path: PathBuf,
        probe: TurnProbe,
        scan: TurnScan,
    },
    FinishChecked {
        path: PathBuf,
        result: Result<Vec<String>, String>,
    },
    RetireInspected {
        path: PathBuf,
        result: Result<(String, Retirement), String>,
    },
    LateChecked {
        path: PathBuf,
        refusal: Option<String>,
    },
    Classified {
        path: PathBuf,
        result: Result<Vec<u64>, String>,
    },
    Retired {
        path: PathBuf,
        result: Result<(Removed, Vec<String>), GatewayError>,
    },
    ContinueVetted {
        key: RequestKey,
        result: Result<BranchState, String>,
    },
    ContinueAdded {
        key: RequestKey,
        result: Result<PathBuf, String>,
    },
    Switched {
        key: RequestKey,
        result: Result<(), String>,
    },
    Relocated {
        purpose: Relocate,
        files: Vec<PathBuf>,
        result: Result<Relocations, String>,
    },
    ParkChecked(Vec<(BranchKey, Result<BranchState, String>)>),
    Pruned {
        orphaned: Vec<PathBuf>,
        leftovers: Vec<Leftover>,
    },
    Persisted(Result<(), String>),
}

/// What a headless relocation was for.
pub(crate) enum Relocate {
    /// The closed sessions of a continued parked branch.
    Continue(RequestKey),
    /// Leftover sessions of a removed worktree; `all`: every one of them was in the batch.
    Leftover { worktree: PathBuf, all: bool },
}

/// A followed repository as a worker read it.
pub(crate) struct Look {
    repo: Result<RepoInfo, String>,
    worktree: Option<Worktree>,
    /// Linked worktrees with their merge target and whether it was recorded.
    linked: Vec<(Worktree, Option<String>, bool)>,
}

/// Sessions still carrying a removed worktree's folder.
pub(crate) struct Leftover {
    worktree: PathBuf,
    primary: PathBuf,
    files: Vec<PathBuf>,
    all_movable: bool,
    none: bool,
}

#[derive(Default)]
struct LookState {
    look: Option<Arc<Look>>,
    busy: bool,
    dirty: bool,
    next: Option<Instant>,
}

/// A command a flow typed into a session.
struct Internal {
    owner: String,
    purpose: Purpose,
}

enum Purpose {
    Finish(PathBuf),
    Move { flow: FlowRef, session: SessionKey },
}

#[derive(Clone)]
enum FlowRef {
    Retire(PathBuf),
    Continue(RequestKey),
}

struct Ready {
    base: String,
    target: MergeTarget,
}

/// What a look at the finish session's file found ([`scan_turn`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TurnScan {
    /// Read up to here (complete lines only); `None`: nothing was read and where the turn's
    /// entries begin is still unknown.
    end: Option<u64>,
    /// The turn's user message was read.
    asked: bool,
    /// An assistant message followed it (the model answered).
    answered: bool,
}

/// Reads the session file `file` from `offset` for a turn's evidence: a user message written
/// at or after `not_before_ms` (the prompt's time, or the end of the turn last counted), then
/// an assistant message. `asked`: that user message was read before. Without a known offset
/// (the file was not known when the prompt went in, or was rewritten shorter) it is read from
/// its start. Only complete lines count; a line still being written is read next time.
/// Blocking.
fn scan_turn(file: &Path, offset: Option<u64>, asked: bool, not_before_ms: i64) -> TurnScan {
    use std::io::{BufRead, Seek};
    #[derive(serde::Deserialize)]
    struct Entry {
        #[serde(rename = "type")]
        kind: Option<String>,
        timestamp: Option<String>,
        message: Option<Role>,
    }
    #[derive(serde::Deserialize)]
    struct Role {
        role: Option<String>,
    }
    let none = TurnScan {
        end: offset,
        asked,
        answered: false,
    };
    let Ok(mut f) = std::fs::File::open(file) else {
        return none;
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return none;
    };
    let (start, asked) = match offset {
        Some(o) if o <= len => (o, asked),
        _ => (0, false),
    };
    if f.seek(std::io::SeekFrom::Start(start)).is_err() {
        return none;
    }
    let mut out = TurnScan {
        end: Some(start),
        asked,
        answered: false,
    };
    let mut read = start;
    let mut reader = std::io::BufReader::new(f);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(n) if n > 0 && line.ends_with(b"\n") => {}
            _ => return out,
        }
        read += line.len() as u64;
        out.end = Some(read);
        let Ok(entry) = serde_json::from_slice::<Entry>(&line) else {
            continue;
        };
        if entry.kind.as_deref() != Some("message") {
            continue;
        }
        match entry.message.and_then(|m| m.role).as_deref() {
            Some("user")
                if entry
                    .timestamp
                    .as_deref()
                    .and_then(deck_core::usage::parse_iso_ms)
                    .is_some_and(|t| t >= not_before_ms) =>
            {
                out.asked = true;
            }
            Some("assistant") if out.asked => out.answered = true,
            _ => {}
        }
    }
}

/// Watches the finish session's turns: by looks at its activity, and by its session file — a
/// turn's user message and the reply after it — so a turn that begins and ends between two
/// looks still counts, and an idle look before the turn began never does.
#[derive(Default)]
struct TurnWatch {
    /// The prompt this watch belongs to (the typed command's unique sequence number; 0: none
    /// yet). Reads of another prompt are stale.
    id: u64,
    was_busy: bool,
    /// The session file being followed and how far it was read (`None`: from its start).
    file: Option<PathBuf>,
    offset: Option<u64>,
    /// A user message older than this (unix ms) belongs to an earlier turn.
    not_before_ms: i64,
    asked: bool,
    answered: bool,
    /// Turns counted so far: a read started before the last one ended is stale.
    turns: u64,
    /// The read in flight: its watch and ticket.
    in_flight: Option<(u64, u64)>,
    tickets: u64,
    /// No read starts before this.
    next_read: Option<Instant>,
}

/// A read of the finish session's file to run on a worker ([`TurnWatch::probe`]).
pub(crate) struct TurnProbe {
    watch: u64,
    ticket: u64,
    turn: u64,
    file: PathBuf,
    offset: Option<u64>,
    asked: bool,
    not_before_ms: i64,
}

impl TurnProbe {
    /// Blocking.
    fn run(&self) -> TurnScan {
        scan_turn(&self.file, self.offset, self.asked, self.not_before_ms)
    }
}

impl TurnWatch {
    /// The prompt `id` (unique among every flow's typed commands) is about to be typed at
    /// `now_ms`; `file`'s length now (`len`) marks where the turn's entries begin (`None` with
    /// a known file: it does not exist yet).
    fn submitted(&mut self, id: u64, file: Option<PathBuf>, len: Option<u64>, now_ms: i64) {
        let known = file.is_some();
        *self = TurnWatch {
            id,
            file,
            offset: if known { len.or(Some(0)) } else { None },
            not_before_ms: now_ms,
            // A read of the previous prompt still runs: one at a time.
            in_flight: self.in_flight,
            tickets: self.tickets,
            ..TurnWatch::default()
        };
    }

    /// A read of the session file to start `now`, if one is due (one at a time, one per
    /// [`FLOW_TICK`]). `current`: the session's file now (a file other than the one marked is
    /// read from its start).
    fn probe(&mut self, current: Option<&Path>, now: Instant) -> Option<TurnProbe> {
        let current = current?;
        if self.id == 0 || self.in_flight.is_some() || self.next_read.is_some_and(|t| now < t) {
            return None;
        }
        if self.file.as_deref() != Some(current) {
            self.file = Some(current.to_path_buf());
            self.offset = None;
            self.asked = false;
        }
        self.tickets += 1;
        self.in_flight = Some((self.id, self.tickets));
        self.next_read = Some(now + FLOW_TICK);
        Some(TurnProbe {
            watch: self.id,
            ticket: self.tickets,
            turn: self.turns,
            file: current.to_path_buf(),
            offset: self.offset,
            asked: self.asked,
            not_before_ms: self.not_before_ms,
        })
    }

    /// A read came back. One of another watch, prompt or turn changes nothing (read again).
    fn probed(&mut self, probe: &TurnProbe, scan: TurnScan) {
        if self.in_flight != Some((probe.watch, probe.ticket)) {
            return;
        }
        self.in_flight = None;
        if probe.watch != self.id
            || probe.turn != self.turns
            || self.file.as_ref() != Some(&probe.file)
        {
            return;
        }
        if scan.end.is_some() {
            self.offset = scan.end;
        }
        self.asked = scan.asked;
        self.answered |= scan.answered;
    }

    /// One look at the session at `now_ms`; true when a turn ended (`may_end`: a check may
    /// start now): idle after a busy look, or idle once the file shows the turn's question and
    /// answer. Only a user message written after that counts for the next turn.
    fn look(&mut self, activity: Activity, may_end: bool, now_ms: i64) -> bool {
        match activity {
            Activity::Busy => {
                self.was_busy = true;
                false
            }
            Activity::Idle if may_end && (self.was_busy || self.answered) => {
                self.was_busy = false;
                self.asked = false;
                self.answered = false;
                self.not_before_ms = self.not_before_ms.max(now_ms);
                self.turns += 1;
                true
            }
            _ => false,
        }
    }
}

struct Finish {
    key: RequestKey,
    wt: Worktree,
    label: String,
    cancel: Arc<AtomicBool>,
    /// The merge target and where to merge, once a worker looked.
    ready: Option<Ready>,
    started: Instant,
    /// The session the prompt goes to, once one was found.
    session: Option<SessionKey>,
    /// The closed session to open when none runs inside the worktree.
    resume: Option<SessionEntry>,
    injected: bool,
    retry_at: Option<Instant>,
    turn: TurnWatch,
    checking: bool,
    warned: bool,
    pending: Option<RequestKey>,
}

enum RetireStage {
    Inspecting,
    Moving(Relocation),
    Classifying(Vec<SessionKey>),
    /// Sessions opened after the classifier started (`late`) are being judged; `keys` were
    /// classified, `stop` of them run inside.
    LateCheck {
        keys: Vec<SessionKey>,
        stop: Vec<SessionKey>,
        late: Vec<SessionKey>,
    },
    Removing,
}

struct Retire {
    key: RequestKey,
    wt: Worktree,
    label: String,
    park: bool,
    base: String,
    cancel: Option<Arc<AtomicBool>>,
    stage: RetireStage,
    /// Ids of the sessions running in it, read before anything moved (the parked record).
    ids: Vec<String>,
    moved: Vec<SessionKey>,
}

struct Mover {
    session: SessionKey,
    file: Option<PathBuf>,
    sent: bool,
    retry_at: Option<Instant>,
}

/// Running sessions moving by omp's `/move` before a flow goes on.
struct Relocation {
    dest: PathBuf,
    /// Leaving this worktree (a removal); `None`: arriving in `dest` (a continue).
    leaving: Option<Worktree>,
    started: Instant,
    pending: Vec<Mover>,
    moved: Vec<SessionKey>,
}

impl Relocation {
    fn new(dest: PathBuf, leaving: Option<Worktree>, movers: Vec<Mover>, now: Instant) -> Self {
        Relocation {
            dest,
            leaving,
            started: now,
            pending: movers,
            moved: Vec::new(),
        }
    }

    /// Whether a session is where it is going. Leaving may fall back to the launch folder
    /// (staying put is the safe reading); arriving needs the live folder or the scan.
    fn arrived(&self, live: Option<&Path>, launched: &Path, scanned: Option<&Path>) -> bool {
        match &self.leaving {
            Some(wt) => {
                !inside(wt, live.unwrap_or(launched)) || scanned.is_some_and(|d| !inside(wt, d))
            }
            None => {
                live.is_some_and(|d| d.starts_with(&self.dest))
                    || scanned.is_some_and(|d| d.starts_with(&self.dest))
            }
        }
    }

    fn finished(&self, now: Instant) -> bool {
        self.pending.is_empty() || now.duration_since(self.started) >= RELOCATE_TIMEOUT
    }

    fn pause(&mut self, session: &SessionKey, now: Instant) {
        if let Some(m) = self.pending.iter_mut().find(|m| m.session == *session) {
            m.sent = false;
            m.retry_at = Some(now + RETRY);
        }
    }
}

struct Continue {
    key: RequestKey,
    rec: ParkedBranch,
    place: ContinueIn,
    /// The session to open (id); `None`: the newest of the record.
    session: Option<String>,
    /// The new worktree, once made.
    dest: Option<PathBuf>,
    reloc: Option<Relocation>,
    closed_busy: bool,
    closed: Vec<(PathBuf, Result<PathBuf, String>)>,
    chosen: Option<SessionEntry>,
}

/// A `/wt` move waiting to land in its worktree.
struct PendingMove {
    key: RequestKey,
    session: SessionKey,
    /// The worktree the session was in before (`None`: not a linked worktree).
    from: Option<PathBuf>,
    base: Option<String>,
    since: Instant,
}

#[derive(Default)]
pub(super) struct Worktrees {
    finishing: HashMap<PathBuf, Finish>,
    retiring: HashMap<PathBuf, Retire>,
    continuing: HashMap<RequestKey, Continue>,
    moves: Vec<PendingMove>,
    internal: HashMap<RequestKey, Internal>,
    seq: u64,
    parkable: HashSet<PathBuf>,
    park_states: HashMap<(PathBuf, String), BranchState>,
    park_next: Option<Instant>,
    park_busy: bool,
    pruned: bool,
    in_flight: HashSet<PathBuf>,
    looks: HashMap<PathBuf, LookState>,
    seen_cfg: Option<Arc<AppConfig>>,
    seen_sessions: Option<u64>,
}

impl Worktrees {
    /// The client a flow's typed command belongs to.
    pub(super) fn owner(&self, key: &RequestKey) -> Option<&str> {
        self.internal.get(key).map(|i| i.owner.as_str())
    }

    fn busy(&self) -> bool {
        !self.finishing.is_empty()
            || !self.retiring.is_empty()
            || !self.continuing.is_empty()
            || !self.moves.is_empty()
    }
}

// ---- pure helpers ---------------------------------------------------------------------------

/// What a worktree's flows call it: its branch, else the folder name.
pub(super) fn worktree_label(w: &Worktree) -> String {
    w.branch.clone().unwrap_or_else(|| {
        w.path.file_name().map_or_else(
            || w.path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    })
}

/// Whether `dir` is the worktree folder or lies inside it (lexically).
fn inside(w: &Worktree, dir: &Path) -> bool {
    dir.starts_with(&w.path)
}

/// The label of a live session (`live`: key and label) none of `judged` covers: opened after
/// the last look at where sessions run.
fn unjudged(live: &[(SessionKey, String)], judged: &[&[SessionKey]]) -> Option<String> {
    live.iter()
        .find(|(key, _)| !judged.iter().any(|j| j.contains(key)))
        .map(|(_, label)| label.clone())
}

/// Why removing `wt` (called `name`) must stop for controllers opened after the classifier
/// started (`late`: neither classified nor moved out by this flow): one runs inside it, or
/// where one runs cannot be told. Judged by the classifier's own rules (symlinks resolved,
/// Windows spellings normalised). Blocking (reads the file system): runs on a worker.
fn late_refusal(wt: &Worktree, name: &str, late: &[TabCwd]) -> Option<String> {
    if late.is_empty() {
        return None;
    }
    match worktree::classify(wt, late) {
        Ok(stop) => stop
            .first()
            .and_then(|u| late.iter().find(|t| t.uid == *u))
            .map(|t| {
                format!(
                    "Cannot remove “{name}”: {} was opened in it just now. Close it or move it out, then try again.",
                    t.label
                )
            }),
        Err(e) => Some(format!("Cannot remove “{name}”: {e}")),
    }
}

/// Two session file paths are the same session: omp's `/move` changes a session file's folder,
/// not its name.
fn same_session(a: &Path, b: &Path) -> bool {
    matches!((a.file_name(), b.file_name()), (Some(x), Some(y)) if x == y)
}

/// The branch to record as a new worktree's base: the local branch when its remote-tracking
/// `origin/…` twin was picked, nothing for a detached `HEAD` (merge targets are then inferred).
fn base_to_record(base_ref: &str, branches: &[String]) -> Option<String> {
    if base_ref == "HEAD" {
        return None;
    }
    let local = base_ref
        .strip_prefix("origin/")
        .filter(|rest| branches.iter().any(|b| b == rest));
    Some(local.unwrap_or(base_ref).to_owned())
}

/// `omp worktree add` needs a destination that is absent (below a folder) or empty.
fn check_destination(dest: &Path) -> Result<(), String> {
    match std::fs::metadata(dest) {
        Ok(m) if !m.is_dir() => Err("A file is in the way".to_owned()),
        Ok(_) => match std::fs::read_dir(dest).map(|mut it| it.next().is_none()) {
            Ok(true) => Ok(()),
            Ok(_) => Err("That folder is not empty".to_owned()),
            Err(e) => Err(format!("Cannot read that folder: {e}")),
        },
        Err(e) => match dest.ancestors().skip(1).find(|a| a.exists()) {
            Some(a) if !a.is_dir() => Err(format!("{} is not a folder", a.display())),
            Some(_) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Some(_) => Err(format!("Cannot use that location: {e}")),
            None => Err("That location does not exist".to_owned()),
        },
    }
}

/// What retiring the worktree can do, from its status.
fn retirement(st: &WorktreeStatus, base: &str, label: &str) -> Retirement {
    let merged = worktree::blockers(st, base);
    if merged.is_empty() {
        return Retirement::Remove;
    }
    if st.merged {
        return Retirement::Blocked(format!("Cannot remove “{label}”: {}", merged.join("; ")));
    }
    let park = worktree::park_blockers(st);
    if !park.is_empty() {
        return Retirement::Blocked(format!(
            "Cannot remove “{label}”: {}. Commit and push the work to park it, or merge it first.",
            park.join("; ")
        ));
    }
    Retirement::Park
}

/// The refusal of a retire asking for `park` when the worktree allows `verdict`.
fn retire_refusal(verdict: &Retirement, park: bool, label: &str, base: &str) -> Option<String> {
    match (verdict, park) {
        (Retirement::Remove, false) | (Retirement::Park, true) => None,
        (Retirement::Park, false) => Some(format!(
            "“{label}” is clean and pushed but not merged into {base} yet — park it instead"
        )),
        (Retirement::Remove, true) => Some(format!(
            "“{label}” is merged into {base} — remove it instead"
        )),
        (Retirement::Blocked(why), _) => Some(why.clone()),
    }
}

/// The merge target (`recorded`, else inferred) and status of `wt`. Blocking.
fn inspect(wt: &Worktree, recorded: Option<String>) -> Result<(String, WorktreeStatus), String> {
    let base = recorded
        .or_else(|| worktree::infer_base(wt))
        .ok_or_else(|| "OmpDeck does not know which branch it is meant to merge into".to_owned())?;
    let st = worktree::status(wt, &base).map_err(|e| e.to_string())?;
    Ok((base, st))
}

/// omp's `worktree.base` setting: where it keeps managed worktrees (`None`: unset, blank or
/// unreadable; the default location then applies). Blocking.
fn base_setting(omp: Option<&Omp>) -> Option<String> {
    let catalog = omp?
        .load_settings()
        .map_err(|e| log::debug!("reading omp's settings for worktree.base: {e}"))
        .ok()?;
    catalog
        .tabs
        .iter()
        .flat_map(|t| &t.settings)
        .find(|s| s.key == "worktree.base")
        .and_then(|s| s.value.as_ref())
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// The default location of a worktree of `primary` on `branch`. Blocking.
fn default_dest(omp: Option<&Omp>, primary: &Path, branch: &str) -> PathBuf {
    let leaf = if branch.trim().is_empty() {
        "worktree"
    } else {
        branch.trim()
    };
    let setting = base_setting(omp);
    worktree::default_location(&worktree::managed_base(setting.as_deref()), primary, leaf)
}

/// A path git printed, as a native path.
fn native(printed: &str) -> PathBuf {
    #[cfg(windows)]
    let printed = printed.replace('/', "\\");
    PathBuf::from(printed)
}

/// The linked worktrees of the repository whose main checkout is `primary`. Blocking.
fn linked_worktrees(primary: &Path) -> Result<Vec<Worktree>, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args([
        "-c",
        "core.fsmonitor=false",
        "worktree",
        "list",
        "--porcelain",
    ])
    .current_dir(primary)
    .stdin(std::process::Stdio::null());
    deck_core::platform::no_window(&mut cmd);
    let out = cmd
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        // The first entry is the main checkout.
        .skip(1)
        .filter_map(|p| worktree::resolve(&native(p)))
        .collect())
}

/// Reads the repository holding `dir`. Blocking.
fn look(dir: &Path, bases: &BTreeMap<PathBuf, String>) -> Look {
    let repo = worktree::repo_info(dir).map_err(|e| e.to_string());
    let worktree = worktree::resolve(dir);
    let linked = match &repo {
        Ok(info) => linked_worktrees(&info.primary)
            .map_err(|e| log::debug!("listing the worktrees of {}: {e}", info.primary.display()))
            .unwrap_or_default()
            .into_iter()
            .map(|w| match bases.get(&w.path) {
                Some(b) => {
                    let b = b.clone();
                    (w, Some(b), true)
                }
                None => {
                    let b = worktree::infer_base(&w);
                    (w, b, false)
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    Look {
        repo,
        worktree,
        linked,
    }
}

/// A change to the worktree records in the settings.
#[derive(Clone, Debug)]
enum CfgOp {
    RecordBase(PathBuf, String),
    ForgetBase(PathBuf),
    MarkRemoved(PathBuf, RemovedWorktree),
    Unremove(PathBuf),
    /// Remembers a parked branch (replacing the record of the same branch; sessions merge).
    Park(ParkedBranch),
    Unpark(PathBuf, String),
    /// A removed folder: its remembered tabs, its recent entry and a scope inside it go.
    ForgetFolder(PathBuf),
}

/// The settings `ops` change in `cfg`.
fn changes(cfg: &AppConfig, ops: &[CfgOp]) -> Vec<Setting> {
    let mut bases = cfg.worktree_bases.clone();
    let mut removed = cfg.removed_worktrees.clone();
    let mut parked = cfg.parked_branches.clone();
    let mut recent = cfg.recent_folders.clone();
    let mut scope = cfg.folder_scope.clone();
    let mut tabs: Vec<PathBuf> = Vec::new();
    for op in ops {
        match op {
            CfgOp::RecordBase(p, b) => {
                bases.insert(p.clone(), b.clone());
            }
            CfgOp::ForgetBase(p) => {
                bases.remove(p);
            }
            CfgOp::MarkRemoved(p, r) => {
                removed.insert(p.clone(), r.clone());
            }
            CfgOp::Unremove(p) => {
                removed.remove(p);
            }
            CfgOp::Park(rec) => {
                let mut rec = rec.clone();
                if let Some(i) = parked
                    .iter()
                    .position(|r| r.primary == rec.primary && r.branch == rec.branch)
                {
                    for id in parked.remove(i).sessions {
                        if !rec.sessions.contains(&id) {
                            rec.sessions.push(id);
                        }
                    }
                }
                parked.push(rec);
            }
            CfgOp::Unpark(primary, branch) => {
                parked.retain(|r| !(r.primary == *primary && r.branch == *branch));
            }
            CfgOp::ForgetFolder(p) => {
                recent.retain(|r| r != p);
                if scope.as_deref().is_some_and(|s| s.starts_with(p)) {
                    scope = None;
                }
                if cfg.folder_tabs.contains_key(p) && !tabs.contains(p) {
                    tabs.push(p.clone());
                }
            }
        }
    }
    let mut out = Vec::new();
    if bases != cfg.worktree_bases {
        out.push(Setting::WorktreeBases(bases));
    }
    if removed != cfg.removed_worktrees {
        out.push(Setting::RemovedWorktrees(removed));
    }
    if parked != cfg.parked_branches {
        out.push(Setting::ParkedBranches(parked));
    }
    if recent != cfg.recent_folders {
        out.push(Setting::RecentFolders(recent));
    }
    if scope != cfg.folder_scope {
        out.push(Setting::FolderScope(scope));
    }
    out.extend(
        tabs.into_iter()
            .map(|key| Setting::FolderTabs { key, value: None }),
    );
    out
}

/// Applies `ops` to the committed settings (rebased onto another writer's commit). Blocking.
fn persist(settings: &SettingsService, ops: &[CfgOp]) -> Result<(), String> {
    for _ in 0..5 {
        let snap = settings.snapshot();
        let changes = changes(&snap.config, ops);
        if changes.is_empty() {
            return Ok(());
        }
        match settings
            .submit(Edit {
                base: snap.config,
                changes,
            })
            .wait()
        {
            Ok(_) => return Ok(()),
            Err(SettingsError::Conflict { .. }) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("the settings kept changing".to_owned())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn failed(s: String) -> GatewayError {
    GatewayError::Failed(s)
}

// ---- the engine side ------------------------------------------------------------------------

impl Engine {
    /// Refusals of the worktree mutations decided before anything is journaled.
    pub(super) fn worktree_precheck(&self, op: &Mutation) -> Option<GatewayError> {
        let refused = |s: String| Some(GatewayError::Refused(s));
        let w = &self.worktree;
        match op {
            Mutation::WorktreeNew { branch, dest, .. } => {
                if self.omp.is_none() {
                    refused("omp was not found".to_owned())
                } else if branch.trim().is_empty() {
                    refused("Name the new branch".to_owned())
                } else if dest.as_deref().is_some_and(|d| !d.is_absolute()) {
                    refused("The location must be an absolute path".to_owned())
                } else {
                    None
                }
            }
            Mutation::WorktreeMoveSession { branch, .. } if branch.trim().is_empty() => {
                refused("Name the new branch".to_owned())
            }
            Mutation::WorktreeFinish { worktree } | Mutation::WorktreeRetire { worktree, .. } => {
                let label = worktree_label(worktree);
                if w.retiring.contains_key(&worktree.path) {
                    refused(format!("“{label}” is being removed already"))
                } else if matches!(op, Mutation::WorktreeFinish { .. })
                    && w.finishing.contains_key(&worktree.path)
                {
                    refused(format!("“{label}” is being finished already"))
                } else {
                    None
                }
            }
            Mutation::WorktreeContinue {
                primary,
                branch,
                place,
                ..
            } => {
                if !self
                    .cfg
                    .parked_branches
                    .iter()
                    .any(|r| r.primary == *primary && r.branch == *branch)
                {
                    refused(NOT_PARKED.to_owned())
                } else if *place == ContinueIn::Worktree && self.omp.is_none() {
                    refused("omp was not found".to_owned())
                } else if w
                    .continuing
                    .values()
                    .any(|c| c.rec.primary == *primary && c.rec.branch == *branch)
                {
                    refused(format!("Branch {branch} is being continued already"))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Lists a flow as a running job.
    fn wt_job(&mut self, key: &RequestKey, label: &'static str, cancel: Option<Arc<AtomicBool>>) {
        self.jobs.insert(
            key.clone(),
            JobState {
                label,
                progress: None,
                cancel,
                lane: false,
                post: Post::Refresh,
            },
        );
        self.dirty.gateway = true;
    }

    fn wt_stage(&mut self, key: &RequestKey, stage: String) {
        self.job_progress(
            key.clone(),
            Progress {
                stage,
                done: 0,
                total: None,
            },
        );
        self.wt_views_changed();
    }

    fn wt_worker(&self, name: &str, f: impl FnOnce() -> WtFin + Send + 'static) {
        self.spawn_worker(name, move || Fin::Worktree(Box::new(f())));
    }

    /// Applies `ops` to the settings on a worker.
    fn wt_persist(&self, ops: Vec<CfgOp>) {
        if ops.is_empty() {
            return;
        }
        let settings = self.settings.clone();
        self.wt_worker("worktree-settings", move || {
            WtFin::Persisted(persist(&settings, &ops))
        });
    }

    /// Runs an accepted worktree mutation.
    pub(super) fn execute_worktree(&mut self, conn: u64, id: u64, key: RequestKey, op: Mutation) {
        match op {
            Mutation::WorktreeNew {
                repo,
                branch,
                base_ref,
                dest,
                open,
            } => {
                self.accepted(conn, id, &key);
                self.wt_job(&key, "worktree-new", None);
                let omp = self.omp.clone();
                let settings = self.settings.clone();
                self.wt_worker("worktree-new", move || WtFin::Created {
                    key,
                    open,
                    result: create_new(omp.as_ref(), &settings, &repo, &branch, base_ref, dest),
                });
            }
            Mutation::WorktreeMoveSession { target, branch } => {
                let Some(i) = self.sess.find(&target.key) else {
                    return self.vanished(conn, id, key);
                };
                let cwd = self.sess.managed[i].current_cwd().to_path_buf();
                self.accepted(conn, id, &key);
                self.wt_worker("worktree-move", move || WtFin::MoveBase {
                    key,
                    target,
                    branch: branch.trim().to_owned(),
                    base: worktree::repo_info(&cwd)
                        .ok()
                        .and_then(|i| i.current_branch),
                });
            }
            Mutation::WorktreeFinish { worktree } => {
                self.accepted(conn, id, &key);
                let cancel = Arc::new(AtomicBool::new(false));
                self.wt_job(&key, "worktree-finish", Some(cancel.clone()));
                let label = worktree_label(&worktree);
                let path = worktree.path.clone();
                let recorded = self.cfg.worktree_bases.get(&path).cloned();
                let asked = worktree.clone();
                self.worktree.finishing.insert(
                    path.clone(),
                    Finish {
                        key,
                        wt: worktree,
                        label: label.clone(),
                        cancel,
                        ready: None,
                        started: Instant::now(),
                        session: None,
                        resume: None,
                        injected: false,
                        retry_at: None,
                        turn: TurnWatch::default(),
                        checking: false,
                        warned: false,
                        pending: None,
                    },
                );
                self.wt_views_changed();
                self.wt_worker("worktree-finish", move || {
                    let result = inspect(&asked, recorded)
                        .map_err(|e| format!("Worktree “{label}”: {e}"))
                        .and_then(|(base, _)| {
                            worktree::find_merge_target(&asked.primary, &base)
                                .map(|t| (base, t))
                                .map_err(|e| format!("Cannot finish “{label}”: {e}"))
                        });
                    WtFin::FinishReady { path, result }
                });
            }
            Mutation::WorktreeRetire { worktree, park } => {
                self.accepted(conn, id, &key);
                // A manual removal ends the worktree's finish flow.
                if self.worktree.finishing.contains_key(&worktree.path) {
                    self.end_finish(&worktree.path, Err(GatewayError::Cancelled));
                }
                let cancel = Arc::new(AtomicBool::new(false));
                self.wt_job(&key, "worktree-retire", Some(cancel.clone()));
                let path = worktree.path.clone();
                let label = worktree_label(&worktree);
                let recorded = self.cfg.worktree_bases.get(&path).cloned();
                let asked = worktree.clone();
                self.worktree.retiring.insert(
                    path.clone(),
                    Retire {
                        key,
                        wt: worktree,
                        label: label.clone(),
                        park,
                        base: String::new(),
                        cancel: Some(cancel),
                        stage: RetireStage::Inspecting,
                        ids: Vec::new(),
                        moved: Vec::new(),
                    },
                );
                self.wt_views_changed();
                self.wt_worker("worktree-retire", move || WtFin::RetireInspected {
                    path,
                    result: inspect(&asked, recorded)
                        .map(|(base, st)| {
                            let v = retirement(&st, &base, &label);
                            (base, v)
                        })
                        .map_err(|e| format!("Worktree “{label}”: {e}")),
                });
            }
            Mutation::WorktreeContinue {
                primary,
                branch,
                session,
                place,
            } => {
                let Some(rec) = self
                    .cfg
                    .parked_branches
                    .iter()
                    .find(|r| r.primary == primary && r.branch == branch)
                    .cloned()
                else {
                    // Unparked between the check and now (cannot happen on this thread).
                    self.accepted(conn, id, &key);
                    return self.finish_journal(
                        &key,
                        crate::gateway::protocol::Outcome::Failed(NOT_PARKED.to_owned()),
                    );
                };
                self.accepted(conn, id, &key);
                self.wt_job(&key, "worktree-continue", None);
                self.worktree.continuing.insert(
                    key.clone(),
                    Continue {
                        key: key.clone(),
                        rec: rec.clone(),
                        place,
                        session,
                        dest: None,
                        reloc: None,
                        closed_busy: false,
                        closed: Vec::new(),
                        chosen: None,
                    },
                );
                if place == ContinueIn::SwitchBack {
                    self.wt_switch(key, rec.primary, rec.base);
                } else {
                    self.wt_worker("worktree-continue", move || WtFin::ContinueVetted {
                        key,
                        result: worktree::branch_state(&rec.primary, &rec.branch, &rec.base)
                            .map_err(|e| format!("Cannot look at branch {}: {e}", rec.branch)),
                    });
                }
            }
            _ => {}
        }
    }

    /// Answers [`super::super::protocol::Query::WorktreeVerdict`] from a worker.
    pub(super) fn query_verdict(&self, conn: u64, id: u64, wt: Worktree) {
        let recorded = self.cfg.worktree_bases.get(&wt.path).cloned();
        self.wt_worker("worktree-verdict", move || {
            let label = worktree_label(&wt);
            let result = inspect(&wt, recorded)
                .map(|(base, status)| WorktreeVerdict {
                    verdict: retirement(&status, &base, &label),
                    base,
                    status,
                })
                .map_err(|e| format!("Worktree “{label}”: {e}"));
            WtFin::Verdict {
                conn,
                id,
                path: wt.path,
                result,
            }
        });
    }

    /// The value of [`Topic::Worktrees`] for `dir`.
    pub(super) fn worktrees_value(&self, dir: &Path) -> TopicValue {
        let w = &self.worktree;
        let look = w.looks.get(dir).and_then(|s| s.look.clone());
        let primary = look
            .as_ref()
            .and_then(|l| l.repo.as_ref().ok())
            .map(|i| i.primary.clone());
        let linked = look
            .as_ref()
            .map(|l| {
                l.linked
                    .iter()
                    .map(|(wt, base, recorded)| {
                        let flow = w
                            .finishing
                            .get(&wt.path)
                            .map(|f| WorktreeFlow::Finishing {
                                job: f.key.clone(),
                                stage: self
                                    .jobs
                                    .get(&f.key)
                                    .and_then(|j| j.progress.as_ref())
                                    .map(|p| p.stage.clone()),
                            })
                            .or_else(|| {
                                w.retiring.get(&wt.path).map(|r| WorktreeFlow::Retiring {
                                    job: r.key.clone(),
                                    park: r.park,
                                })
                            });
                        let recorded_now = self.cfg.worktree_bases.get(&wt.path);
                        WorktreeEntry {
                            worktree: wt.clone(),
                            base: recorded_now.cloned().or_else(|| base.clone()),
                            base_recorded: recorded_now.is_some() || *recorded,
                            parkable: w.parkable.contains(&wt.path),
                            flow,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let parked = primary
            .map(|p| {
                self.cfg
                    .parked_branches
                    .iter()
                    .filter(|r| r.primary == p)
                    .map(|r| ParkedEntry {
                        record: r.clone(),
                        state: w
                            .park_states
                            .get(&(r.primary.clone(), r.branch.clone()))
                            .cloned(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        TopicValue::Worktrees(Box::new(WorktreesView {
            dir: dir.to_path_buf(),
            repo: look.as_ref().map(|l| l.repo.clone()),
            worktree: look.as_ref().and_then(|l| l.worktree.clone()),
            linked,
            parked,
        }))
    }

    fn wt_views_changed(&mut self) {
        let dirs: Vec<PathBuf> = self.worktree.looks.keys().cloned().collect();
        for d in dirs {
            self.changed(Topic::Worktrees(d));
        }
    }

    /// Marks every followed repository for another look.
    fn wt_relook(&mut self) {
        for s in self.worktree.looks.values_mut() {
            s.dirty = true;
        }
    }

    // ---- per step ---------------------------------------------------------------------------

    pub(super) fn step_worktrees(&mut self, now: Instant) {
        let cfg_changed = self
            .worktree
            .seen_cfg
            .as_ref()
            .is_none_or(|c| !Arc::ptr_eq(c, &self.cfg));
        let generation = self.sessions_watch.generation();
        let sessions_changed = self.worktree.seen_sessions != Some(generation);
        if cfg_changed || sessions_changed {
            self.worktree.seen_cfg = Some(self.cfg.clone());
            self.worktree.seen_sessions = Some(generation);
            self.wt_relook();
            if cfg_changed {
                self.wt_views_changed();
            }
        }
        self.step_looks(now);
        self.prune_removed_once();
        self.check_parked(now);
        self.worktree
            .moves
            .retain(|m| now.duration_since(m.since) < MOVE_EXPIRES);
        self.track_moves();
        let paths: Vec<PathBuf> = self.worktree.finishing.keys().cloned().collect();
        for p in paths {
            self.drive_finish(&p, now);
        }
        let paths: Vec<PathBuf> = self.worktree.retiring.keys().cloned().collect();
        for p in paths {
            self.drive_retire(&p, now);
        }
        let keys: Vec<RequestKey> = self.worktree.continuing.keys().cloned().collect();
        for k in keys {
            self.drive_continue(&k, now);
        }
        if self.worktree.busy() {
            self.wake_within(FLOW_TICK);
        }
    }

    fn step_looks(&mut self, now: Instant) {
        let wanted: HashSet<PathBuf> = self
            .conns
            .values()
            .flat_map(|c| c.subs.iter())
            .filter_map(|t| match t {
                Topic::Worktrees(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        self.worktree.looks.retain(|d, _| wanted.contains(d));
        let mut start: Vec<PathBuf> = Vec::new();
        for d in wanted {
            let s = self.worktree.looks.entry(d.clone()).or_default();
            let due = s.look.is_none() || s.dirty || s.next.is_some_and(|t| now >= t);
            if due && !s.busy {
                s.busy = true;
                s.dirty = false;
                start.push(d);
            }
        }
        for dir in start {
            let bases = self.cfg.worktree_bases.clone();
            self.wt_worker("worktree-look", move || {
                let look = look(&dir, &bases);
                WtFin::Looked { dir, look }
            });
        }
    }

    pub(super) fn on_worktree_fin(&mut self, fin: WtFin) {
        let now = Instant::now();
        match fin {
            WtFin::Looked { dir, look } => {
                let Some(s) = self.worktree.looks.get_mut(&dir) else {
                    return;
                };
                s.busy = false;
                s.next = Some(now + LOOK_EVERY);
                let same = s.look.as_deref().is_some_and(|old| {
                    old.repo == look.repo
                        && old.worktree == look.worktree
                        && old.linked == look.linked
                });
                if !same {
                    s.look = Some(Arc::new(look));
                    self.changed(Topic::Worktrees(dir));
                }
            }
            WtFin::Verdict {
                conn,
                id,
                path,
                result,
            } => {
                if let Ok(v) = &result {
                    self.set_parkable(&path, v.verdict == Retirement::Park);
                }
                self.reply(
                    conn,
                    id,
                    result.map(Answer::WorktreeVerdict).map_err(failed),
                );
            }
            WtFin::Created { key, open, result } => self.on_created(key, open, result),
            WtFin::MoveBase {
                key,
                target,
                branch,
                base,
            } => self.on_move_base(key, target, branch, base),
            WtFin::FinishReady { path, result } => self.on_finish_ready(&path, result, now),
            WtFin::FinishChecked { path, result } => self.on_finish_checked(&path, result),
            WtFin::FinishTurn { path, probe, scan } => {
                if let Some(f) = self.worktree.finishing.get_mut(&path) {
                    f.turn.probed(&probe, scan);
                }
            }
            WtFin::RetireInspected { path, result } => self.on_retire_inspected(&path, result, now),
            WtFin::Classified { path, result } => self.on_classified(&path, result),
            WtFin::LateChecked { path, refusal } => self.on_late_checked(&path, refusal),
            WtFin::Retired { path, result } => self.on_retired(&path, result),
            WtFin::ContinueVetted { key, result } => self.on_continue_vetted(key, result),
            WtFin::ContinueAdded { key, result } => self.on_continue_added(key, result, now),
            WtFin::Switched { key, result } => self.on_switched(key, result),
            WtFin::Relocated {
                purpose,
                files,
                result,
            } => self.on_relocated(purpose, &files, result),
            WtFin::ParkChecked(results) => self.on_park_checked(results, now),
            WtFin::Pruned {
                orphaned,
                leftovers,
            } => self.on_pruned(orphaned, leftovers),
            WtFin::Persisted(result) => {
                if let Err(e) = result {
                    log::warn!("saving the worktree records failed: {e}");
                }
            }
        }
    }

    fn set_parkable(&mut self, path: &Path, parkable: bool) {
        let changed = if parkable {
            self.worktree.parkable.insert(path.to_path_buf())
        } else {
            self.worktree.parkable.remove(path)
        };
        if changed {
            self.wt_views_changed();
        }
    }

    // ---- typed commands ---------------------------------------------------------------------

    /// Types `command` into session `i` for a flow of `owner`.
    fn wt_submit(
        &mut self,
        i: usize,
        owner: &str,
        purpose: Purpose,
        command: Command,
    ) -> Result<(), Rejected> {
        self.worktree.seq += 1;
        let key = RequestKey {
            client: INTERNAL.to_owned(),
            seq: self.worktree.seq,
        };
        let target = self.sess.managed[i].ctl.target().clone();
        // The finish turn's entries begin past the session file's length now, before omp can
        // write a byte of it.
        let mark = matches!(purpose, Purpose::Finish(_)).then(|| {
            let file = self.sess.managed[i]
                .ctl
                .session_path()
                .map(Path::to_path_buf);
            let len = file
                .as_deref()
                .and_then(|p| std::fs::metadata(p).ok())
                .map(|m| m.len());
            let at = i64::try_from(unix_ms()).unwrap_or(i64::MAX);
            (file, len, at)
        });
        self.sess.managed[i].ctl.submit(Submission {
            key: key.clone(),
            target,
            command,
        })?;
        if let Purpose::Finish(path) = &purpose
            && let Some(f) = self.worktree.finishing.get_mut(path)
        {
            f.pending = Some(key.clone());
            if let Some((file, len, at)) = mark {
                f.turn.submitted(key.seq, file, len, at);
            }
        }
        self.worktree.internal.insert(
            key,
            Internal {
                owner: owner.to_owned(),
                purpose,
            },
        );
        self.dirty.controllers = true;
        self.waker.now();
        Ok(())
    }

    /// A request ended: a flow's typed command goes on or gives up; a `/wt` move that did not
    /// go in is forgotten.
    pub(super) fn worktree_request_ended(&mut self, record: &RequestRecord) {
        let lost = matches!(
            record.delivery,
            Delivery::Refused(CtlRefusal::Precondition(_) | CtlRefusal::DialogOpen)
                | Delivery::Aborted
                | Delivery::Stalled
        );
        let cancelled = matches!(record.delivery, Delivery::Cancelled(_));
        if lost || cancelled {
            self.worktree.moves.retain(|m| m.key != record.key);
        }
        let Some(internal) = self.worktree.internal.remove(&record.key) else {
            return;
        };
        let now = Instant::now();
        match internal.purpose {
            Purpose::Finish(path) => {
                let Some(f) = self.worktree.finishing.get_mut(&path) else {
                    return;
                };
                if f.pending.as_ref() == Some(&record.key) {
                    f.pending = None;
                }
                if cancelled || record.delivery == Delivery::Declined {
                    let label = f.label.clone();
                    self.end_finish(
                        &path,
                        Err(failed(format!(
                            "Stopped finishing “{label}”: the finish prompt was not sent — the worktree stays"
                        ))),
                    );
                } else if lost {
                    // omp was not ready (starting, a dialog open, working): offered again
                    // until the finish times out.
                    f.injected = false;
                    f.retry_at = Some(now + RETRY);
                }
            }
            Purpose::Move { flow, session } => {
                if !(lost || cancelled) {
                    return;
                }
                let reloc = match flow {
                    FlowRef::Retire(p) => match self.worktree.retiring.get_mut(&p) {
                        Some(Retire {
                            stage: RetireStage::Moving(r),
                            ..
                        }) => Some(r),
                        _ => None,
                    },
                    FlowRef::Continue(k) => self
                        .worktree
                        .continuing
                        .get_mut(&k)
                        .and_then(|c| c.reloc.as_mut()),
                };
                if let Some(r) = reloc {
                    r.pause(&session, now);
                }
            }
        }
    }

    /// The requests a flow typed for `client` that wait for a draft confirmation (declined when
    /// the client leaves).
    pub(super) fn worktree_owned(&self, client: &str) -> HashSet<RequestKey> {
        self.worktree
            .internal
            .iter()
            .filter(|(_, i)| i.owner == client)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Client `client` lost the right to run commands: its flows end.
    pub(super) fn cancel_worktree_work(&mut self, client: &str) {
        let finishes: Vec<PathBuf> = self
            .worktree
            .finishing
            .iter()
            .filter(|(_, f)| f.key.client == client)
            .map(|(p, _)| p.clone())
            .collect();
        for p in finishes {
            self.end_finish(&p, Err(GatewayError::Cancelled));
        }
        let retires: Vec<PathBuf> = self
            .worktree
            .retiring
            .iter()
            .filter(|(_, r)| {
                r.key.client == client
                    && matches!(r.stage, RetireStage::Inspecting | RetireStage::Moving(_))
            })
            .map(|(p, _)| p.clone())
            .collect();
        for p in retires {
            self.end_retire(&p, Err(GatewayError::Cancelled));
        }
        let continues: Vec<RequestKey> = self
            .worktree
            .continuing
            .keys()
            .filter(|k| k.client == client)
            .cloned()
            .collect();
        for k in continues {
            self.end_continue(&k, Err(GatewayError::Cancelled));
        }
        self.worktree.moves.retain(|m| m.key.client != client);
    }

    // ---- sessions ---------------------------------------------------------------------------

    /// The running omp session (not exited) whose folder lies inside `wt`.
    fn session_inside(&self, wt: &Worktree) -> Option<usize> {
        self.sess.managed.iter().position(|m| {
            m.role == SessionRole::Session && !m.exited() && inside(wt, m.current_cwd())
        })
    }

    fn scanned(&self) -> Arc<Vec<ProjectGroup>> {
        self.sessions_watch.latest().unwrap_or_default()
    }

    /// The newest scanned session of worktree `wt`.
    fn newest_session_in(&self, wt: &Worktree) -> Option<SessionEntry> {
        self.scanned()
            .iter()
            .filter(|g| g.worktree.as_ref().is_some_and(|w| w.path == wt.path))
            .flat_map(|g| &g.sessions)
            .max_by_key(|e| e.modified)
            .cloned()
    }

    /// Opens the session file `entry` as the result of `key` (an open controller of it is the
    /// result as it is).
    fn wt_open(
        &mut self,
        key: Option<RequestKey>,
        entry: &SessionEntry,
        path: PathBuf,
        cwd: PathBuf,
    ) {
        let mux_name =
            (!self.backends.is_empty()).then(|| deck_core::mux::session_name_for(&entry.id));
        // A controller of this session that already exited would swallow the launch.
        self.drop_exited_for(&path, mux_name.as_deref());
        let spec = LaunchSpec {
            mux_name,
            cwd,
            argv: omp_argv(Some(&path)),
            session_path: Some(path),
            label: entry.title.clone().unwrap_or_default(),
            role: SessionRole::Session,
            copy_of: None,
            view: self.cfg.session_view,
        };
        self.sess.queue(LaunchJob {
            spec,
            key,
            exact: false,
            owner: Owner::Snapshot,
            restore: false,
            tool: None,
            confine: None,
        });
        self.pump_launches();
    }

    // ---- new worktree and /wt ---------------------------------------------------------------

    fn on_created(&mut self, key: RequestKey, open: bool, result: Result<PathBuf, String>) {
        self.wt_relook();
        let dest = match result {
            Ok(d) => d,
            Err(e) => return self.job_failed(key, failed(e)),
        };
        if !open {
            return self.job_finished(key, Ok(Done::Path(dest)));
        }
        self.sess.queue(LaunchJob {
            spec: LaunchSpec {
                mux_name: (!self.backends.is_empty())
                    .then(|| deck_core::mux::new_session_name(SystemTime::now())),
                cwd: dest,
                session_path: None,
                label: String::new(),
                argv: omp_argv(None),
                role: SessionRole::Session,
                copy_of: None,
                view: self.cfg.session_view,
            },
            key: Some(key),
            exact: false,
            owner: Owner::Snapshot,
            restore: false,
            tool: None,
            confine: None,
        });
        self.pump_launches();
    }

    fn on_move_base(
        &mut self,
        key: RequestKey,
        target: SessionTarget,
        branch: String,
        base: Option<String>,
    ) {
        let i = match self.check_target(&target) {
            Ok(i) => i,
            Err(e) => return self.job_failed(key, e),
        };
        let from = self.sess.managed[i].ctl.session_path().and_then(|p| {
            self.scanned().iter().find_map(|g| {
                g.sessions
                    .iter()
                    .any(|e| same_session(&e.path, p))
                    .then(|| g.worktree.as_ref().map(|w| w.path.clone()))
                    .flatten()
            })
        });
        let session = target.key.clone();
        let submitted = self.sess.managed[i].ctl.submit(Submission {
            key: key.clone(),
            target,
            command: Command::WorktreeMove { branch },
        });
        self.dirty.controllers = true;
        match submitted {
            Ok(_) => {
                self.sess.submits.insert(key.clone(), session.clone());
                self.worktree.moves.push(PendingMove {
                    key,
                    session,
                    from,
                    base,
                    since: Instant::now(),
                });
                self.waker.now();
            }
            Err(r) => self.job_failed(key, GatewayError::Rejected(r)),
        }
    }

    /// A session moved by `/wt` landed in a worktree: its base is recorded.
    fn track_moves(&mut self) {
        if self.worktree.moves.is_empty() {
            return;
        }
        let groups = self.scanned();
        let mut landed: Vec<(usize, PathBuf)> = Vec::new();
        for (n, mv) in self.worktree.moves.iter().enumerate() {
            let Some(i) = self.sess.find(&mv.session) else {
                continue;
            };
            let Some(path) = self.sess.managed[i].ctl.session_path() else {
                continue;
            };
            let group = groups.iter().find(|g| {
                g.sessions
                    .iter()
                    .any(|e| e.path == path || same_session(&e.path, path))
            });
            if let Some(w) = group.and_then(|g| g.worktree.as_ref())
                && mv.from.as_ref() != Some(&w.path)
            {
                landed.push((n, w.path.clone()));
            }
        }
        if landed.is_empty() {
            return;
        }
        let mut ops = Vec::new();
        for (n, wt) in landed.into_iter().rev() {
            let mv = self.worktree.moves.remove(n);
            if let Some(base) = mv.base {
                ops.push(CfgOp::RecordBase(wt.clone(), base));
            }
            ops.push(CfgOp::Unremove(wt));
        }
        self.wt_persist(ops);
        self.wt_relook();
    }

    // ---- finish -----------------------------------------------------------------------------

    fn on_finish_ready(
        &mut self,
        path: &Path,
        result: Result<(String, MergeTarget), String>,
        now: Instant,
    ) {
        let Some(f) = self.worktree.finishing.get(path) else {
            return;
        };
        let (base, target) = match result {
            Ok(r) => r,
            Err(e) => return self.end_finish(path, Err(failed(e))),
        };
        let wt = f.wt.clone();
        let label = f.label.clone();
        let running = self.session_inside(&wt);
        let resume = if running.is_some() {
            None
        } else {
            self.newest_session_in(&wt)
        };
        if running.is_none() && resume.is_none() {
            return self.end_finish(
                path,
                Err(failed(
                    "This worktree has no session to ask — open a session in it first".to_owned(),
                )),
            );
        }
        let key = {
            let Some(f) = self.worktree.finishing.get_mut(path) else {
                return;
            };
            f.started = now;
            f.resume = resume;
            f.ready = Some(Ready {
                base: base.clone(),
                target,
            });
            f.key.clone()
        };
        self.wt_stage(
            &key,
            format!(
                "Finishing “{label}”: the session is asked to commit and merge into {base}; the worktree is removed once that is done"
            ),
        );
    }

    fn drive_finish(&mut self, path: &Path, now: Instant) {
        let Some(f) = self.worktree.finishing.get(path) else {
            return;
        };
        if f.cancel.load(Ordering::Relaxed) {
            return self.end_finish(path, Err(GatewayError::Cancelled));
        }
        let Some(ready) = &f.ready else {
            return;
        };
        let label = f.label.clone();
        // The prompt has to go in within the timeout, whatever state the session is stuck in.
        if !f.injected && now.duration_since(f.started) > FINISH_TIMEOUT {
            return self.end_finish(
                path,
                Err(failed(format!(
                    "Stopped finishing “{label}”: its session was not ready for the prompt in time"
                ))),
            );
        }
        let wt = f.wt.clone();
        let known = f
            .session
            .as_ref()
            .and_then(|k| self.sess.find(k))
            .filter(|i| !self.sess.managed[*i].exited());
        let Some(i) = known.or_else(|| self.session_inside(&wt)) else {
            if f.session.is_some() || f.injected {
                return self.end_finish(
                    path,
                    Err(failed(format!(
                        "“{label}” has no running session anymore — stopped waiting"
                    ))),
                );
            }
            // No session runs in the worktree: open its newest one (once), then wait for it.
            let Some(f) = self.worktree.finishing.get_mut(path) else {
                return;
            };
            if let Some(entry) = f.resume.take() {
                let (p, cwd) = (entry.path.clone(), entry.cwd.clone());
                self.wt_open(None, &entry, p, cwd);
            }
            return;
        };
        let prompt = (!f.injected).then(|| {
            worktree::finish_prompt(wt.branch.as_deref(), &ready.base, &wt.path, &ready.target)
        });
        let base = ready.base.clone();
        let m = &self.sess.managed[i];
        let session = m.ctl.target().key.clone();
        let (activity, busy) = (m.ctl.activity(), m.ctl.is_busy());
        let file = m.ctl.session_path().map(Path::to_path_buf);
        let Some(f) = self.worktree.finishing.get_mut(path) else {
            return;
        };
        f.session = Some(session);
        if let Some(prompt) = prompt {
            let may = activity == Activity::Idle
                && !busy
                && f.pending.is_none()
                && f.retry_at.is_none_or(|t| now >= t);
            if !may {
                return;
            }
            let owner = f.key.client.clone();
            f.retry_at = Some(now + RETRY);
            let r = self.wt_submit(
                i,
                &owner,
                Purpose::Finish(path.to_path_buf()),
                Command::WorktreeFinish { prompt },
            );
            if let Some(f) = self.worktree.finishing.get_mut(path) {
                match r {
                    Ok(()) => {
                        f.injected = true;
                        f.retry_at = None;
                    }
                    Err(e) => log::debug!("the finish prompt of “{label}” was not taken: {e}"),
                }
            }
            return;
        }
        // After the prompt: every turn's end checks the worktree, one at a time. While idle
        // without a turn's end, the session file tells whether the turn ran meanwhile.
        let now_ms = i64::try_from(unix_ms()).unwrap_or(i64::MAX);
        if f.turn.look(activity, !f.checking, now_ms) {
            f.checking = true;
            let path = path.to_path_buf();
            self.wt_worker("worktree-check", move || WtFin::FinishChecked {
                path,
                result: worktree::status(&wt, &base)
                    .map(|st| worktree::blockers(&st, &base))
                    .map_err(|e| e.to_string()),
            });
        } else if activity == Activity::Idle
            && !f.checking
            && let Some(probe) = f.turn.probe(file.as_deref(), now)
        {
            let path = path.to_path_buf();
            self.wt_worker("worktree-turn", move || {
                let scan = probe.run();
                WtFin::FinishTurn { path, probe, scan }
            });
        }
    }

    fn on_finish_checked(&mut self, path: &Path, result: Result<Vec<String>, String>) {
        let Some(f) = self.worktree.finishing.get_mut(path) else {
            return;
        };
        f.checking = false;
        let text = match result {
            Ok(b) if b.is_empty() => return self.retire_finished(path),
            Ok(b) => format!(
                "{} is not finished yet: {}. OmpDeck keeps waiting and checks again after the agent's next turn.",
                f.label,
                b.join("; ")
            ),
            Err(e) => format!("Could not check {}: {e}", f.label),
        };
        // Explained once; the wait goes on.
        if !f.warned {
            f.warned = true;
            let key = f.key.clone();
            self.wt_stage(&key, text);
        }
    }

    fn end_finish(&mut self, path: &Path, result: Result<Done, GatewayError>) {
        if let Some(f) = self.worktree.finishing.remove(path) {
            self.job_finished(f.key, result);
            self.wt_views_changed();
        }
    }

    /// The finished worktree is clean and merged: the finish job goes on as its removal.
    fn retire_finished(&mut self, path: &Path) {
        let Some(f) = self.worktree.finishing.remove(path) else {
            return;
        };
        let Some(ready) = f.ready else {
            return;
        };
        if let Some(j) = self.jobs.get_mut(&f.key) {
            j.cancel = None;
        }
        self.worktree.retiring.insert(
            path.to_path_buf(),
            Retire {
                key: f.key,
                wt: f.wt,
                label: f.label,
                park: false,
                base: ready.base,
                cancel: None,
                stage: RetireStage::Inspecting,
                ids: Vec::new(),
                moved: Vec::new(),
            },
        );
        self.start_moving(path, Instant::now());
    }

    // ---- remove / park ----------------------------------------------------------------------

    fn on_retire_inspected(
        &mut self,
        path: &Path,
        result: Result<(String, Retirement), String>,
        now: Instant,
    ) {
        let Some(r) = self.worktree.retiring.get(path) else {
            return;
        };
        let (base, verdict) = match result {
            Ok(v) => v,
            Err(e) => return self.end_retire(path, Err(failed(e))),
        };
        let refusal = retire_refusal(&verdict, r.park, &r.label, &base);
        self.set_parkable(path, verdict == Retirement::Park);
        if let Some(why) = refusal {
            return self.end_retire(path, Err(GatewayError::Refused(why)));
        }
        if let Some(r) = self.worktree.retiring.get_mut(path) {
            r.base = base;
        }
        self.start_moving(path, now);
    }

    /// The running sessions inside the worktree move into its main checkout first.
    fn start_moving(&mut self, path: &Path, now: Instant) {
        let Some(r) = self.worktree.retiring.get(path) else {
            return;
        };
        let wt = r.wt.clone();
        let movers: Vec<Mover> = self
            .sess
            .managed
            .iter()
            .filter(|m| {
                m.role == SessionRole::Session && !m.exited() && inside(&wt, m.current_cwd())
            })
            .map(|m| Mover {
                session: m.ctl.target().key.clone(),
                file: m.ctl.session_path().map(Path::to_path_buf),
                sent: false,
                retry_at: None,
            })
            .collect();
        let groups = self.scanned();
        let ids: Vec<String> = movers
            .iter()
            .filter_map(|m| m.file.as_deref())
            .filter_map(|f| {
                groups
                    .iter()
                    .flat_map(|g| &g.sessions)
                    .find(|e| same_session(&e.path, f))
                    .map(|e| e.id.clone())
            })
            .collect();
        let empty = movers.is_empty();
        let (key, label) = (r.key.clone(), r.label.clone());
        if let Some(r) = self.worktree.retiring.get_mut(path) {
            r.ids = ids;
            r.stage = RetireStage::Moving(Relocation::new(
                wt.primary.clone(),
                Some(wt.clone()),
                movers,
                now,
            ));
        }
        if empty {
            self.classify_retire(path);
        } else {
            self.wt_stage(
                &key,
                format!(
                    "Moving the open sessions of “{label}” into {} once omp is idle (a session still busy after 30 s is stopped)…",
                    abbreviate_home(&wt.primary)
                ),
            );
        }
    }

    fn drive_retire(&mut self, path: &Path, now: Instant) {
        let Some(r) = self.worktree.retiring.get_mut(path) else {
            return;
        };
        if r.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
            && matches!(r.stage, RetireStage::Inspecting | RetireStage::Moving(_))
        {
            return self.end_retire(path, Err(GatewayError::Cancelled));
        }
        let RetireStage::Moving(reloc) = &mut r.stage else {
            return;
        };
        let mut reloc = std::mem::replace(
            reloc,
            Relocation::new(PathBuf::new(), None, Vec::new(), now),
        );
        let owner = r.key.client.clone();
        self.drive_relocation(
            &mut reloc,
            &owner,
            &FlowRef::Retire(path.to_path_buf()),
            now,
        );
        let finished = reloc.finished(now);
        let Some(r) = self.worktree.retiring.get_mut(path) else {
            return;
        };
        r.moved.append(&mut reloc.moved);
        r.stage = RetireStage::Moving(reloc);
        if finished {
            self.classify_retire(path);
        }
    }

    /// Advances running sessions moving by `/move`: types it into idle ones, notices the ones
    /// that arrived or went away.
    fn drive_relocation(
        &mut self,
        reloc: &mut Relocation,
        owner: &str,
        flow: &FlowRef,
        now: Instant,
    ) {
        let groups = self.scanned();
        let mut send: Vec<(usize, SessionKey)> = Vec::new();
        let mut left: Vec<(usize, Option<PathBuf>)> = Vec::new();
        let mut gone: Vec<SessionKey> = Vec::new();
        for mv in &reloc.pending {
            let Some(i) = self
                .sess
                .find(&mv.session)
                .filter(|i| !self.sess.managed[*i].exited())
            else {
                gone.push(mv.session.clone());
                continue;
            };
            let m = &self.sess.managed[i];
            let file = m.ctl.session_path();
            let changed = matches!((mv.file.as_deref(), file), (Some(a), Some(b)) if a != b);
            let scanned = file.and_then(|p| {
                groups
                    .iter()
                    .flat_map(|g| &g.sessions)
                    .find(|e| same_session(&e.path, p))
            });
            if changed || reloc.arrived(m.live_cwd(), &m.cwd, scanned.map(|e| e.cwd.as_path())) {
                left.push((i, scanned.map(|e| e.path.clone())));
            } else if m.ctl.activity() == Activity::Idle
                && !m.ctl.is_busy()
                && !mv.sent
                && mv.retry_at.is_none_or(|t| now >= t)
            {
                send.push((i, mv.session.clone()));
            }
        }
        reloc.pending.retain(|m| !gone.contains(&m.session));
        for (i, scanned) in left {
            let key = self.sess.managed[i].ctl.target().key.clone();
            reloc.pending.retain(|m| m.session != key);
            reloc.moved.push(key);
            // It runs in the destination from here on (a re-launch starts there).
            if self.sess.managed[i].cwd != reloc.dest {
                self.sess.managed[i].cwd = reloc.dest.clone();
                self.sess.touch_manifest();
            }
            // A direct program does not report its file: point it at the one the scan lists.
            if let Some(new) = scanned
                && self.sess.managed[i].is_direct()
                && self.sess.managed[i].ctl.session_path() != Some(new.as_path())
            {
                self.set_session_path(i, Some(new));
            }
        }
        for (i, session) in send {
            let r = self.wt_submit(
                i,
                owner,
                Purpose::Move {
                    flow: flow.clone(),
                    session: session.clone(),
                },
                Command::MoveTo {
                    dest: reloc.dest.clone(),
                },
            );
            if let Some(m) = reloc.pending.iter_mut().find(|m| m.session == session) {
                match r {
                    Ok(()) => {
                        m.sent = true;
                        m.retry_at = None;
                    }
                    Err(_) => {
                        m.sent = false;
                        m.retry_at = Some(now + RETRY);
                    }
                }
            }
        }
    }

    /// Decides (off this thread) which remaining sessions still run inside the worktree.
    fn classify_retire(&mut self, path: &Path) {
        let Some(r) = self.worktree.retiring.get_mut(path) else {
            return;
        };
        r.cancel = None;
        if let Some(j) = self.jobs.get_mut(&r.key) {
            j.cancel = None;
        }
        let wt = r.wt.clone();
        let moved = r.moved.clone();
        let mut keys = Vec::new();
        let mut tabs = Vec::new();
        for m in self
            .sess
            .managed
            .iter()
            .filter(|m| m.role == SessionRole::Session && !m.exited())
        {
            let key = m.ctl.target().key.clone();
            if moved.contains(&key) {
                continue;
            }
            tabs.push(TabCwd {
                uid: keys.len() as u64,
                label: m.display_label(),
                stored: m.cwd.clone(),
                live: m.live_cwd().map(Path::to_path_buf),
            });
            keys.push(key);
        }
        if let Some(r) = self.worktree.retiring.get_mut(path) {
            r.stage = RetireStage::Classifying(keys);
        }
        let path = path.to_path_buf();
        self.wt_worker("worktree-classify", move || WtFin::Classified {
            path,
            result: worktree::classify(&wt, &tabs),
        });
    }

    fn on_classified(&mut self, path: &Path, result: Result<Vec<u64>, String>) {
        let Some(r) = self.worktree.retiring.get_mut(path) else {
            return;
        };
        let stop = match result {
            Ok(s) => s,
            Err(e) => {
                let label = r.label.clone();
                return self.end_retire(path, Err(failed(format!("Cannot remove “{label}”: {e}"))));
            }
        };
        let RetireStage::Classifying(keys) = std::mem::replace(&mut r.stage, RetireStage::Removing)
        else {
            return;
        };
        let stop: Vec<SessionKey> = stop
            .into_iter()
            .filter_map(|u| usize::try_from(u).ok().and_then(|u| keys.get(u)).cloned())
            .collect();
        // A session launched after the classifier started is in nobody's verdict: removal
        // would delete its working directory. It is judged the same way, off this thread. The
        // sessions this flow moved out were left out of the classification on purpose.
        let moved = r.moved.clone();
        let (late, tabs): (Vec<SessionKey>, Vec<TabCwd>) = self
            .sess
            .managed
            .iter()
            .filter(|m| m.role == SessionRole::Session && !m.exited())
            .filter(|m| {
                let key = &m.ctl.target().key;
                !keys.contains(key) && !moved.contains(key)
            })
            .enumerate()
            .map(|(uid, m)| {
                let tab = TabCwd {
                    uid: uid as u64,
                    label: m.display_label(),
                    stored: m.cwd.clone(),
                    live: m.live_cwd().map(Path::to_path_buf),
                };
                (m.ctl.target().key.clone(), tab)
            })
            .unzip();
        if late.is_empty() {
            return self.remove_worktree(path, stop);
        }
        let Some(r) = self.worktree.retiring.get_mut(path) else {
            return;
        };
        r.stage = RetireStage::LateCheck { keys, stop, late };
        let (wt, label) = (r.wt.clone(), r.label.clone());
        let path = path.to_path_buf();
        self.wt_worker("worktree-late-check", move || WtFin::LateChecked {
            refusal: late_refusal(&wt, &label, &tabs),
            path,
        });
    }

    /// The late sessions were judged: refused, or the removal goes on — unless yet another
    /// session started meanwhile (refused here, nothing judged it).
    fn on_late_checked(&mut self, path: &Path, refusal: Option<String>) {
        let Some(r) = self.worktree.retiring.get_mut(path) else {
            return;
        };
        let RetireStage::LateCheck { keys, stop, late } =
            std::mem::replace(&mut r.stage, RetireStage::Removing)
        else {
            return;
        };
        if let Some(why) = refusal {
            return self.end_retire(path, Err(GatewayError::Refused(why)));
        }
        let live: Vec<(SessionKey, String)> = self
            .sess
            .managed
            .iter()
            .filter(|m| m.role == SessionRole::Session && !m.exited())
            .map(|m| (m.ctl.target().key.clone(), m.display_label()))
            .collect();
        if let Some(label) = unjudged(&live, &[&keys, &late, &r.moved]) {
            let name = r.label.clone();
            return self.end_retire(
                path,
                Err(GatewayError::Refused(format!(
                    "Cannot remove “{name}”: {label} was opened just now. Try again."
                ))),
            );
        }
        self.remove_worktree(path, stop);
    }

    /// Stops the sessions in `stop` and removes the worktree (on a worker).
    fn remove_worktree(&mut self, path: &Path, stop: Vec<SessionKey>) {
        let Some(r) = self.worktree.retiring.get(path) else {
            return;
        };
        let (wt, base, park, label) = (r.wt.clone(), r.base.clone(), r.park, r.label.clone());
        let ids = r.ids.clone();
        // Backend sessions are stopped by the job; direct programs end here.
        let mut names = Vec::new();
        for k in &stop {
            if let Some(i) = self.sess.find(k) {
                let m = &mut self.sess.managed[i];
                match &m.mux_name {
                    Some(n) if m.mux.is_some() => names.push(n.clone()),
                    _ => m.ctl.terminal_mut().kill(),
                }
            }
        }
        // Every other open session keeps running (moved out, or outside the worktree).
        let keep: Vec<String> = self
            .sess
            .managed
            .iter()
            .filter_map(|m| m.mux_name.clone())
            .filter(|n| !names.contains(n))
            .collect();
        let ctx = self.job_ctx();
        let settings = self.settings.clone();
        let path = path.to_path_buf();
        self.wt_worker("worktree-remove", move || {
            let mut stop = names;
            stop.extend(jobs::sessions_inside(
                &ctx.backends,
                &ctx.root.path,
                &wt.path,
                &[keep, stop.clone()].concat(),
            ));
            let how = if park {
                Removal::Park
            } else {
                Removal::Merged {
                    delete_branch: true,
                }
            };
            let result = match jobs::worktree_remove(&ctx, &wt, &base, how, &stop) {
                Ok(Done::Removed { removed, sessions }) => {
                    let mut ops = vec![
                        CfgOp::MarkRemoved(
                            wt.path.clone(),
                            RemovedWorktree {
                                primary: wt.primary.clone(),
                                branch: wt.branch.clone(),
                            },
                        ),
                        CfgOp::ForgetBase(wt.path.clone()),
                        CfgOp::ForgetFolder(wt.path.clone()),
                    ];
                    if park && let Some(branch) = wt.branch.clone() {
                        let mut all = sessions.clone();
                        for id in ids {
                            if !all.contains(&id) {
                                all.push(id);
                            }
                        }
                        ops.push(CfgOp::Park(ParkedBranch {
                            primary: wt.primary.clone(),
                            branch,
                            base: base.clone(),
                            sessions: all,
                            parked_ms: unix_ms(),
                        }));
                    }
                    if let Err(e) = persist(&settings, &ops) {
                        log::warn!("saving the records of the removed worktree failed: {e}");
                    }
                    Ok((removed, sessions))
                }
                Ok(_) => Err(failed(
                    "the removal answered with something else".to_owned(),
                )),
                Err(e) => Err(failed(format!("Could not remove “{label}”: {e}"))),
            };
            WtFin::Retired { path, result }
        });
    }

    fn on_retired(&mut self, path: &Path, result: Result<(Removed, Vec<String>), GatewayError>) {
        let Some(r) = self.worktree.retiring.get(path) else {
            return;
        };
        let (moved, park) = (u32::try_from(r.moved.len()).unwrap_or(u32::MAX), r.park);
        if result.is_ok() {
            self.worktree.parkable.remove(path);
            if park {
                // The new record is looked at soon.
                self.worktree.park_next = Some(Instant::now());
            }
        }
        self.wt_relook();
        self.end_retire(
            path,
            result.map(|(removed, sessions)| Done::Retired {
                removed,
                sessions,
                moved,
                parked: park,
            }),
        );
    }

    fn end_retire(&mut self, path: &Path, result: Result<Done, GatewayError>) {
        if let Some(r) = self.worktree.retiring.remove(path) {
            self.job_finished(r.key, result);
            self.wt_views_changed();
        }
    }

    // ---- parked branches --------------------------------------------------------------------

    /// At startup and every ten minutes: a parked record goes when its branch no longer exists,
    /// or is merged and checked out nowhere. Branches are never deleted here.
    fn check_parked(&mut self, now: Instant) {
        if self.cfg.parked_branches.is_empty() {
            self.worktree.park_next = None;
            if !self.worktree.park_states.is_empty() {
                self.worktree.park_states.clear();
                self.wt_views_changed();
            }
            return;
        }
        if self.worktree.park_busy
            || self.worktree.park_next.is_some_and(|t| now < t)
            || self.sessions_watch.latest().is_none()
        {
            return;
        }
        self.worktree.park_busy = true;
        self.worktree.park_next = Some(now + PARK_CHECK);
        let records = self.cfg.parked_branches.clone();
        self.wt_worker("worktree-parked", move || {
            WtFin::ParkChecked(
                records
                    .into_iter()
                    .map(|r| {
                        let state = worktree::branch_state(&r.primary, &r.branch, &r.base)
                            .map_err(|e| e.to_string());
                        ((r.primary, r.branch), state)
                    })
                    .collect(),
            )
        });
    }

    fn on_park_checked(
        &mut self,
        results: Vec<(BranchKey, Result<BranchState, String>)>,
        now: Instant,
    ) {
        self.worktree.park_busy = false;
        let mut ops = Vec::new();
        for ((primary, branch), state) in results {
            match state {
                Err(e) => {
                    log::debug!("parked branch {branch}: {e}");
                    self.worktree.park_next = Some(now + PARK_RETRY);
                }
                Ok(st) if !st.exists || (st.merged && st.checked_out.is_none()) => {
                    self.worktree
                        .park_states
                        .remove(&(primary.clone(), branch.clone()));
                    ops.push(CfgOp::Unpark(primary, branch));
                }
                Ok(st) => {
                    self.worktree.park_states.insert((primary, branch), st);
                }
            }
        }
        self.wt_persist(ops);
        self.wt_views_changed();
    }

    fn drop_parked(&mut self, rec: &ParkedBranch) {
        self.worktree
            .park_states
            .remove(&(rec.primary.clone(), rec.branch.clone()));
        self.wt_persist(vec![CfgOp::Unpark(rec.primary.clone(), rec.branch.clone())]);
        self.wt_views_changed();
    }

    fn set_park_state(&mut self, rec: &ParkedBranch, checked_out: Option<PathBuf>) {
        let key = (rec.primary.clone(), rec.branch.clone());
        match self.worktree.park_states.get_mut(&key) {
            Some(s) => s.checked_out = checked_out,
            None => {
                self.worktree.park_states.insert(
                    key,
                    BranchState {
                        exists: true,
                        checked_out,
                        merged: false,
                    },
                );
            }
        }
        self.wt_views_changed();
    }

    // ---- continue ---------------------------------------------------------------------------

    fn on_continue_vetted(&mut self, key: RequestKey, result: Result<BranchState, String>) {
        let Some(c) = self.worktree.continuing.get(&key) else {
            return;
        };
        let (rec, place) = (c.rec.clone(), c.place);
        let st = match result {
            Ok(st) => st,
            Err(e) => return self.end_continue(&key, Err(failed(e))),
        };
        if !st.exists {
            self.drop_parked(&rec);
            return self.end_continue(
                &key,
                Err(failed(format!(
                    "Branch {} no longer exists — it is not parked anymore",
                    rec.branch
                ))),
            );
        }
        self.worktree
            .park_states
            .insert((rec.primary.clone(), rec.branch.clone()), st.clone());
        self.wt_views_changed();
        let checked_out_text = |at: &Path| {
            if at == rec.primary {
                "the main checkout".to_owned()
            } else {
                abbreviate_home(at)
            }
        };
        match (place, st.checked_out) {
            (ContinueIn::Main, Some(at)) if at == rec.primary => self.continue_open(&key),
            (_, Some(at)) => self.end_continue(
                &key,
                Err(failed(format!(
                    "Branch {} is checked out in {}",
                    rec.branch,
                    checked_out_text(&at)
                ))),
            ),
            (ContinueIn::Main, None) => self.wt_switch(key, rec.primary, rec.branch),
            (ContinueIn::Worktree, None) => {
                let omp = self.omp.clone();
                let settings = self.settings.clone();
                self.wt_stage(&key, format!("Creating a worktree on “{}”…", rec.branch));
                self.wt_worker("worktree-continue", move || WtFin::ContinueAdded {
                    key,
                    result: add_parked(omp.as_ref(), &settings, &rec),
                });
            }
            (ContinueIn::SwitchBack, None) => {}
        }
    }

    fn wt_switch(&self, key: RequestKey, dir: PathBuf, branch: String) {
        self.wt_worker("worktree-switch", move || WtFin::Switched {
            key,
            result: worktree::switch_branch(&dir, &branch)
                .map_err(|e| format!("Cannot switch the main checkout to {branch}: {e}")),
        });
    }

    fn on_switched(&mut self, key: RequestKey, result: Result<(), String>) {
        let Some(c) = self.worktree.continuing.get(&key) else {
            return;
        };
        let (rec, place) = (c.rec.clone(), c.place);
        if let Err(e) = result {
            return self.end_continue(&key, Err(failed(e)));
        }
        let main = place == ContinueIn::Main;
        self.set_park_state(&rec, main.then(|| rec.primary.clone()));
        if main {
            self.continue_open(&key);
        } else {
            self.end_continue(&key, Ok(Done::Unit));
        }
    }

    /// The parked sessions in the latest scan, and the one to open.
    fn parked_members(
        &self,
        rec: &ParkedBranch,
        wanted: Option<&str>,
    ) -> (Vec<SessionEntry>, Option<SessionEntry>) {
        let members: Vec<SessionEntry> = self
            .scanned()
            .iter()
            .flat_map(|g| &g.sessions)
            .filter(|e| rec.sessions.contains(&e.id))
            .cloned()
            .collect();
        let chosen = match wanted {
            Some(id) => members.iter().find(|e| e.id == id).cloned(),
            None => members.iter().max_by_key(|e| e.modified).cloned(),
        };
        (members, chosen)
    }

    /// Continuing in the main checkout: the chosen session opens as it is.
    fn continue_open(&mut self, key: &RequestKey) {
        let Some(c) = self.worktree.continuing.remove(key) else {
            return;
        };
        let (_, chosen) = self.parked_members(&c.rec, c.session.as_deref());
        self.wt_views_changed();
        match chosen {
            Some(e) => {
                let (p, cwd) = (e.path.clone(), e.cwd.clone());
                self.wt_open(Some(c.key), &e, p, cwd);
            }
            None => self.job_finished(c.key, Ok(Done::Unit)),
        }
    }

    /// The worktree exists: the branch is no longer parked, its open sessions move in (`/move`),
    /// its closed ones are relocated headlessly, then the chosen session opens.
    fn on_continue_added(
        &mut self,
        key: RequestKey,
        result: Result<PathBuf, String>,
        now: Instant,
    ) {
        let Some(c) = self.worktree.continuing.get(&key) else {
            return;
        };
        let rec = c.rec.clone();
        let wanted = c.session.clone();
        let dest = match result {
            Ok(d) => d,
            Err(e) => return self.end_continue(&key, Err(failed(e))),
        };
        self.worktree
            .park_states
            .remove(&(rec.primary.clone(), rec.branch.clone()));
        self.wt_relook();
        let (members, chosen) = self.parked_members(&rec, wanted.as_deref());
        let held = |m: &super::sessions::Managed, path: &Path| {
            m.ctl.session_path().is_some_and(|p| same_session(p, path))
        };
        let movers: Vec<Mover> = self
            .sess
            .managed
            .iter()
            .filter(|m| {
                m.role == SessionRole::Session
                    && !m.exited()
                    && members.iter().any(|e| held(m, &e.path))
            })
            .map(|m| Mover {
                session: m.ctl.target().key.clone(),
                file: m.ctl.session_path().map(Path::to_path_buf),
                sent: false,
                retry_at: None,
            })
            .collect();
        let snap = self.running.latest();
        let files: Vec<PathBuf> = members
            .iter()
            .filter(|e| {
                !snap.as_ref().is_some_and(|s| s.is_running(&e.id, &e.path))
                    && !self.sess.managed.iter().any(|m| held(m, &e.path))
            })
            .map(|e| e.path.clone())
            .filter(|f| self.worktree.in_flight.insert(f.clone()))
            .collect();
        let moving = !movers.is_empty();
        let Some(c) = self.worktree.continuing.get_mut(&key) else {
            return;
        };
        c.dest = Some(dest.clone());
        c.chosen = chosen;
        if moving {
            c.reloc = Some(Relocation::new(dest.clone(), None, movers, now));
        }
        if !files.is_empty() {
            c.closed_busy = true;
            let ctx = self.job_ctx();
            let (to, batch) = (dest.clone(), files.clone());
            let purpose = Relocate::Continue(key.clone());
            self.wt_worker("worktree-relocate", move || WtFin::Relocated {
                purpose,
                result: match jobs::relocate_sessions(&ctx, &batch, &to) {
                    Ok(Done::Relocated(r)) => Ok(r),
                    Ok(_) => Ok(Vec::new()),
                    Err(e) => Err(e.to_string()),
                },
                files: batch,
            });
        }
        if moving {
            self.wt_stage(
                &key,
                format!(
                    "Moving the open sessions of “{}” into {} once omp is idle (a session still busy after 30 s stays where it is)…",
                    rec.branch,
                    abbreviate_home(&dest)
                ),
            );
        }
    }

    fn drive_continue(&mut self, key: &RequestKey, now: Instant) {
        let Some(c) = self.worktree.continuing.get_mut(key) else {
            return;
        };
        if c.dest.is_none() {
            return;
        }
        let mut stayed = 0;
        if let Some(mut reloc) = c.reloc.take() {
            let owner = c.key.client.clone();
            self.drive_relocation(&mut reloc, &owner, &FlowRef::Continue(key.clone()), now);
            let finished = reloc.finished(now);
            stayed = reloc.pending.len();
            let Some(c) = self.worktree.continuing.get_mut(key) else {
                return;
            };
            if !finished {
                c.reloc = Some(reloc);
                return;
            }
        }
        let Some(c) = self.worktree.continuing.get(key) else {
            return;
        };
        if c.closed_busy {
            return;
        }
        let branch = c.rec.branch.clone();
        if stayed > 0 {
            self.wt_stage(
                key,
                format!(
                    "{stayed} session{} of “{branch}” did not move: still busy or not accepting /move",
                    if stayed == 1 { "" } else { "s" }
                ),
            );
        }
        let Some(c) = self.worktree.continuing.remove(key) else {
            return;
        };
        self.wt_views_changed();
        let dest = c.dest.clone().unwrap_or_default();
        let Some(chosen) = c.chosen else {
            return self.job_finished(c.key, Ok(Done::Path(dest)));
        };
        // Relocated headlessly: it opens at its new file in the new worktree.
        let moved = c
            .closed
            .iter()
            .find(|(old, _)| *old == chosen.path)
            .and_then(|(_, r)| r.as_ref().ok())
            .cloned();
        match moved {
            Some(new) => self.wt_open(Some(c.key), &chosen, new, dest),
            None => {
                let (p, cwd) = (chosen.path.clone(), chosen.cwd.clone());
                self.wt_open(Some(c.key), &chosen, p, cwd);
            }
        }
    }

    fn end_continue(&mut self, key: &RequestKey, result: Result<Done, GatewayError>) {
        if let Some(c) = self.worktree.continuing.remove(key) {
            self.job_finished(c.key, result);
            self.wt_views_changed();
        }
    }

    // ---- removed worktrees' leftovers -------------------------------------------------------

    /// Once per run (after the first scan): forget removed worktrees whose main checkout is gone
    /// too, and move the closed sessions still carrying a removed worktree's folder into its
    /// main checkout.
    fn prune_removed_once(&mut self) {
        if self.worktree.pruned {
            return;
        }
        if self.cfg.removed_worktrees.is_empty() {
            self.worktree.pruned = true;
            return;
        }
        let (Some(groups), Some(snap)) = (self.sessions_watch.latest(), self.running.latest())
        else {
            return;
        };
        self.worktree.pruned = true;
        let open: Vec<PathBuf> = self
            .sess
            .managed
            .iter()
            .filter_map(|m| m.ctl.session_path().map(Path::to_path_buf))
            .collect();
        let leftovers: Vec<Leftover> = self
            .cfg
            .removed_worktrees
            .iter()
            .map(|(path, r)| {
                let all: Vec<&SessionEntry> = groups
                    .iter()
                    .flat_map(|g| &g.sessions)
                    .filter(|e| e.cwd == *path)
                    .collect();
                let files: Vec<PathBuf> = all
                    .iter()
                    .filter(|e| !snap.is_running(&e.id, &e.path) && !open.contains(&e.path))
                    .map(|e| e.path.clone())
                    .collect();
                Leftover {
                    worktree: path.clone(),
                    primary: r.primary.clone(),
                    all_movable: files.len() == all.len(),
                    none: all.is_empty(),
                    files,
                }
            })
            .collect();
        let removed = self.cfg.removed_worktrees.clone();
        self.wt_worker("worktree-prune", move || WtFin::Pruned {
            orphaned: removed
                .iter()
                .filter(|(_, r)| !r.primary.is_dir())
                .map(|(p, _)| p.clone())
                .collect(),
            leftovers,
        });
    }

    fn on_pruned(&mut self, orphaned: Vec<PathBuf>, leftovers: Vec<Leftover>) {
        let mut ops: Vec<CfgOp> = orphaned.iter().cloned().map(CfgOp::Unremove).collect();
        for l in leftovers
            .into_iter()
            .filter(|l| !orphaned.contains(&l.worktree))
        {
            if l.none {
                ops.push(CfgOp::Unremove(l.worktree));
                continue;
            }
            let n = l.files.len();
            let files: Vec<PathBuf> = l
                .files
                .into_iter()
                .filter(|f| self.worktree.in_flight.insert(f.clone()))
                .collect();
            if files.is_empty() {
                continue;
            }
            let all = l.all_movable && files.len() == n;
            let ctx = self.job_ctx();
            let purpose = Relocate::Leftover {
                worktree: l.worktree,
                all,
            };
            let dest = l.primary;
            self.wt_worker("worktree-leftovers", move || WtFin::Relocated {
                purpose,
                result: match jobs::relocate_sessions(&ctx, &files, &dest) {
                    Ok(Done::Relocated(r)) => Ok(r),
                    Ok(_) => Ok(Vec::new()),
                    Err(e) => Err(e.to_string()),
                },
                files,
            });
        }
        self.wt_persist(ops);
    }

    fn on_relocated(
        &mut self,
        purpose: Relocate,
        files: &[PathBuf],
        result: Result<Relocations, String>,
    ) {
        for f in files {
            self.worktree.in_flight.remove(f);
        }
        self.sessions_watch.refresh_now();
        match purpose {
            Relocate::Leftover { worktree, all } => match result {
                Err(e) => log::warn!("could not move sessions of a removed worktree: {e}"),
                Ok(results) => {
                    let failed: Vec<&String> = results
                        .iter()
                        .filter_map(|(_, r)| r.as_ref().err())
                        .collect();
                    if let Some(why) = failed.first() {
                        log::warn!(
                            "could not move {} of {} sessions of a removed worktree: {why}",
                            failed.len(),
                            results.len()
                        );
                    } else if all {
                        self.wt_persist(vec![CfgOp::Unremove(worktree)]);
                    }
                }
            },
            Relocate::Continue(key) => {
                let note = match &result {
                    Err(e) => Some(format!("Could not move the parked sessions: {e}")),
                    Ok(r) => r
                        .iter()
                        .find_map(|(_, r)| r.as_ref().err())
                        .map(|why| format!("Could not move a parked session: {why}")),
                };
                if let Some(c) = self.worktree.continuing.get_mut(&key) {
                    c.closed_busy = false;
                    c.closed = result.unwrap_or_default();
                }
                if let Some(note) = note {
                    self.wt_stage(&key, note);
                }
            }
        }
    }
}

/// [`Query::WorktreeDestination`](super::super::protocol::Query::WorktreeDestination): `dest`,
/// else the default location, and [`check_destination`]'s verdict on it. Blocking.
pub(in crate::gateway) fn destination(
    omp: Option<&Omp>,
    repo: &Path,
    branch: &str,
    dest: Option<PathBuf>,
) -> Result<DestinationView, String> {
    let dest = match dest {
        Some(dest) => dest,
        None => {
            let info = worktree::repo_info(repo).map_err(|e| e.to_string())?;
            default_dest(omp, &info.primary, branch.trim())
        }
    };
    Ok(DestinationView {
        error: check_destination(&dest).err(),
        dest,
    })
}

/// `WorktreeNew` on a worker: the worktree is made and its merge target recorded.
fn create_new(
    omp: Option<&Omp>,
    settings: &SettingsService,
    repo: &Path,
    branch: &str,
    base_ref: Option<String>,
    dest: Option<PathBuf>,
) -> Result<PathBuf, String> {
    let fail = |e: &dyn std::fmt::Display| format!("Could not create the worktree: {e}");
    let omp = omp.ok_or_else(|| fail(&"omp was not found"))?;
    let info = worktree::repo_info(repo).map_err(|e| fail(&e))?;
    if !info.has_commits {
        return Err(fail(
            &"this repository has no commits yet — make one first, git cannot branch from nothing",
        ));
    }
    let branch = branch.trim();
    worktree::check_branch_name(&info.root, branch).map_err(|e| fail(&e))?;
    let base_ref = base_ref
        .filter(|b| !b.trim().is_empty())
        .or_else(|| info.current_branch.clone())
        .unwrap_or_else(|| "HEAD".to_owned());
    let dest = dest.unwrap_or_else(|| default_dest(Some(omp), &info.primary, branch));
    check_destination(&dest).map_err(|e| fail(&e))?;
    worktree::create(omp, &info.root, branch, &base_ref, &dest).map_err(|e| fail(&e))?;
    let mut ops = vec![CfgOp::Unremove(dest.clone())];
    if let Some(base) = base_to_record(&base_ref, &info.branches) {
        ops.push(CfgOp::RecordBase(dest.clone(), base));
    }
    if let Err(e) = persist(settings, &ops) {
        log::warn!("recording the new worktree's merge target failed: {e}");
    }
    Ok(dest)
}

/// "Continue in worktree" on a worker: a worktree on the parked branch at its default location;
/// the branch is no longer parked.
fn add_parked(
    omp: Option<&Omp>,
    settings: &SettingsService,
    rec: &ParkedBranch,
) -> Result<PathBuf, String> {
    let omp = omp.ok_or_else(|| "omp was not found".to_owned())?;
    let dest = default_dest(Some(omp), &rec.primary, &rec.branch);
    check_destination(&dest)
        .map_err(|e| format!("Cannot create a worktree for {}: {e}", rec.branch))?;
    worktree::add_existing(omp, &rec.primary, &rec.branch, &dest)
        .map_err(|e| format!("Could not create the worktree: {e}"))?;
    let ops = [
        CfgOp::RecordBase(dest.clone(), rec.base.clone()),
        CfgOp::Unremove(dest.clone()),
        CfgOp::Unpark(rec.primary.clone(), rec.branch.clone()),
    ];
    if let Err(e) = persist(settings, &ops) {
        log::warn!("saving the continued branch's records failed: {e}");
    }
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt(path: &str, primary: &str, branch: Option<&str>) -> Worktree {
        Worktree {
            path: PathBuf::from(path),
            primary: PathBuf::from(primary),
            branch: branch.map(str::to_owned),
        }
    }

    fn status(dirty: bool, merged: bool, pushed: bool) -> WorktreeStatus {
        WorktreeStatus {
            branch: Some("wt/x".to_owned()),
            dirty,
            merged,
            ahead: u32::from(!merged),
            pushed,
            upstream: pushed.then(|| "origin/wt/x".to_owned()),
        }
    }

    #[test]
    fn labels_fall_back_to_the_folder_name() {
        assert_eq!(worktree_label(&wt("/r/wt-a", "/r", Some("feat"))), "feat");
        assert_eq!(worktree_label(&wt("/r/wt-a", "/r", None)), "wt-a");
    }

    #[test]
    fn recorded_base_prefers_the_local_twin_of_a_remote_ref() {
        let branches = vec!["main".to_owned(), "origin/main".to_owned()];
        assert_eq!(
            base_to_record("origin/main", &branches).as_deref(),
            Some("main")
        );
        assert_eq!(
            base_to_record("origin/dev", &branches).as_deref(),
            Some("origin/dev")
        );
        assert_eq!(base_to_record("HEAD", &branches), None);
        assert_eq!(base_to_record("main", &branches).as_deref(), Some("main"));
    }

    #[test]
    fn a_moved_session_file_is_the_same_session_by_name() {
        assert!(same_session(
            Path::new("/a/1_x.jsonl"),
            Path::new("/b/1_x.jsonl")
        ));
        assert!(!same_session(
            Path::new("/a/1_x.jsonl"),
            Path::new("/a/2_x.jsonl")
        ));
    }

    #[test]
    fn a_destination_must_be_absent_below_a_folder_or_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(check_destination(&dir.path().join("new")), Ok(()));
        assert_eq!(check_destination(dir.path()), Ok(()));
        std::fs::write(dir.path().join("f"), "x").expect("file");
        assert_eq!(
            check_destination(dir.path()),
            Err("That folder is not empty".to_owned())
        );
        assert_eq!(
            check_destination(&dir.path().join("f")),
            Err("A file is in the way".to_owned())
        );
        assert!(
            check_destination(&dir.path().join("f/below"))
                .is_err_and(|e| e.ends_with("is not a folder"))
        );
    }

    #[test]
    fn merged_removes_pushed_parks_anything_else_is_explained() {
        assert_eq!(
            retirement(&status(false, true, true), "main", "x"),
            Retirement::Remove
        );
        assert_eq!(
            retirement(&status(false, false, true), "main", "x"),
            Retirement::Park
        );
        assert!(matches!(
            retirement(&status(true, true, true), "main", "x"),
            Retirement::Blocked(m) if m.starts_with("Cannot remove “x”: ")
        ));
        assert!(matches!(
            retirement(&status(false, false, false), "main", "x"),
            Retirement::Blocked(m) if m.ends_with("Commit and push the work to park it, or merge it first.")
        ));
    }

    #[test]
    fn a_retire_asking_for_the_other_way_is_refused_with_the_right_hint() {
        assert_eq!(
            retire_refusal(&Retirement::Remove, false, "x", "main"),
            None
        );
        assert_eq!(retire_refusal(&Retirement::Park, true, "x", "main"), None);
        assert!(
            retire_refusal(&Retirement::Park, false, "x", "main")
                .is_some_and(|m| m.ends_with("park it instead"))
        );
        assert!(
            retire_refusal(&Retirement::Remove, true, "x", "main")
                .is_some_and(|m| m.ends_with("remove it instead"))
        );
        assert_eq!(
            retire_refusal(&Retirement::Blocked("why".to_owned()), true, "x", "main").as_deref(),
            Some("why")
        );
    }

    #[test]
    fn leaving_a_worktree_falls_back_to_the_launch_folder_arriving_needs_evidence() {
        let w = wt("/r/wt", "/r", Some("b"));
        let now = Instant::now();
        let leave = Relocation::new(PathBuf::from("/r"), Some(w), Vec::new(), now);
        assert!(!leave.arrived(None, Path::new("/r/wt"), None));
        assert!(leave.arrived(Some(Path::new("/r")), Path::new("/r/wt"), None));
        assert!(leave.arrived(None, Path::new("/r/wt"), Some(Path::new("/r"))));
        let arrive = Relocation::new(PathBuf::from("/r/new"), None, Vec::new(), now);
        assert!(!arrive.arrived(None, Path::new("/r/new"), None));
        assert!(arrive.arrived(Some(Path::new("/r/new/sub")), Path::new("/r"), None));
        assert!(arrive.arrived(None, Path::new("/r"), Some(Path::new("/r/new"))));
        assert!(arrive.finished(now + RELOCATE_TIMEOUT));
    }

    #[test]
    fn a_finish_turn_ends_on_a_busy_look_or_on_the_files_evidence_never_on_idle_alone() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let file = tmp.path().join("s.jsonl");
        let iso = |s: u32| format!("2026-10-30T12:00:{s:02}.000Z");
        let ms = |s: u32| deck_core::usage::parse_iso_ms(&iso(s)).expect("iso time");
        let line = |role: &str, s: u32| {
            format!(
                "{{\"type\":\"message\",\"timestamp\":\"{}\",\"message\":{{\"role\":\"{role}\"}}}}\n",
                iso(s)
            )
        };
        let (user, reply) = (|s| line("user", s), |s| line("assistant", s));
        let append = |path: &Path, s: &str| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut f| f.write_all(s.as_bytes()))
                .expect("append");
        };
        let len = |path: &Path| std::fs::metadata(path).map(|m| m.len()).ok();
        let clock = std::cell::Cell::new(Instant::now());
        let tick = || {
            clock.set(clock.get() + FLOW_TICK);
            clock.get()
        };
        let read = |w: &mut TurnWatch, path: &Path| {
            let now = tick();
            let probe = w.probe(Some(path), now).expect("a read is due");
            assert!(w.probe(Some(path), now).is_none(), "one read at a time");
            let scan = probe.run();
            w.probed(&probe, scan);
            assert!(
                w.probe(Some(path), now).is_none(),
                "the next read waits a tick"
            );
        };
        // Earlier turns of the session are before the mark.
        append(
            &file,
            &format!("{{\"type\":\"session\"}}\n{}{}", user(1), reply(2)),
        );

        // Busy then idle: ended at once, one check at a time.
        let mut w = TurnWatch::default();
        w.submitted(1, Some(file.clone()), len(&file), ms(10));
        assert!(!w.look(Activity::Busy, true, ms(10)));
        assert!(!w.look(Activity::Idle, false, ms(11)), "a check is running");
        assert!(w.look(Activity::Idle, true, ms(11)));
        assert!(
            !w.look(Activity::Idle, true, ms(11)),
            "checked once per turn"
        );

        // Taken but idle with nothing written: however long, the turn has not ended.
        let mut w = TurnWatch::default();
        w.submitted(2, Some(file.clone()), len(&file), ms(10));
        for _ in 0..5 {
            read(&mut w, &file);
            assert!(!w.look(Activity::Idle, true, ms(10)), "no evidence yet");
        }
        // The turn begins (its question written) — still not over.
        append(&file, &user(11));
        read(&mut w, &file);
        assert!(!w.look(Activity::Idle, true, ms(11)), "asked, not answered");
        // A reply being written is read once complete.
        append(
            &file,
            &format!(
                "{{\"type\":\"message\",\"timestamp\":\"{}\",\"message\":{{\"role\":\"assis",
                iso(12)
            ),
        );
        read(&mut w, &file);
        assert!(
            !w.look(Activity::Idle, true, ms(12)),
            "half a line is no evidence"
        );
        append(&file, "tant\"}}\n");
        // A turn quicker than the looks: idle throughout, ended by what the file shows.
        read(&mut w, &file);
        assert!(!w.look(Activity::Idle, false, ms(12)), "a check is running");
        assert!(w.look(Activity::Idle, true, ms(12)));
        read(&mut w, &file);
        assert!(
            !w.look(Activity::Idle, true, ms(12)),
            "checked once per turn"
        );
        // The user's next turn counts again.
        append(&file, &format!("{}{}", user(13), reply(14)));
        read(&mut w, &file);
        assert!(w.look(Activity::Idle, true, ms(15)));

        // A turn caught busy ends on its idle look; its own entries, read after that, do not
        // end another turn — a later question and answer do.
        let mut w = TurnWatch::default();
        w.submitted(3, Some(file.clone()), len(&file), ms(20));
        assert!(!w.look(Activity::Busy, true, ms(20)));
        assert!(!w.look(Activity::Unknown, true, ms(20)));
        append(&file, &format!("{}{}", user(21), reply(22)));
        assert!(w.look(Activity::Idle, true, ms(23)));
        read(&mut w, &file);
        assert!(
            !w.look(Activity::Idle, true, ms(23)),
            "that turn was counted"
        );
        append(&file, &format!("{}{}", user(24), reply(25)));
        read(&mut w, &file);
        assert!(w.look(Activity::Idle, true, ms(26)), "the next turn counts");
        // The counted turn and a later one, both unread at once.
        assert!(!w.look(Activity::Busy, true, ms(27)));
        append(&file, &format!("{}{}", user(27), reply(28)));
        assert!(w.look(Activity::Idle, true, ms(29)));
        append(&file, &format!("{}{}", user(30), reply(31)));
        read(&mut w, &file);
        assert!(
            w.look(Activity::Idle, true, ms(32)),
            "the later turn counts"
        );
        // A read that started before a turn ended is read again.
        append(&file, &format!("{}{}", user(33), reply(34)));
        let early = w.probe(Some(&file), tick()).expect("a read is due");
        assert!(!w.look(Activity::Busy, true, ms(35)));
        assert!(w.look(Activity::Idle, true, ms(35)));
        let scan = early.run();
        assert!(scan.answered);
        w.probed(&early, scan);
        assert!(!w.look(Activity::Idle, true, ms(35)), "not counted twice");

        // The prompt typed again: a read of the earlier one is stale.
        let mut w = TurnWatch::default();
        w.submitted(4, Some(file.clone()), Some(0), 0);
        let stale = w.probe(Some(&file), tick()).expect("a read is due");
        w.submitted(5, Some(file.clone()), len(&file), ms(40));
        assert!(w.probe(Some(&file), tick()).is_none(), "one read at a time");
        let scan = stale.run();
        assert!(scan.answered, "the whole file has turns");
        w.probed(&stale, scan);
        assert!(!w.look(Activity::Idle, true, ms(40)));
        read(&mut w, &file);
        assert!(!w.look(Activity::Idle, true, ms(40)));

        // A finish cancelled and started again while its read was out: the read belongs to
        // the old watch and changes nothing in the new one.
        let mut old = TurnWatch::default();
        old.submitted(6, Some(file.clone()), Some(0), 0);
        let gone = old.probe(Some(&file), tick()).expect("a read is due");
        let mut w = TurnWatch::default();
        w.submitted(7, Some(file.clone()), len(&file), ms(41));
        let mine = w.probe(Some(&file), tick()).expect("a read is due");
        let scan = gone.run();
        assert!(scan.answered);
        w.probed(&gone, scan);
        assert!(
            !w.look(Activity::Idle, true, ms(41)),
            "another finish's read"
        );
        assert!(
            w.probe(Some(&file), tick()).is_none(),
            "its own read is still out"
        );
        let scan = mine.run();
        w.probed(&mine, scan);
        assert!(!w.look(Activity::Idle, true, ms(41)));
        append(&file, &format!("{}{}", user(42), reply(43)));
        read(&mut w, &file);
        assert!(w.look(Activity::Idle, true, ms(44)));

        // An unknown mark (the session file was not known yet): read from its start once
        // readable; only entries written since the prompt count. A failed read keeps it
        // unknown.
        let later = tmp.path().join("later.jsonl");
        let mut w = TurnWatch::default();
        w.submitted(8, None, None, ms(45));
        read(&mut w, &later);
        assert_eq!(w.offset, None, "nothing read: still unknown");
        append(&later, &format!("{}{}", user(1), reply(2)));
        read(&mut w, &later);
        assert!(
            !w.look(Activity::Idle, true, ms(45)),
            "entries before the prompt"
        );
        append(&later, &format!("{}{}", user(46), reply(47)));
        read(&mut w, &later);
        assert!(w.look(Activity::Idle, true, ms(48)));
        // A file rewritten shorter than read so far is read again from its start.
        std::fs::write(&later, user(5)).expect("rewrite");
        read(&mut w, &later);
        assert!(!w.look(Activity::Idle, true, ms(48)));

        // A missing file at a known mark is read from its start once it appears.
        let fresh = tmp.path().join("new.jsonl");
        let mut w = TurnWatch::default();
        w.submitted(9, Some(fresh.clone()), None, ms(50));
        append(&fresh, &format!("{}{}", user(51), reply(52)));
        read(&mut w, &fresh);
        assert!(w.look(Activity::Idle, true, ms(53)));
    }

    #[test]
    fn a_session_opened_after_the_late_check_is_refused_without_a_look() {
        let (a, b, c, d) = (
            SessionKey::direct(),
            SessionKey::direct(),
            SessionKey::direct(),
            SessionKey::direct(),
        );
        let live = vec![
            (a.clone(), "classified".to_owned()),
            (b.clone(), "late".to_owned()),
            (c.clone(), "moved".to_owned()),
        ];
        let judged: [&[SessionKey]; 3] = [
            std::slice::from_ref(&a),
            std::slice::from_ref(&b),
            std::slice::from_ref(&c),
        ];
        assert_eq!(unjudged(&live, &judged), None);
        let mut newer = live.clone();
        newer.push((d, "newer".to_owned()));
        assert_eq!(unjudged(&newer, &judged).as_deref(), Some("newer"));
    }

    #[cfg(unix)]
    #[test]
    fn a_late_controller_is_judged_where_its_folder_leads() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("temp dir");
        let (repo, tree, other) = (
            tmp.path().join("repo"),
            tmp.path().join("wt"),
            tmp.path().join("other"),
        );
        for d in [&repo, &tree.join("sub"), &other] {
            std::fs::create_dir_all(d).expect("dir");
        }
        // An alias of the worktree: lexically elsewhere, really inside.
        symlink(&tree, tmp.path().join("alias")).expect("alias link");
        let w = Worktree {
            path: tree.clone(),
            primary: repo.clone(),
            branch: Some("b".to_owned()),
        };
        let tab = |live: Option<PathBuf>, stored: &Path| TabCwd {
            uid: 7,
            label: "late".to_owned(),
            stored: stored.to_path_buf(),
            live,
        };
        assert_eq!(late_refusal(&w, "b", &[]), None, "nothing opened late");
        let refused = |t: TabCwd| late_refusal(&w, "b", &[t]);
        assert!(
            refused(tab(Some(tmp.path().join("alias/sub")), &other))
                .is_some_and(|m| m.contains("late was opened in it")),
            "running in the worktree through a symlink"
        );
        assert!(refused(tab(Some(tree.join("sub")), &other)).is_some());
        assert_eq!(
            refused(tab(Some(other.clone()), &tree)),
            None,
            "verified outside"
        );
        // No live folder: judged by where it was launched; next to the worktree's repository
        // nobody can tell, so removal stops.
        assert!(refused(tab(None, &tmp.path().join("alias"))).is_some());
        assert!(refused(tab(None, &repo)).is_some_and(|m| m.contains("cannot tell")));
        assert_eq!(refused(tab(None, &other)), None);
    }

    #[test]
    fn records_change_only_what_the_flow_touched() {
        let mut cfg = AppConfig {
            recent_folders: vec![PathBuf::from("/r/wt"), PathBuf::from("/r")],
            folder_scope: Some(PathBuf::from("/r/wt/sub")),
            parked_branches: vec![ParkedBranch {
                primary: PathBuf::from("/r"),
                branch: "b".to_owned(),
                base: "main".to_owned(),
                sessions: vec!["old".to_owned()],
                parked_ms: 1,
            }],
            ..AppConfig::default()
        };
        cfg.worktree_bases
            .insert(PathBuf::from("/r/wt"), "main".to_owned());
        let out = changes(
            &cfg,
            &[
                CfgOp::MarkRemoved(
                    PathBuf::from("/r/wt"),
                    RemovedWorktree {
                        primary: PathBuf::from("/r"),
                        branch: Some("b".to_owned()),
                    },
                ),
                CfgOp::ForgetBase(PathBuf::from("/r/wt")),
                CfgOp::ForgetFolder(PathBuf::from("/r/wt")),
                CfgOp::Park(ParkedBranch {
                    primary: PathBuf::from("/r"),
                    branch: "b".to_owned(),
                    base: "main".to_owned(),
                    sessions: vec!["new".to_owned()],
                    parked_ms: 2,
                }),
            ],
        );
        let mut next = cfg.clone();
        for s in &out {
            match s {
                Setting::WorktreeBases(v) => next.worktree_bases = v.clone(),
                Setting::RemovedWorktrees(v) => next.removed_worktrees = v.clone(),
                Setting::ParkedBranches(v) => next.parked_branches = v.clone(),
                Setting::RecentFolders(v) => next.recent_folders = v.clone(),
                Setting::FolderScope(v) => next.folder_scope = v.clone(),
                other => panic!("unexpected change {other:?}"),
            }
        }
        assert!(next.worktree_bases.is_empty());
        assert!(next.removed_worktrees.contains_key(Path::new("/r/wt")));
        assert_eq!(next.recent_folders, vec![PathBuf::from("/r")]);
        assert_eq!(next.folder_scope, None);
        assert_eq!(next.parked_branches.len(), 1);
        assert_eq!(next.parked_branches[0].sessions, ["new", "old"]);
        assert_eq!(next.parked_branches[0].parked_ms, 2);
        assert!(changes(&next, &[CfgOp::ForgetBase(PathBuf::from("/x"))]).is_empty());
    }
}
