//! Typed executors of the operations that run on worker threads: everything that blocks on git,
//! omp, the file system or the network. The engine decides *whether* something runs (epoch,
//! journal, barrier, one-at-a-time lanes); these functions only run it over the real core and
//! service code and map results to protocol values.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::engine::worktree as engine_worktree;
use deck_core::agent_defs::{self, EditError};
use deck_core::backup::drive::{LOGIN_TIMEOUT, LoginFlow};
use deck_core::backup::server::{ServerAuth, ServerConfig};
use deck_core::backup::{self, Destination, PreparedProjectRestore, PreparedRestore, Service};
use deck_core::changes::{self, ChangedFile};
use deck_core::ctl::{CtlClient, CtlCommand, CtlReply};
use deck_core::inventory::{InventoryEntry, SessionStats, StatsCache};
use deck_core::omp::{Omp, SECRET_PLACEHOLDER, SettingsCatalog};
use deck_core::sessions::{SessionsRoot, find_copy, find_fork, sessions_under};
use deck_core::worktree::{self, Removal, Removed, Worktree};

use super::protocol::{
    Answer, Done, GatewayError, Progress, Query, RestorePlanView, RunningDetail,
    ServerConnectOutcome, ServerView,
};
use crate::backend::Backends;
use crate::lifecycle;
use crate::session_actions;

/// How long a background fork waits for a running session's control socket.
const BG_CONNECT: Duration = Duration::from_secs(3);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn failed(e: impl std::fmt::Display) -> GatewayError {
    GatewayError::Failed(e.to_string())
}

/// Server-side objects a client refers to by number: OAuth flows and prepared restores never
/// leave the gateway (they hold tokens, temp files and validated archives). A prepared restore
/// belongs to the client that prepared it: only that client may apply or discard it.
#[derive(Default)]
pub struct Handles {
    next: AtomicU64,
    logins: Mutex<HashMap<u64, Login>>,
    restores: Mutex<HashMap<u64, (String, PreparedRestore)>>,
    project_restores: Mutex<HashMap<u64, (String, PreparedProjectRestore)>>,
}

struct Login {
    /// Taken by the job waiting for the browser.
    flow: Option<LoginFlow>,
    cancel: Arc<AtomicBool>,
}

impl Handles {
    fn next(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Cancels a sign-in: a waiting job returns `Cancelled`; an unclaimed flow is dropped.
    pub fn cancel_login(&self, handle: u64) -> bool {
        let mut logins = lock(&self.logins);
        match logins.get(&handle) {
            Some(l) if l.flow.is_none() => {
                l.cancel.store(true, Ordering::Relaxed);
                true
            }
            Some(_) => {
                logins.remove(&handle);
                true
            }
            None => false,
        }
    }

    /// Drops a prepared Drive restore of `client`.
    pub fn discard_restore(&self, handle: u64, client: &str) -> bool {
        let mut restores = lock(&self.restores);
        let own = restores.get(&handle).is_some_and(|(o, _)| o == client);
        own && restores.remove(&handle).is_some()
    }

    pub fn discard_project_restore(&self, handle: u64, client: &str) -> bool {
        let mut restores = lock(&self.project_restores);
        let own = restores.get(&handle).is_some_and(|(o, _)| o == client);
        own && restores.remove(&handle).is_some()
    }

    /// The client that prepared Drive restore `handle`.
    pub fn restore_owner(&self, handle: u64) -> Option<String> {
        lock(&self.restores).get(&handle).map(|(o, _)| o.clone())
    }

    /// The client that prepared project restore `handle`.
    pub fn project_restore_owner(&self, handle: u64) -> Option<String> {
        lock(&self.project_restores)
            .get(&handle)
            .map(|(o, _)| o.clone())
    }

    /// The local copies project restore `handle` overwrites under `conflicts`: `(session id,
    /// local session file)` of every backed-up session whose choice is
    /// [`backup::ConflictChoice::Overwrite`] and that is not in `excluded` (left out of the
    /// restore, so its local copy is untouched).
    pub fn overwritten_copies(
        &self,
        handle: u64,
        conflicts: &std::collections::BTreeMap<String, backup::ConflictChoice>,
        excluded: &std::collections::BTreeSet<String>,
    ) -> Vec<(String, PathBuf)> {
        let restores = lock(&self.project_restores);
        let Some((_, prepared)) = restores.get(&handle) else {
            return Vec::new();
        };
        prepared
            .plan
            .sessions
            .iter()
            .filter(|s| !excluded.contains(&s.id))
            .filter(|s| conflicts.get(&s.id) == Some(&backup::ConflictChoice::Overwrite))
            .flat_map(|s| s.local.iter().map(|l| (s.id.clone(), l.path.clone())))
            .collect()
    }

    /// A sign-in is waiting for the browser.
    pub fn login_waiting(&self) -> bool {
        !lock(&self.logins).is_empty()
    }
}

/// What every worker job may use. Cheap to clone (shared parts are `Arc`s).
#[derive(Clone)]
pub struct JobCtx {
    pub omp: Option<Omp>,
    pub root: SessionsRoot,
    pub backends: Backends,
    pub ctl_ns: String,
    /// The backup service (all destinations).
    pub service: Arc<Service>,
    /// The active destination when the job started.
    pub destination: Destination,
    pub handles: Arc<Handles>,
    pub stats: Arc<Mutex<StatsCache>>,
    /// The latest inventory scan (stats are computed for its entries).
    pub inventory: Arc<Mutex<Arc<Vec<InventoryEntry>>>>,
}

impl JobCtx {
    fn omp(&self) -> Result<&Omp, GatewayError> {
        self.omp
            .as_ref()
            .ok_or_else(|| GatewayError::Refused("omp was not found".to_owned()))
    }

    /// The Google side, for sign-in and sign-out.
    fn drive(&self) -> Result<&Service, GatewayError> {
        if self.service.creds.is_none() {
            return Err(GatewayError::Refused(
                "Google Drive backup is not available in this build".to_owned(),
            ));
        }
        Ok(&self.service)
    }

