//! The session manager's data model: the inventory rows, their display cells, filters, sorting,
//! selection and totals. Pure logic (no egui painting, no I/O), so every rule is unit-tested;
//! [`crate::sessions_view`] only lays it out and [`crate::app`]'s `session_manager` feeds it.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use deck_core::inventory::{InventoryEntry, SessionStats};
use deck_core::platform::TreeMemory;
use deck_core::sessions::abbreviate_home;
use deck_core::status::TurnOutcome;

use crate::app::human_size;
use crate::sidebar::format_age;
use crate::usage_fmt::money;

/// `1 session`, `3 sessions`.
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const MIB: u64 = 1024 * 1024;

/// What the app knows about a session's process right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Live {
    /// An omp process runs the session (tmux / host monitor).
    pub running: bool,
    /// A terminal tab shows the session.
    pub in_tab: bool,
    /// omp is mid-turn (a tab's title says so).
    pub busy: bool,
}

/// Statistics of one session as far as the worker got.
#[derive(Clone, Debug, PartialEq)]
pub enum StatsSlot {
    Pending,
    Ready(Box<SessionStats>),
    Failed(String),
}

/// A status badge in the table's last column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Badge {
    Running,
    InTab,
    Busy,
    /// The last turn ended in an error.
    Error,
    /// The last turn was aborted.
    Aborted,
    /// Fork of the session with this label (`None` = the parent is not in the store).
    Fork(Option<String>),
    FolderMissing,
    /// Pinned in omp's `session-pins.json`.
    Pinned,
    Stub,
}

impl Badge {
    pub fn text(&self) -> std::borrow::Cow<'static, str> {
        use std::borrow::Cow::{Borrowed, Owned};
        match self {
            Badge::Running => Borrowed("Running"),
            Badge::InTab => Borrowed("Open in tab"),
            Badge::Busy => Borrowed("Busy"),
            Badge::Error => Borrowed("Error"),
            Badge::Aborted => Borrowed("Aborted"),
            Badge::Fork(Some(p)) => Owned(format!("Fork of {p}")),
            Badge::Fork(None) => Borrowed("Fork"),
            Badge::FolderMissing => Borrowed("Folder missing"),
            Badge::Pinned => Borrowed("Pinned"),
            Badge::Stub => Borrowed("Stub"),
        }
    }

    /// The pill text in the table: short enough to fit the Status column (the tooltip and the
    /// detail pane carry [`Badge::text`]).
    pub fn short(&self) -> &'static str {
        match self {
            Badge::Running => "Running",
            Badge::InTab => "In tab",
            Badge::Busy => "Busy",
            Badge::Error => "Error",
            Badge::Aborted => "Aborted",
            Badge::Fork(_) => "Fork",
            Badge::FolderMissing => "No folder",
            Badge::Pinned => "Pinned",
            Badge::Stub => "Stub",
        }
    }
}

/// A project label split into its folder name and the (home-abbreviated) folder holding it:
/// `~/PersProject/OmpDeck` -> (`OmpDeck`, `~/PersProject`). A label without a parent (a worktree
/// label, `~`) has none.
pub fn split_project_label(label: &str) -> (&str, Option<&str>) {
    match label.rsplit_once('/') {
        Some((parent, base)) if !base.is_empty() => {
            (base, Some(if parent.is_empty() { "/" } else { parent }))
        }
        _ => (label, None),
    }
}

/// Pre-formatted text of a row (rebuilt only when the data changes, never per frame).
#[derive(Clone, Debug, Default)]
pub struct Cells {
    pub title: String,
    pub project: String,
    pub size: String,
    /// Memory of the running process tree (empty for sessions that do not run).
    pub ram: String,
    pub messages: String,
    pub tokens: String,
    pub cost: String,
    pub model: String,
    pub created: String,
    pub activity: String,
}

pub struct Row {
    pub entry: InventoryEntry,
    pub stats: StatsSlot,
    pub live: Live,
    /// Memory of the process tree running the session (from the Running poll).
    pub ram: Option<TreeMemory>,
    pub badges: Vec<Badge>,
    /// Pinned in omp (`session-pins.json`, keyed by session id).
    pub pinned: bool,
    pub cells: Cells,
    /// Laid-out cell text, reused across frames (see [`crate::paint_cache`]).
    pub paint: std::cell::RefCell<crate::paint_cache::PaintCache>,
    /// Lower-cased title for sorting.
    title_key: String,
    project_key: String,
    /// Lower-cased title + prompt + cwd: what the text filter searches.
    haystack: String,
}

impl Row {
    /// Title, else the first prompt, else `(empty)`.
    pub fn label(&self) -> &str {
        title_of(&self.entry)
    }

    pub fn stats(&self) -> Option<&SessionStats> {
        match &self.stats {
            StatsSlot::Ready(s) => Some(s),
            _ => None,
        }
    }

    /// Last entry written to the session (from the stats), else the file's mtime.
    pub fn activity(&self) -> SystemTime {
        self.stats()
            .and_then(|s| s.last_activity)
            .unwrap_or(self.entry.modified)
    }

    pub fn path(&self) -> &Path {
        &self.entry.path
    }
}

fn title_of(e: &InventoryEntry) -> &str {
    e.title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| e.first_prompt.as_deref().filter(|t| !t.trim().is_empty()))
        .unwrap_or("(empty)")
}

// ------------------------------------------------------------------ filter

/// The "Older than" chip: off, then 7 / 30 / 90 days (by last activity).
pub const OLDER_STEPS: [u32; 3] = [7, 30, 90];
/// The "Larger than" chip: off, then 10 / 100 MB.
pub const LARGER_STEPS: [u64; 2] = [10, 100];

/// Text filter plus the quick chips; every active criterion must hold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub text: String,
    pub older_days: Option<u32>,
    pub larger_mb: Option<u64>,
    pub stubs: bool,
    pub missing: bool,
    pub not_running: bool,
    pub running: bool,
}

impl Filter {
    pub fn is_active(&self) -> bool {
        *self != Filter::default()
    }

    /// Off → first step → … → last step → off.
    pub fn cycle_older(&mut self) {
        self.older_days = next_step(&OLDER_STEPS, self.older_days);
    }

    pub fn cycle_larger(&mut self) {
        self.larger_mb = next_step(&LARGER_STEPS, self.larger_mb);
    }

    pub fn matches(&self, row: &Row, now: SystemTime) -> bool {
        let needle = self.text.trim();
        if !needle.is_empty() {
            let lower = needle.to_lowercase();
            if !row.haystack.contains(&lower) && !row.entry.id.to_lowercase().starts_with(&lower) {
                return false;
            }
        }
        if let Some(days) = self.older_days {
            let cutoff = now
                .checked_sub(DAY * days)
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if row.activity() > cutoff {
                return false;
            }
        }
        if let Some(mb) = self.larger_mb
            && row.entry.total_bytes() < mb * MIB
        {
            return false;
        }
        if self.stubs && !row.entry.is_stub() {
            return false;
        }
        if self.missing && row.entry.cwd_exists {
            return false;
        }
        if self.running && !row.live.running {
            return false;
        }
        !(self.not_running && row.live.running)
    }
}

