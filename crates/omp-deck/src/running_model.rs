//! The session manager's **Running** view model: every live session of every polled backend with
//! its memory, uptime and badges, plus sorting, selection and the numbers the kill confirmation
//! shows. Pure logic (no egui painting, no I/O): [`crate::sessions_view`] paints it and
//! `app::session_manager` feeds it from the worker.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use deck_core::mux::RunningSession;
use deck_core::platform::TreeMemory;

use crate::app::human_size;
use crate::sessions_model::{Check, by_key, plural};
use crate::titlebar::strip_omp_prefix;
use deck_service::backend::kind_label;
use deck_service::statusfmt::Activity;

/// The two views of the session manager hub.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HubMode {
    /// Every saved session (inventory).
    #[default]
    Sessions,
    /// Live sessions, their memory, kill.
    Running,
}

/// One answer of the running-sessions worker.
#[derive(Clone, Debug, Default)]
pub struct RunPoll {
    pub sessions: Vec<RunningSession>,
    /// Memory of the tmux server process (shared by all tmux sessions), when tmux is in use.
    pub tmux_server: Option<TreeMemory>,
    /// The tmux backend is available (the server line is shown even when its memory is unknown).
    pub tmux: bool,
}

/// Badges that depend on the app (tabs) as well as on the poll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    pub in_tab: bool,
    pub busy: bool,
    pub attention: bool,
}

pub struct RunRow {
    pub raw: RunningSession,
    /// Session title (file → inventory), else the pane title, else the mux name.
    pub title: String,
    /// Project label of the session's folder (`—` when unknown).
    pub project: String,
    pub flags: Flags,
    /// Pre-formatted memory cell (`—` without a reading).
    pub ram: String,
    /// Pre-formatted uptime cell.
    pub uptime: String,
    /// Laid-out cell text, reused across frames (see [`crate::paint_cache`]).
    pub paint: std::cell::RefCell<crate::paint_cache::PaintCache>,
    /// What the pane title says omp is doing (before a tab's fresher title overrides it).
    pane_activity: Activity,
    title_key: String,
    project_key: String,
    uptime_secs: Option<u64>,
}

impl RunRow {
    fn new(raw: RunningSession) -> RunRow {
        let pane_activity = Activity::derive(
            raw.run_state.as_ref(),
            raw.title.as_deref(),
            raw.tsp_facts.as_ref(),
        );
        let ram = ram_text(raw.memory);
        RunRow {
            title: raw.name.clone(),
            title_key: raw.name.to_lowercase(),
            project: String::new(),
            project_key: String::new(),
            flags: Flags {
                in_tab: false,
                busy: pane_activity.is_busy(),
                attention: pane_activity == Activity::Attention,
            },
            ram,
            uptime: String::new(),
            paint: Default::default(),
            pane_activity,
            uptime_secs: None,
            raw,
        }
    }

    pub fn name(&self) -> &str {
        &self.raw.name
    }

    pub fn bytes(&self) -> Option<u64> {
        self.raw.memory.map(|m| m.bytes)
    }
}

/// `412.3 MB`, or `—` when the memory could not be read.
fn ram_text(m: Option<TreeMemory>) -> String {
    m.map_or_else(|| "—".to_owned(), |m| human_size(m.bytes))
}

/// The row title: the session's own title when its file is known, else what the pane's terminal
/// title says (omp's `π <glyph>` prefix removed), else the mux session name.
pub fn resolve_title(inventory: Option<&str>, pane: Option<&str>, name: &str) -> String {
    let non_empty = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_owned());
    inventory
        .and_then(non_empty)
        .or_else(|| pane.map(strip_omp_prefix).and_then(non_empty))
        .unwrap_or_else(|| name.to_owned())
}

/// `<1m`, `37m`, `2h 05m`, `3d 4h`.
pub fn format_uptime(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    match secs {
        s if s < MIN => "<1m".to_owned(),
        s if s < HOUR => format!("{}m", s / MIN),
        s if s < DAY => format!("{}h {:02}m", s / HOUR, s % HOUR / MIN),
        s => format!("{}d {}h", s / DAY, s % DAY / HOUR),
    }
}

// -------------------------------------------------------------------- sort

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunCol {
    Title,
    Project,
    Backend,
    Ram,
    Uptime,
    Status,
}