    /// The backup service acting on the active destination.
    fn backup(&self) -> Service {
        self.service.with_destination(self.destination)
    }
}

fn since(unix_ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(unix_ms)
}

/// omp's settings catalog with every secret value replaced by the placeholder (omp already
/// redacts them; this keeps the guarantee independent of omp's output).
pub fn sanitize(mut catalog: SettingsCatalog) -> SettingsCatalog {
    for s in catalog.tabs.iter_mut().flat_map(|t| t.settings.iter_mut()) {
        if s.secret && s.value.is_some() {
            s.value = Some(serde_json::Value::String(SECRET_PLACEHOLDER.to_owned()));
        }
    }
    catalog
}

/// The plan of a prepared config restore, without its archive.
pub fn plan_view(p: &PreparedRestore) -> RestorePlanView {
    RestorePlanView {
        name: p.name.clone(),
        created: p.manifest.created.clone(),
        host: p.manifest.host.clone(),
        replaced: p.plan.replaced.clone(),
        added: p.plan.added.clone(),
        dirs: p.plan.dirs.clone(),
        removed: p.plan.removed.clone(),
        safety_dir: p.safety_dir.clone(),
    }
}

/// Read-only queries that block. `None`: not a worker query (the engine answers it).
pub fn query(ctx: &JobCtx, q: &Query) -> Option<Result<Answer, GatewayError>> {
    let r = match q {
        Query::UsageActivity { days } => {
            deck_core::usage::activity(&deck_core::usage::stats_db_path(), *days)
                .map(Answer::UsageActivity)
                .map_err(failed)
        }
        Query::OmpVersion => ctx
            .omp()
            .and_then(|o| o.version().map_err(failed))
            .map(Answer::OmpVersion),
        Query::OmpCheckUpdate => ctx
            .omp()
            .and_then(|o| o.check_update().map_err(failed))
            .map(Answer::OmpCheckUpdate),
        Query::Models => ctx
            .omp()
            .and_then(|o| o.models().map_err(failed))
            .map(Answer::Models),
        Query::OmpSettings => ctx
            .omp()
            .and_then(|o| o.load_settings().map_err(failed))
            .map(|c| Answer::OmpSettings(sanitize(c))),
        Query::Diff { root, file, side } => changes::diff(root, file, *side)
            .map(Answer::Diff)
            .map_err(failed),
        Query::Patch { root } => changes::patch(root).map(Answer::Patch).map_err(failed),
        Query::RepoInfo(dir) => worktree::repo_info(dir)
            .map(Answer::RepoInfo)
            .map_err(failed),
        Query::CheckBranch { repo, name } => worktree::check_branch_name(repo, name)
            .map(|()| Answer::CheckBranch)
            .map_err(failed),
        Query::WorktreeDestination { repo, branch, dest } => {
            engine_worktree::destination(ctx.omp.as_ref(), repo, branch, dest.clone())
                .map(Answer::WorktreeDestination)
                .map_err(failed)
        }
        Query::ResolveWorktree(dir) => Ok(Answer::ResolveWorktree(worktree::resolve(dir))),
        Query::WorktreeStatus { worktree: wt, base } => worktree::status(wt, base)
            .map(Answer::WorktreeStatus)
            .map_err(failed),
        Query::InferBase(wt) => Ok(Answer::InferBase(worktree::infer_base(wt))),
        Query::MergeTarget { primary, base } => worktree::find_merge_target(primary, base)
            .map(Answer::MergeTarget)
            .map_err(GatewayError::Failed),
        Query::BranchState {
            primary,
            branch,
            base,
        } => worktree::branch_state(primary, branch, base)
            .map(Answer::BranchState)
            .map_err(failed),
        Query::BackupList => ctx.backup().list().map(Answer::BackupList).map_err(failed),
        Query::ProjectBackups => ctx
            .backup()
            .list_projects()
            .map(Answer::ProjectBackups)
            .map_err(failed),
        Query::ProjectSyncState => Ok(Answer::ProjectSyncState(ctx.backup().project_state())),
        Query::ProjectMeasure { project } => backup::measure(&ctx.root, project)
            .map(Answer::ProjectMeasure)
            .map_err(failed),
        Query::Stats(paths) => Ok(Answer::Stats(stats(ctx, paths))),
        Query::FindFork {
            parent_id,
            since_unix_ms,
        } => Ok(Answer::Path(find_fork(
            &ctx.root,
            parent_id,
            since(*since_unix_ms),
        ))),
        Query::FindCopy {
            parents,
            dest_cwd,
            since_unix_ms,
        } => {
            let parents: Vec<&str> = parents.iter().map(String::as_str).collect();
            Ok(Answer::Path(find_copy(
                &ctx.root,
                &parents,
                dest_cwd,
                since(*since_unix_ms),
            )))
        }
        Query::RunningDetail => Ok(Answer::RunningDetail(running_detail(
            &ctx.backends,
            &ctx.root.path,
        ))),
        Query::AgentDefs { projects } => Ok(Answer::AgentDefs(agent_catalog(ctx, projects))),
        Query::Topic(_)
        | Query::Screen(_)
        | Query::WorktreeSessions(_)
        | Query::WorktreeVerdict(_) => return None,
    };
    Some(r)
}

/// Every backend's live sessions (a name listed by several belongs to the first) and the tmux
/// server's memory. Blocking.
fn running_detail(backends: &Backends, root: &Path) -> RunningDetail {
    let mut detail = RunningDetail::default();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for mux in backends.all() {
        detail.sessions.extend(
            mux.running(root)
                .into_iter()
                .filter(|s| seen.insert(s.name.clone())),
        );
        if matches!(mux, deck_core::mux::Mux::Tmux(_)) {
            detail.tmux = true;
            detail.tmux_server = mux.server_memory();
        }
    }
    detail
}

/// The live backend sessions whose folder lies inside `dir`, except `keep`. Blocking.
pub fn sessions_inside(
    backends: &Backends,
    root: &Path,
    dir: &Path,
    keep: &[String],
) -> Vec<String> {
    running_detail(backends, root)
        .sessions
        .into_iter()
        .filter(|s| s.cwd.as_deref().is_some_and(|c| c.starts_with(dir)) && !keep.contains(&s.name))
        .map(|s| s.name)
        .collect()
}

/// Kills exactly `names` (`wait`: until no backend lists them); the failures, with why.
/// Blocking.
pub fn kill_names(backends: &Backends, names: &[String], wait: bool) -> Result<Done, GatewayError> {
    let failed = names
        .iter()
        .filter_map(|name| {
            let r = if wait {
                backends.stop(name)
            } else {
                backends.kill(name)
            };
            r.err().map(|e| (name.clone(), e))
        })
        .collect();
    Ok(Done::Killed(failed))
}

/// Stats of the listed sessions of the latest inventory scan (cached by size and mtime).
fn stats(ctx: &JobCtx, paths: &[PathBuf]) -> Vec<(PathBuf, Option<SessionStats>)> {
    let entries = lock(&ctx.inventory).clone();
    let mut cache = lock(&ctx.stats);
    paths
        .iter()
        .map(|p| {
            let stat = entries
                .iter()
                .find(|e| e.path == *p)
                .and_then(|e| cache.compute(e).ok());
            (p.clone(), stat)
        })
        .collect()
}

fn refs(files: &[ChangedFile]) -> Vec<&ChangedFile> {
    files.iter().collect()
}

pub fn stage(root: &Path, files: &[ChangedFile]) -> Result<Done, GatewayError> {
    changes::stage(root, &refs(files))
        .map(|()| Done::Unit)
        .map_err(failed)
}

pub fn unstage(root: &Path, files: &[ChangedFile]) -> Result<Done, GatewayError> {
    changes::unstage(root, &refs(files))
        .map(|()| Done::Unit)
        .map_err(failed)
}

pub fn discard(root: &Path, files: &[ChangedFile]) -> Result<Done, GatewayError> {
    changes::discard(root, &refs(files))
        .map(|()| Done::Unit)
        .map_err(failed)
}

pub fn delete_untracked(root: &Path, files: &[ChangedFile]) -> Result<Done, GatewayError> {
    changes::delete_untracked(root, &refs(files))
        .map(|()| Done::Unit)
        .map_err(failed)
}

/// The folders a paired device may act in (browse, start a session, sync, restore into): the
/// browse roots and project folders, compared where symlinks lead. Checked again on the worker
/// that uses a folder, so a link changed after the request cannot redirect it.
#[derive(Clone, Debug)]
pub struct Confine {
    roots: Vec<PathBuf>,
}

/// [`Confine`]'s roots with every symlink resolved.
pub struct ResolvedRoots(Vec<PathBuf>);

/// `p` with every symlink resolved, in the spelling the rest of OmpDeck uses (no Windows
/// verbatim prefix). A missing tail (a restore may create its target) is kept as written below
/// its deepest existing entry, which must be a folder; `None` when nothing resolves, that
/// entry is a file, an entry on the way exists yet does not resolve (a dangling link), or the
/// tail is not plain names (`..` could climb out again). Blocking.
fn resolved(p: &Path) -> Option<PathBuf> {
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur = p;
    loop {
        match std::fs::canonicalize(cur) {
            // A missing tail can only be created below a folder, never below a file.
            Ok(c) if !tail.is_empty() && !c.is_dir() => return None,
            Ok(c) => {
                let mut out = deck_core::platform::strip_verbatim(c);
                out.extend(tail.iter().rev());
                return Some(out);
            }
            Err(_) if std::fs::symlink_metadata(cur).is_ok() => return None,
            Err(_) => {}
        }
        let mut parts = cur.components();
        let Some(std::path::Component::Normal(name)) = parts.next_back() else {
            return None;
        };
        tail.push(name);
        cur = parts.as_path();
    }
}

impl Confine {
    pub fn new(roots: Vec<PathBuf>) -> Confine {
        Confine { roots }
    }

