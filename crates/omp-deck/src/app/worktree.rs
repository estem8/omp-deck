//! App-side glue for git worktree sessions: the new-worktree and move dialogs, the confirmations,
//! and the tab bookkeeping that follows a session moving into (or out of) a worktree. The flows
//! themselves — creating, moving a session in, finishing, removing or parking, continuing a
//! parked branch — run in the gateway (`Mutation::Worktree*`, `Topic::Worktrees`), so the phone
//! drives the same logic; this module only asks, shows what the gateway reports and updates the
//! tabs of this window.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use deck_core::config::ParkedBranch;
use deck_core::sessions::{SessionEntry, abbreviate_home};
use deck_core::worktree::{Removed, Worktree, WorktreeStatus};
use deck_service::controller::{RequestKey, SessionTarget};
use deck_service::gateway::protocol::{
    Answer, ContinueIn, Done, Mutation, Progress, Query, Retirement, Split, Topic, TopicValue,
    WorktreesView,
};

use super::desktop::DesktopOpen;
use super::{App, PendingConfirm};
use crate::confirm::Confirm;
use crate::tabs::TabRole;
use crate::transfer;
use crate::worktree_dialog::{DestAsk, Kind, LocationRes, Outcome, Plan, WorktreeDialog, brief};
use crate::worktree_ui::{self, ParkState};

/// A move that was typed into a tab: the session reappears in the worktree omp creates.
const MOVE_EXPIRES: Duration = Duration::from_secs(10 * 60);

/// How long a continued session waits for its controller record before it is reported.
const CONTINUE_WAIT: Duration = Duration::from_secs(10);

struct PendingMove {
    uid: u64,
    since: Instant,
}

pub(super) struct WtUi {
    pub(super) dialog: Option<WorktreeDialog>,
    /// omp's `worktree.base` setting, once the settings catalog was read.
    moves: Vec<PendingMove>,
    /// Each tab's session file as of the last relocation check.
    seen: HashMap<u64, Option<PathBuf>>,
    /// Each tab's live folder as of the last relocation check.
    seen_cwd: HashMap<u64, Option<PathBuf>>,
    /// Sessions being moved into their primary by an open.
    in_flight: HashSet<PathBuf>,
    /// The last unresolved lookup per tab (file, scan generation): not repeated until the scan changes.
    tried: HashMap<u64, (PathBuf, u64)>,
    /// What the gateway reports of each repository that has linked worktrees or parked branches
    /// (by primary), with the topic revision it was read at.
    views: HashMap<PathBuf, (u64, WorktreesView)>,
    /// Worktrees (by folder) a finish flow runs on: the sidebar shows "Finishing…". The value is
    /// whether this window started the job (and so can cancel it).
    pub(super) finishing: HashMap<PathBuf, bool>,
    /// Where each parked branch is checked out (as the gateway last looked).
    pub(super) park_states: Vec<ParkState>,
    /// Worktrees last seen clean and pushed but not merged: their remove action parks.
    pub(super) parkable: HashSet<PathBuf>,
    /// Finish jobs this window started (by worktree folder), for an instant cancel.
    finish_jobs: HashMap<PathBuf, RequestKey>,
    /// The last progress text shown per worktree: a repeat is not toasted again.
    last_stage: HashMap<PathBuf, String>,
    /// Worktrees whose removal or park this window asked for and has no answer to yet.
    removing: HashSet<PathBuf>,
    /// Parked branches (primary, branch) with a continue in progress.
    continuing: HashSet<(PathBuf, String)>,
    /// Continued sessions whose desktop tab waits for the controller record, since when.
    continued: Vec<(SessionTarget, Instant)>,
}

impl WtUi {
    pub(super) fn new() -> WtUi {
        WtUi {
            dialog: None,
            moves: Vec::new(),
            seen: HashMap::new(),
            seen_cwd: HashMap::new(),
            in_flight: HashSet::new(),
            tried: HashMap::new(),
            views: HashMap::new(),
            finishing: HashMap::new(),
            continued: Vec::new(),
            park_states: Vec::new(),
            parkable: HashSet::new(),
            finish_jobs: HashMap::new(),
            last_stage: HashMap::new(),
            removing: HashSet::new(),
            continuing: HashSet::new(),
        }
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The folder a tab's omp runs in now (the gateway's record), else the one it was launched in.
fn tab_dir(t: &crate::tabs::Tab) -> PathBuf {
    t.info()
        .and_then(|i| i.live_cwd.clone())
        .unwrap_or_else(|| t.cwd.clone())
}

/// The reply a query is expected to carry, or why not.
fn unexpected<T>(
    r: Result<Answer, String>,
    pick: impl FnOnce(Answer) -> Option<T>,
) -> Result<T, String> {
    match r {
        Ok(a) => pick(a).ok_or_else(|| "the gateway answered with something else".to_owned()),
        Err(e) => Err(e),
    }
}

impl App {
    // ------------------------------------------------------------ dialogs

    /// "New session in worktree…" for the project in `dir`.
    pub(super) fn open_new_worktree(&mut self, dir: PathBuf) {
        if !self.omp_found() || self.wt.dialog.is_some() {
            return;
        }
        self.start_dialog(Kind::New { dir });
    }