fn next_step<T: Copy + PartialEq>(steps: &[T], cur: Option<T>) -> Option<T> {
    match cur {
        None => steps.first().copied(),
        Some(c) => {
            let i = steps.iter().position(|s| *s == c)?;
            steps.get(i + 1).copied()
        }
    }
}

// -------------------------------------------------------------------- sort

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    Title,
    Project,
    Size,
    Ram,
    Messages,
    Tokens,
    Cost,
    Model,
    Created,
    Activity,
    Status,
}

impl Column {
    /// Text columns start ascending (A→Z), the rest descending (biggest / newest first).
    pub fn first_desc(self) -> bool {
        !matches!(self, Column::Title | Column::Project | Column::Model)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub col: Column,
    pub desc: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Sort {
            col: Column::Activity,
            desc: true,
        }
    }
}

impl Sort {
    /// Header click: the active column flips, another one starts in its natural direction.
    pub fn click(&mut self, col: Column) {
        if self.col == col {
            self.desc = !self.desc;
        } else {
            *self = Sort {
                col,
                desc: col.first_desc(),
            };
        }
    }
}

/// Rows without a value (stats not computed yet, no model, no created time) sort last in both
/// directions.
pub(crate) fn by_key<T>(
    a: Option<T>,
    b: Option<T>,
    desc: bool,
    cmp: impl Fn(&T, &T) -> Ordering,
) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let o = cmp(&x, &y);
            if desc { o.reverse() } else { o }
        }
    }
}

fn status_rank(row: &Row) -> u8 {
    // Most urgent first when descending: running > busy > error/aborted > in tab > missing > stub.
    let mut rank = 0;
    for b in &row.badges {
        rank = rank.max(match b {
            Badge::Running => 7,
            Badge::Busy => 6,
            Badge::Error => 5,
            Badge::Aborted => 4,
            Badge::InTab => 3,
            Badge::FolderMissing => 2,
            Badge::Stub => 1,
            Badge::Fork(_) | Badge::Pinned => 0,
        });
    }
    rank
}

fn cmp_rows(a: &Row, b: &Row, sort: Sort) -> Ordering {
    let d = sort.desc;
    let f64_cmp = |x: &f64, y: &f64| x.total_cmp(y);
    let main = match sort.col {
        Column::Title => by_key(Some(&a.title_key), Some(&b.title_key), d, |x, y| x.cmp(y)),
        Column::Project => by_key(Some(&a.project_key), Some(&b.project_key), d, |x, y| {
            x.cmp(y)
        }),
        Column::Size => by_key(
            Some(a.entry.total_bytes()),
            Some(b.entry.total_bytes()),
            d,
            u64::cmp,
        ),
        Column::Ram => by_key(a.ram.map(|m| m.bytes), b.ram.map(|m| m.bytes), d, u64::cmp),
        Column::Messages => by_key(
            a.stats().map(|s| (s.user_messages, s.assistant_turns)),
            b.stats().map(|s| (s.user_messages, s.assistant_turns)),
            d,
            |x, y| x.cmp(y),
        ),
        Column::Tokens => by_key(
            a.stats().map(|s| s.input_tokens + s.output_tokens),
            b.stats().map(|s| s.input_tokens + s.output_tokens),
            d,
            u64::cmp,
        ),
        Column::Cost => by_key(
            a.stats().map(|s| s.cost + s.subagent_cost),
            b.stats().map(|s| s.cost + s.subagent_cost),
            d,
            f64_cmp,
        ),
        Column::Model => by_key(
            a.stats()
                .and_then(|s| s.models.first())
                .map(|m| m.to_lowercase()),
            b.stats()
                .and_then(|s| s.models.first())
                .map(|m| m.to_lowercase()),
            d,
            |x, y| x.cmp(y),
        ),
        Column::Created => by_key(a.entry.created, b.entry.created, d, |x, y| x.cmp(y)),
        Column::Activity => by_key(Some(a.activity()), Some(b.activity()), d, |x, y| x.cmp(y)),
        Column::Status => by_key(Some(status_rank(a)), Some(status_rank(b)), d, u8::cmp),
    };
    // Pinned sessions come first whatever the column (omp's resume list does the same); ties:
    // most recent activity first, then the path (a stable, total order).
    b.pinned
        .cmp(&a.pinned)
        .then(main)
        .then_with(|| b.activity().cmp(&a.activity()))
        .then_with(|| a.entry.path.cmp(&b.entry.path))
}

// --------------------------------------------------------------- selection

/// Checked rows (by session file) and the anchor of Shift+click ranges.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    set: HashSet<PathBuf>,
    anchor: Option<PathBuf>,
}

impl Selection {
    pub fn contains(&self, p: &Path) -> bool {
        self.set.contains(p)
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    pub fn clear(&mut self) {
        self.set.clear();
        self.anchor = None;
    }
}

/// State of the header checkbox over the visible rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    None,
    Some,
    All,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub count: usize,
    pub bytes: u64,
}

/// One project folder in the left pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub cwd: PathBuf,
    pub label: String,
    pub count: usize,
    pub bytes: u64,
    pub missing: bool,
}

// ------------------------------------------------------------------- model

pub struct Model {
    rows: Vec<Row>,
    /// Statistics by session file; survives rescans (a changed file is recomputed by the worker).
    stats: HashMap<PathBuf, StatsSlot>,
    live: HashMap<PathBuf, Live>,
    ram: HashMap<PathBuf, TreeMemory>,
    /// Display name of a project (worktrees show their branch); others are `~`-abbreviated.
    labels: HashMap<PathBuf, String>,
    errors: HashMap<PathBuf, String>,
    /// Ids of the pinned sessions.
    pins: HashSet<String>,
    pub filter: Filter,
    pub sort: Sort,
    /// Project the table is scoped to (`None` = all projects).
    pub scope: Option<PathBuf>,
    pub sel: Selection,
    /// The session whose detail pane is open.
    pub detail: Option<PathBuf>,
    visible: Vec<usize>,
    projects: Vec<Project>,
    index: HashMap<PathBuf, usize>,
    /// Row order / filter result is out of date.
    view_stale: bool,
    /// Display cells are out of date.
    cells_stale: bool,
    /// Bumped whenever the display cells are rebuilt (invalidates laid-out text).
    cells_rev: u64,
    cells_minute: u64,
    now: SystemTime,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            rows: Vec::new(),
            stats: HashMap::new(),
            live: HashMap::new(),
            ram: HashMap::new(),
            labels: HashMap::new(),
            errors: HashMap::new(),
            pins: HashSet::new(),
            filter: Filter::default(),
            sort: Sort::default(),
            scope: None,
            sel: Selection::default(),
            detail: None,
            visible: Vec::new(),
            projects: Vec::new(),
            index: HashMap::new(),
            view_stale: true,
            cells_stale: true,
            cells_rev: 0,
            cells_minute: 0,
            now: SystemTime::UNIX_EPOCH,
        }
    }
}