    /// The roots resolved (one that cannot be is left out). Blocking.
    pub fn resolve(&self) -> ResolvedRoots {
        ResolvedRoots(self.roots.iter().filter_map(|r| resolved(r)).collect())
    }

    /// [`ResolvedRoots::admit`] against freshly resolved roots, refused with a message.
    /// Blocking.
    pub fn admit(&self, dir: &Path) -> Result<PathBuf, GatewayError> {
        self.resolve().admit(dir).ok_or_else(|| {
            GatewayError::Refused("That folder is outside the folders a device may use".to_owned())
        })
    }
}

impl ResolvedRoots {
    /// `dir` resolved, if it leads at or below a root (a symlinked folder counts where it
    /// leads, never where it is listed). Blocking.
    pub fn admit(&self, dir: &Path) -> Option<PathBuf> {
        resolved(dir).filter(|d| self.0.iter().any(|r| d.starts_with(r)))
    }
}

pub fn worktree_create(
    ctx: &JobCtx,
    repo: &Path,
    branch: &str,
    base_ref: &str,
    dest: &Path,
) -> Result<Done, GatewayError> {
    worktree::create(ctx.omp()?, repo, branch, base_ref, dest)
        .map(|()| Done::Path(dest.to_path_buf()))
        .map_err(failed)
}

pub fn worktree_add_existing(
    ctx: &JobCtx,
    repo: &Path,
    branch: &str,
    dest: &Path,
) -> Result<Done, GatewayError> {
    worktree::add_existing(ctx.omp()?, repo, branch, dest)
        .map(|()| Done::Path(dest.to_path_buf()))
        .map_err(failed)
}

pub fn switch_branch(dir: &Path, branch: &str) -> Result<Done, GatewayError> {
    worktree::switch_branch(dir, branch)
        .map(|()| Done::Unit)
        .map_err(failed)
}

/// Stops `stop` (backend names), moves every session of the worktree (header cwd inside it,
/// stubs included) into its primary repository with omp's `/move`, then removes the worktree
/// (`how`: merged, or parked with its branch kept). Nothing is moved while removal would be
/// refused, and the first failed move keeps the worktree. A removal whose git step failed after
/// the folder was deleted still counts as removed (the branch note says why).
pub fn worktree_remove(
    ctx: &JobCtx,
    wt: &Worktree,
    base: &str,
    how: Removal,
    stop: &[String],
) -> Result<Done, GatewayError> {
    lifecycle::kill_all(&ctx.backends, stop, "worktree removal");
    if wt.path.is_dir() {
        let st = worktree::status(wt, base).map_err(failed)?;
        let why = match how {
            Removal::Merged { .. } => worktree::blockers(&st, base),
            Removal::Park => worktree::park_blockers(&st),
        };
        if !why.is_empty() {
            return Err(GatewayError::Refused(format!(
                "the worktree cannot be removed: {}",
                why.join("; ")
            )));
        }
    }
    let found = sessions_under(&ctx.root, &wt.path);
    if !found.is_empty() {
        let omp = ctx.omp()?;
        for (file, _) in &found {
            omp.relocate_session(file, &wt.primary).map_err(|e| {
                GatewayError::Failed(format!(
                    "could not move a session of this worktree into {}: {e}; the worktree was kept",
                    wt.primary.display()
                ))
            })?;
        }
    }
    let sessions = found.into_iter().map(|(_, id)| id).collect();
    match worktree::remove(wt, base, how) {
        Ok(removed) => Ok(Done::Removed { removed, sessions }),
        Err(e) if !wt.path.exists() => Ok(Done::Removed {
            removed: Removed {
                branch_deleted: false,
                branch_note: Some(e.to_string()),
            },
            sessions,
        }),
        Err(e) => Err(failed(e)),
    }
}

/// Moves each closed session in `files` into `dest` with omp's `/move`, one after the other.
pub fn relocate_sessions(
    ctx: &JobCtx,
    files: &[PathBuf],
    dest: &Path,
) -> Result<Done, GatewayError> {
    let omp = ctx.omp()?;
    let results = files
        .iter()
        .map(|f| {
            let r = omp.relocate_session(f, dest).map_err(|e| e.to_string());
            (f.clone(), r)
        })
        .collect();
    Ok(Done::Relocated(results))
}

pub fn omp_set(ctx: &JobCtx, key: &str, value: &serde_json::Value) -> Result<Done, GatewayError> {
    let omp = ctx.omp()?;
    let catalog = omp.load_settings().map_err(failed)?;
    let setting = catalog
        .tabs
        .iter()
        .flat_map(|t| &t.settings)
        .find(|s| s.key == key)
        .ok_or_else(|| GatewayError::Refused(format!("omp has no setting {key}")))?;
    omp.set_setting(setting, value)
        .map(Done::Setting)
        .map_err(failed)
}

pub fn omp_reset(ctx: &JobCtx, key: &str) -> Result<Done, GatewayError> {
    ctx.omp()?
        .reset_setting(key)
        .map(|()| Done::Unit)
        .map_err(failed)
}

pub fn account_policies(
    ctx: &JobCtx,
    provider: &str,
    policies: &[deck_core::usage::AccountPolicy],
) -> Result<Done, GatewayError> {
    ctx.omp()?
        .set_account_policies(provider, policies)
        .map(Done::Setting)
        .map_err(failed)
}

/// The definition directories (file reads), omp's agent settings (`omp config get`) and its
/// built-in agents (`omp agents unpack`), side by side.
fn agent_catalog(ctx: &JobCtx, projects: &[PathBuf]) -> agent_defs::Catalog {
    std::thread::scope(|s| {
        let settings = s.spawn(|| {
            ctx.omp()
                .map_err(|e| e.to_string())
                .and_then(|o| o.agent_settings().map_err(|e| e.to_string()))
        });
        let bundled = s.spawn(|| {
            ctx.omp()
                .map_err(|e| e.to_string())
                .and_then(|o| o.bundled_agent_sources().map_err(|e| e.to_string()))
        });
        let mut catalog = agent_defs::discover(&ctx.root.agent_dir(), projects);
        let bundled = bundled
            .join()
            .unwrap_or_else(|_| Err("built-in agents worker panicked".to_owned()));
        catalog.dirs.push(match bundled {
            Ok(sources) => agent_defs::bundled(&sources),
            Err(e) => agent_defs::AgentDir {
                error: Some(e),
                ..agent_defs::bundled(&[])
            },
        });
        catalog.settings = Some(
            settings
                .join()
                .unwrap_or_else(|_| Err("settings worker panicked".to_owned())),
        );
        catalog
    })
}

/// Agent file writes run one at a time, so two of them can never both pass the revision check
/// (or the exists check) of the same file.
static AGENT_WRITES: Mutex<()> = Mutex::new(());

fn agent_error(e: EditError) -> GatewayError {
    match e {
        EditError::NotAgentFile(_) | EditError::NotAgentsDir(_) => {
            GatewayError::Refused(e.to_string())
        }
        e => failed(e),
    }
}

/// One agent definition edit.
pub fn agent_edit(
    ctx: &JobCtx,
    path: &Path,
    revision: &str,
    edit: &agent_defs::Edit,
) -> Result<Done, GatewayError> {
    let _one = lock(&AGENT_WRITES);
    agent_defs::save(path, &ctx.root.agent_dir(), revision, edit)
        .map(|f| Done::AgentFile(Box::new(f)))
        .map_err(agent_error)
}

/// A new definition in `dir`.
pub fn agent_create(
    ctx: &JobCtx,
    dir: &Path,
    agent: &agent_defs::NewAgent,
) -> Result<Done, GatewayError> {
    let content = agent.content().map_err(agent_error)?;
    let _one = lock(&AGENT_WRITES);
    agent_defs::create(dir, &ctx.root.agent_dir(), &agent.name, &content)
        .map(|f| Done::AgentFile(Box::new(f)))
        .map_err(agent_error)
}

/// omp's built-in `name`, copied as is into `dir` (`<name>.md`), where it replaces the built-in.
pub fn agent_customize(ctx: &JobCtx, dir: &Path, name: &str) -> Result<Done, GatewayError> {
    let sources = ctx.omp()?.bundled_agent_sources().map_err(failed)?;
    let (_, content) = sources
        .iter()
        .find(|(file, content)| {
            agent_defs::parse(Path::new(file), content.as_bytes()).loaded_name() == Some(name)
        })
        .ok_or_else(|| GatewayError::Refused(format!("omp has no built-in agent {name}")))?;
    let _one = lock(&AGENT_WRITES);
    agent_defs::create(dir, &ctx.root.agent_dir(), name, content)
        .map(|f| Done::AgentFile(Box::new(f)))
        .map_err(agent_error)
}

/// Deletes one definition file.
pub fn agent_delete(ctx: &JobCtx, path: &Path, revision: &str) -> Result<Done, GatewayError> {
    let _one = lock(&AGENT_WRITES);
    agent_defs::delete(path, &ctx.root.agent_dir(), revision)
        .map(|()| Done::Unit)
        .map_err(agent_error)
}

pub fn copy_session(ctx: &JobCtx, file: &Path, dest_cwd: &Path) -> Result<Done, GatewayError> {
    ctx.omp()?
        .copy_session(file, dest_cwd)
        .map(Done::Path)
        .map_err(failed)
}

pub fn move_session(
    ctx: &JobCtx,
    file: &Path,
    dest_cwd: &Path,
    stop: &[String],
) -> Result<Done, GatewayError> {
    let stop = with_ctl_holders(ctx, &[file], stop);
    lifecycle::stop_then_move(ctx.omp()?, &ctx.backends, &stop, file, dest_cwd)
        .map(Done::Moved)
        .map_err(GatewayError::Failed)
}

pub fn delete_sessions(
    ctx: &JobCtx,
    targets: &[deck_core::omp::DeleteTarget],
    stop: &[String],
) -> Result<Done, GatewayError> {
    let paths: Vec<&Path> = targets.iter().map(|t| t.path.as_path()).collect();
    let stop = with_ctl_holders(ctx, &paths, stop);
    let mut results = Vec::with_capacity(targets.len());
    lifecycle::stop_then_delete(ctx.omp()?, &ctx.backends, &stop, targets, |p, r| {
        results.push((p.to_path_buf(), r.err()));
    });
    Ok(Done::Deleted(results))
}

/// How long the live sessions asked by [`with_ctl_holders`] get, together, to report their file.
const CTL_PROBE: Duration = Duration::from_secs(2);

/// `stop` plus every other live session whose omp runs one of `paths`. The caller's names come
/// from the files the session processes hold open, and omp drops that handle on every atomic
/// rewrite (e.g. the title after the first turn) until its next append: an idle session can hold
/// no handle at all, was never stopped, and omp's session lease refused the delete or move. So
/// each live session with a control endpoint that holds no session file is asked which file its
/// omp runs. Blocking (at most [`CTL_PROBE`]); runs on the job's worker.
fn with_ctl_holders(ctx: &JobCtx, paths: &[&Path], stop: &[String]) -> Vec<String> {
    let mut names = stop.to_vec();
    let mut probes: Vec<(String, CtlClient)> = Vec::new();
    for mux in ctx.backends.all() {
        let alive = mux.alive_sessions();
        if alive.is_empty() {
            continue;
        }
        let files = mux.session_files(&ctx.root.path);
        for name in alive {
            if names.contains(&name) || files.get(&name).is_some_and(|f| !f.is_empty()) {
                continue;
            }
            let dir = deck_core::ctl::ctl_dir(&ctx.ctl_ns, &name);
            if super::running::ctl_endpoint_exists(&dir) {
                probes.push((name, CtlClient::spawn(dir, || {})));
            }
        }
    }
    let wanted: Vec<PathBuf> = paths.iter().map(|p| canonical(p)).collect();
    let start = Instant::now();
    while !probes.is_empty() && start.elapsed() < CTL_PROBE {
        probes.retain(|(name, client)| {
            let Some(snap) = client.latest() else {
                return true;
            };
            if snap
                .session_file
                .as_deref()
                .is_some_and(|f| wanted.contains(&canonical(f)))
                && !names.contains(name)
            {
                names.push(name.clone());
            }
            false
        });
        if !probes.is_empty() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    names
}

/// `path` with symlinks resolved, or as given when it cannot be resolved.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn export_html(ctx: &JobCtx, file: &Path, dest: &Path) -> Result<Done, GatewayError> {
    session_actions::export_html(ctx.omp()?.program(), file, dest)
        .map(Done::Path)
        .map_err(GatewayError::Failed)
}

/// A fork of a session no gateway controller runs with live control: through the running
/// session's control socket when it has one, else headless when nothing runs it.
pub fn fork_at(
    ctx: &JobCtx,
    file: &Path,
    entry_id: &str,
    running: Option<&str>,
) -> Result<Done, GatewayError> {
    match running {
        None => ctx
            .omp()?
            .fork_at(file, entry_id)
            .map(Done::Path)
            .map_err(failed),
        Some(name) => {
            let client = CtlClient::spawn(deck_core::ctl::ctl_dir(&ctx.ctl_ns, name), || {});
            let reply = ctl_roundtrip(
                &client,
                CtlCommand::ForkAt {
                    root: file.to_path_buf(),
                    entry_id: entry_id.to_owned(),
                },
            )?;
            match (reply.ok, reply.new_file) {
                (true, Some(p)) => Ok(Done::Path(p)),
                (true, None) => Err(GatewayError::Failed(
                    "omp did not report the new session file".to_owned(),
                )),
                (false, _) => Err(GatewayError::Failed(
                    reply.error.unwrap_or_else(|| "the fork failed".to_owned()),
                )),
            }
        }
    }
}

/// Sends `cmd` once the client is connected and waits for its reply.
fn ctl_roundtrip(client: &CtlClient, cmd: CtlCommand) -> Result<CtlReply, GatewayError> {
    let start = Instant::now();
    while !client.connected() {
        if start.elapsed() > BG_CONNECT {
            return Err(GatewayError::Failed(
                "could not reach the running session's control socket".to_owned(),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let id = client.send(cmd);
    loop {
        if let Some((_, reply)) = client.take_replies().into_iter().find(|(r, _)| *r == id) {
            return Ok(reply);
        }
        if !client.connected() {
            return Err(GatewayError::Failed(
                "the control link to that session was lost".to_owned(),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---- Google Drive ------------------------------------------------------------------------------

/// Starts a sign-in: the flow stays here; the client opens `url` and then waits with
/// [`drive_login_finish`].
pub fn drive_login_begin(ctx: &JobCtx) -> Result<Done, GatewayError> {
    let flow = ctx
        .drive()?
        .begin_login()
        .map_err(|e| failed(format!("Google {e}")))?;
    let url = flow.url().to_owned();
    let handle = ctx.handles.next();
    lock(&ctx.handles.logins).insert(
        handle,
        Login {
            flow: Some(flow),
            cancel: Arc::new(AtomicBool::new(false)),
        },
    );
    Ok(Done::DriveLogin { handle, url })
}

pub fn drive_login_finish(ctx: &JobCtx, handle: u64) -> Result<Done, GatewayError> {
    let service = ctx.drive()?;
    let (flow, cancel) = {
        let mut logins = lock(&ctx.handles.logins);
        let login = logins
            .get_mut(&handle)
            .ok_or_else(|| GatewayError::Refused("that sign-in is gone".to_owned()))?;
        let flow = login
            .flow
            .take()
            .ok_or_else(|| GatewayError::Refused("that sign-in is already waiting".to_owned()))?;
        (flow, login.cancel.clone())
    };
    let result = service.finish_login(flow, &cancel, LOGIN_TIMEOUT);
    lock(&ctx.handles.logins).remove(&handle);
    match result {
        Ok(account) => Ok(Done::DriveAccount {
            email: account.email,
        }),
        Err(backup::Error::Cancelled) => Err(GatewayError::Cancelled),
        Err(e) => Err(failed(format!("Google {e}"))),
    }
}

pub fn drive_sign_out(ctx: &JobCtx) -> Result<Done, GatewayError> {
    ctx.drive()?.sign_out().map(|()| Done::Unit).map_err(failed)
}

pub fn backup(ctx: &JobCtx, force: bool) -> Result<Done, GatewayError> {
    ctx.backup().backup(force).map(Done::Backup).map_err(failed)
}

pub fn prepare_restore(
    ctx: &JobCtx,
    client: &str,
    file: &backup::BackupFile,
) -> Result<Done, GatewayError> {
    let prepared = ctx.backup().prepare_restore(file).map_err(failed)?;
    let plan = plan_view(&prepared);
    let handle = ctx.handles.next();
    lock(&ctx.handles.restores).insert(handle, (client.to_owned(), prepared));
    Ok(Done::Prepared { handle, plan })
}

pub fn apply_restore(ctx: &JobCtx, client: &str, handle: u64) -> Result<Done, GatewayError> {
    let prepared = take_own(&ctx.handles.restores, handle, client)?;
    ctx.backup()
        .apply_restore(&prepared)
        .map(Done::Restored)
        .map_err(failed)
}

pub fn delete_backup(ctx: &JobCtx, file: &backup::BackupFile) -> Result<Done, GatewayError> {
    ctx.backup()
        .delete_backup(file)
        .map(|()| Done::Unit)
        .map_err(failed)
}

/// Deletes config backups in order, stopping at the first failure.
pub fn delete_backups(ctx: &JobCtx, files: &[backup::BackupFile]) -> Result<Done, GatewayError> {
    let service = ctx.backup();
    delete_each(files, |f| f.id.as_str(), |f| service.delete_backup(f))
}

/// Deletes project backups in order, stopping at the first failure.
pub fn project_delete_backups(
    ctx: &JobCtx,
    backups: &[backup::ProjectBackup],
) -> Result<Done, GatewayError> {
    let service = ctx.backup();
    delete_each(
        backups,
        |b| b.file.id.as_str(),
        |b| service.delete_project_backup(b),
    )
}

/// Runs `delete` over `items` in order: `Unit` when every one was deleted, else a failure
/// naming how many (and which ids) were deleted before the error.
fn delete_each<T, E: std::fmt::Display>(
    items: &[T],
    id: impl Fn(&T) -> &str,
    mut delete: impl FnMut(&T) -> Result<(), E>,
) -> Result<Done, GatewayError> {
    for (n, item) in items.iter().enumerate() {
        if let Err(e) = delete(item) {
            let deleted = &items[..n];
            let message = if deleted.is_empty() {
                format!("no backup deleted: {e}")
            } else {
                let ids: Vec<&str> = deleted.iter().map(&id).collect();
                format!(
                    "deleted {n} of {} backups ({}) before an error: {e}",
                    items.len(),
                    ids.join(", ")
                )
            };
            return Err(GatewayError::Failed(message));
        }
    }
    Ok(Done::Unit)
}

/// Takes the prepared restore `handle` out of `restores` when `client` prepared it.
fn take_own<T>(
    restores: &Mutex<HashMap<u64, (String, T)>>,
    handle: u64,
    client: &str,
) -> Result<T, GatewayError> {
    let mut restores = lock(restores);
    match restores.get(&handle) {
        None => Err(GatewayError::Refused(
            "that restore is gone; prepare it again".to_owned(),
        )),
        Some((owner, _)) if owner != client => {
            Err(GatewayError::Refused(NOT_YOUR_RESTORE.to_owned()))
        }
        Some(_) => restores
            .remove(&handle)
            .map(|(_, p)| p)
            .ok_or_else(|| GatewayError::Refused("that restore is gone".to_owned())),
    }
}

/// The refusal for a restore handle used by another client than the one that prepared it.
pub const NOT_YOUR_RESTORE: &str = "that restore was prepared by another client";

// ---- Backup server ------------------------------------------------------------------------------

/// Checks and saves the backup server. An unknown or changed identity is an answer the client
/// acts on (trust prompt, "remove and add again"), not a failure.
pub fn server_connect(
    ctx: &JobCtx,
    mut config: ServerConfig,
    keep_secret: bool,
) -> Result<Done, GatewayError> {
    if let Some(saved) = ctx.service.server_account().map_err(failed)? {
        inherit(&mut config, &saved.config, keep_secret);
    }
    let outcome = match ctx.service.server_connect(config) {
        Ok(account) => ServerConnectOutcome::Connected(ServerView::of(&account)),
        Err(backup::Error::UntrustedServer { fingerprint }) => {
            ServerConnectOutcome::Untrusted { fingerprint }
        }
        Err(backup::Error::ServerChanged { pinned, presented }) => {
            ServerConnectOutcome::Changed { pinned, presented }
        }
        Err(e) => return Err(failed(e)),
    };
    Ok(Done::ServerConnect(outcome))
}

/// An edit of the saved server at the same address keeps its pinned identity and, with
/// `keep_secret`, the saved password / key passphrase of the same login when none was given.
/// Nothing is carried to another protocol, host or port.
fn inherit(config: &mut ServerConfig, saved: &ServerConfig, keep_secret: bool) {
    let Ok(n) = config.clone().normalized() else {
        return;
    };
    if n.protocol != saved.protocol
        || !n.host.eq_ignore_ascii_case(&saved.host)
        || n.effective_port() != saved.effective_port()
    {
        return;
    }
    if config.pinned.is_none() {
        config.pinned.clone_from(&saved.pinned);
    }
    if !keep_secret {
        return;
    }
    match (&mut config.auth, &saved.auth) {
        (
            ServerAuth::Password { user, password },
            ServerAuth::Password {
                user: was,
                password: secret,
            },
        ) if password.is_empty() && user.trim() == was => password.clone_from(secret),
        (
            ServerAuth::Key {
                user, passphrase, ..
            },
            ServerAuth::Key {
                user: was,
                passphrase: secret,
                ..
            },
        ) if passphrase.is_none() && user.trim() == was => passphrase.clone_from(secret),
        _ => {}
    }
}

pub fn server_forget(ctx: &JobCtx) -> Result<Done, GatewayError> {
    ctx.service
        .server_forget()
        .map(|()| Done::Unit)
        .map_err(failed)
}

fn progress_of(p: backup::Progress) -> Progress {
    Progress {
        stage: format!("{:?}", p.stage),
        done: p.done,
        total: Some(p.total),
    }
}

pub fn project_backup(
    ctx: &JobCtx,
    project: &Path,
    parts: backup::Parts,
    excluded: &std::collections::BTreeSet<String>,
    force: bool,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<Done, GatewayError> {
    ctx.backup()
        .backup_project(&ctx.root, project, parts, excluded, force, cancel, &|p| {
            progress(progress_of(p));
        })
        .map(Done::ProjectBackup)
        .map_err(failed)
}

pub fn project_delete_backup(
    ctx: &JobCtx,
    backup: &backup::ProjectBackup,
) -> Result<Done, GatewayError> {
    ctx.backup()
        .delete_project_backup(backup)
        .map(|()| Done::Unit)
        .map_err(failed)
}

pub fn project_prepare_restore(
    ctx: &JobCtx,
    backup: &backup::ProjectBackup,
    client: &str,
    target: &Path,
    parts: backup::Parts,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<Done, GatewayError> {
    let prepared = ctx
        .backup()
        .prepare_project_restore(backup, target, parts, &ctx.root, cancel, &|p| {
            progress(progress_of(p));
        })
        .map_err(failed)?;
    let plan = prepared.plan.clone();
    let handle = ctx.handles.next();
    lock(&ctx.handles.project_restores).insert(handle, (client.to_owned(), prepared));
    Ok(Done::ProjectPrepared { handle, plan })
}

/// A device's prepared restore still targets a folder it may use (checked on the worker that
/// applies it: the folder could have been swapped for a link since it was prepared).
pub fn admit_restore_target(
    ctx: &JobCtx,
    client: &str,
    handle: u64,
    confine: &Confine,
) -> Result<(), GatewayError> {
    let target = lock(&ctx.handles.project_restores)
        .get(&handle)
        .filter(|(owner, _)| owner == client)
        .map(|(_, p)| p.plan.target.clone());
    match target {
        Some(t) => confine.admit(&t).map(drop),
        // Missing or foreign: the apply answers that itself.
        None => Ok(()),
    }
}

/// What a project restore's apply decides: the per-session `conflicts` choices, the backed-up
/// sessions `excluded` from the restore, and the sessions to `stop` (those running the local
/// copies the restore overwrites).
pub struct RestoreChoices<'a> {
    pub conflicts: &'a std::collections::BTreeMap<String, backup::ConflictChoice>,
    pub excluded: &'a std::collections::BTreeSet<String>,
    pub stop: &'a [String],
}

/// Kills the backend sessions in `choices.stop`, then applies `client`'s prepared restore with
/// `choices`, leaving the excluded sessions out.
pub fn project_apply_restore(
    ctx: &JobCtx,
    client: &str,
    handle: u64,
    choices: &RestoreChoices<'_>,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<Done, GatewayError> {
    let service = ctx.backup();
    let mut prepared = take_own(&ctx.handles.project_restores, handle, client)?;
    prepared.leave_out_sessions(choices.excluded);
    lifecycle::kill_all(&ctx.backends, choices.stop, "restore");
    service
        .apply_project_restore(
            prepared,
            choices.conflicts,
            ctx.omp.as_ref(),
            cancel,
            &|p| {
                progress(progress_of(p));
            },
        )
        .map(Done::ProjectRestored)
        .map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_core::omp::{Setting, SettingKind, SettingsTab};

    #[cfg(unix)]
    #[test]
    fn a_device_folder_counts_where_it_leads_and_is_handed_on_resolved() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("temp dir");
        let base = resolved(tmp.path()).expect("temp dir resolves");
        let (home, outside) = (base.join("home"), base.join("outside"));
        for d in [home.join("proj/src"), outside.join("secret")] {
            std::fs::create_dir_all(d).expect("dir");
        }
        std::fs::write(home.join("notes.txt"), b"x").expect("file");
        symlink(&outside, home.join("escape")).expect("escape link");
        symlink(home.join("proj"), home.join("alias")).expect("alias link");
        symlink(base.join("nowhere"), home.join("dangling")).expect("dangling link");
        // A root reached through a symlink is compared by where it leads too.
        symlink(&home, base.join("home-link")).expect("root link");
        let confine = Confine::new(vec![base.join("home-link"), base.join("missing")]);
        let roots = confine.resolve();
        // Admitted folders come back resolved: the use cannot be redirected by the link.
        assert_eq!(roots.admit(&home), Some(home.clone()));
        assert_eq!(
            roots.admit(&home.join("alias/src")),
            Some(home.join("proj/src"))
        );
        assert_eq!(
            roots.admit(&base.join("home-link/proj")),
            Some(home.join("proj"))
        );
        // A missing tail below a resolved folder (a restore target to create) stays inside.
        assert_eq!(
            roots.admit(&home.join("new/deeper")),
            Some(home.join("new/deeper"))
        );
        for out in [
            home.join("escape"),
            home.join("escape/secret"),
            home.join("escape/new"),
            base.join("home-link/escape"),
            outside.clone(),
            home.join("dangling"),
            home.join("dangling/new"),
            // Nothing can be created below a file.
            home.join("notes.txt/new"),
            home.join("notes.txt/new/deeper"),
            home.join("new/../../outside"),
        ] {
            assert_eq!(roots.admit(&out), None, "{}", out.display());
        }
        assert!(matches!(
            confine.admit(&home.join("escape")),
            Err(GatewayError::Refused(_))
        ));
        assert_eq!(confine.admit(&home).ok(), Some(home));
    }

    #[test]
    fn a_secret_value_never_leaves_the_gateway() {
        let setting = |key: &str, secret: bool, value: &str| Setting {
            key: key.to_owned(),
            kind: SettingKind::String,
            value: Some(serde_json::Value::String(value.to_owned())),
            default: None,
            description: String::new(),
            secret,
        };
        let catalog = SettingsCatalog {
            tabs: vec![SettingsTab {
                id: "t".into(),
                label: "T".into(),
                settings: vec![
                    setting("api", true, "sk-live-123"),
                    setting("theme", false, "dark"),
                ],
            }],
        };
        let out = sanitize(catalog);
        let text = serde_json::to_string(&out).expect("json");
        assert!(!text.contains("sk-live-123"));
        assert_eq!(
            out.tabs[0].settings[0].value,
            Some(serde_json::Value::String(SECRET_PLACEHOLDER.to_owned()))
        );
        assert_eq!(
            out.tabs[0].settings[1].value,
            Some(serde_json::Value::String("dark".to_owned()))
        );
    }

    #[test]
    fn bulk_deletes_run_in_order_and_stop_at_the_first_failure() {
        let ids = ["a", "b", "c", "d"];
        let mut tried = Vec::new();
        let all = delete_each(
            &ids,
            |s| *s,
            |s| {
                tried.push(*s);
                Ok::<(), String>(())
            },
        );
        assert!(matches!(all, Ok(Done::Unit)));
        assert_eq!(tried, ids);

        let mut tried = Vec::new();
        let partial = delete_each(
            &ids,
            |s| *s,
            |s| {
                tried.push(*s);
                if *s == "c" { Err("HTTP 500") } else { Ok(()) }
            },
        );
        assert_eq!(tried, ["a", "b", "c"], "nothing after the failure is tried");
        match partial {
            Err(GatewayError::Failed(m)) => {
                assert_eq!(m, "deleted 2 of 4 backups (a, b) before an error: HTTP 500");
            }
            other => panic!("{other:?}"),
        }

        match delete_each(&ids, |s| *s, |_| Err("offline")) {
            Err(GatewayError::Failed(m)) => assert_eq!(m, "no backup deleted: offline"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_edit_keeps_pin_and_secret_only_for_the_same_server_and_login() {
        use deck_core::backup::server::Protocol;
        let password = |user: &str, pw: &str| ServerAuth::Password {
            user: user.into(),
            password: pw.into(),
        };
        let saved = ServerConfig {
            protocol: Protocol::Ftps,
            host: "nas.local".into(),
            port: 21,
            auth: password("me", "secret"),
            folder: "omp-deck-backups".into(),
            pinned: Some("SHA256:AA".into()),
        };
        let edit = |host: &str, auth: ServerAuth| ServerConfig {
            protocol: Protocol::Ftps,
            host: host.into(),
            port: 0,
            auth,
            folder: "other".into(),
            pinned: None,
        };

        let mut same = edit(" NAS.local ", password(" me ", ""));
        inherit(&mut same, &saved, true);
        assert_eq!(same.pinned.as_deref(), Some("SHA256:AA"));
        assert_eq!(same.auth, password(" me ", "secret"));

        let mut no_keep = edit("nas.local", password("me", ""));
        inherit(&mut no_keep, &saved, false);
        assert_eq!(no_keep.pinned.as_deref(), Some("SHA256:AA"));
        assert_eq!(no_keep.auth, password("me", ""));

        let mut other_user = edit("nas.local", password("you", ""));
        inherit(&mut other_user, &saved, true);
        assert_eq!(other_user.auth, password("you", ""));

        let mut other_host = edit("elsewhere", password("me", ""));
        inherit(&mut other_host, &saved, true);
        assert_eq!(other_host.pinned, None);
        assert_eq!(other_host.auth, password("me", ""));
    }
}