    /// "Move to new worktree…" for the session open in a tab.
    pub(super) fn open_move_to_worktree(&mut self, entry: &SessionEntry) {
        if !self.omp_found() || self.wt.dialog.is_some() {
            return;
        }
        let tab = self.tabs.iter().find(|t| {
            t.session_path()
                .is_some_and(|p| worktree_ui::same_session(p, &entry.path))
        });
        let Some(tab) = tab.filter(|t| !t.terminal().is_exited() && t.role == TabRole::Session)
        else {
            self.toasts
                .info(&self.ctx, "Open the session in a tab first");
            return;
        };
        self.start_dialog(Kind::Move {
            uid: tab.uid,
            target: tab.target().clone(),
            dir: tab.cwd.clone(),
            title: entry.label().to_owned(),
        });
    }

    fn start_dialog(&mut self, kind: Kind) {
        let dialog = WorktreeDialog::new(kind, unix_secs());
        let dir = dialog.dir().to_path_buf();
        self.wt.dialog = Some(dialog);
        self.gw.query(Query::RepoInfo(dir), |app, r| {
            let info = unexpected(r, |a| match a {
                Answer::RepoInfo(i) => Some(i),
                _ => None,
            });
            if let Some(d) = &mut app.wt.dialog {
                d.on_repo(info);
            }
        });
    }

    /// Starts one dialog check: the gateway judges the branch name and the destination folder.
    fn run_check(&mut self, req: crate::worktree_dialog::CheckReq) {
        let generation = req.generation;
        let branch = req.branch().to_owned();
        if branch.is_empty() {
            if let Some(d) = &mut self.wt.dialog {
                d.apply_branch(generation, Err("Enter a branch name".to_owned()));
            }
        } else {
            let repo = req.dir().to_path_buf();
            self.gw
                .query(Query::CheckBranch { repo, name: branch }, move |app, r| {
                    let verdict = unexpected(r, |a| match a {
                        Answer::CheckBranch => Some(()),
                        _ => None,
                    })
                    .map_err(|e| brief(&e));
                    if let Some(d) = &mut app.wt.dialog {
                        d.apply_branch(generation, verdict);
                    }
                });
        }
        match req.destination() {
            DestAsk::Nothing => {
                if let Some(d) = &mut self.wt.dialog {
                    d.apply_location(generation, LocationRes::none());
                }
            }
            DestAsk::Rejected(msg) => {
                if let Some(d) = &mut self.wt.dialog {
                    d.apply_location(generation, LocationRes::rejected(msg));
                }
            }
            DestAsk::Gateway { dest, auto } => {
                let query = Query::WorktreeDestination {
                    repo: req.dir().to_path_buf(),
                    branch: req.leaf().to_owned(),
                    dest,
                };
                self.gw.query(query, move |app, r| {
                    let verdict = unexpected(r, |a| match a {
                        Answer::WorktreeDestination(v) => Some(v),
                        _ => None,
                    })
                    .map_err(|e| brief(&e));
                    if let Some(d) = &mut app.wt.dialog {
                        d.apply_location(generation, LocationRes::answered(auto, verdict));
                    }
                });
            }
        }
    }

    pub(super) fn show_worktree_dialog(&mut self, ctx: &egui::Context) {
        let Some(dlg) = self.wt.dialog.as_mut() else {
            return;
        };
        let req = dlg.take_check(self.home.as_deref());
        if let Some(due) = dlg.check_due_in() {
            ctx.request_repaint_after(due);
        }
        if let Some(req) = req {
            self.run_check(req);
        }
        let Some(dlg) = self.wt.dialog.as_mut() else {
            return;
        };
        match dlg.show(ctx, self.picker_available) {
            Outcome::Open => {}
            Outcome::Cancel => {
                self.wt.dialog = None;
                self.focus_pending = true;
            }
            Outcome::Browse => {
                dlg.browsing = true;
                let start = dlg.start_dir(self.home.as_deref());
                self.gw.native(
                    move || transfer::pick_folder(&start),
                    |app, path| {
                        if let Some(d) = &mut app.wt.dialog {
                            d.browsing = false;
                            if let Some(p) = path {
                                d.picked(&p);
                            }
                        }
                    },
                );
            }
            Outcome::Confirm(plan) => {
                self.wt.dialog = None;
                self.focus_pending = true;
                self.run_plan(plan);
            }
        }
    }