impl RunCol {
    /// Text columns start ascending (A→Z), the rest descending (biggest / longest / most urgent).
    pub fn first_desc(self) -> bool {
        !matches!(self, RunCol::Title | RunCol::Project | RunCol::Backend)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunSort {
    pub col: RunCol,
    pub desc: bool,
}

impl Default for RunSort {
    fn default() -> Self {
        RunSort {
            col: RunCol::Ram,
            desc: true,
        }
    }
}

impl RunSort {
    pub fn click(&mut self, col: RunCol) {
        if self.col == col {
            self.desc = !self.desc;
        } else {
            *self = RunSort {
                col,
                desc: col.first_desc(),
            };
        }
    }
}

fn status_rank(r: &RunRow) -> u8 {
    u8::from(r.flags.in_tab) + 2 * u8::from(r.flags.busy) + 4 * u8::from(r.flags.attention)
}

fn cmp_rows(a: &RunRow, b: &RunRow, sort: RunSort) -> Ordering {
    let d = sort.desc;
    let main = match sort.col {
        RunCol::Title => by_key(Some(&a.title_key), Some(&b.title_key), d, |x, y| x.cmp(y)),
        RunCol::Project => by_key(Some(&a.project_key), Some(&b.project_key), d, |x, y| {
            x.cmp(y)
        }),
        RunCol::Backend => by_key(
            Some(kind_label(a.raw.backend)),
            Some(kind_label(b.raw.backend)),
            d,
            |x, y| x.cmp(y),
        ),
        RunCol::Ram => by_key(a.bytes(), b.bytes(), d, u64::cmp),
        RunCol::Uptime => by_key(a.uptime_secs, b.uptime_secs, d, u64::cmp),
        RunCol::Status => by_key(Some(status_rank(a)), Some(status_rank(b)), d, u8::cmp),
    };
    // Ties: the bigger memory first, then the name (a stable, total order).
    main.then_with(|| by_key(a.bytes(), b.bytes(), true, u64::cmp))
        .then_with(|| a.raw.name.cmp(&b.raw.name))
}

// -------------------------------------------------------------- kill plan

/// What a kill confirmation tells the user about the sessions it would end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KillPlan {
    pub count: usize,
    /// Memory freed (sum of the readings; rows without one add nothing).
    pub bytes: u64,
    /// Rows without a memory reading.
    pub unread: usize,
    /// Mid-turn: the turn is interrupted.
    pub busy: usize,
    /// Open in a tab: the tab shows `[process exited: …]`.
    pub in_tab: usize,
}

impl KillPlan {
    pub fn of<'a>(rows: impl IntoIterator<Item = &'a RunRow>) -> KillPlan {
        rows.into_iter().fold(KillPlan::default(), |p, r| KillPlan {
            count: p.count + 1,
            bytes: p.bytes + r.bytes().unwrap_or(0),
            unread: p.unread + usize::from(r.raw.memory.is_none()),
            busy: p.busy + usize::from(r.flags.busy),
            in_tab: p.in_tab + usize::from(r.flags.in_tab),
        })
    }

    pub fn title(&self) -> String {
        format!("Kill {}?", plural(self.count, "session", "sessions"))
    }

    pub fn body(&self) -> String {
        let mut body = format!(
            "{} will be terminated. The session files stay, so you can resume them later.\n",
            plural(self.count, "running session", "running sessions")
        );
        if self.bytes > 0 {
            body.push_str(&format!("Frees about {} of memory", human_size(self.bytes)));
            if self.unread > 0 {
                body.push_str(&format!(
                    " ({} without a reading not counted)",
                    plural(self.unread, "session", "sessions")
                ));
            }
            body.push_str(".\n");
        }
        if self.busy > 0 {
            body.push_str(&format!(
                "{} working — the current turn will be interrupted.\n",
                plural(self.busy, "session is", "sessions are")
            ));
        }
        if self.in_tab > 0 {
            body.push_str(&format!(
                "{} open in a tab — the tab will show [process exited].\n",
                plural(self.in_tab, "session is", "sessions are")
            ));
        }
        body.truncate(body.trim_end().len());
        body
    }
}

// ------------------------------------------------------------------- model

pub struct RunModel {
    rows: Vec<RunRow>,
    /// Row indices in sort order.
    order: Vec<usize>,
    pub sort: RunSort,
    sel: HashSet<String>,
    anchor: Option<String>,
    server: Option<TreeMemory>,
    tmux: bool,
    loaded: bool,
    /// Sessions a kill is in flight for.
    killing: HashSet<String>,
    order_stale: bool,
    uptime_stale: bool,
    minute: u64,
    now: SystemTime,
}