fn minute_of(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 60)
}

impl Model {
    /// Revision of the display cells.
    pub fn cells_rev(&self) -> u64 {
        self.cells_rev
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn row_by_path(&self, p: &Path) -> Option<&Row> {
        self.index.get(p).and_then(|&i| self.rows.get(i))
    }

    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    /// Row indices (into [`Model::rows`]) that pass the scope and filters, in sort order.
    pub fn visible(&self) -> &[usize] {
        &self.visible
    }

    pub fn error(&self, p: &Path) -> Option<&str> {
        self.errors.get(p).map(String::as_str)
    }

    pub fn set_error(&mut self, p: &Path, msg: String) {
        self.errors.insert(p.to_path_buf(), msg);
    }

    pub fn clear_errors(&mut self) {
        self.errors.clear();
    }

    /// Sessions whose stats have not arrived.
    pub fn pending_stats(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.stats == StatsSlot::Pending)
            .count()
    }

    /// Replace the inventory (after a scan). Keeps stats of surviving files, drops selection,
    /// detail and scope that no longer exist.
    pub fn set_entries(&mut self, entries: Vec<InventoryEntry>) {
        // Per-path state survives only while the file still holds the same session: a path that
        // reappears with another id is a different session (replaced file).
        let old_ids: HashMap<&Path, &str> = self
            .rows
            .iter()
            .map(|r| (r.entry.path.as_path(), r.entry.id.as_str()))
            .collect();
        let keep: HashSet<PathBuf> = entries
            .iter()
            .filter(|e| old_ids.get(e.path.as_path()).is_none_or(|id| *id == e.id))
            .map(|e| e.path.clone())
            .collect();
        drop(old_ids);
        self.stats.retain(|p, _| keep.contains(p));
        self.errors.retain(|p, _| keep.contains(p));
        self.rows = entries
            .into_iter()
            .map(|entry| Row {
                stats: self
                    .stats
                    .get(&entry.path)
                    .cloned()
                    .unwrap_or(StatsSlot::Pending),
                live: self.live.get(&entry.path).copied().unwrap_or_default(),
                ram: self.ram.get(&entry.path).copied(),
                pinned: self.pins.contains(&entry.id),
                entry,
                badges: Vec::new(),
                cells: Cells::default(),
                paint: Default::default(),
                title_key: String::new(),
                project_key: String::new(),
                haystack: String::new(),
            })
            .collect();
        self.index = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.entry.path.clone(), i))
            .collect();
        let alive = keep;
        self.sel.set.retain(|p| alive.contains(p));
        if self.sel.anchor.as_ref().is_some_and(|a| !alive.contains(a)) {
            self.sel.anchor = None;
        }
        if self.detail.as_ref().is_some_and(|d| !alive.contains(d)) {
            self.detail = None;
        }
        if let Some(scope) = &self.scope
            && !self.rows.iter().any(|r| &r.entry.cwd == scope)
        {
            self.scope = None;
        }
        self.cells_stale = true;
        self.view_stale = true;
    }

    /// A statistics result from the worker.
    pub fn set_stats(&mut self, path: &Path, result: Result<SessionStats, String>) {
        let slot = match result {
            Ok(s) => StatsSlot::Ready(Box::new(s)),
            Err(e) => StatsSlot::Failed(e),
        };
        self.stats.insert(path.to_path_buf(), slot.clone());
        if let Some(&i) = self.index.get(path) {
            self.rows[i].stats = slot;
            self.cells_stale = true;
            self.view_stale = true;
        }
    }

    /// Memory of the process tree running each session file (missing = not running or unread).
    pub fn set_ram(&mut self, ram: HashMap<PathBuf, TreeMemory>) {
        if ram == self.ram {
            return;
        }
        self.ram = ram;
        for r in &mut self.rows {
            r.ram = self.ram.get(&r.entry.path).copied();
        }
        self.cells_stale = true;
        self.view_stale = true;
    }

    /// Process / tab state per session file (missing = not live).
    pub fn set_live(&mut self, live: HashMap<PathBuf, Live>) {
        if live == self.live {
            return;
        }
        self.live = live;
        for r in &mut self.rows {
            r.live = self.live.get(&r.entry.path).copied().unwrap_or_default();
        }
        self.cells_stale = true;
        self.view_stale = true;
    }

    pub fn set_labels(&mut self, labels: HashMap<PathBuf, String>) {
        if labels == self.labels {
            return;
        }
        self.labels = labels;
        self.cells_stale = true;
    }

    /// The pinned session ids (omp's `session-pins.json`). Rows are re-flagged and re-sorted.
    pub fn set_pins(&mut self, pins: HashSet<String>) {
        if pins == self.pins {
            return;
        }
        self.pins = pins;
        for r in &mut self.rows {
            r.pinned = self.pins.contains(&r.entry.id);
        }
        self.cells_stale = true;
        self.view_stale = true;
    }

    /// Indices into [`Model::rows`] of the sessions the resume list shows: those of `scope`
    /// (`None` = every project) that match `text`, pinned first, then the most recently active
    /// (omp's resume picker order). Call after [`Model::prepare`].
    pub fn resume_order(&self, scope: Option<&Path>, text: &str) -> Vec<usize> {
        let filter = Filter {
            text: text.to_owned(),
            ..Filter::default()
        };
        let sort = Sort::default();
        let mut out: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                scope.is_none_or(|s| r.entry.cwd == s) && filter.matches(r, self.now)
            })
            .collect();
        out.sort_by(|&a, &b| cmp_rows(&self.rows[a], &self.rows[b], sort));
        out
    }

    pub fn project_label(&self, cwd: &Path) -> String {
        self.labels
            .get(cwd)
            .cloned()
            .unwrap_or_else(|| abbreviate_home(cwd))
    }

    /// Bring cells, order and projects up to date. Cheap when nothing changed; call once per
    /// frame with the current time.
    pub fn prepare(&mut self, now: SystemTime) {
        self.now = now;
        let minute = minute_of(now);
        if minute != self.cells_minute {
            self.cells_minute = minute;
            self.cells_stale = true;
        }
        if self.cells_stale {
            self.rebuild_cells();
            self.cells_stale = false;
            self.cells_rev += 1;
            self.view_stale = true;
        }
        if self.view_stale {
            self.rebuild_view();
            self.view_stale = false;
        }
    }

    /// Filter / sort / scope changed (the fields are public).
    pub fn mark_view_dirty(&mut self) {
        self.view_stale = true;
    }

    fn rebuild_cells(&mut self) {
        let now = self.now;
        let titles: Vec<String> = self
            .rows
            .iter()
            .map(|r| title_of(&r.entry).to_owned())
            .collect();
        let labels: Vec<String> = self
            .rows
            .iter()
            .map(|r| self.project_label(&r.entry.cwd))
            .collect();
        let parents: Vec<Option<usize>> = self
            .rows
            .iter()
            .map(|r| {
                r.entry
                    .parent
                    .as_ref()
                    .and_then(|p| self.index.get(p).copied())
            })
            .collect();
        for (i, label) in labels.into_iter().enumerate() {
            let fork = self.rows[i]
                .entry
                .parent
                .is_some()
                .then(|| parents[i].map(|p| truncate(&titles[p], 28)));
            let r = &mut self.rows[i];
            r.badges = badges(r, fork);
            r.cells = cells(r, &titles[i], &label, now);
            r.title_key = titles[i].to_lowercase();
            r.haystack = format!(
                "{}\n{}\n{}",
                r.entry.title.as_deref().unwrap_or_default(),
                r.entry.first_prompt.as_deref().unwrap_or_default(),
                r.entry.cwd.display()
            )
            .to_lowercase();
            r.project_key = label.to_lowercase();
        }
    }

    fn rebuild_view(&mut self) {
        let mut projects: HashMap<&Path, Project> = HashMap::new();
        for r in &self.rows {
            let p = projects
                .entry(r.entry.cwd.as_path())
                .or_insert_with(|| Project {
                    cwd: r.entry.cwd.clone(),
                    label: self.project_label(&r.entry.cwd),
                    count: 0,
                    bytes: 0,
                    missing: !r.entry.cwd_exists,
                });
            p.count += 1;
            p.bytes += r.entry.total_bytes();
        }
        let mut projects: Vec<Project> = projects.into_values().collect();
        projects.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.label.cmp(&b.label)));
        self.projects = projects;

        let (now, sort) = (self.now, self.sort);
        let mut vis: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                self.scope.as_ref().is_none_or(|s| &r.entry.cwd == s) && self.filter.matches(r, now)
            })
            .collect();
        vis.sort_by(|&a, &b| cmp_rows(&self.rows[a], &self.rows[b], sort));
        self.visible = vis;
    }

    // ------------------------------------------------------------- totals

    /// Everything in the current scope (before the filters).
    pub fn scope_totals(&self) -> Totals {
        self.rows
            .iter()
            .filter(|r| self.scope.as_ref().is_none_or(|s| &r.entry.cwd == s))
            .fold(Totals::default(), |t, r| Totals {
                count: t.count + 1,
                bytes: t.bytes + r.entry.total_bytes(),
            })
    }

    /// The selected rows: how many and the bytes deleting them frees.
    pub fn selection_totals(&self) -> Totals {
        self.rows
            .iter()
            .filter(|r| self.sel.contains(&r.entry.path))
            .fold(Totals::default(), |t, r| Totals {
                count: t.count + 1,
                bytes: t.bytes + r.entry.total_bytes(),
            })
    }

    /// The selected rows, in table order.
    pub fn selected_rows(&self) -> Vec<&Row> {
        let mut out: Vec<&Row> = self
            .rows
            .iter()
            .filter(|r| self.sel.contains(&r.entry.path))
            .collect();
        out.sort_by(|a, b| cmp_rows(a, b, self.sort));
        out
    }

    // ---------------------------------------------------------- selection

    /// Checkbox click on the visible row at `pos`. With Shift and an anchor still visible, every
    /// visible row between the anchor and `pos` is added; otherwise the row toggles.
    pub fn click_row(&mut self, pos: usize, shift: bool) {
        let Some(&i) = self.visible.get(pos) else {
            return;
        };
        let path = self.rows[i].entry.path.clone();
        let anchor_pos = self.sel.anchor.as_ref().and_then(|a| {
            self.visible
                .iter()
                .position(|&v| &self.rows[v].entry.path == a)
        });
        match (shift, anchor_pos) {
            (true, Some(a)) => {
                let (lo, hi) = (a.min(pos), a.max(pos));
                for &v in &self.visible[lo..=hi] {
                    self.sel.set.insert(self.rows[v].entry.path.clone());
                }
            }
            _ => {
                if !self.sel.set.remove(&path) {
                    self.sel.set.insert(path.clone());
                }
            }
        }
        self.sel.anchor = Some(path);
    }

    pub fn header_check(&self) -> Check {
        let n = self
            .visible
            .iter()
            .filter(|&&i| self.sel.contains(&self.rows[i].entry.path))
            .count();
        match n {
            0 => Check::None,
            n if n == self.visible.len() => Check::All,
            _ => Check::Some,
        }
    }

    /// Header checkbox: everything visible on, unless it already is (then those off).
    pub fn toggle_all_visible(&mut self) {
        if self.header_check() == Check::All {
            for &i in &self.visible {
                self.sel.set.remove(&self.rows[i].entry.path);
            }
        } else {
            for &i in &self.visible {
                self.sel.set.insert(self.rows[i].entry.path.clone());
            }
        }
        self.sel.anchor = None;
    }

    /// Select exactly these sessions (after an operation that asked for a retry, tests).
    #[cfg(test)]
    pub fn select_only(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.sel.set = paths.into_iter().collect();
        self.sel.anchor = None;
    }

    /// Drop a session that was just deleted or moved away (the next scan confirms it).
    pub fn forget(&mut self, path: &Path) {
        let entries: Vec<InventoryEntry> = std::mem::take(&mut self.rows)
            .into_iter()
            .map(|r| r.entry)
            .filter(|e| e.path != path)
            .collect();
        self.set_entries(entries);
    }

    /// Rows that are forks of `path` (their header `parentSession`).
    pub fn forks_of(&self, path: &Path) -> Vec<&Row> {
        self.rows
            .iter()
            .filter(|r| r.entry.parent.as_deref() == Some(path))
            .collect()
    }

    pub fn parent_of(&self, row: &Row) -> Option<&Row> {
        row.entry
            .parent
            .as_deref()
            .and_then(|p| self.row_by_path(p))
    }
}