    fn run_plan(&mut self, plan: Plan) {
        match plan {
            Plan::New {
                repo,
                branch,
                base_ref,
                dest,
            } => {
                if !self.omp_found() {
                    return;
                }
                self.toasts
                    .info(&self.ctx, format!("Creating worktree “{branch}”…"));
                // The gateway makes the worktree (at its managed default location when `dest` is
                // `None`) and records the merge target; the tab opens here.
                let op = Mutation::WorktreeNew {
                    repo,
                    branch,
                    base_ref: Some(base_ref),
                    dest,
                    open: false,
                };
                self.gw.mutate(op, |app, r| match r {
                    Ok(Done::Path(dest)) => {
                        app.refresh_sessions();
                        app.new_session(dest);
                    }
                    Ok(other) => app.toasts.error(
                        &app.ctx,
                        format!("Could not create the worktree: unexpected answer {other:?}"),
                    ),
                    Err(e) => app.toasts.error(&app.ctx, brief(&e)),
                });
            }
            Plan::Move {
                uid,
                target,
                branch,
            } => {
                let open = self
                    .tabs
                    .iter()
                    .any(|t| t.uid == uid && !t.terminal().is_exited());
                if !open {
                    self.toasts
                        .error(&self.ctx, "The session's tab was closed — nothing moved");
                    return;
                }
                // Bound to the session the dialog was opened on: if it moved on to another
                // conversation meanwhile, the gateway refuses and nothing is typed. The move is
                // only tracked once the gateway accepted the command.
                let op = Mutation::WorktreeMoveSession { target, branch };
                self.gw.mutate(op, move |app, r| match r {
                    Ok(_) => app.wt.moves.push(PendingMove {
                        uid,
                        since: Instant::now(),
                    }),
                    Err(e) => app.toasts.error(
                        &app.ctx,
                        format!("Could not move the session: {}", brief(&e)),
                    ),
                });
            }
        }
    }

    // ------------------------------------------------------------ gateway state

    /// Follows `Topic::Worktrees` of every repository that has linked worktrees or parked
    /// branches in this window's lists, and derives what the sidebar shows from it. Also keeps the
    /// tab bookkeeping of sessions that moved.
    pub(super) fn drive_worktrees(&mut self) {
        self.expire_moves();
        self.track_relocations();
        self.follow_worktrees();
        self.open_queued_continues();
    }

    fn follow_worktrees(&mut self) {
        let mut wanted: HashSet<PathBuf> = self
            .cfg
            .parked_branches
            .iter()
            .map(|r| r.primary.clone())
            .collect();
        if let Some(groups) = self.groups() {
            wanted.extend(
                groups
                    .iter()
                    .filter_map(|g| g.worktree.as_ref())
                    .map(|w| w.primary.clone()),
            );
        }
        let mut changed = false;
        let gone: Vec<PathBuf> = self
            .wt
            .views
            .keys()
            .filter(|p| !wanted.contains(*p))
            .cloned()
            .collect();
        for p in gone {
            self.gw.unsubscribe(&Topic::Worktrees(p.clone()));
            self.wt.views.remove(&p);
            changed = true;
        }
        for primary in wanted {
            let topic = Topic::Worktrees(primary.clone());
            if !self.wt.views.contains_key(&primary) {
                self.gw.subscribe(topic.clone());
            }
            let revision = self.gw.revision(&topic);
            let known = self.wt.views.get(&primary).map(|(r, _)| *r);
            if known == Some(revision) {
                continue;
            }
            let Some(TopicValue::Worktrees(view)) = self.gw.topic(&topic) else {
                continue;
            };
            self.wt.views.insert(primary, (revision, *view));
            changed = true;
        }
        if !changed {
            return;
        }
        self.wt.finishing.clear();
        self.wt.parkable.clear();
        self.wt.park_states.clear();
        for (_, view) in self.wt.views.values() {
            for e in &view.linked {
                if let Some(deck_service::gateway::protocol::WorktreeFlow::Finishing {
                    job, ..
                }) = &e.flow
                {
                    // Only a job this window sent can be cancelled from here: the gateway
                    // refuses another client's job.
                    let own = self.wt.finish_jobs.get(&e.worktree.path) == Some(job);
                    self.wt.finishing.insert(e.worktree.path.clone(), own);
                }
                if e.parkable {
                    self.wt.parkable.insert(e.worktree.path.clone());
                }
            }
            for p in &view.parked {
                if let Some(state) = &p.state {
                    self.wt.park_states.push(ParkState {
                        primary: p.record.primary.clone(),
                        branch: p.record.branch.clone(),
                        checked_out: state.checked_out.clone(),
                    });
                }
            }
        }
        self.ctx.request_repaint();
    }

    /// The merge target the gateway knows for `wt`.
    fn base_of(&self, wt: &Worktree) -> Option<String> {
        self.wt
            .views
            .get(&wt.primary)?
            .1
            .linked
            .iter()
            .find(|e| e.worktree.path == wt.path)?
            .base
            .clone()
    }

    /// The finish job this window started on `path`: only its own jobs can be cancelled from
    /// here (the gateway refuses another client's).
    fn finish_job(&self, path: &Path) -> Option<RequestKey> {
        self.wt.finish_jobs.get(path).cloned()
    }

    // ------------------------------------------------------------ finishing