impl Default for RunModel {
    fn default() -> Self {
        RunModel {
            rows: Vec::new(),
            order: Vec::new(),
            sort: RunSort::default(),
            sel: HashSet::new(),
            anchor: None,
            server: None,
            tmux: false,
            loaded: false,
            killing: HashSet::new(),
            order_stale: true,
            uptime_stale: true,
            minute: 0,
            now: SystemTime::UNIX_EPOCH,
        }
    }
}

fn minute_of(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 60)
}

impl RunModel {
    pub fn rows(&self) -> &[RunRow] {
        &self.rows
    }

    /// Row indices (into [`RunModel::rows`]) in sort order.
    pub fn order(&self) -> &[usize] {
        &self.order
    }

    /// At least one poll has answered.
    pub fn loaded(&self) -> bool {
        self.loaded
    }

    pub fn tmux_server(&self) -> Option<TreeMemory> {
        self.server
    }

    /// The tmux backend is in use (show its server line).
    pub fn tmux(&self) -> bool {
        self.tmux
    }

    pub fn is_killing(&self, name: &str) -> bool {
        self.killing.contains(name)
    }

    pub fn any_killing(&self) -> bool {
        !self.killing.is_empty()
    }

    pub fn set_killing(&mut self, names: impl IntoIterator<Item = String>) {
        self.killing = names.into_iter().collect();
    }

    pub fn clear_killing(&mut self) {
        self.killing.clear();
    }

    /// Forget everything (the hub was reopened): the next poll refills it.
    pub fn reset(&mut self) {
        self.sel.clear();
        self.anchor = None;
    }

    /// Replace the rows with a fresh poll. Selection keeps the sessions that are still alive.
    /// Call [`resolve`](Self::resolve) right after.
    pub fn set_poll(&mut self, poll: RunPoll) {
        self.rows = poll.sessions.into_iter().map(RunRow::new).collect();
        self.server = poll.tmux_server;
        self.tmux = poll.tmux;
        self.loaded = true;
        let alive: HashSet<&str> = self.rows.iter().map(RunRow::name).collect();
        self.sel.retain(|n| alive.contains(n.as_str()));
        if self.anchor.as_deref().is_some_and(|a| !alive.contains(a)) {
            self.anchor = None;
        }
        self.killing.retain(|n| alive.contains(n.as_str()));
        self.order_stale = true;
        self.uptime_stale = true;
    }

    /// Titles and projects: `entry_of(file)` gives the inventory's title and folder of a session
    /// file; `label_of(folder)` the project label.
    pub fn resolve(
        &mut self,
        entry_of: impl Fn(&Path) -> Option<(String, PathBuf)>,
        label_of: impl Fn(&Path) -> String,
    ) {
        for r in &mut self.rows {
            let entry = r.raw.files.iter().find_map(|f| entry_of(f));
            r.title = resolve_title(
                entry.as_ref().map(|(t, _)| t.as_str()),
                r.raw.title.as_deref(),
                &r.raw.name,
            );
            r.title_key = r.title.to_lowercase();
            let cwd = entry.map(|(_, cwd)| cwd).or_else(|| r.raw.cwd.clone());
            r.project = cwd.as_deref().map_or_else(|| "—".to_owned(), &label_of);
            r.project_key = r.project.to_lowercase();
        }
        self.order_stale = true;
    }

    /// Tab state per row: `None` = no live tab shows the session; `Some(activity)` = a tab does
    /// and its terminal title says `activity` (`Unknown` falls back to the pane title). Rewrites
    /// nothing unless a flag changed.
    pub fn set_tabs(&mut self, tab_of: impl Fn(&RunRow) -> Option<Activity>) {
        let mut changed = false;
        for i in 0..self.rows.len() {
            let tab = tab_of(&self.rows[i]);
            let r = &mut self.rows[i];
            let act = tab
                .filter(|a| *a != Activity::Unknown)
                .unwrap_or(r.pane_activity);
            let flags = Flags {
                in_tab: tab.is_some(),
                busy: act.is_busy(),
                attention: act == Activity::Attention,
            };
            if flags != r.flags {
                r.flags = flags;
                changed = true;
            }
        }
        if changed {
            self.order_stale = true;
        }
    }

    /// Bring uptime cells and the order up to date; cheap when nothing changed.
    pub fn prepare(&mut self, now: SystemTime) {
        self.now = now;
        let minute = minute_of(now);
        if minute != self.minute {
            self.minute = minute;
            self.uptime_stale = true;
        }
        if self.uptime_stale {
            for r in &mut self.rows {
                r.uptime_secs = r
                    .raw
                    .started
                    .map(|s| now.duration_since(s).map_or(0, |d| d.as_secs()));
                r.uptime = r.uptime_secs.map_or_else(|| "—".to_owned(), format_uptime);
            }
            self.uptime_stale = false;
            self.order_stale = true;
        }
        if self.order_stale {
            let sort = self.sort;
            let mut order: Vec<usize> = (0..self.rows.len()).collect();
            order.sort_by(|&a, &b| cmp_rows(&self.rows[a], &self.rows[b], sort));
            self.order = order;
            self.order_stale = false;
        }
    }