// ------------------------------------------------------------- formatting

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// `2026-10-24 14:03 UTC`.
pub fn utc_stamp(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let date = deck_core::usage::Date::from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!("{date} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

/// `950`, `12.3k`, `4.5M`.
pub fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// A model id shortened for the Model column: provider prefix, `@…` revision, `-latest` and a
/// trailing date are dropped and Claude ids put the version after the family
/// (`anthropic/claude-opus-4-5-20251101` → `opus-4.5`, `claude-3-7-sonnet` → `sonnet-3.7`).
/// Other ids keep their name (`openai/gpt-5.2-codex` → `gpt-5.2-codex`).
pub fn compact_model(id: &str) -> String {
    let name = id.rsplit('/').next().unwrap_or(id);
    let name = name.split('@').next().unwrap_or(name);
    let mut toks: Vec<&str> = name.split('-').collect();
    if toks.last() == Some(&"latest") {
        toks.pop();
    }
    let digits = |t: &str, n: usize| t.len() == n && t.bytes().all(|b| b.is_ascii_digit());
    match toks.as_slice() {
        [.., y, m, d] if digits(y, 4) && digits(m, 2) && digits(d, 2) => {
            toks.truncate(toks.len() - 3);
        }
        [.., d] if digits(d, 8) => {
            toks.pop();
        }
        _ => {}
    }
    if toks.first() != Some(&"claude") || toks.len() < 2 {
        return toks.join("-");
    }
    let is_ver = |t: &str| t.len() <= 2 && t.bytes().all(|b| b.is_ascii_digit());
    let version = toks[1..]
        .iter()
        .copied()
        .filter(|t| is_ver(t))
        .collect::<Vec<_>>()
        .join(".");
    let mut names = toks[1..].iter().copied().filter(|t| !is_ver(t));
    let Some(family) = names.next() else {
        return toks.join("-");
    };
    let mut out = family.to_owned();
    if !version.is_empty() {
        out.push('-');
        out.push_str(&version);
    }
    for t in names {
        out.push('-');
        out.push_str(t);
    }
    out
}

fn badges(r: &Row, fork: Option<Option<String>>) -> Vec<Badge> {
    let mut out = Vec::new();
    if r.pinned {
        out.push(Badge::Pinned);
    }
    if r.live.running {
        out.push(Badge::Running);
    }
    if r.live.in_tab {
        out.push(Badge::InTab);
    }
    if r.live.busy {
        out.push(Badge::Busy);
    }
    match r.stats().map(|s| &s.last_outcome) {
        Some(TurnOutcome::Error(_)) => out.push(Badge::Error),
        Some(TurnOutcome::Aborted) => out.push(Badge::Aborted),
        _ => {}
    }
    if let Some(parent) = fork {
        out.push(Badge::Fork(parent));
    }
    if !r.entry.cwd_exists {
        out.push(Badge::FolderMissing);
    }
    if r.entry.is_stub() {
        out.push(Badge::Stub);
    }
    out
}

fn cells(r: &Row, title: &str, project: &str, now: SystemTime) -> Cells {
    let e = &r.entry;
    let (messages, tokens, cost, model) = match &r.stats {
        StatsSlot::Ready(s) => (
            format!("{} · {}", s.user_messages, s.assistant_turns),
            format!("{} / {}", compact(s.input_tokens), compact(s.output_tokens)),
            money(s.cost + s.subagent_cost),
            s.models
                .first()
                .map_or_else(|| "—".to_owned(), |m| compact_model(m)),
        ),
        StatsSlot::Pending => ("…".into(), "…".into(), "…".into(), "…".into()),
        StatsSlot::Failed(_) => ("—".into(), "—".into(), "—".into(), "—".into()),
    };
    Cells {
        title: title.to_owned(),
        project: project.to_owned(),
        size: human_size(e.total_bytes()),
        ram: r.ram.map_or_else(String::new, |m| human_size(m.bytes)),
        messages,
        tokens,
        cost,
        model,
        created: e
            .created
            .map_or_else(|| "–".to_owned(), |c| format_age(now, c)),
        activity: format_age(now, r.activity()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn project_labels_split_into_name_and_parent() {
        assert_eq!(
            split_project_label("~/PersProject/OmpDeck"),
            ("OmpDeck", Some("~/PersProject"))
        );
        assert_eq!(split_project_label("/srv"), ("srv", Some("/")));
        assert_eq!(split_project_label("feature-x"), ("feature-x", None));
        assert_eq!(split_project_label("~"), ("~", None));
    }

    #[test]
    fn short_badges_stay_short() {
        assert_eq!(
            Badge::Fork(Some("A long parent title".into())).short(),
            "Fork"
        );
        assert!(Badge::FolderMissing.short().len() <= 9);
    }

    use super::*;

    const NOW_SECS: u64 = 1_800_000_000;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_SECS)
    }

    fn ago(days: u64) -> SystemTime {
        now() - DAY * u32::try_from(days).expect("days")
    }

    fn entry(name: &str, cwd: &str, days_ago: u64, bytes: u64) -> InventoryEntry {
        InventoryEntry {
            id: format!("id-{name}"),
            path: PathBuf::from(format!("/s/{name}.jsonl")),
            cwd: PathBuf::from(cwd),
            title: Some(name.to_owned()),
            first_prompt: Some(format!("prompt of {name}")),
            created: Some(ago(days_ago + 1)),
            modified: ago(days_ago),
            jsonl_bytes: bytes,
            artifact_bytes: 0,
            artifact_files: 0,
            subagents: 0,
            parent: None,
            cwd_exists: true,
        }
    }

    fn model(entries: Vec<InventoryEntry>) -> Model {
        let mut m = Model::default();
        m.set_entries(entries);
        m.prepare(now());
        m
    }

    fn names(m: &Model) -> Vec<String> {
        m.visible()
            .iter()
            .map(|&i| m.rows()[i].entry.title.clone().unwrap_or_default())
            .collect()
    }

    fn stats(user: u32, input: u64, output: u64, cost: f64) -> SessionStats {
        SessionStats {
            user_messages: user,
            input_tokens: input,
            output_tokens: output,
            cost,
            ..SessionStats::default()
        }
    }

    fn apply(m: &mut Model) {
        m.mark_view_dirty();
        m.prepare(now());
    }

    #[test]
    fn default_sort_is_last_activity_newest_first() {
        let m = model(vec![
            entry("old", "/p", 30, 1),
            entry("new", "/p", 1, 1),
            entry("mid", "/p", 10, 1),
        ]);
        assert_eq!(names(&m), ["new", "mid", "old"]);
    }

    #[test]
    fn stats_last_activity_overrides_mtime() {
        let mut m = model(vec![entry("a", "/p", 1, 1), entry("b", "/p", 5, 1)]);
        let s = SessionStats {
            last_activity: Some(ago(0)),
            ..Default::default()
        };
        m.set_stats(Path::new("/s/b.jsonl"), Ok(s));
        m.prepare(now());
        assert_eq!(names(&m), ["b", "a"]);
    }

    #[test]
    fn size_sort_both_directions_with_tie_break_by_activity_then_path() {
        let mut m = model(vec![
            entry("c", "/p", 3, 50),
            entry("a", "/p", 9, 50),
            entry("b", "/p", 3, 50),
            entry("big", "/p", 20, 900),
        ]);
        m.sort = Sort {
            col: Column::Size,
            desc: true,
        };
        apply(&mut m);
        // Equal sizes: the more recent activity first; equal activity: path ascending.
        assert_eq!(names(&m), ["big", "b", "c", "a"]);
        m.sort.desc = false;
        apply(&mut m);
        assert_eq!(names(&m), ["b", "c", "a", "big"]);
    }

    #[test]
    fn title_sort_is_case_insensitive_and_starts_ascending() {
        let mut m = model(vec![
            entry("banana", "/p", 1, 1),
            entry("Apple", "/p", 2, 1),
            entry("cherry", "/p", 3, 1),
        ]);
        let mut sort = Sort::default();
        sort.click(Column::Title);
        assert!(!sort.desc);
        m.sort = sort;
        apply(&mut m);
        assert_eq!(names(&m), ["Apple", "banana", "cherry"]);
        m.sort.click(Column::Title);
        apply(&mut m);
        assert_eq!(names(&m), ["cherry", "banana", "Apple"]);
    }

    #[test]
    fn numeric_columns_start_descending_and_pending_stats_sort_last_either_way() {
        let mut m = model(vec![
            entry("cheap", "/p", 1, 1),
            entry("dear", "/p", 2, 1),
            entry("unknown", "/p", 3, 1),
        ]);
        m.set_stats(Path::new("/s/cheap.jsonl"), Ok(stats(1, 10, 5, 0.5)));
        m.set_stats(Path::new("/s/dear.jsonl"), Ok(stats(9, 900, 90, 7.0)));
        let mut sort = Sort::default();
        sort.click(Column::Cost);
        assert!(sort.desc);
        m.sort = sort;
        m.prepare(now());
        assert_eq!(names(&m), ["dear", "cheap", "unknown"]);
        m.sort.click(Column::Cost);
        apply(&mut m);
        assert_eq!(names(&m), ["cheap", "dear", "unknown"]);
        for col in [Column::Messages, Column::Tokens] {
            m.sort = Sort { col, desc: true };
            apply(&mut m);
            assert_eq!(names(&m), ["dear", "cheap", "unknown"], "{col:?}");
        }
    }

    #[test]
    fn status_sort_puts_running_first_when_descending() {
        let mut m = model(vec![
            entry("idle", "/p", 1, 1),
            entry("stub", "/p", 2, 1),
            entry("live", "/p", 3, 1),
        ]);
        m.rows[1].entry.title = None;
        m.rows[1].entry.first_prompt = None;
        m.set_live(HashMap::from([(
            PathBuf::from("/s/live.jsonl"),
            Live {
                running: true,
                ..Live::default()
            },
        )]));
        m.sort = Sort {
            col: Column::Status,
            desc: true,
        };
        m.prepare(now());
        let order: Vec<&str> = m
            .visible()
            .iter()
            .map(|&i| m.rows()[i].entry.id.as_str())
            .collect();
        assert_eq!(order, ["id-live", "id-stub", "id-idle"]);
    }

    #[test]
    fn text_filter_matches_title_prompt_cwd_and_id_prefix_case_insensitively() {
        let mut e = entry("Refactor", "/home/u/Proj", 1, 1);
        e.first_prompt = Some("Please FIX the parser".into());
        let mut m = model(vec![e, entry("other", "/x", 1, 1)]);
        for (text, hit) in [
            ("refactor", true),
            ("fix the PARSER", true),
            ("u/proj", true),
            ("ID-REF", true),
            ("d-ref", false), // an id *prefix*, not a substring
            ("nothing", false),
            ("  refactor  ", true),
        ] {
            m.filter.text = text.to_owned();
            apply(&mut m);
            let found = names(&m).contains(&"Refactor".to_owned());
            assert_eq!(found, hit, "{text:?}");
        }
    }

    #[test]
    fn chips_combine_with_and_semantics() {
        let mb = MIB;
        let mut stub = entry("stub", "/gone", 40, 100);
        stub.title = None;
        stub.first_prompt = None;
        stub.cwd_exists = false;
        let mut m = model(vec![
            entry("recent-big", "/p", 1, 200 * mb),
            entry("old-big", "/p", 60, 200 * mb),
            entry("old-small", "/p", 60, 1000),
            entry("old-mid", "/p", 100, 20 * mb),
            stub,
        ]);
        m.set_live(HashMap::from([(
            PathBuf::from("/s/old-big.jsonl"),
            Live {
                running: true,
                ..Live::default()
            },
        )]));
        let shown = |m: &mut Model, f: Filter| {
            m.filter = f;
            apply(m);
            let mut n = names(m);
            n.sort();
            n
        };
        let base = Filter::default();
        assert_eq!(shown(&mut m, base.clone()).len(), 5);
        let older30 = Filter {
            older_days: Some(30),
            ..base.clone()
        };
        assert_eq!(
            shown(&mut m, older30.clone()),
            ["", "old-big", "old-mid", "old-small"]
        );
        let older_large = Filter {
            larger_mb: Some(100),
            ..older30.clone()
        };
        assert_eq!(shown(&mut m, older_large.clone()), ["old-big"]);
        let not_running = Filter {
            not_running: true,
            ..older_large
        };
        assert!(shown(&mut m, not_running).is_empty());
        let stubs_missing = Filter {
            stubs: true,
            missing: true,
            older_days: Some(30),
            ..base.clone()
        };
        assert_eq!(shown(&mut m, stubs_missing), [""]);
        let missing_only = Filter {
            missing: true,
            ..base
        };
        assert_eq!(shown(&mut m, missing_only), [""]);
    }

    #[test]
    fn older_than_uses_last_activity_not_file_time() {
        let mut m = model(vec![entry("a", "/p", 100, 1)]);
        let s = SessionStats {
            last_activity: Some(ago(2)),
            ..Default::default()
        };
        m.set_stats(Path::new("/s/a.jsonl"), Ok(s));
        m.filter.older_days = Some(30);
        m.prepare(now());
        assert!(names(&m).is_empty());
    }

    #[test]
    fn chip_steps_cycle_through_the_values_and_back_to_off() {
        let mut f = Filter::default();
        let mut seen = Vec::new();
        for _ in 0..4 {
            f.cycle_older();
            seen.push(f.older_days);
        }
        assert_eq!(seen, [Some(7), Some(30), Some(90), None]);
        f.cycle_larger();
        f.cycle_larger();
        assert_eq!(f.larger_mb, Some(100));
        f.cycle_larger();
        assert_eq!(f.larger_mb, None);
        assert!(!f.is_active());
    }

    #[test]
    fn scope_limits_rows_but_totals_cover_the_scope_not_the_filter() {
        let mut m = model(vec![
            entry("a", "/p", 1, 100),
            entry("b", "/p", 2, 300),
            entry("c", "/q", 3, 1000),
        ]);
        m.scope = Some(PathBuf::from("/p"));
        m.filter.text = "a".into();
        apply(&mut m);
        assert_eq!(names(&m), ["a"]);
        assert_eq!(
            m.scope_totals(),
            Totals {
                count: 2,
                bytes: 400
            }
        );
    }

    #[test]
    fn projects_are_sorted_by_size_with_missing_marker() {
        let mut gone = entry("g", "/gone", 1, 5000);
        gone.cwd_exists = false;
        let m = model(vec![
            entry("a", "/p", 1, 100),
            entry("b", "/p", 2, 300),
            gone,
        ]);
        let p: Vec<(&str, usize, u64, bool)> = m
            .projects()
            .iter()
            .map(|p| (p.cwd.to_str().unwrap_or(""), p.count, p.bytes, p.missing))
            .collect();
        assert_eq!(p, [("/gone", 1, 5000, true), ("/p", 2, 400, false)]);
    }

    #[test]
    fn project_labels_override_the_abbreviated_path() {
        let mut m = model(vec![entry("a", "/wt/x", 1, 1)]);
        m.set_labels(HashMap::from([(
            PathBuf::from("/wt/x"),
            "feature-x".into(),
        )]));
        m.prepare(now());
        assert_eq!(m.projects()[0].label, "feature-x");
        assert_eq!(m.rows()[0].cells.project, "feature-x");
    }

    fn five() -> Model {
        model(vec![
            entry("e1", "/p", 1, 1),
            entry("e2", "/p", 2, 2),
            entry("e3", "/p", 3, 4),
            entry("e4", "/p", 4, 8),
            entry("e5", "/p", 5, 16),
        ])
    }

    fn selected_names(m: &Model) -> Vec<String> {
        let mut v: Vec<String> = m
            .selected_rows()
            .iter()
            .map(|r| r.entry.title.clone().unwrap_or_default())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn click_toggles_and_shift_click_adds_the_visible_range() {
        let mut m = five();
        m.click_row(1, false);
        assert_eq!(selected_names(&m), ["e2"]);
        m.click_row(3, true);
        assert_eq!(selected_names(&m), ["e2", "e3", "e4"]);
        // Shift upwards from the new anchor (e4) reaches e1.
        m.click_row(0, true);
        assert_eq!(selected_names(&m), ["e1", "e2", "e3", "e4"]);
        // A plain click toggles one row and re-anchors.
        m.click_row(2, false);
        assert_eq!(selected_names(&m), ["e1", "e2", "e4"]);
    }

    #[test]
    fn shift_click_without_a_visible_anchor_just_toggles() {
        let mut m = five();
        m.click_row(1, false);
        m.filter.text = "e4".into();
        apply(&mut m); // the anchor (e2) is filtered out
        m.click_row(0, true);
        assert_eq!(selected_names(&m), ["e2", "e4"]);
    }

    #[test]
    fn shift_range_follows_the_current_sort_order() {
        let mut m = five();
        m.sort = Sort {
            col: Column::Size,
            desc: true,
        };
        apply(&mut m); // e5 e4 e3 e2 e1
        m.click_row(0, false);
        m.click_row(2, true);
        assert_eq!(selected_names(&m), ["e3", "e4", "e5"]);
    }

    #[test]
    fn header_checkbox_selects_only_visible_rows_and_toggles_back() {
        let mut m = five();
        m.filter.text = "e".into();
        m.click_row(0, false); // e1 selected
        apply(&mut m);
        assert_eq!(m.header_check(), Check::Some);
        m.filter.text = "e1".into();
        apply(&mut m);
        assert_eq!(m.header_check(), Check::All);
        m.filter.text = String::new();
        apply(&mut m);
        m.toggle_all_visible();
        assert_eq!(m.selection_totals().count, 5);
        assert_eq!(m.header_check(), Check::All);
        m.filter.text = "e3".into();
        apply(&mut m);
        m.toggle_all_visible(); // e3 off, the hidden rows stay selected
        assert_eq!(m.selection_totals().count, 4);
        assert_eq!(m.header_check(), Check::None);
    }

    #[test]
    fn refresh_drops_selection_detail_and_scope_of_vanished_sessions() {
        let mut m = five();
        m.toggle_all_visible();
        m.detail = Some(PathBuf::from("/s/e5.jsonl"));
        m.scope = Some(PathBuf::from("/p"));
        m.set_entries(vec![entry("e1", "/p", 1, 1), entry("e2", "/p", 2, 2)]);
        m.prepare(now());
        assert_eq!(selected_names(&m), ["e1", "e2"]);
        assert_eq!(m.detail, None);
        m.set_entries(vec![entry("n", "/other", 1, 1)]);
        m.prepare(now());
        assert!(m.sel.is_empty());
        assert_eq!(m.scope, None);
    }

    #[test]
    fn selection_totals_sum_total_bytes_of_selected_rows_only() {
        let mut a = entry("a", "/p", 1, 100);
        a.artifact_bytes = 50;
        let mut m = model(vec![a, entry("b", "/p", 2, 7), entry("c", "/p", 3, 1000)]);
        m.select_only([PathBuf::from("/s/a.jsonl"), PathBuf::from("/s/b.jsonl")]);
        assert_eq!(
            m.selection_totals(),
            Totals {
                count: 2,
                bytes: 157
            }
        );
    }

    #[test]
    fn stats_survive_a_rescan_for_files_still_present() {
        let mut m = model(vec![entry("a", "/p", 1, 1), entry("b", "/p", 2, 1)]);
        m.set_stats(Path::new("/s/a.jsonl"), Ok(stats(3, 1, 1, 1.0)));
        m.set_entries(vec![entry("a", "/p", 1, 5)]);
        m.prepare(now());
        assert!(m.rows()[0].stats().is_some());
        assert_eq!(m.pending_stats(), 0);
    }

    #[test]
    fn fork_badge_names_the_parent_and_forks_are_listed() {
        let parent = entry("parent", "/p", 5, 1);
        let mut child = entry("child", "/q", 1, 1);
        child.parent = Some(parent.path.clone());
        let mut orphan = entry("orphan", "/q", 2, 1);
        orphan.parent = Some(PathBuf::from("/s/deleted.jsonl"));
        let m = model(vec![parent, child, orphan]);
        let badges = |name: &str| {
            m.rows()
                .iter()
                .find(|r| r.entry.title.as_deref() == Some(name))
                .map(|r| r.badges.clone())
                .unwrap_or_default()
        };
        assert_eq!(badges("child"), [Badge::Fork(Some("parent".into()))]);
        assert_eq!(badges("orphan"), [Badge::Fork(None)]);
        assert!(badges("parent").is_empty());
        assert_eq!(m.forks_of(Path::new("/s/parent.jsonl")).len(), 1);
    }

    #[test]
    fn stub_title_falls_back_to_prompt_then_empty() {
        let mut a = entry("a", "/p", 1, 1);
        a.title = None;
        let mut b = entry("b", "/p", 2, 1);
        b.title = None;
        b.first_prompt = None;
        let m = model(vec![a, b]);
        assert_eq!(m.rows()[0].cells.title, "prompt of a");
        assert_eq!(m.rows()[1].cells.title, "(empty)");
        assert!(m.rows()[1].badges.contains(&Badge::Stub));
        assert!(!m.rows()[0].badges.contains(&Badge::Stub));
    }

    #[test]
    fn error_and_aborted_last_turns_get_badges() {
        let mut m = model(vec![entry("e", "/p", 1, 1), entry("x", "/p", 2, 1)]);
        let s = SessionStats {
            last_outcome: TurnOutcome::Error(Some("boom".into())),
            ..Default::default()
        };
        m.set_stats(Path::new("/s/e.jsonl"), Ok(s));
        let s = SessionStats {
            last_outcome: TurnOutcome::Aborted,
            ..Default::default()
        };
        m.set_stats(Path::new("/s/x.jsonl"), Ok(s));
        m.prepare(now());
        assert!(m.rows()[0].badges.contains(&Badge::Error));
        assert!(m.rows()[1].badges.contains(&Badge::Aborted));
    }

    #[test]
    fn state_is_dropped_when_a_path_comes_back_with_another_session_id() {
        let mut m = model(vec![entry("a", "/p", 1, 1), entry("b", "/p", 2, 1)]);
        let (pa, pb) = (PathBuf::from("/s/a.jsonl"), PathBuf::from("/s/b.jsonl"));
        m.set_stats(&pa, Ok(stats(3, 1, 1, 0.5)));
        m.set_stats(&pb, Ok(stats(4, 1, 1, 0.5)));
        m.set_error(&pa, "boom".to_owned());
        m.select_only([pa.clone(), pb.clone()]);
        m.sel.anchor = Some(pa.clone());
        m.detail = Some(pa.clone());
        let mut replaced = entry("a", "/p", 1, 1);
        replaced.id = "id-other".to_owned();
        m.set_entries(vec![replaced, entry("b", "/p", 2, 1)]);
        assert!(!m.sel.contains(&pa) && m.sel.contains(&pb));
        assert!(m.sel.anchor.is_none() && m.detail.is_none());
        assert!(m.error(&pa).is_none());
        assert_eq!(
            m.row_by_path(&pa).map(|r| r.stats.clone()),
            Some(StatsSlot::Pending)
        );
        assert!(m.row_by_path(&pb).and_then(|r| r.stats()).is_some());
    }

    #[test]
    fn ram_column_sorts_unread_rows_last_and_the_running_chip_filters() {
        let mut m = model(vec![
            entry("a", "/p", 1, 1),
            entry("b", "/p", 2, 1),
            entry("c", "/p", 3, 1),
        ]);
        let mem = |bytes| TreeMemory {
            bytes,
            processes: 2,
        };
        m.set_ram(
            [
                (PathBuf::from("/s/a.jsonl"), mem(10 * MIB)),
                (PathBuf::from("/s/c.jsonl"), mem(300 * MIB)),
            ]
            .into(),
        );
        m.sort = Sort {
            col: Column::Ram,
            desc: true,
        };
        apply(&mut m);
        assert_eq!(names(&m), ["c", "a", "b"]);
        assert_eq!(m.rows()[1].cells.ram, "", "no reading, no cell");
        assert_eq!(m.rows()[0].cells.ram, "10.0 MB");
        m.sort.desc = false;
        apply(&mut m);
        assert_eq!(
            names(&m),
            ["a", "c", "b"],
            "unread rows stay last ascending too"
        );

        m.set_live(
            [(
                PathBuf::from("/s/a.jsonl"),
                Live {
                    running: true,
                    ..Live::default()
                },
            )]
            .into(),
        );
        m.filter.running = true;
        apply(&mut m);
        assert_eq!(names(&m), ["a"]);
    }

    #[test]
    fn model_names_are_compacted() {
        let c = compact_model;
        assert_eq!(c("claude-opus-4-5"), "opus-4.5");
        assert_eq!(c("anthropic/claude-opus-4-5-20251101"), "opus-4.5");
        assert_eq!(c("claude-sonnet-4-5@20250929"), "sonnet-4.5");
        assert_eq!(c("claude-3-7-sonnet-20250219"), "sonnet-3.7");
        assert_eq!(c("claude-haiku-4-5-latest"), "haiku-4.5");
        assert_eq!(c("claude-opus-4"), "opus-4");
        assert_eq!(c("claude-opus-4-5-thinking"), "opus-4.5-thinking");
        assert_eq!(c("openai/gpt-5.2-codex"), "gpt-5.2-codex");
        assert_eq!(c("gemini-2.5-pro-2025-06-05"), "gemini-2.5-pro");
        assert_eq!(c("—"), "—");
    }

    #[test]
    fn compact_counts() {
        assert_eq!(compact(950), "950");
        assert_eq!(compact(12_345), "12.3k");
        assert_eq!(compact(4_500_000), "4.5M");
    }

    fn resume_titles(m: &Model, scope: Option<&Path>, text: &str) -> Vec<String> {
        m.resume_order(scope, text)
            .iter()
            .map(|&i| m.rows()[i].entry.title.clone().unwrap_or_default())
            .collect()
    }

    #[test]
    fn resume_lists_pinned_sessions_first_then_the_newest() {
        let mut m = model(vec![
            entry("new", "/p", 1, 10),
            entry("old", "/p", 9, 10),
            entry("older", "/p", 20, 10),
            entry("mid", "/p", 5, 10),
        ]);
        assert_eq!(
            resume_titles(&m, None, ""),
            ["new", "mid", "old", "older"],
            "newest first without pins"
        );
        m.set_pins(HashSet::from(["id-older".to_owned(), "id-old".to_owned()]));
        m.prepare(now());
        assert_eq!(
            resume_titles(&m, None, ""),
            ["old", "older", "new", "mid"],
            "the pinned two first (newest of them first), then the rest newest first"
        );
        m.set_pins(HashSet::new());
        m.prepare(now());
        assert_eq!(
            resume_titles(&m, None, "")[0],
            "new",
            "unpinning restores the order"
        );
    }

    #[test]
    fn resume_is_scoped_to_the_project_and_filtered() {
        let mut m = model(vec![
            entry("a1", "/p", 1, 10),
            entry("a2", "/p", 3, 10),
            entry("b1", "/q", 2, 10),
        ]);
        m.prepare(now());
        assert_eq!(resume_titles(&m, Some(Path::new("/p")), ""), ["a1", "a2"]);
        assert_eq!(
            resume_titles(&m, None, ""),
            ["a1", "b1", "a2"],
            "every project when unscoped"
        );
        assert_eq!(
            resume_titles(&m, None, "prompt of b1"),
            ["b1"],
            "the filter reads prompts"
        );
        assert!(resume_titles(&m, Some(Path::new("/q")), "a1").is_empty());
    }

    #[test]
    fn pinned_sessions_stay_on_top_whatever_the_sort_column() {
        let mut m = model(vec![
            entry("big", "/p", 1, 9_000),
            entry("small", "/p", 2, 10),
            entry("pinned-mid", "/p", 3, 500),
        ]);
        m.set_pins(HashSet::from(["id-pinned-mid".to_owned()]));
        m.prepare(now());
        assert_eq!(names(&m)[0], "pinned-mid");
        let row = m
            .rows()
            .iter()
            .find(|r| r.pinned)
            .map(|r| r.badges.contains(&Badge::Pinned));
        assert_eq!(row, Some(true), "the Pinned badge marks the row");
    }
}