    pub(super) fn request_finish(&mut self, wt: Worktree) {
        if self.wt.finishing.contains_key(&wt.path)
            || self.wt.finish_jobs.contains_key(&wt.path)
            || self.wt.removing.contains(&wt.path)
        {
            return;
        }
        let label = worktree_ui::worktree_label(&wt);
        let into = self
            .base_of(&wt)
            .map(|b| format!(" into {b}"))
            .unwrap_or_default();
        self.toasts.info(
            &self.ctx,
            format!(
                "Finishing “{label}”: the session is asked to commit and merge{into}; the worktree is removed once that is done"
            ),
        );
        let path = wt.path.clone();
        let progress_path = wt.path.clone();
        let tracked = wt.path.clone();
        let op = Mutation::WorktreeFinish {
            worktree: wt.clone(),
        };
        let key = self.gw.mutate_with_progress(
            op,
            move |app, p: &Progress| {
                if app.wt.last_stage.get(&progress_path) != Some(&p.stage) {
                    app.wt
                        .last_stage
                        .insert(progress_path.clone(), p.stage.clone());
                    app.toasts.info(&app.ctx, p.stage.clone());
                }
            },
            move |app, r| {
                app.wt.finish_jobs.remove(&path);
                app.wt.last_stage.remove(&path);
                match r {
                    // Stopped by the user (toasted when asked) or by another client.
                    Err(e) if e == "cancelled" => {}
                    // The gateway words its refusals and failures for the user.
                    Err(e) => app.toasts.error(&app.ctx, e),
                    Ok(done) => app.on_retired(&wt, Ok(done)),
                }
            },
        );
        self.wt.finish_jobs.insert(tracked, key);
    }

    pub(super) fn cancel_finish(&mut self, path: &Path) {
        let Some(job) = self.finish_job(path) else {
            if self.wt.finishing.contains_key(path) {
                self.toasts.info(
                    &self.ctx,
                    "Another device or window started this finish; stop it there",
                );
            }
            return;
        };
        self.gw.cancel(job, |app, r| match r {
            Ok(()) => app
                .toasts
                .info(&app.ctx, "Stopped waiting — the worktree stays"),
            Err(e) => app.toasts.error(&app.ctx, e),
        });
    }

    // ------------------------------------------------------------ removing

    /// "Remove worktree…": the gateway says whether it can be removed (clean and merged) or
    /// parked (clean and pushed), else why not; the confirmation names what will happen.
    pub(super) fn request_remove(&mut self, wt: Worktree) {
        if self.wt.removing.contains(&wt.path) {
            return;
        }
        let asked = wt.clone();
        self.gw.query(Query::WorktreeVerdict(asked), move |app, r| {
            let verdict = unexpected(r, |a| match a {
                Answer::WorktreeVerdict(v) => Some(v),
                _ => None,
            });
            match verdict {
                Ok(v) => app.confirm_retire(wt, &v.base, &v.status, v.verdict),
                Err(e) => {
                    let label = worktree_ui::worktree_label(&wt);
                    app.toasts
                        .error(&app.ctx, format!("Worktree “{label}”: {}", brief(&e)));
                }
            }
        });
    }

    fn confirm_retire(
        &mut self,
        wt: Worktree,
        base: &str,
        status: &WorktreeStatus,
        verdict: Retirement,
    ) {
        let label = worktree_ui::worktree_label(&wt);
        match verdict {
            Retirement::Blocked(why) => self.toasts.error(&self.ctx, why),
            Retirement::Remove => {
                self.confirm = Some((
                    Confirm {
                        title: "Remove worktree?".to_owned(),
                        body: format!(
                            "{} is clean and merged into {base}.\n\nIts sessions move into {}. Tabs running in {} move there too once omp is idle and keep running; a tab still busy after 30 s is stopped. This deletes {}.",
                            match &wt.branch {
                                Some(_) => format!("Branch “{label}”"),
                                None => format!("The detached worktree “{label}”"),
                            },
                            abbreviate_home(&wt.primary),
                            wt.path.display(),
                            if wt.branch.is_some() {
                                "the folder and the branch"
                            } else {
                                "the folder"
                            }
                        ),
                        confirm_label: "Remove".to_owned(),
                        danger: true,
                    },
                    PendingConfirm::RemoveWorktree { wt, park: false },
                ));
            }
            Retirement::Park => {
                let branch = wt.branch.clone().unwrap_or_default();
                let upstream = status
                    .upstream
                    .clone()
                    .unwrap_or_else(|| "its upstream".to_owned());
                self.confirm = Some((
                    Confirm {
                        title: "Park worktree?".to_owned(),
                        body: format!(
                            "Branch “{branch}” is clean and pushed to {upstream}, but not merged into {base} yet (for example, its PR is still open).\n\nThe local branch is kept and never deleted by OmpDeck — delete it by hand when you are done. Sessions move into {}; tabs running in {} move there too once omp is idle and keep running (a tab still busy after 30 s is stopped). This removes the folder {}. Continue later from a session's menu.",
                            abbreviate_home(&wt.primary),
                            wt.path.display(),
                            wt.path.display()
                        ),
                        confirm_label: "Park".to_owned(),
                        danger: false,
                    },
                    PendingConfirm::RemoveWorktree { wt, park: true },
                ));
            }
        }
    }