    /// The sort column changed (the field is public).
    pub fn mark_order_dirty(&mut self) {
        self.order_stale = true;
    }

    // ------------------------------------------------------------- totals

    pub fn total_bytes(&self) -> u64 {
        self.rows.iter().filter_map(RunRow::bytes).sum()
    }

    pub fn selected_rows(&self) -> Vec<&RunRow> {
        self.order
            .iter()
            .map(|&i| &self.rows[i])
            .filter(|r| self.sel.contains(r.name()))
            .collect()
    }

    pub fn selected_names(&self) -> Vec<String> {
        self.selected_rows()
            .iter()
            .map(|r| r.name().to_owned())
            .collect()
    }

    pub fn plan_for(&self, names: &[String]) -> KillPlan {
        KillPlan::of(
            self.rows
                .iter()
                .filter(|r| names.iter().any(|n| n == r.name())),
        )
    }

    pub fn selection_plan(&self) -> KillPlan {
        KillPlan::of(self.rows.iter().filter(|r| self.sel.contains(r.name())))
    }

    // ---------------------------------------------------------- selection

    pub fn is_selected(&self, name: &str) -> bool {
        self.sel.contains(name)
    }

    pub fn clear_selection(&mut self) {
        self.sel.clear();
        self.anchor = None;
    }

    /// Click on the row at `pos` of the sorted order; with Shift and an anchor, the range between
    /// them is added, otherwise the row toggles.
    pub fn click_row(&mut self, pos: usize, shift: bool) {
        let Some(&i) = self.order.get(pos) else {
            return;
        };
        let name = self.rows[i].name().to_owned();
        let anchor_pos = self
            .anchor
            .as_ref()
            .and_then(|a| self.order.iter().position(|&v| self.rows[v].name() == a));
        match (shift, anchor_pos) {
            (true, Some(a)) => {
                for &v in &self.order[a.min(pos)..=a.max(pos)] {
                    self.sel.insert(self.rows[v].name().to_owned());
                }
            }
            _ => {
                if !self.sel.remove(&name) {
                    self.sel.insert(name.clone());
                }
            }
        }
        self.anchor = Some(name);
    }

    pub fn header_check(&self) -> Check {
        let n = self
            .rows
            .iter()
            .filter(|r| self.sel.contains(r.name()))
            .count();
        match n {
            0 => Check::None,
            n if n == self.rows.len() => Check::All,
            _ => Check::Some,
        }
    }

    pub fn toggle_all(&mut self) {
        if self.header_check() == Check::All {
            self.sel.clear();
        } else {
            self.sel = self.rows.iter().map(|r| r.name().to_owned()).collect();
        }
        self.anchor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_core::mux::MuxKind;
    use std::time::Duration;

    fn session(name: &str, bytes: Option<u64>) -> RunningSession {
        RunningSession {
            name: name.to_owned(),
            backend: MuxKind::Tmux,
            files: Vec::new(),
            title: None,
            run_state: None,
            tsp_facts: None,
            cwd: None,
            started: None,
            pid: Some(1),
            memory: bytes.map(|bytes| TreeMemory {
                bytes,
                processes: 3,
            }),
        }
    }

    fn model(sessions: Vec<RunningSession>) -> RunModel {
        let mut m = RunModel::default();
        m.set_poll(RunPoll {
            sessions,
            tmux_server: None,
            tmux: true,
        });
        m.prepare(SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000));
        m
    }

    fn names(m: &RunModel) -> Vec<&str> {
        m.order().iter().map(|&i| m.rows()[i].name()).collect()
    }

    #[test]
    fn title_prefers_the_session_then_the_pane_then_the_name() {
        assert_eq!(
            resolve_title(Some("Fix bug"), Some("π > x"), "omp-a"),
            "Fix bug"
        );
        assert_eq!(
            resolve_title(None, Some("π ⠦ my project"), "omp-a"),
            "my project"
        );
        assert_eq!(resolve_title(Some("  "), Some("plain"), "omp-a"), "plain");
        assert_eq!(resolve_title(None, Some("π ! "), "omp-a"), "omp-a");
        assert_eq!(resolve_title(None, None, "omp-a"), "omp-a");
    }

