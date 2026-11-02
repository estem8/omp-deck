//! App-side glue of the session manager hub: the gateway's inventory topic and statistics
//! queries, the bulk delete / copy / move / stop operations with their confirmations, and the
//! hub's open / close.
//!
//! Rendering is [`crate::sessions_view`], the data rules are [`crate::sessions_model`]; this
//! module sends the requests (the gateway runs omp and the backends) and owns the cleanup shared
//! with the sidebar's single-session actions.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf, absolute};
use std::time::{Duration, Instant, SystemTime};

use deck_core::inventory::{InventoryEntry, SessionStats};
use deck_core::mux::session_name_for;
use deck_core::omp::{DeleteTarget, MoveKind, MoveOutcome};
use deck_core::platform::TreeMemory;
use deck_core::sessions::{SessionEntry, abbreviate_home};
use deck_service::gateway::protocol::{Answer, Done as Reply, Mutation, Query, Topic, TopicValue};
use deck_service::statusfmt::Activity;
use egui::{Key, Modifiers};

use super::{App, PendingConfirm};
use crate::app::human_size;
use crate::confirm::Confirm;
use crate::running_model::{HubMode, RunModel, RunPoll};
use crate::sessions_model::{Live, Model, plural};
use crate::sessions_view::{self, OpProgress, SessionsAction, ViewInput, ViewState};
use crate::tabs::{Anchor, ClosedTab, MovedTab};
use crate::transfer::{FolderDialog, Subject, TransferKind};
use crate::worktree_ui;

/// Statistics are requested in batches of this many sessions (newest first).
const STATS_BATCH: usize = 32;
/// How often the Running view re-reads the live sessions.
const RUN_POLL: Duration = Duration::from_secs(2);
/// How often the Sessions view refreshes its RAM column (only while something runs).
const RAM_POLL: Duration = Duration::from_secs(5);

/// What `prepare_move` captured about the tab(s) of a session that is about to be moved.
pub(crate) struct MovePrep {
    pub(super) entry: SessionEntry,
    pub(super) kill_names: Vec<String>,
    pub(super) tab: Option<ClosedTab>,
    pub(super) anchor: Anchor,
    pub(super) refocus: Option<u64>,
}

impl MovePrep {
    /// The tab to bring back once the gateway answered the move: on the new path after success,
    /// on the original one (re-attached if its session survived, per `is_alive`) after a
    /// failure. `persistent` = a backend keeps sessions alive across tabs.
    pub(super) fn finish(
        self,
        result: &Result<MoveOutcome, String>,
        dest: &Path,
        persistent: bool,
        is_alive: &dyn Fn(&str) -> bool,
    ) -> Option<Box<MovedTab>> {
        let MovePrep {
            entry,
            kill_names,
            tab,
            anchor,
            refocus,
        } = self;
        tab.map(|mut tab| {
            match result {
                Ok(outcome) => {
                    let new_id = match &outcome.kind {
                        MoveKind::Forked { new_id } => new_id,
                        _ => &entry.id,
                    };
                    tab.mux_name = persistent.then(|| session_name_for(new_id));
                    tab.cwd = dest.to_path_buf();
                    tab.session_path = Some(outcome.new_path.clone());
                }
                Err(_) => {
                    // Re-attach a session that survived; else resume the original file under
                    // its canonical name.
                    let alive = kill_names.iter().find(|n| is_alive(n)).cloned();
                    tab.mux_name =
                        alive.or_else(|| persistent.then(|| session_name_for(&entry.id)));
                }
            }
            Box::new(MovedTab {
                tab,
                anchor,
                refocus,
            })
        })
    }
}

fn session_entry(e: &InventoryEntry) -> SessionEntry {
    SessionEntry {
        id: e.id.clone(),
        path: e.path.clone(),
        cwd: e.cwd.clone(),
        title: e.title.clone(),
        first_prompt: e.first_prompt.clone(),
        modified: e.modified,
    }
}

/// The folder every session shares (`None` for none or a mix).
fn common_cwd<'a>(mut cwds: impl Iterator<Item = &'a Path>) -> Option<PathBuf> {
    let first = cwds.next()?;
    cwds.all(|c| c == first).then(|| first.to_path_buf())
}

/// The running-sessions worker: at most one poll in flight.
#[derive(Default)]
struct RunPolling {
    in_flight: bool,
    last: Option<Instant>,
}

// ------------------------------------------------------------------- state

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpKind {
    Delete,
    Copy,
    Move,
    Stop,
}

/// A bulk operation in flight.
struct Op {
    id: u64,
    kind: OpKind,
    progress: OpProgress,
    ok: usize,
    failed: usize,
    /// Moves that kept the original and landed as a new session.
    forked: usize,
    /// Copies left out because the session already lives in the destination.
    skipped: usize,
    /// Bytes of the sessions deleted so far.
    freed: u64,
    sizes: HashMap<PathBuf, u64>,
    dest: Option<PathBuf>,
    first_error: Option<String>,
}