    /// The user confirmed: the gateway moves the running sessions into the primary checkout,
    /// stops the rest, moves every session file and removes (or parks) the worktree.
    pub(super) fn execute_remove(&mut self, wt: Worktree, park: bool) {
        if !self.wt.removing.insert(wt.path.clone()) {
            return;
        }
        let inside = self.tabs.iter().any(|t| {
            t.role == TabRole::Session
                && !t.terminal().is_exited()
                && worktree_ui::inside(&wt, &tab_dir(t))
        });
        if inside {
            self.toasts.info(
                &self.ctx,
                format!(
                    "Moving the open sessions of “{}” into {} once omp is idle (a tab still busy after 30 s is stopped)…",
                    worktree_ui::worktree_label(&wt),
                    abbreviate_home(&wt.primary)
                ),
            );
        }
        let op = Mutation::WorktreeRetire {
            worktree: wt.clone(),
            park,
        };
        self.gw.mutate(op, move |app, r| {
            app.wt.removing.remove(&wt.path);
            app.on_retired(&wt, r);
        });
    }

    /// The gateway removed or parked `wt` (a retire, or a finish that reached the removal).
    fn on_retired(&mut self, wt: &Worktree, result: Result<Done, String>) {
        let label = worktree_ui::worktree_label(wt);
        let (removed, moved, parked): (Removed, u32, bool) = match result {
            Ok(Done::Retired {
                removed,
                moved,
                parked,
                ..
            }) => (removed, moved, parked),
            Ok(other) => {
                self.toasts.error(
                    &self.ctx,
                    format!("Could not remove “{label}”: unexpected answer {other:?}"),
                );
                return;
            }
            // The gateway words its failures for the user ("Could not remove “x”: …").
            Err(e) => {
                self.toasts.error(&self.ctx, e);
                self.refresh_sessions();
                return;
            }
        };
        let mut text = if parked {
            format!(
                "Parked “{label}”: the folder is removed, the branch is kept (OmpDeck never deletes it)"
            )
        } else {
            worktree_ui::removal_text(&label, wt.branch.as_deref(), &removed)
        };
        if moved > 0 {
            text += &format!(
                "; {moved} running session{} moved into {}",
                if moved == 1 { "" } else { "s" },
                abbreviate_home(&wt.primary)
            );
        }
        if !parked
            && removed.branch_note.is_some()
            && wt.branch.is_some()
            && !removed.branch_deleted
        {
            self.toasts.info(&self.ctx, text);
        } else {
            self.toasts.success(&self.ctx, text);
        }
        // Tabs of this window: the ones that kept running now live in the primary checkout; the
        // ones the gateway stopped are closed.
        for i in (0..self.tabs.len()).rev() {
            let Some(t) = self.tabs.get(i) else {
                continue;
            };
            if t.role != TabRole::Session || !worktree_ui::inside(wt, &tab_dir(t)) {
                continue;
            }
            if t.terminal().is_exited() {
                self.close_tab(i, false);
            } else if let Some(t) = self.tabs.get_mut(i) {
                t.cwd.clone_from(&wt.primary);
                self.dirty = true;
            }
        }
        if self
            .scope
            .as_deref()
            .is_some_and(|s| s.starts_with(&wt.path))
        {
            self.set_scope(None);
        }
        self.refresh_sessions();
        if self.cfg.project_sync.iter().any(|r| r.path == wt.path) && self.confirm.is_none() {
            self.confirm = Some((
                Confirm {
                    title: "Stop backing up the removed worktree?".to_owned(),
                    body: format!(
                        "{} is in the project backups list but no longer exists. Its existing backups stay.",
                        wt.path.display()
                    ),
                    confirm_label: "Remove from list".to_owned(),
                    danger: false,
                },
                PendingConfirm::DropSyncRow {
                    path: wt.path.clone(),
                },
            ));
        }
    }

    // ------------------------------------------------------------ parked branches

    fn parked_of(&self, entry: &SessionEntry) -> Option<ParkedBranch> {
        self.cfg
            .parked_branches
            .iter()
            .find(|r| r.sessions.contains(&entry.id))
            .cloned()
    }

    /// "Continue in worktree": the gateway makes a worktree on the parked branch at its default
    /// location, moves the branch's sessions into it and opens the clicked one.
    pub(super) fn continue_in_worktree(&mut self, entry: SessionEntry) {
        if !self.omp_found() {
            return;
        }
        self.continue_parked(&entry, ContinueIn::Worktree, true);
    }

    /// "Continue in main checkout": after a confirmation that names the sessions running there
    /// the gateway checks the branch out in the main checkout and the session opens.
    pub(super) fn continue_in_main(&mut self, entry: SessionEntry) {
        let Some(rec) = self.parked_of(&entry) else {
            self.toasts
                .error(&self.ctx, "This session is not parked anymore");
            return;
        };
        let state = self
            .wt
            .park_states
            .iter()
            .find(|s| s.primary == rec.primary && s.branch == rec.branch)
            .and_then(|s| s.checked_out.clone());
        match state {
            // Already there: nothing to switch, the gateway just opens the session.
            Some(at) if at == rec.primary => self.continue_parked(&entry, ContinueIn::Main, true),
            _ => self.confirm_switch(entry, rec.primary, rec.branch, true),
        }
    }