    #[test]
    fn ram_sort_puts_unreadable_rows_last_in_both_directions() {
        let mut m = model(vec![
            session("none", None),
            session("small", Some(10)),
            session("big", Some(500)),
        ]);
        assert_eq!(
            names(&m),
            ["big", "small", "none"],
            "RAM descending by default"
        );
        m.sort.click(RunCol::Ram);
        m.mark_order_dirty();
        m.prepare(SystemTime::UNIX_EPOCH);
        assert_eq!(names(&m), ["small", "big", "none"]);
    }

    #[test]
    fn a_new_column_starts_in_its_natural_direction() {
        let mut s = RunSort::default();
        s.click(RunCol::Title);
        assert_eq!((s.col, s.desc), (RunCol::Title, false));
        s.click(RunCol::Title);
        assert!(s.desc);
        s.click(RunCol::Uptime);
        assert_eq!((s.col, s.desc), (RunCol::Uptime, true));
    }

    #[test]
    fn uptime_formats() {
        assert_eq!(format_uptime(5), "<1m");
        assert_eq!(format_uptime(37 * 60), "37m");
        assert_eq!(format_uptime(2 * 3600 + 5 * 60), "2h 05m");
        assert_eq!(format_uptime(3 * 86_400 + 4 * 3600), "3d 4h");
    }

    #[test]
    fn kill_plan_sums_memory_and_counts_warnings() {
        let mut m = model(vec![
            session("a", Some(1024 * 1024)),
            session("b", Some(3 * 1024 * 1024)),
            session("c", None),
        ]);
        m.set_tabs(|r| match r.name() {
            "a" => Some(Activity::Busy),
            "b" => Some(Activity::Idle),
            _ => None,
        });
        let all: Vec<String> = ["a", "b", "c"].map(str::to_owned).into();
        let plan = m.plan_for(&all);
        assert_eq!(
            plan,
            KillPlan {
                count: 3,
                bytes: 4 * 1024 * 1024,
                unread: 1,
                busy: 1,
                in_tab: 2,
            }
        );
        assert_eq!(plan.title(), "Kill 3 sessions?");
        let body = plan.body();
        assert!(body.contains("Frees about 4.0 MB"), "{body}");
        assert!(body.contains("1 session is working"), "{body}");
        assert!(body.contains("2 sessions are open in a tab"), "{body}");
        assert!(body.contains("1 session without a reading"), "{body}");
        assert_eq!(m.plan_for(&["b".to_owned()]).count, 1);
        assert_eq!(m.plan_for(&["gone".to_owned()]), KillPlan::default());
    }

    #[test]
    fn a_calm_kill_has_no_warnings() {
        let m = model(vec![session("a", Some(2 * 1024 * 1024))]);
        let body = m.plan_for(&["a".to_owned()]).body();
        assert!(
            !body.contains("interrupted") && !body.contains("tab"),
            "{body}"
        );
        assert!(
            body.starts_with("1 running session will be terminated"),
            "{body}"
        );
    }

    #[test]
    fn selection_follows_the_sort_and_survives_a_poll_only_for_live_rows() {
        let mut m = model(vec![
            session("a", Some(30)),
            session("b", Some(20)),
            session("c", Some(10)),
        ]);
        m.click_row(0, false);
        m.click_row(2, true);
        assert_eq!(m.selected_names(), ["a", "b", "c"]);
        m.clear_selection();
        m.click_row(1, false);
        assert_eq!(m.selected_names(), ["b"]);
        m.set_poll(RunPoll {
            sessions: vec![session("a", Some(30)), session("c", Some(10))],
            tmux_server: None,
            tmux: true,
        });
        m.prepare(SystemTime::UNIX_EPOCH);
        assert!(m.selected_names().is_empty(), "b is gone");
        m.toggle_all();
        assert_eq!(m.header_check(), Check::All);
        m.toggle_all();
        assert_eq!(m.header_check(), Check::None);
    }

    #[test]
    fn a_tab_title_overrides_the_pane_title_for_busy() {
        let mut s = session("a", Some(1));
        s.title = Some("π > idle".to_owned());
        let mut m = model(vec![s]);
        assert!(!m.rows()[0].flags.busy);
        m.set_tabs(|_| Some(Activity::Busy));
        assert!(m.rows()[0].flags.busy && m.rows()[0].flags.in_tab);
        m.set_tabs(|_| Some(Activity::Unknown));
        assert!(!m.rows()[0].flags.busy && m.rows()[0].flags.in_tab);
        m.set_tabs(|_| None);
        assert!(!m.rows()[0].flags.in_tab);
    }
}