#[derive(Default)]
pub(super) struct SessionHub {
    pub(super) open: bool,
    model: Model,
    view: ViewState,
    /// Id of the latest inventory read; older statistics answers are dropped.
    generation: u64,
    /// The gateway's inventory revision last taken into the model.
    inventory_rev: u64,
    /// Entries whose statistics were asked for, by what the entry looked like then.
    asked: HashMap<PathBuf, (SystemTime, u64)>,
    loaded: bool,
    op: Option<Op>,
    next_op: u64,
    labels_gen: Option<u64>,
    mode: HubMode,
    run: RunModel,
    run_poll: RunPolling,
}

/// What one session of a bulk operation ended with.
pub(super) enum Done {
    Deleted,
    Copied,
    Moved(MoveKind),
    Stopped,
}

// -------------------------------------------------------------------- impl

impl App {
    /// Ctrl+Shift+S.
    pub(super) fn sessions_shortcut(&self, ctx: &egui::Context) -> bool {
        ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::S))
    }

    pub(super) fn toggle_sessions(&mut self) {
        if self.sessions.open {
            self.close_sessions();
        } else {
            self.open_sessions(None);
        }
    }

    /// Open the hub (scoped to `scope` when given) and follow the gateway's inventory.
    pub(super) fn open_sessions(&mut self, scope: Option<PathBuf>) {
        let hub = &mut self.sessions;
        hub.open = true;
        hub.view.release_focus = true;
        hub.model.filter = Default::default();
        hub.model.sel.clear();
        hub.model.detail = None;
        hub.model.scope = scope;
        hub.model.mark_view_dirty();
        hub.loaded = !hub.model.rows().is_empty();
        hub.mode = HubMode::Sessions;
        hub.run.reset();
        hub.run_poll.last = None;
        // The entries are read from the cache; every one's statistics are asked for again.
        hub.inventory_rev = 0;
        hub.asked.clear();
        hub.generation += 1;
        self.gw.subscribe(Topic::Inventory);
    }

    pub(super) fn close_sessions(&mut self) {
        let hub = &mut self.sessions;
        hub.open = false;
        hub.generation += 1;
        self.gw.unsubscribe(&Topic::Inventory);
        self.focus_pending = true;
    }

    /// Reads the inventory again from the gateway (a refresh, or an operation just finished).
    fn start_scan(&mut self) {
        if !self.sessions.open {
            return;
        }
        self.sessions.inventory_rev = 0;
    }

    /// Takes a newer inventory from the gateway's cache into the model and asks for the
    /// statistics of what is new or changed.
    fn take_inventory(&mut self) {
        let rev = self.gw.revision(&Topic::Inventory);
        if rev == 0 || rev == self.sessions.inventory_rev {
            return;
        }
        let Some(TopicValue::Inventory(entries)) = self.gw.topic(&Topic::Inventory) else {
            return;
        };
        self.sessions.inventory_rev = rev;
        self.sessions.generation += 1;
        self.sessions.loaded = true;
        self.start_stats(&entries);
        self.sessions.model.set_entries(entries);
        self.resolve_running();
        self.refresh_ram();
    }

    /// Asks the gateway for the statistics of the entries (newest first) it has not parsed for
    /// this version of the file yet, in batches whose answers stream in.
    fn start_stats(&mut self, entries: &[InventoryEntry]) {
        let hub = &mut self.sessions;
        hub.asked
            .retain(|p, _| entries.iter().any(|e| &e.path == p));
        let due: Vec<PathBuf> = entries
            .iter()
            .filter(|e| {
                let fingerprint = (e.modified, e.total_bytes());
                hub.asked.insert(e.path.clone(), fingerprint) != Some(fingerprint)
            })
            .map(|e| e.path.clone())
            .collect();
        let generation = hub.generation;
        for batch in due.chunks(STATS_BATCH) {
            self.gw.query(Query::Stats(batch.to_vec()), move |app, r| {
                if generation != app.sessions.generation {
                    return;
                }
                let batch = match r {
                    Ok(Answer::Stats(b)) => b,
                    Ok(_) => return,
                    Err(e) => {
                        log::warn!("session statistics: {e}");
                        return;
                    }
                };
                for (path, stats) in batch {
                    let result: Result<SessionStats, String> =
                        stats.ok_or_else(|| "its statistics could not be read".to_owned());
                    app.sessions.model.set_stats(&path, result);
                }
            });
        }
    }

    /// Per frame while open: tell the model which sessions run / have tabs and what the
    /// worktree projects are called.
    fn sync_hub_inputs(&mut self) {
        self.take_inventory();
        let generation = self.gw.sessions_generation();
        if self.sessions.labels_gen != Some(generation) {
            self.sessions.labels_gen = Some(generation);
            let labels: HashMap<PathBuf, String> = self
                .groups()
                .map(|groups| {
                    groups
                        .iter()
                        .filter_map(|g| {
                            g.worktree
                                .as_ref()
                                .map(|w| (g.cwd.clone(), worktree_ui::worktree_label(w)))
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.sessions.model.set_labels(labels);
            self.resolve_running();
        }
        let snap = self.running_snapshot();
        let mut live: HashMap<PathBuf, Live> = HashMap::new();
        for r in self.sessions.model.rows() {
            if snap.is_running(&r.entry.id, &r.entry.path) {
                live.entry(r.entry.path.clone()).or_default().running = true;
            }
        }
        for t in self.tabs.iter() {
            if t.terminal().is_exited() {
                continue;
            }
            if let Some(p) = t.session_path() {
                let l = live.entry(p.to_path_buf()).or_default();
                l.in_tab = true;
                l.busy |= Activity::of_terminal(t.terminal()) != Activity::Idle;
            }
        }
        self.sessions.model.set_live(live);
        let tabs = &self.tabs;
        self.sessions.run.set_tabs(|r| {
            tabs.iter()
                .find(|t| {
                    !t.terminal().is_exited()
                        && (t.mux_name.as_deref() == Some(r.name())
                            || t.session_path()
                                .is_some_and(|p| r.raw.files.iter().any(|f| f == p)))
                })
                .map(|t| Activity::of_terminal(t.terminal()))
        });
    }

    /// Per frame: draw the hub while it is open and run what it asks for.
    pub(super) fn show_sessions(&mut self, ctx: &egui::Context) {
        if !self.sessions.open {
            return;
        }
        self.sync_hub_inputs();
        self.drive_run_poll(ctx);
        let blocked = self.confirm.is_some()
            || self.folder_dialog.is_some()
            || self.dialog.is_some()
            || self.wt.dialog.is_some()
            || self.palette.is_some();
        let (omp, can_stop) = (self.omp_found(), self.backends_available());
        let hub = &mut self.sessions;
        let input = ViewInput {
            scanning: !hub.loaded,
            loaded: hub.loaded,
            mode: hub.mode,
            op: hub.op.as_ref().map(|o| &o.progress),
            blocked,
            now: SystemTime::now(),
            omp,
            can_stop,
        };
        let actions = sessions_view::show(ctx, &mut hub.model, &mut hub.run, &mut hub.view, &input);
        // Ages tick over without any other event.
        ctx.request_repaint_after(Duration::from_secs(30));
        for a in actions {
            self.apply_sessions_action(a);
        }
    }

    // ------------------------------------------------------------ running

    /// Poll the live sessions: every 2 s in the Running view; in the Sessions view once on open
    /// and every 5 s while something runs (the RAM column). Never while the hub is closed.
    fn drive_run_poll(&mut self, ctx: &egui::Context) {
        let can_stop = self.backends_available();
        let up = self.gw.is_up();
        let hub = &mut self.sessions;
        if !can_stop {
            if !hub.run.loaded() {
                hub.run.set_poll(RunPoll::default());
            }
            return;
        }
        if hub.run_poll.in_flight || !up {
            return;
        }
        let every = match hub.mode {
            HubMode::Running => Some(RUN_POLL),
            HubMode::Sessions => hub
                .model
                .rows()
                .iter()
                .any(|r| r.live.running)
                .then_some(RAM_POLL),
        };
        let last = hub.run_poll.last;
        let wait = match (last, every) {
            (None, _) => Duration::ZERO,
            (Some(t), Some(e)) => e.saturating_sub(t.elapsed()),
            (Some(_), None) => return,
        };
        if !wait.is_zero() {
            ctx.request_repaint_after(wait);
            return;
        }
        hub.run_poll.in_flight = true;
        hub.run_poll.last = Some(Instant::now());
        self.gw.query(Query::RunningDetail, |app, r| {
            app.sessions.run_poll.in_flight = false;
            match r {
                Ok(Answer::RunningDetail(d)) => {
                    app.sessions.run.set_poll(RunPoll {
                        sessions: d.sessions,
                        tmux_server: d.tmux_server,
                        tmux: d.tmux,
                    });
                    app.resolve_running();
                    app.refresh_ram();
                }
                Ok(_) => {}
                Err(e) => log::warn!("running sessions: {e}"),
            }
        });
    }

    /// Titles and project labels of the running rows, from the inventory.
    fn resolve_running(&mut self) {
        let hub = &mut self.sessions;
        let model = &hub.model;
        hub.run.resolve(
            |p| {
                model
                    .row_by_path(p)
                    .map(|r| (r.label().to_owned(), r.entry.cwd.clone()))
            },
            |cwd| model.project_label(cwd),
        );
    }

    /// The Sessions table's RAM column: each session file gets the memory of the live session
    /// that holds it (the bigger one when several do); a live session that holds no file counts
    /// for the session whose canonical name it carries (a fresh omp, same rule as "running").
    fn refresh_ram(&mut self) {
        let hub = &mut self.sessions;
        let mut ram: HashMap<PathBuf, TreeMemory> = HashMap::new();
        let mut put = |path: &Path, m: TreeMemory| {
            ram.entry(path.to_path_buf())
                .and_modify(|e| {
                    if m.bytes > e.bytes {
                        *e = m;
                    }
                })
                .or_insert(m);
        };
        let mut fileless: HashMap<&str, TreeMemory> = HashMap::new();
        for r in hub.run.rows() {
            let Some(m) = r.raw.memory else {
                continue;
            };
            if r.raw.files.is_empty() {
                fileless.insert(r.name(), m);
            }
            for f in &r.raw.files {
                put(f, m);
            }
        }
        if !fileless.is_empty() {
            for row in hub.model.rows() {
                if let Some(m) = fileless.get(session_name_for(&row.entry.id).as_str()) {
                    put(&row.entry.path, *m);
                }
            }
        }
        hub.model.set_ram(ram);
    }

    fn on_killed(&mut self, total: usize, failed: &[(String, String)]) {
        self.sessions.run.clear_killing();
        if failed.is_empty() {
            self.toasts.success(
                &self.ctx,
                format!("Killed {}", plural(total, "session", "sessions")),
            );
        } else {
            let why = crate::summary::strip_context(&failed[0].1);
            self.toasts.error(
                &self.ctx,
                format!("Could not kill {} of {total} — {why}", failed.len()),
            );
        }
        // The list refreshes at once.
        self.sessions.run_poll.last = None;
        self.ctx.request_repaint();
    }

    fn request_kill_running(&mut self, names: Vec<String>) {
        let hub = &self.sessions;
        if hub.run.any_killing() {
            return;
        }
        let present: Vec<String> = names
            .into_iter()
            .filter(|n| hub.run.rows().iter().any(|r| r.name() == n))
            .collect();
        let plan = hub.run.plan_for(&present);
        if plan.count == 0 {
            self.toasts
                .info(&self.ctx, "None of those sessions is running anymore.");
            return;
        }
        self.confirm = Some((
            Confirm {
                title: plan.title(),
                body: plan.body(),
                confirm_label: "Kill".to_owned(),
                danger: true,
            },
            PendingConfirm::KillRunning(present),
        ));
    }

    /// Kill exactly `names` (same backend path as the single "Stop session process": tmux
    /// detaches its clients first). Their tabs stay open and show the exit.
    pub(super) fn execute_kill_running(&mut self, names: Vec<String>) {
        if names.is_empty() || self.sessions.run.any_killing() {
            return;
        }
        self.sessions.run.set_killing(names.iter().cloned());
        let total = names.len();
        let asked = names.clone();
        self.gw
            .mutate(Mutation::KillNames { names, wait: true }, move |app, r| {
                let failed = match r {
                    Ok(Reply::Killed(failed)) => failed,
                    Ok(_) => Vec::new(),
                    // Nothing is known to have been killed: every name counts as failed.
                    Err(e) => asked.into_iter().map(|n| (n, e.clone())).collect(),
                };
                app.on_killed(total, &failed);
            });
    }

    fn apply_sessions_action(&mut self, action: SessionsAction) {
        match action {
            SessionsAction::Close => self.close_sessions(),
            SessionsAction::SetMode(mode) => {
                if self.sessions.mode != mode {
                    self.sessions.mode = mode;
                    // Fresh numbers at once on entering the Running view.
                    self.sessions.run_poll.last = None;
                }
            }
            SessionsAction::KillRunning(names) => self.request_kill_running(names),
            SessionsAction::Refresh => {
                self.start_scan();
                self.refresh_sessions();
            }
            SessionsAction::Open(path) => {
                if let Some(row) = self.sessions.model.row_by_path(&path) {
                    let entry = session_entry(&row.entry);
                    self.close_sessions();
                    self.open_session(&entry);
                }
            }
            SessionsAction::Delete(paths) => self.request_delete_sessions(&paths),
            SessionsAction::Transfer { kind, paths, dest } => match dest {
                Some(dest) => self.begin_hub_transfer(kind, paths, dest),
                None => {
                    let cwd = common_cwd(
                        paths
                            .iter()
                            .filter_map(|p| self.sessions.model.row_by_path(p))
                            .map(|r| r.entry.cwd.as_path()),
                    );
                    self.folder_dialog = Some(FolderDialog::many(kind, paths, cwd));
                }
            },
            SessionsAction::Stop(paths) => self.request_stop_sessions(paths),
            SessionsAction::CopyText { text, what } => self.copy_text(&text, what),
            SessionsAction::OpenFolder(dir) => self.open_folder(dir),
            SessionsAction::Reveal(file) => {
                if let Some(dir) = file.parent() {
                    self.open_folder(dir.to_path_buf());
                }
            }
        }
    }

    /// The folder dialog was confirmed: continue the single-session or the bulk path.
    pub(super) fn finish_folder_dialog(&mut self, d: FolderDialog, dest: PathBuf) {
        let kind = d.kind;
        match d.subject {
            Subject::One(entry) => self.begin_transfer(kind, entry, dest),
            Subject::Many { paths, .. } => self.begin_hub_transfer(kind, paths, dest),
        }
    }

    fn begin_hub_transfer(&mut self, kind: TransferKind, paths: Vec<PathBuf>, dest: PathBuf) {
        match kind {
            TransferKind::Copy => self.execute_copy_sessions(paths, dest),
            TransferKind::Move => self.request_move_sessions(paths, dest),
        }
    }

    // ------------------------------------------------------------ helpers

    /// The rows behind `paths` that exist and are not busy with another operation. Each
    /// refusal is recorded on its row.
    fn claim_rows(&mut self, paths: &[PathBuf]) -> Vec<InventoryEntry> {
        let mut out = Vec::new();
        for p in paths {
            let Some(row) = self.sessions.model.row_by_path(p) else {
                continue;
            };
            let entry = row.entry.clone();
            if self.moving.contains(p) {
                self.sessions
                    .model
                    .set_error(p, "Another operation is working on this session".to_owned());
                continue;
            }
            out.push(entry);
        }
        out
    }

    fn begin_op(&mut self, kind: OpKind, entries: &[InventoryEntry], dest: Option<PathBuf>) -> u64 {
        let hub = &mut self.sessions;
        hub.next_op += 1;
        let id = hub.next_op;
        hub.model.clear_errors();
        let verb = match kind {
            OpKind::Delete => "Deleting",
            OpKind::Copy => "Copying",
            OpKind::Move => "Moving",
            OpKind::Stop => "Killing",
        };
        hub.op = Some(Op {
            id,
            kind,
            progress: OpProgress {
                verb,
                done: 0,
                total: entries.len(),
            },
            ok: 0,
            failed: 0,
            forked: 0,
            skipped: 0,
            freed: 0,
            sizes: entries
                .iter()
                .map(|e| (e.path.clone(), e.total_bytes()))
                .collect(),
            dest,
            first_error: None,
        });
        for e in entries {
            self.moving.insert(e.path.clone());
        }
        id
    }

    fn on_step(
        &mut self,
        op: u64,
        path: &Path,
        outcome: Result<Done, String>,
        tab: Option<Box<MovedTab>>,
    ) {
        self.moving.remove(path);
        self.restore_moved_tab(path, tab);
        let current = self.sessions.op.as_mut().filter(|o| o.id == op);
        let Some(o) = current else {
            return;
        };
        o.progress.done += 1;
        let finished = o.progress.done >= o.progress.total;
        match outcome {
            Ok(done) => {
                o.ok += 1;
                match done {
                    Done::Deleted => {
                        o.freed += o.sizes.get(path).copied().unwrap_or(0);
                        self.sessions.model.forget(path);
                    }
                    Done::Moved(kind) => {
                        if let MoveKind::Forked { .. } = kind {
                            o.forked += 1;
                        } else {
                            self.sessions.model.forget(path);
                        }
                    }
                    Done::Copied | Done::Stopped => {}
                }
            }
            Err(e) => {
                o.failed += 1;
                o.first_error.get_or_insert_with(|| e.clone());
                log::warn!("session manager: {}: {e}", path.display());
                self.sessions
                    .model
                    .set_error(path, failure_line(&e).to_owned());
            }
        }
        if finished {
            self.on_op_finished(op);
        }
    }

    fn on_op_finished(&mut self, op: u64) {
        let Some(o) = self.sessions.op.take_if(|o| o.id == op) else {
            return;
        };
        let to = o.dest.as_deref().map(abbreviate_home).unwrap_or_default();
        let total = o.progress.total;
        if o.failed == 0 {
            let text = match o.kind {
                OpKind::Delete => format!(
                    "Deleted {} · freed {}",
                    plural(o.ok, "session", "sessions"),
                    human_size(o.freed)
                ),
                OpKind::Copy if o.skipped > 0 => {
                    format!("Copied {} · skipped {} already in {to}", o.ok, o.skipped)
                }
                OpKind::Copy => format!("Copied {} to {to}", plural(o.ok, "session", "sessions")),
                OpKind::Move if o.forked > 0 => format!(
                    "Moved {} to {to} — {} kept the original and landed as a new session",
                    plural(o.ok, "session", "sessions"),
                    o.forked
                ),
                OpKind::Move => format!("Moved {} to {to}", plural(o.ok, "session", "sessions")),
                OpKind::Stop => format!("Killed {}", plural(o.ok, "session", "sessions")),
            };
            self.toasts.success(&self.ctx, text);
        } else {
            let verb = match o.kind {
                OpKind::Delete => "delete",
                OpKind::Copy => "copy",
                OpKind::Move => "move",
                OpKind::Stop => "kill",
            };
            let why = o
                .first_error
                .as_deref()
                .map(failure_line)
                .unwrap_or_default();
            if let Some(full) = o.first_error.as_deref() {
                log::warn!("session manager: {verb} failed: {full}");
            }
            // The toast is short: the skip note replaces the "failed rows are marked" hint.
            let tail = if o.skipped > 0 {
                format!(" Skipped {} already in {to}.", o.skipped)
            } else {
                " The failed rows are marked in the table.".to_owned()
            };
            self.toasts.error(
                &self.ctx,
                format!("Could not {verb} {} of {total} — {why}.{tail}", o.failed),
            );
        }
        self.focus_pending = true;
        self.start_scan();
        self.refresh_sessions();
    }

    // ------------------------------------------------------------- delete

    fn request_delete_sessions(&mut self, paths: &[PathBuf]) {
        if !self.omp_found() || self.sessions.op.is_some() {
            return;
        }
        let model = &self.sessions.model;
        let rows: Vec<_> = paths.iter().filter_map(|p| model.row_by_path(p)).collect();
        if rows.is_empty() {
            return;
        }
        let bytes: u64 = rows.iter().map(|r| r.entry.total_bytes()).sum();
        let projects = rows
            .iter()
            .map(|r| r.entry.cwd.as_path())
            .collect::<HashSet<_>>()
            .len();
        let running = rows.iter().filter(|r| r.live.running).count();
        let busy = rows.iter().filter(|r| r.live.busy).count();
        let mut body = format!(
            "{} from {}.\nFrees {} (session files, subagent transcripts and artifacts). \
             This cannot be undone.",
            plural(rows.len(), "session", "sessions"),
            plural(projects, "project", "projects"),
            human_size(bytes),
        );
        if running > 0 {
            body.push_str(&format!(
                "\n{running} running — their omp processes are stopped first."
            ));
        }
        if busy > 0 {
            body.push_str(&format!(
                "\n{busy} {} working: the current turn is interrupted.",
                if busy == 1 { "is" } else { "are" }
            ));
        }
        let title = format!("Delete {}?", plural(rows.len(), "session", "sessions"));
        let paths: Vec<DeleteTarget> = rows
            .iter()
            .map(|r| DeleteTarget {
                path: r.entry.path.clone(),
                id: r.entry.id.clone(),
            })
            .collect();
        self.confirm = Some((
            Confirm {
                title,
                body,
                confirm_label: "Delete".to_owned(),
                danger: true,
            },
            PendingConfirm::DeleteSessions(paths),
        ));
    }

    pub(super) fn execute_delete_sessions(&mut self, targets: Vec<DeleteTarget>) {
        if !self.omp_found() || self.sessions.op.is_some() {
            return;
        }
        let paths: Vec<PathBuf> = targets.iter().map(|t| t.path.clone()).collect();
        let entries = self.claim_rows(&paths);
        if entries.is_empty() {
            return;
        }
        // The ids captured at confirmation; omp re-checks them under the session lease.
        let files: Vec<DeleteTarget> = targets
            .into_iter()
            .filter(|t| entries.iter().any(|e| e.path == t.path))
            .collect();
        let mut kill_names: Vec<String> = Vec::new();
        for e in &entries {
            for n in self.session_kill_names(&session_entry(e), true) {
                if !kill_names.contains(&n) {
                    kill_names.push(n);
                }
            }
        }
        let op = self.begin_op(OpKind::Delete, &entries, None);
        let asked: Vec<PathBuf> = files.iter().map(|t| t.path.clone()).collect();
        self.gw.mutate(
            Mutation::Delete {
                targets: files,
                stop: kill_names,
            },
            move |app, r| match r {
                Ok(Reply::Deleted(results)) => {
                    for (path, failure) in results {
                        let outcome = failure.map_or(Ok(Done::Deleted), Err);
                        app.on_step(op, &path, outcome, None);
                    }
                }
                Ok(_) => app.fail_all(op, &asked, "the gateway answered with something else"),
                Err(e) => app.fail_all(op, &asked, &e),
            },
        );
    }

    /// Every path of `paths` ended with the same failure.
    fn fail_all(&mut self, op: u64, paths: &[PathBuf], why: &str) {
        for p in paths {
            self.on_step(op, p, Err(why.to_owned()), None);
        }
    }

    // --------------------------------------------------------------- copy

    /// Sessions already in `dest` are left out (same rule as the sidebar's target list).
    fn partition_by_dest(
        &mut self,
        entries: Vec<InventoryEntry>,
        dest: &Path,
    ) -> Vec<InventoryEntry> {
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let dest = canon(dest);
        let (here, other): (Vec<_>, Vec<_>) =
            entries.into_iter().partition(|e| canon(&e.cwd) == dest);
        for e in here {
            self.sessions
                .model
                .set_error(&e.path, "Already in that project".to_owned());
        }
        other
    }

    fn execute_copy_sessions(&mut self, paths: Vec<PathBuf>, dest: PathBuf) {
        if !self.omp_found() || self.sessions.op.is_some() {
            return;
        }
        let claimed = self.claim_rows(&paths);
        let claimed_n = claimed.len();
        let entries = self.partition_by_dest(claimed, &dest);
        if entries.is_empty() {
            self.toasts.info(
                &self.ctx,
                "Nothing to copy: the sessions are already there.",
            );
            return;
        }
        let op = self.begin_op(OpKind::Copy, &entries, Some(dest.clone()));
        if let Some(o) = self.sessions.op.as_mut() {
            o.skipped = claimed_n - entries.len();
        }
        let files: VecDeque<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
        match absolute(&dest) {
            Ok(dest) => self.copy_next(op, files, dest),
            Err(e) => {
                let files: Vec<PathBuf> = files.into();
                self.fail_all(op, &files, &e.to_string());
            }
        }
    }

    /// Copies the next session of the queue; each is its own request, one after the other.
    fn copy_next(&mut self, op: u64, mut files: VecDeque<PathBuf>, dest: PathBuf) {
        let Some(src) = files.pop_front() else {
            return;
        };
        let done_path = src.clone();
        self.gw.mutate(
            Mutation::Copy {
                file: src,
                dest_cwd: dest.clone(),
            },
            move |app, r| {
                let outcome = r.map(|_| Done::Copied);
                app.on_step(op, &done_path, outcome, None);
                app.copy_next(op, files, dest);
            },
        );
    }

    // --------------------------------------------------------------- move

    fn request_move_sessions(&mut self, paths: Vec<PathBuf>, dest: PathBuf) {
        if !self.omp_found() || self.sessions.op.is_some() {
            return;
        }
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let dest_c = canon(&dest);
        let model = &self.sessions.model;
        let rows: Vec<_> = paths
            .iter()
            .filter_map(|p| model.row_by_path(p))
            .filter(|r| canon(&r.entry.cwd) != dest_c)
            .collect();
        if rows.is_empty() {
            self.toasts.info(
                &self.ctx,
                "Nothing to move: the sessions are already there.",
            );
            return;
        }
        let running = rows
            .iter()
            .filter(|r| r.live.running || r.live.in_tab)
            .count();
        let mut body = format!(
            "Move {} to {}? omp moves each session file and its artifacts; the originals are \
             removed from their projects.",
            plural(rows.len(), "session", "sessions"),
            abbreviate_home(&dest),
        );
        if running > 0 {
            body.push_str(&format!(" {running} running or open — stopped first."));
        }
        let paths: Vec<PathBuf> = rows.iter().map(|r| r.entry.path.clone()).collect();
        self.confirm = Some((
            Confirm {
                title: "Move sessions?".to_owned(),
                body,
                confirm_label: "Move".to_owned(),
                danger: running > 0,
            },
            PendingConfirm::MoveSessions { paths, dest },
        ));
    }

    pub(super) fn execute_move_sessions(&mut self, paths: Vec<PathBuf>, dest: PathBuf) {
        if !self.omp_found() || self.sessions.op.is_some() {
            return;
        }
        let claimed = self.claim_rows(&paths);
        let entries = self.partition_by_dest(claimed, &dest);
        if entries.is_empty() {
            return;
        }
        let op = self.begin_op(OpKind::Move, &entries, Some(dest.clone()));
        // Tabs close now and come back one by one as their session finishes.
        let preps: VecDeque<MovePrep> = entries
            .iter()
            .map(|e| self.prepare_move(session_entry(e)))
            .collect();
        match absolute(&dest) {
            Ok(dest) => self.move_next(op, preps, dest),
            Err(e) => {
                for prep in preps {
                    let path = prep.entry.path.clone();
                    let err = Err(e.to_string());
                    let tab = self.finish_move(prep, &err, &dest);
                    self.on_step(op, &path, err.map(|o| Done::Moved(o.kind)), tab);
                }
            }
        }
    }

    /// Moves the next session of the queue; each is its own request, one after the other.
    fn move_next(&mut self, op: u64, mut preps: VecDeque<MovePrep>, dest: PathBuf) {
        let Some(prep) = preps.pop_front() else {
            return;
        };
        let request = Mutation::Move {
            file: prep.entry.path.clone(),
            dest_cwd: dest.clone(),
            stop: prep.kill_names.clone(),
        };
        self.gw.mutate(request, move |app, r| {
            let result = r.and_then(|d| match d {
                Reply::Moved(o) => Ok(o),
                _ => Err("the gateway answered with something else".to_owned()),
            });
            let path = prep.entry.path.clone();
            let tab = app.finish_move(prep, &result, &dest);
            app.on_step(op, &path, result.map(|o| Done::Moved(o.kind)), tab);
            app.move_next(op, preps, dest);
        });
    }

    /// The tab of a finished move, ready to come back (see [`MovePrep::finish`]).
    pub(super) fn finish_move(
        &self,
        prep: MovePrep,
        result: &Result<MoveOutcome, String>,
        dest: &Path,
    ) -> Option<Box<MovedTab>> {
        let snap = self.running_snapshot();
        prep.finish(result, dest, self.backends_available(), &|n| {
            snap.is_alive(n)
        })
    }

    // --------------------------------------------------------------- stop

    fn request_stop_sessions(&mut self, paths: Vec<PathBuf>) {
        if self.sessions.op.is_some() {
            return;
        }
        let model = &self.sessions.model;
        let rows: Vec<_> = paths
            .iter()
            .filter_map(|p| model.row_by_path(p))
            .filter(|r| r.live.running)
            .collect();
        if rows.is_empty() {
            self.toasts
                .info(&self.ctx, "None of those sessions is running anymore.");
            return;
        }
        let busy = rows.iter().filter(|r| r.live.busy).count();
        let mut body = format!(
            "{} will be terminated. The session files stay, so you can resume them later.",
            plural(rows.len(), "running session", "running sessions"),
        );
        if busy > 0 {
            body.push_str(&format!(" {busy} of them is working right now."));
        }
        let title = format!(
            "Kill {}?",
            plural(rows.len(), "session process", "session processes")
        );
        let paths: Vec<PathBuf> = rows.iter().map(|r| r.entry.path.clone()).collect();
        self.confirm = Some((
            Confirm {
                title,
                body,
                confirm_label: "Kill".to_owned(),
                danger: true,
            },
            PendingConfirm::StopSessions(paths),
        ));
    }

    pub(super) fn execute_stop_sessions(&mut self, paths: Vec<PathBuf>) {
        if self.sessions.op.is_some() {
            return;
        }
        let entries = self.claim_rows(&paths);
        // Tabs stay open (they show the exit); only the processes die.
        let mut jobs: Vec<(PathBuf, Vec<String>)> = Vec::new();
        for e in &entries {
            let names = self.session_kill_names(&session_entry(e), false);
            jobs.push((e.path.clone(), names));
        }
        if entries.is_empty() {
            return;
        }
        let op = self.begin_op(OpKind::Stop, &entries, None);
        for (path, names) in jobs {
            if names.is_empty() {
                self.on_step(op, &path, Err("it is not running anymore".to_owned()), None);
                continue;
            }
            self.gw
                .mutate(Mutation::KillNames { names, wait: false }, move |app, r| {
                    let outcome = match r {
                        Ok(Reply::Killed(failed)) => failed
                            .into_iter()
                            .next()
                            .map_or(Ok(Done::Stopped), |(_, why)| Err(why)),
                        Ok(_) => Err("the gateway answered with something else".to_owned()),
                        Err(e) => Err(e),
                    };
                    app.on_step(op, &path, outcome, None);
                });
        }
    }
}

/// The message of a failure without omp's source excerpts and stack frames: the first
/// non-empty line that is not a `at …` frame (the full text goes to the log).
fn failure_line(text: &str) -> &str {
    let text = crate::summary::strip_context(text);
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("at "))
        .unwrap_or_else(|| text.trim())
}

#[cfg(test)]
mod tests {
    #[test]
    fn failure_line_drops_stack_frames() {
        let raw = "Could not restore model\n    at foo (bundle.js:1:2)\n    at bar (bundle.js:3:4)";
        assert_eq!(failure_line(raw), "Could not restore model");
        assert_eq!(failure_line("plain"), "plain");
    }

    use super::*;

    #[test]
    fn common_cwd_is_none_for_a_mix_or_nothing() {
        let a = PathBuf::from("/p");
        let b = PathBuf::from("/q");
        assert_eq!(
            common_cwd([a.as_path(), a.as_path()].into_iter()),
            Some(a.clone())
        );
        assert_eq!(common_cwd([a.as_path(), b.as_path()].into_iter()), None);
        assert_eq!(common_cwd(std::iter::empty()), None);
    }

    #[test]
    fn plural_forms() {
        assert_eq!(plural(1, "session", "sessions"), "1 session");
        assert_eq!(plural(0, "session", "sessions"), "0 sessions");
        assert_eq!(plural(12, "session", "sessions"), "12 sessions");
    }
}