    /// "Switch main checkout back to <base>": the parked branch stays; nothing opens.
    pub(super) fn switch_main_back(&mut self, entry: SessionEntry) {
        let Some(rec) = self.parked_of(&entry) else {
            self.toasts
                .error(&self.ctx, "This session is not parked anymore");
            return;
        };
        self.confirm_switch(entry, rec.primary, rec.base, false);
    }

    /// Sends the gateway's continue flow for the branch `entry` was parked on.
    fn continue_parked(&mut self, entry: &SessionEntry, place: ContinueIn, open: bool) {
        let Some(rec) = self.parked_of(entry) else {
            self.toasts
                .error(&self.ctx, "This session is not parked anymore");
            return;
        };
        let key = (rec.primary.clone(), rec.branch.clone());
        if !self.wt.continuing.insert(key.clone()) {
            return;
        }
        let branch = rec.branch.clone();
        let op = Mutation::WorktreeContinue {
            primary: rec.primary,
            branch: rec.branch,
            session: Some(entry.id.clone()),
            place,
        };
        self.gw.mutate_with_progress(
            op,
            |app, p: &Progress| app.toasts.info(&app.ctx, p.stage.clone()),
            move |app, r| {
                app.wt.continuing.remove(&key);
                app.refresh_sessions();
                match r {
                    Ok(Done::Session(target, _)) if open => {
                        let text = match place {
                            ContinueIn::Main => format!("The main checkout is on {branch} now"),
                            _ => format!("Continuing “{branch}”"),
                        };
                        app.toasts.success(&app.ctx, text);
                        app.show_continued(&target);
                    }
                    Ok(Done::Unit) if place == ContinueIn::SwitchBack => app.toasts.success(
                        &app.ctx,
                        "The main checkout is back on its base branch".to_owned(),
                    ),
                    Ok(Done::Path(dest)) => app.toasts.success(
                        &app.ctx,
                        format!("Continuing “{branch}” in {}", abbreviate_home(&dest)),
                    ),
                    Ok(_) => app
                        .toasts
                        .success(&app.ctx, format!("Continued “{branch}”")),
                    Err(e) => app.toasts.error(&app.ctx, brief(&e)),
                }
            },
        );
    }

    /// The session the gateway opened for a continued branch: its tab takes focus, or opens.
    fn show_continued(&mut self, target: &SessionTarget) {
        if !self.open_continued(target) {
            // `Done::Session` can beat the controller cache: wait for its record.
            self.wt.continued.push((target.clone(), Instant::now()));
            self.ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    /// Focuses the tab of `target`, or asks for its session file to be opened once the gateway's
    /// record names it. `false` while the record (or its file) is not there yet.
    fn open_continued(&mut self, target: &SessionTarget) -> bool {
        if let Some(i) = self.tabs.iter().position(|t| t.target().key == target.key) {
            self.activate(i);
            return true;
        }
        let file = self.gw.controllers().and_then(|all| {
            all.iter()
                .find(|c| c.target.key == target.key)
                .and_then(|c| c.session_path.clone())
        });
        match file {
            Some(file) => {
                self.desktop_opens.push(DesktopOpen::new(file, Split::None));
                // `run_desktop_opens` runs before this in the frame: an idle window needs a new one.
                self.ctx.request_repaint();
                true
            }
            None => false,
        }
    }

    /// Opens the continued sessions whose controller record arrived since; one that never shows
    /// up within [`CONTINUE_WAIT`] is reported.
    fn open_queued_continues(&mut self) {
        if self.wt.continued.is_empty() {
            return;
        }
        for (target, since) in std::mem::take(&mut self.wt.continued) {
            if self.open_continued(&target) {
                continue;
            }
            if since.elapsed() >= CONTINUE_WAIT {
                self.toasts.error(
                    &self.ctx,
                    "The continued session did not show up; open it from the sidebar",
                );
            } else {
                self.wt.continued.push((target, since));
            }
        }
        if !self.wt.continued.is_empty() {
            self.ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    /// Asks the gateway which live backend sessions exist, then warns about every session that
    /// runs in the main checkout (tab or not) before switching its branch.
    fn confirm_switch(
        &mut self,
        entry: SessionEntry,
        primary: PathBuf,
        target: String,
        open: bool,
    ) {
        self.gw.query(Query::RunningDetail, move |app, r| {
            let running = match r {
                Ok(Answer::RunningDetail(d)) => d.sessions,
                _ => Vec::new(),
            };
            app.show_switch_confirm(entry, primary, target, open, &running);
        });
    }

    fn show_switch_confirm(
        &mut self,
        entry: SessionEntry,
        primary: PathBuf,
        target: String,
        open: bool,
        running: &[deck_core::mux::RunningSession],
    ) {
        let groups = self.groups();
        let entries = || {
            groups
                .iter()
                .flat_map(|g| g.iter())
                .flat_map(|g| &g.sessions)
        };
        let tab_label = |path: &Path| -> Option<String> {
            self.tabs
                .iter()
                .find(|t| {
                    t.session_path()
                        .is_some_and(|p| worktree_ui::same_session(p, path))
                })
                .map(|t| t.fallback_title.clone())
        };
        let mut live: Vec<worktree_ui::LiveSession> = Vec::new();
        if let Some(infos) = self.gw.controllers() {
            for i in infos.iter().filter(|i| {
                i.role == deck_service::gateway::protocol::SessionRole::Session
                    && i.exit_code.is_none()
            }) {
                let name = match &i.backend {
                    deck_service::gateway::protocol::BackendRef::Mux { name, .. } => {
                        Some(name.clone())
                    }
                    deck_service::gateway::protocol::BackendRef::Direct => None,
                };
                let files: Vec<PathBuf> = i.session_path.iter().cloned().collect();
                // Only the fallback: `main_checkout_sessions` names known sessions by title.
                let label = i
                    .session_path
                    .as_deref()
                    .and_then(tab_label)
                    .or_else(|| i.title.clone())
                    .unwrap_or_else(|| i.label.clone());
                live.push(worktree_ui::LiveSession {
                    name,
                    files,
                    dir: Some(i.live_cwd.clone().unwrap_or_else(|| i.cwd.clone())),
                    label,
                });
            }
        }
        for r in running {
            let label = r
                .files
                .iter()
                .find_map(|f| tab_label(f))
                .or_else(|| r.title.clone())
                .unwrap_or_else(|| r.name.clone());
            live.push(worktree_ui::LiveSession {
                name: Some(r.name.clone()),
                files: r.files.clone(),
                dir: r.cwd.clone(),
                label,
            });
        }
        let worktrees: Vec<PathBuf> = self
            .groups()
            .map(|gs| {
                gs.iter()
                    .filter_map(|g| g.worktree.as_ref())
                    .map(|w| w.path.clone())
                    .collect()
            })
            .unwrap_or_default();
        let entries: Vec<&SessionEntry> = entries().collect();
        let tabs =
            worktree_ui::main_checkout_sessions(&live, &primary, &worktrees, &entry.path, &entries);
        let seen = if tabs.is_empty() {
            "No other session runs in the main checkout.".to_owned()
        } else {
            format!(
                "These sessions run in the main checkout and will see branch {target}:\n{}",
                tabs.iter()
                    .map(|t| format!("• {t}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        let (title, lead, confirm_label) = if open {
            (
                "Continue in main checkout?",
                format!(
                    "Branch “{target}” is checked out in the main checkout {}, and this session opens there.",
                    abbreviate_home(&primary)
                ),
                "Switch",
            )
        } else {
            (
                "Switch main checkout back?",
                format!(
                    "The main checkout {} switches back to {target}; the parked branch stays.",
                    abbreviate_home(&primary)
                ),
                "Switch back",
            )
        };
        self.confirm = Some((
            Confirm {
                title: title.to_owned(),
                body: format!(
                    "{lead}\n\n{seen}\n\nA checkout with uncommitted changes is refused; nothing is discarded."
                ),
                confirm_label: confirm_label.to_owned(),
                danger: false,
            },
            PendingConfirm::SwitchMain { entry, open },
        ));
    }

    /// The switch was confirmed: the gateway checks the parked branch out (opening the session)
    /// or switches the main checkout back to the branch's base.
    pub(super) fn execute_switch_main(&mut self, entry: SessionEntry, open: bool) {
        let place = if open {
            ContinueIn::Main
        } else {
            ContinueIn::SwitchBack
        };
        self.continue_parked(&entry, place, open);
    }

    // ------------------------------------------------------------ moves into a worktree

    /// A session of a removed worktree is opened: omp moves it into the primary checkout first,
    /// then the tab opens on the moved file there. When the move fails the session opens as
    /// before (launched in the primary) with an error toast; when its file is gone by then
    /// (another move got it first) the fresh scan is searched for the session by id.
    pub(super) fn open_relocated(
        &mut self,
        entry: &SessionEntry,
        primary: PathBuf,
        view: deck_core::config::SessionView,
    ) {
        if !self.wt.in_flight.insert(entry.path.clone()) {
            self.toasts.info(
                &self.ctx,
                "This session is being moved — try again in a moment",
            );
            return;
        }
        let entry = entry.clone();
        self.gw.mutate(
            Mutation::RelocateSessions {
                files: vec![entry.path.clone()],
                dest: primary.clone(),
            },
            move |app, r| {
                app.wt.in_flight.remove(&entry.path);
                let moved = match r {
                    Ok(Done::Relocated(results)) => results
                        .into_iter()
                        .next()
                        .map_or_else(|| Err("nothing was moved".to_owned()), |(_, r)| r),
                    Ok(_) => Err("the gateway answered with something else".to_owned()),
                    Err(e) => Err(e),
                };
                match moved {
                    Ok(new_path) => {
                        app.refresh_sessions();
                        app.open_entry_view(&entry, new_path, primary, view);
                    }
                    Err(e) => {
                        app.toasts.error(
                            &app.ctx,
                            format!(
                                "Could not move the session into {}: {}",
                                abbreviate_home(&primary),
                                brief(&e)
                            ),
                        );
                        let old = entry.path.clone();
                        app.gw.native(
                            move || old.exists(),
                            move |app, exists| {
                                app.open_after_failed_move(&entry, primary, exists, view)
                            },
                        );
                    }
                }
            },
        );
    }

    /// The move of `entry` failed: open its file where it is, or — when it no longer exists —
    /// the session with the same id from the latest scan.
    fn open_after_failed_move(
        &mut self,
        entry: &SessionEntry,
        primary: PathBuf,
        exists: bool,
        view: deck_core::config::SessionView,
    ) {
        if exists {
            self.open_entry_view(entry, entry.path.clone(), primary, view);
            return;
        }
        self.refresh_sessions();
        let moved = self.groups().and_then(|gs| {
            gs.iter()
                .flat_map(|g| &g.sessions)
                .find(|e| e.id == entry.id && e.path != entry.path)
                .cloned()
        });
        match moved {
            Some(found) => {
                self.open_entry_view(&found, found.path.clone(), found.cwd.clone(), view)
            }
            None => self.toasts.error(
                &self.ctx,
                "The session's file is gone — rescan and try again",
            ),
        }
    }

    /// A move typed into tab `uid` never went in: its "Moved to worktree" note is not waited for.
    pub(super) fn forget_move(&mut self, uid: u64) {
        self.wt.moves.retain(|m| m.uid != uid);
    }

    fn expire_moves(&mut self) {
        self.wt.moves.retain(|m| m.since.elapsed() < MOVE_EXPIRES);
    }

    /// Records `new` as tab `uid`'s session file (here and in the gateway) when it differs.
    fn fix_tab_path(&mut self, uid: u64, new: &Path) {
        let Some(t) = self.tabs.iter_mut().find(|t| t.uid == uid) else {
            return;
        };
        if t.session_path() == Some(new) {
            return;
        }
        t.hint_session_path(Some(new.to_path_buf()));
        let target = t.target().clone();
        self.gw.mutate(
            Mutation::SetSessionPath {
                target,
                path: Some(new.to_path_buf()),
            },
            |_, result| {
                if let Err(e) = result {
                    log::debug!("setting the session file failed: {e}");
                }
            },
        );
        self.dirty = true;
    }

    /// A tab whose session file changed (omp relocated it, `/new`, `/resume`, …) takes the folder
    /// of the new file; a pending move that landed in a worktree says so.
    fn track_relocations(&mut self) {
        let generation = self.gw.sessions_generation();
        let live: HashSet<u64> = self.tabs.iter().map(|t| t.uid).collect();
        self.wt.seen.retain(|u, _| live.contains(u));
        self.wt.seen_cwd.retain(|u, _| live.contains(u));
        self.wt.tried.retain(|u, _| live.contains(u));
        let mut changed = Vec::new();
        for tab in self.tabs.iter() {
            let live_cwd = tab.info().and_then(|i| i.live_cwd.clone());
            let path = tab.session_path_owned();
            let (Some(prev_path), Some(prev_cwd)) = (
                self.wt.seen.get(&tab.uid).cloned(),
                self.wt.seen_cwd.get(&tab.uid).cloned(),
            ) else {
                self.wt.seen.insert(tab.uid, path);
                self.wt.seen_cwd.insert(tab.uid, live_cwd);
                continue;
            };
            // A move shows as a new session file, or as a new live folder with the recorded
            // file still the old one.
            let cwd_moved = live_cwd.is_some() && live_cwd != prev_cwd;
            if (prev_path != path || cwd_moved)
                && let Some(p) = path
                && self.wt.tried.get(&tab.uid) != Some(&(p.clone(), generation))
            {
                changed.push((tab.uid, p, live_cwd));
            } else if !cwd_moved {
                self.wt.seen_cwd.insert(tab.uid, live_cwd);
            }
        }
        if changed.is_empty() {
            return;
        }
        let groups = self.groups();
        for (uid, path, live_cwd) in changed {
            // By file name: the recorded path may predate the move.
            let found = groups.as_deref().and_then(|gs| {
                gs.iter().find_map(|g| {
                    g.sessions
                        .iter()
                        .find(|e| e.path == path)
                        .or_else(|| {
                            g.sessions
                                .iter()
                                .find(|e| worktree_ui::same_session(&e.path, &path))
                        })
                        .map(|e| (g, e))
                })
            });
            let Some((group, entry)) = found else {
                self.wt.tried.insert(uid, (path, generation));
                continue;
            };
            self.wt.tried.remove(&uid);
            self.wt.seen_cwd.insert(uid, live_cwd.clone());
            if entry.path != path {
                self.fix_tab_path(uid, &entry.path);
            }
            self.wt.seen.insert(uid, Some(entry.path.clone()));
            let folder = live_cwd.unwrap_or_else(|| entry.cwd.clone());
            if let Some(t) = self.tabs.iter_mut().find(|t| t.uid == uid)
                && t.cwd != folder
            {
                t.cwd = folder;
                self.dirty = true;
            }
            if let Some(i) = self.wt.moves.iter().position(|m| m.uid == uid)
                && let Some(w) = &group.worktree
            {
                self.wt.moves.remove(i);
                self.toasts.success(
                    &self.ctx,
                    format!("Moved to worktree “{}”", worktree_ui::worktree_label(w)),
                );
            }
        }
    }
}
