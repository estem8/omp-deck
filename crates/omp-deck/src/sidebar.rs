//! Session sidebar: grouped, filterable list of persisted omp sessions (rounded cards, glowing
//! running dots, animated hover/selection, right-click menus).

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::icons::Icon;
use crate::icons::{
    ICON_ARCHIVE_RESTORE, ICON_ARROW_LEFT_RIGHT, ICON_CHEVRON_DOWN, ICON_CHEVRON_RIGHT,
    ICON_CHEVRONS_DOWN_UP, ICON_CHEVRONS_UP_DOWN, ICON_CIRCLE_STOP, ICON_CLOUD, ICON_CLOUD_UPLOAD,
    ICON_COLUMNS_2, ICON_COPY, ICON_EXTERNAL_LINK, ICON_FILE_TEXT, ICON_FILES, ICON_FOCUS,
    ICON_FOLDER, ICON_FOLDER_GIT_2, ICON_FOLDER_OPEN, ICON_FOLDER_OUTPUT, ICON_FOLDER_X,
    ICON_GIT_BRANCH, ICON_GIT_BRANCH_PLUS, ICON_GIT_MERGE, ICON_HARD_DRIVE, ICON_HISTORY,
    ICON_KEY_ROUND, ICON_LIST_TREE, ICON_MESSAGES_SQUARE, ICON_PLAY, ICON_PLUS, ICON_POWER,
    ICON_REFRESH_CW, ICON_ROTATE_CCW, ICON_ROWS_2, ICON_SEARCH, ICON_SEARCH_X, ICON_TERMINAL,
    ICON_TRASH_2, ICON_X,
};
use deck_core::config::{ParkedBranch, RemovedWorktree, SessionView};
use deck_core::sessions::{ProjectGroup, SessionEntry, SessionsRoot, abbreviate_home};
use deck_core::worktree::Worktree;
use egui::collapsing_header::CollapsingState;
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align2, Color32, CursorIcon, Frame, Id, Margin, Rect, RichText, Sense, Shape, Stroke,
    StrokeKind, Ui, pos2, vec2,
};

use crate::badge::{self, Hue};
use crate::panes::Side;
use crate::scope_bar::{self, ScopeAction, ScopeInput, ScopeState};
use crate::tabs::{self, Tab};
use crate::theme::{self, Palette};
use crate::transfer::{self, TransferKind};
use crate::widgets::{empty_state, icon_text, menu_item, paint_icon};
use crate::worktree_ui::{self, Family, ParkView, Row, RowWorktree};
use crate::{card, tips};
mod running;
use deck_service::gateway::running::RunningSnapshot;
use deck_service::status::TabStatus;

/// "now", "5m", "3h", "2d", "4w", "2y".
pub fn format_age(now: SystemTime, modified: SystemTime) -> String {
    let secs = now
        .duration_since(modified)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const YEAR: u64 = 365 * DAY;
    match secs {
        s if s < MIN => "now".to_owned(),
        s if s < HOUR => format!("{}m", s / MIN),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < WEEK => format!("{}d", s / DAY),
        s if s < YEAR => format!("{}w", s / WEEK),
        s => format!("{}y", s / YEAR),
    }
}

/// Lowercased query; empty matches everything. Matches title/label, first prompt, project cwd
/// (display and raw), and id prefix.
pub fn matches_filter(query_lower: &str, group: &ProjectGroup, entry: &SessionEntry) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    entry.label().to_lowercase().contains(query_lower)
        || entry
            .title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().contains(query_lower))
        || group.display.to_lowercase().contains(query_lower)
        || group
            .worktree
            .as_ref()
            .and_then(|w| w.branch.as_deref())
            .is_some_and(|b| b.to_lowercase().contains(query_lower))
        || entry
            .cwd
            .to_string_lossy()
            .to_lowercase()
            .contains(query_lower)
        || entry.id.to_lowercase().starts_with(query_lower)
}

/// `omp --resume '<path>'` for pasting into a shell.
pub fn resume_command(path: &Path) -> String {
    format!(
        "omp --resume {}",
        deck_core::worktree::shell_quote(&path.to_string_lossy())
    )
}

/// `("~/dev", "proj")` for `"~/dev/proj"`; a display without a usable last segment is all name.
fn split_display(display: &str) -> (&str, &str) {
    match display.rsplit_once(['/', '\\']) {
        Some((parent, name)) if !name.is_empty() => (parent, name),
        _ => ("", display),
    }
}

#[derive(Clone, Debug)]
pub enum SidebarAction {
    Open(SessionEntry),
    OpenView(SessionEntry, SessionView),
    SwitchView {
        uid: u64,
        view: SessionView,
    },
    /// Show the session's tree (reads the file; no tab needed).
    Tree(SessionEntry),
    New(PathBuf),
    NewView(PathBuf, SessionView),
    Refresh,
    /// Open the session manager (scoped to this project when given).
    ManageSessions(Option<PathBuf>),
    /// Put `text` on the clipboard and confirm with a toast naming `what`.
    CopyText {
        text: String,
        what: &'static str,
    },
    /// Hand a directory to the desktop's opener.
    OpenFolder(PathBuf),
    /// Ask to stop the session running this omp session.
    Stop(SessionEntry),
    /// Ask to restart the running session (stop its process, resume it with live control).
    Restart(SessionEntry),
    /// Ask to delete the session (file and artifacts) through omp.
    Delete(SessionEntry),
    /// Copy/move a session; `dest: None` = ask for a folder first.
    Transfer {
        kind: TransferKind,
        entry: SessionEntry,
        dest: Option<PathBuf>,
    },
    /// Show only this folder's sessions (`None` = all projects).
    SetScope(Option<PathBuf>),
    /// Pick the scope folder with the native dialog.
    BrowseScope,
    /// A path typed into the scope menu.
    TypedScope(String),
    /// Switch to the tab at this strip index ("Open tabs").
    ActivateTab(usize),
    /// Close the tab at this strip index (same path as the tab strip's ✕).
    CloseTab(usize),
    /// Kill the process of the tab at this strip index (tabs whose session menu has no "Stop").
    KillTab(usize),
    /// Back up this project folder to Google Drive (opens the backup dialog).
    BackupProject(PathBuf),
    /// Restore a backup of this project folder from Google Drive (opens the restore dialog).
    RestoreProject(PathBuf),
    /// Ask for a branch and create a worktree of the project in this folder, then open a tab in it.
    NewWorktree(PathBuf),
    /// Ask for a branch and move this session (open in a tab) into a new worktree.
    MoveToWorktree(SessionEntry),
    /// Send the worktree's session the finish prompt and remove the worktree once it is merged.
    FinishWorktree(Worktree),
    /// Stop waiting for the worktree (by folder) to finish.
    CancelFinish(PathBuf),
    /// Remove the worktree: ask first; merged ones are deleted with their branch, clean and
    /// pushed ones are parked (the branch stays), anything else is refused.
    RemoveWorktree(Worktree),
    /// A parked session: create a worktree on its branch and move the branch's sessions into it.
    ContinueInWorktree(SessionEntry),
    /// A parked session: check its branch out in the main checkout and open the session there.
    ContinueInMain(SessionEntry),
    /// A parked session whose branch is checked out in the main checkout: switch that back to the
    /// parked branch's base.
    SwitchMainBack(SessionEntry),
    /// Open the tab at this strip index in a new pane beside the focused one.
    SplitTab {
        index: usize,
        side: Side,
    },
    /// Open the session (a tab is opened if needed) in a new pane beside the focused one.
    SplitOpen {
        entry: SessionEntry,
        side: Side,
    },
    /// Open a tab on the live session `name` that has no session file the sidebar lists (a fresh
    /// omp): attaches to it, never starts one.
    AttachRunning {
        name: String,
        title: String,
    },
    /// Ask to stop the live session `name` (no listed session file); `label` names it.
    StopRunning {
        name: String,
        label: String,
    },
}

/// The project backup entries of the context menus: named after the active destination,
/// disabled with a reason while it is not set up or a project job runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackupMenu<'a> {
    /// "Google Drive", `sftp://host`, …
    pub place: &'a str,
    /// `Some` = disabled, with the reason.
    pub off: Option<&'static str>,
}

#[derive(Default)]
pub struct SidebarOutput {
    pub actions: Vec<SidebarAction>,
    /// The user clicked something in the sidebar (other than the filter box).
    pub interacted: bool,
}

pub struct SidebarInput<'a> {
    pub groups: Option<&'a [ProjectGroup]>,
    pub running: &'a RunningSnapshot,
    pub root: &'a SessionsRoot,
    pub root_exists: bool,
    pub active_path: Option<&'a Path>,
    /// Working directory of the active terminal tab (its project group when the session file is
    /// not listed yet).
    pub active_cwd: Option<&'a Path>,
    pub now: SystemTime,
    /// A manual refresh is in flight (the refresh button spins).
    pub refreshing: bool,
    /// Sessions can be stopped (a session backend available).
    pub can_stop: bool,
    pub default_view: SessionView,
    pub host_available: bool,
    /// omp was found: session copy/move are offered.
    pub omp_available: bool,
    /// Session files with a move in flight (their copy/move items are disabled).
    pub busy: &'a HashSet<PathBuf>,
    /// Watcher generation of `groups` (part of the cache key).
    pub generation: u64,
    /// Session files open in a live tab: those rows count as running.
    pub open_paths: &'a [&'a Path],
    /// Per-folder mode: only sessions inside this folder are listed.
    pub scope: Option<&'a Path>,
    /// Recently focused folders, most recent first.
    pub recent: &'a [PathBuf],
    /// kdialog or zenity exists.
    pub picker_available: bool,
    /// The native folder dialog is open.
    pub picking: bool,
    /// Every open tab in strip order ("Open tabs" section) with the session labels and the active
    /// index.
    pub tabs: &'a [Tab],
    pub labels: &'a HashMap<PathBuf, String>,
    pub active_tab: usize,
    /// Live omp state (`Busy` / `Attention` only) of sessions open in a tab, by session file.
    pub tab_activity: &'a HashMap<&'a Path, TabStatus>,
    /// Project backup entries of the context menus.
    pub backup: BackupMenu<'a>,
    /// Worktrees (by folder) that were sent the finish prompt and are being waited for.
    pub finishing: &'a HashMap<PathBuf, bool>,
    /// Worktrees OmpDeck removed: their sessions nest under the primary, marked as removed.
    pub removed: &'a BTreeMap<PathBuf, RemovedWorktree>,
    /// Parked branches: their sessions carry a chip and the continue actions.
    pub parked: ParkView<'a>,
}

/// Which project groups are open. A group is open while filtering, else by its user override,
/// else when it is the active group. The active group is the group of the most recent tab that
/// has one (a tab without one, e.g. an update tab, keeps the previous group). Becoming the active group drops a
/// collapsed (`false`) override so it opens; an open (`true`) override survives. Re-activating
/// the same group changes nothing. Never persisted.
#[derive(Default)]
pub struct Expansion {
    overrides: HashMap<PathBuf, bool>,
    active: Option<PathBuf>,
    /// The repository the active group is a worktree of: it opens with its child.
    active_parent: Option<PathBuf>,
}

impl Expansion {
    fn is_open(&self, cwd: &Path, filtering: bool) -> bool {
        filtering
            || self.overrides.get(cwd).copied().unwrap_or(
                self.active.as_deref() == Some(cwd) || self.active_parent.as_deref() == Some(cwd),
            )
    }

    /// Notes the group of the active tab and the repository it is a worktree of (`None`: the
    /// tab has no group, keep the previous one).
    fn on_activation(&mut self, group: Option<&Path>, parent: Option<&Path>) {
        let Some(g) = group else { return };
        if self.active.as_deref() != Some(g) {
            for p in std::iter::once(g).chain(parent) {
                if self.overrides.get(p) == Some(&false) {
                    self.overrides.remove(p);
                }
            }
            self.active = Some(g.to_path_buf());
            self.active_parent = parent.map(Path::to_path_buf);
        }
    }

    fn set(&mut self, cwd: &Path, open: bool) {
        self.overrides.insert(cwd.to_path_buf(), open);
    }

    fn set_all<'a>(&mut self, cwds: impl Iterator<Item = &'a Path>, open: bool) {
        self.overrides.clear();
        self.overrides.extend(cwds.map(|c| (c.to_path_buf(), open)));
    }
}

/// The group index holding `active_path`, else the group whose cwd is `active_cwd`.
fn active_group_of(
    groups: &[ProjectGroup],
    active_path: Option<&Path>,
    active_cwd: Option<&Path>,
) -> Option<usize> {
    let by_path = active_path.and_then(|p| {
        groups
            .iter()
            .position(|g| g.sessions.iter().any(|e| e.path == p))
    });
    by_path.or_else(|| active_cwd.and_then(|c| groups.iter().position(|g| g.cwd == c)))
}

/// `(data stamp, active path, active cwd)`.
type ActiveKey = ((u64, usize), Option<PathBuf>, Option<PathBuf>);

/// The active-group lookup, valid for one [`ActiveKey`].
#[derive(Default)]
struct ActiveGroupCache {
    key: Option<ActiveKey>,
    group: Option<usize>,
}

impl ActiveGroupCache {
    fn get<'a>(
        &mut self,
        stamp: (u64, usize),
        groups: &'a [ProjectGroup],
        active_path: Option<&Path>,
        active_cwd: Option<&Path>,
    ) -> Option<&'a ProjectGroup> {
        let fresh = self.key.as_ref().is_some_and(|(s, p, c)| {
            *s == stamp && p.as_deref() == active_path && c.as_deref() == active_cwd
        });
        if !fresh {
            self.group = active_group_of(groups, active_path, active_cwd);
            self.key = Some((
                stamp,
                active_path.map(Path::to_path_buf),
                active_cwd.map(Path::to_path_buf),
            ));
        }
        self.group.and_then(|i| groups.get(i))
    }
}

/// Age strings are recomputed at most this often.
const AGE_BUCKET_SECS: u64 = 15;

/// Filtered row indices per project, valid for one `(data stamp, query)`; the stamp is the
/// watcher generation plus the identity of the groups snapshot.
#[derive(Default)]
struct FilterCache {
    key: Option<((u64, usize), String)>,
    visible: Vec<Vec<u32>>,
}

impl FilterCache {
    /// Per-group indices of the rows matching `query` (empty query: nothing is computed).
    fn sync(&mut self, stamp: (u64, usize), query: &str, groups: &[ProjectGroup]) -> &[Vec<u32>] {
        if query.is_empty() {
            return &[];
        }
        let fresh = self
            .key
            .as_ref()
            .is_some_and(|(s, q)| *s == stamp && q == query && self.visible.len() == groups.len());
        if !fresh {
            self.visible = groups
                .iter()
                .map(|group| {
                    group
                        .sessions
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| matches_filter(query, group, e))
                        .map(|(i, _)| i as u32)
                        .collect()
                })
                .collect();
            self.key = Some((stamp, query.to_owned()));
        }
        &self.visible
    }
}

/// Age chip texts by session file, valid for one `(data stamp, time bucket)`.
#[derive(Default)]
struct AgeCache {
    key: Option<((u64, usize), u64)>,
    texts: HashMap<PathBuf, String>,
}

impl AgeCache {
    fn get(&mut self, key: ((u64, usize), u64), now: SystemTime, entry: &SessionEntry) -> &str {
        if self.key != Some(key) {
            self.texts.clear();
            self.key = Some(key);
        }
        if !self.texts.contains_key(entry.path.as_path()) {
            self.texts
                .insert(entry.path.clone(), format_age(now, entry.modified));
        }
        self.texts
            .get(entry.path.as_path())
            .map_or("", String::as_str)
    }
}

/// The nesting of worktree groups, valid for one `(data stamp, scope)`.
#[derive(Default)]
struct LayoutCache {
    key: Option<((u64, usize), Option<PathBuf>, usize)>,
    families: Vec<Family>,
}

impl LayoutCache {
    fn sync(
        &mut self,
        stamp: (u64, usize),
        scope: Option<&Path>,
        groups: &[ProjectGroup],
        removed: &BTreeMap<PathBuf, RemovedWorktree>,
    ) -> &[Family] {
        let fresh = self
            .key
            .as_ref()
            .is_some_and(|(s, sc, n)| *s == stamp && sc.as_deref() == scope && *n == removed.len());
        if !fresh {
            self.families = worktree_ui::layout(groups, scope, removed);
            self.key = Some((stamp, scope.map(Path::to_path_buf), removed.len()));
        }
        &self.families
    }
}

/// Work the sidebar would otherwise redo every frame.
#[derive(Default)]
pub struct SidebarCache {
    filter: FilterCache,
    ages: AgeCache,
    active_group: ActiveGroupCache,
    expansion: Expansion,
    layout: LayoutCache,
    pub scope_ui: ScopeState,
    running: running::RunningCache,
}

fn row_h() -> f32 {
    theme::dens(28.0, 36.0)
}

fn header_h() -> f32 {
    theme::dens(28.0, 32.0)
}

pub fn show(
    ui: &mut Ui,
    input: &SidebarInput<'_>,
    filter: &mut String,
    cache: &mut SidebarCache,
) -> SidebarOutput {
    let pal = Palette::of_ui(ui);
    let mut out = SidebarOutput::default();
    open_tabs_section(ui, input, &mut out);
    running::section(ui, input, &mut cache.running, &mut out);

    let scope_actions = scope_bar::chip(
        ui,
        &ScopeInput {
            scope: input.scope,
            recent: input.recent,
            picker_available: input.picker_available,
            picking: input.picking,
        },
        &mut cache.scope_ui,
    );
    for a in scope_actions {
        out.interacted = true;
        out.actions.push(match a {
            ScopeAction::Set(dir) => SidebarAction::SetScope(dir),
            ScopeAction::Browse => SidebarAction::BrowseScope,
            ScopeAction::Typed(text) => SidebarAction::TypedScope(text),
            ScopeAction::NewSession(dir) => SidebarAction::New(dir),
            ScopeAction::OpenInFileManager(dir) => SidebarAction::OpenFolder(dir),
            ScopeAction::CopyPath(dir) => SidebarAction::CopyText {
                text: dir.to_string_lossy().into_owned(),
                what: "folder path",
            },
        });
    }
    ui.add_space(theme::dens(6.0, 8.0));

    let field_h = theme::dens(28.0, 36.0);
    let neutral = theme::flat();
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), field_h),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if header_button(ui, ICON_REFRESH_CW, "Rescan sessions now", input.refreshing).clicked()
            {
                out.actions.push(SidebarAction::Refresh);
                out.interacted = true;
            }
            if header_button(
                ui,
                ICON_HARD_DRIVE,
                "Session manager — sizes, bulk delete / copy / move (Ctrl+Shift+S)",
                false,
            )
            .clicked()
            {
                out.actions.push(SidebarAction::ManageSessions(None));
                out.interacted = true;
            }
            let id = Id::unique("sidebar-filter");
            let focused = ui.memory(|m| m.has_focus(id));
            ui.add(
                egui::TextEdit::singleline(filter)
                    .id(id)
                    .prefix(icon_text(
                        ICON_SEARCH,
                        if neutral { 15.0 } else { 17.0 },
                        pal.weak,
                    ))
                    .hint_text("Search sessions…")
                    .align(egui::Align2::LEFT_CENTER)
                    .min_size(vec2(0.0, field_h))
                    .desired_width(ui.available_width())
                    .margin(Margin::symmetric(
                        if neutral { 8 } else { 10 },
                        if neutral { 4 } else { 8 },
                    ))
                    .frame(
                        Frame::new()
                            .fill(if neutral { pal.bg } else { pal.surface })
                            .corner_radius(if neutral { theme::ui_rad(8) } else { 11 })
                            .stroke(Stroke::new(
                                1.0,
                                if focused { pal.accent } else { pal.stroke },
                            )),
                    ),
            );
        },
    );
    ui.add_space(theme::dens(6.0, 8.0));
    if let Some(dir) = input.scope
        && scope_bar::new_session_button(ui, dir)
    {
        out.actions.push(SidebarAction::New(dir.to_path_buf()));
        out.interacted = true;
    }
    if input.scope.is_some() {
        ui.add_space(8.0);
    }

    let Some(groups) = input.groups else {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.add(crate::widgets::Spin::new(22.0, pal.accent));
            ui.add_space(6.0);
            ui.label(RichText::new("Scanning sessions…").color(pal.weak));
        });
        return out;
    };
    if !input.root_exists && input.scope.is_none() {
        empty_state(
            ui,
            ICON_FOLDER_X,
            pal.warn,
            "Sessions directory not found",
            "It will appear here once omp has created sessions.",
        );
        ui.add_space(6.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(input.root.path.display().to_string())
                    .monospace()
                    .small()
                    .color(pal.weak),
            );
        });
        return out;
    }
    if groups.is_empty() && input.scope.is_none() {
        empty_state(
            ui,
            ICON_FOLDER_OPEN,
            pal.accent,
            "No sessions yet",
            "Start omp in a project and it will show up here.",
        );
        ui.add_space(6.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(input.root.path.display().to_string())
                    .monospace()
                    .small()
                    .color(pal.weak),
            );
        });
        return out;
    }

    let query = filter.trim().to_lowercase();
    let filtering = !query.is_empty();
    let mut any_shown = false;
    let SidebarCache {
        filter: filter_cache,
        ages,
        active_group: active_cache,
        expansion,
        layout: layout_cache,
        ..
    } = cache;
    let stamp = (input.generation, groups.as_ptr() as usize);
    let active_group = active_cache.get(stamp, groups, input.active_path, input.active_cwd);
    expansion.on_activation(
        active_group.map(|g| g.cwd.as_path()),
        active_group.and_then(|g| {
            g.worktree
                .as_ref()
                .map(|w| w.primary.as_path())
                .or_else(|| input.removed.get(&g.cwd).map(|r| r.primary.as_path()))
        }),
    );
    let visible = filter_cache.sync(stamp, &query, groups);
    let families = layout_cache.sync(stamp, input.scope, groups, input.removed);
    let age_key = (
        stamp,
        input
            .now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            / AGE_BUCKET_SECS,
    );
    let pass = Pass {
        input,
        groups,
        families,
        visible,
        filtering,
        age_key,
    };

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for family in families {
                any_shown |= show_group(ui, &pass, family, (expansion, ages), &mut out);
            }
            if !any_shown {
                if filtering {
                    empty_state(
                        ui,
                        ICON_SEARCH_X,
                        pal.weak,
                        "No matching sessions",
                        match input.scope {
                            Some(_) => "Nothing in this folder matches. Try another title or id.",
                            None => "Try a different title, folder or id.",
                        },
                    );
                } else if let Some(dir) = input.scope {
                    scope_empty(ui, dir);
                }
            }
        });
    out
}

/// What every group of one frame needs.
struct Pass<'a> {
    input: &'a SidebarInput<'a>,
    groups: &'a [ProjectGroup],
    families: &'a [Family],
    /// Filtered row indices per group (only with a query), ascending.
    visible: &'a [Vec<u32>],
    filtering: bool,
    age_key: ((u64, usize), u64),
}

impl Pass<'_> {
    /// Whether a row passes the filter (always without a query).
    fn is_shown(&self, r: &Row) -> bool {
        !self.filtering
            || self
                .visible
                .get(r.group)
                .is_some_and(|v| v.binary_search(&(r.session as u32)).is_ok())
    }
}

/// One project: its header and its rows (its own sessions and those of its worktrees, newest
/// first); false when nothing of it shows.
fn show_group(
    ui: &mut Ui,
    pass: &Pass<'_>,
    family: &Family,
    (expansion, ages): (&mut Expansion, &mut AgeCache),
    out: &mut SidebarOutput,
) -> bool {
    let input = pass.input;
    let group = family.group(pass.groups);
    let shown = if pass.filtering {
        family.rows.iter().filter(|r| pass.is_shown(r)).count()
    } else {
        family.rows.len()
    };
    if shown == 0 {
        return false;
    }
    // A worktree group listed by itself (its repository is outside the scope) that was removed:
    // new sessions go to the repository, not into the vanished folder.
    let removed = group
        .worktree
        .is_none()
        .then(|| input.removed.get(&group.cwd))
        .flatten();
    let open = expansion.is_open(&group.cwd, pass.filtering);

    let pid = ui.make_persistent_id(("project", &group.cwd));
    let mut state = CollapsingState::load_with_default_open(ui.ctx(), pid, true);
    state.set_open(open);
    let open_t = state.openness(ui.ctx());
    let wt_allowed = input.omp_available && group.worktree.is_none() && removed.is_none();
    let hdr = group_header(
        ui,
        &HeaderSpec {
            group,
            shown,
            total: family.rows.len(),
            wt_button: wt_allowed,
            default_view: input.default_view,
        },
        open_t,
        out,
    );
    let mut menu_collapse = None;
    crate::theme::context_menu(&hdr.resp, 236.0, |ui| {
        if menu_item(ui, ICON_PLUS, "New session here", None, true, false).clicked() {
            let target = removed.map_or(&group.cwd, |r| &r.primary);
            out.actions.push(SidebarAction::New(target.clone()));
            out.interacted = true;
            ui.close();
        }
        let (view, _) = view_action(input.default_view, None);
        let label = if view == SessionView::Chat {
            "New chat session"
        } else {
            "New terminal session"
        };
        if menu_item(ui, view_icon(view), label, None, true, false).clicked() {
            let target = removed.map_or(&group.cwd, |r| &r.primary);
            out.actions
                .push(SidebarAction::NewView(target.clone(), view));
            out.interacted = true;
            ui.close();
        }
        if wt_allowed
            && menu_item(
                ui,
                ICON_GIT_BRANCH_PLUS,
                "New session in worktree…",
                None,
                true,
                false,
            )
            .on_hover_text("Create a git worktree on a new branch and start a session in it")
            .clicked()
        {
            out.actions
                .push(SidebarAction::NewWorktree(group.cwd.clone()));
            out.interacted = true;
            ui.close();
        }
        let focused = input.scope == Some(group.cwd.as_path());
        if menu_item(ui, ICON_FOCUS, "Focus this folder", None, !focused, false)
            .on_hover_text("Show only the sessions of this folder")
            .clicked()
        {
            out.actions
                .push(SidebarAction::SetScope(Some(group.cwd.clone())));
            out.interacted = true;
            ui.close();
        }
        if menu_item(ui, ICON_HARD_DRIVE, "Manage sessions…", None, true, false)
            .on_hover_text("Open the session manager on this project")
            .clicked()
        {
            out.actions
                .push(SidebarAction::ManageSessions(Some(group.cwd.clone())));
            out.interacted = true;
            ui.close();
        }
        if menu_item(ui, ICON_FOLDER_OPEN, "Open folder", None, true, false).clicked() {
            out.actions
                .push(SidebarAction::OpenFolder(group.cwd.clone()));
            ui.close();
        }
        if menu_item(ui, ICON_COPY, "Copy path", None, true, false).clicked() {
            out.actions.push(SidebarAction::CopyText {
                text: group.cwd.to_string_lossy().into_owned(),
                what: "project path",
            });
            ui.close();
        }
        backup_items(ui, input.backup, &group.cwd, true, out);
        ui.separator();
        if menu_item(ui, ICON_CHEVRONS_DOWN_UP, "Collapse all", None, true, false).clicked() {
            menu_collapse = Some(true);
            ui.close();
        }
        if menu_item(ui, ICON_CHEVRONS_UP_DOWN, "Expand all", None, true, false).clicked() {
            menu_collapse = Some(false);
            ui.close();
        }
    });
    if let Some(collapse) = menu_collapse {
        expansion.set_all(
            pass.families
                .iter()
                .map(|f| f.group(pass.groups).cwd.as_path()),
            !collapse,
        );
        state.set_open(pass.filtering || !collapse);
    }
    if hdr.toggle && !pass.filtering {
        expansion.set(&group.cwd, !open);
        out.interacted = true;
        state.set_open(!open);
    }
    state.store(ui.ctx());
    state.show_body_unindented(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for r in family.rows.iter().filter(|r| pass.is_shown(r)) {
            let row_group = &pass.groups[r.group];
            let Some(entry) = row_group.sessions.get(r.session) else {
                continue;
            };
            let row = RowCtx {
                selected: input.active_path == Some(entry.path.as_path()),
                running: input.running.is_running(&entry.id, &entry.path)
                    || input.open_paths.contains(&entry.path.as_path()),
                chip: RowWorktree::of(row_group, &entry.id, input.removed, &input.parked).map(
                    |wt| {
                        let own = match wt {
                            RowWorktree::Live(w) => input.finishing.get(&w.path).copied(),
                            _ => None,
                        };
                        RowChip {
                            finishing: own.is_some(),
                            own_finish: own == Some(true),
                            wt,
                        }
                    },
                ),
            };
            let age = ages.get(pass.age_key, input.now, entry);
            session_row(ui, entry, age, &row, input, out);
        }
        ui.add_space(2.0);
    });
    ui.add_space(4.0);
    true
}

/// A worktree action's hover button as the row needs it every frame: the icon and whether it is
/// enabled, from the row's chip alone (no allocation, no lookups). The full [`RowAct`] (labels,
/// action) is built only when hovered, clicked or in a menu.
#[derive(Clone, Copy)]
struct Slot {
    icon: Icon,
    enabled: bool,
}

/// The hover-button slots of a row, in the order of [`live_acts`] / [`parked_acts`]
/// (index 0 is the rightmost button).
fn slots(chip: &RowChip<'_>, omp: bool) -> [Option<Slot>; 2] {
    match chip.wt {
        RowWorktree::Live(_) => [
            Some(if chip.finishing {
                Slot {
                    icon: ICON_X,
                    enabled: chip.own_finish,
                }
            } else {
                Slot {
                    icon: ICON_GIT_MERGE,
                    enabled: omp,
                }
            }),
            Some(Slot {
                icon: ICON_TRASH_2,
                enabled: true,
            }),
        ],
        RowWorktree::Parked(r, at) => [
            Some(Slot {
                icon: ICON_GIT_BRANCH_PLUS,
                enabled: omp && at.is_none(),
            }),
            Some(if at == Some(r.primary.as_path()) {
                Slot {
                    icon: ICON_ARROW_LEFT_RIGHT,
                    enabled: true,
                }
            } else {
                Slot {
                    icon: ICON_FOLDER_GIT_2,
                    enabled: at.is_none(),
                }
            }),
        ],
        RowWorktree::Removed(..) => [None, None],
    }
}

/// One worktree action of a session row. The row's context menu and its hover buttons are both
/// built from these, so they always offer the same actions under the same gating.
struct RowAct {
    icon: Icon,
    label: String,
    /// The menu item's hover text while enabled.
    tip: &'static str,
    enabled: bool,
    /// Why it is disabled.
    why: Option<String>,
    danger: bool,
    action: SidebarAction,
}

impl RowAct {
    /// Tooltip of the hover button: the label, plus the reason while disabled.
    fn button_tip(&self) -> String {
        match (&self.why, self.enabled) {
            (Some(why), false) => format!("{} — {why}", self.label),
            _ => self.label.clone(),
        }
    }
}

/// "Finish worktree" / "Cancel finishing" and "Remove worktree…" for a session that runs in
/// worktree `w`.
fn live_acts(env: &MenuEnv<'_>, w: &Worktree) -> Vec<RowAct> {
    let mut acts = Vec::new();
    if let Some(&own) = env.finishing.get(&w.path) {
        acts.push(RowAct {
            icon: ICON_X,
            label: "Cancel finishing".to_owned(),
            tip: "Stop waiting for the agent; the worktree stays",
            enabled: own,
            why: (!own).then(|| "another device or window started this finish".to_owned()),
            danger: false,
            action: SidebarAction::CancelFinish(w.path.clone()),
        });
    } else {
        acts.push(RowAct {
            icon: ICON_GIT_MERGE,
            label: "Finish worktree".to_owned(),
            tip: "Ask the session's agent to commit and merge the branch; the worktree is removed once it is merged and clean, and its sessions move into the project folder",
            enabled: env.omp_available,
            why: (!env.omp_available).then(|| "omp was not found".to_owned()),
            danger: false,
            action: SidebarAction::FinishWorktree(w.clone()),
        });
    }
    let parkable = env.parked.is_parkable(w);
    acts.push(RowAct {
        icon: ICON_TRASH_2,
        label: if parkable {
            "Remove / park worktree…"
        } else {
            "Remove worktree…"
        }
        .to_owned(),
        tip: "Remove the folder; its sessions move into the project folder. A merged branch is deleted with it; a clean, pushed branch that is not merged yet is parked: the branch is kept and can be continued later",
        enabled: true,
        why: None,
        danger: true,
        action: SidebarAction::RemoveWorktree(w.clone()),
    });
    acts
}

/// "Continue in worktree" and "Continue in main checkout" (or "Switch main checkout back to
/// <base>") for a session of the parked branch `r`; `checked_out` is where the branch is checked out.
fn parked_acts(
    env: &MenuEnv<'_>,
    entry: &SessionEntry,
    r: &ParkedBranch,
    checked_out: Option<&Path>,
) -> Vec<RowAct> {
    let in_primary = checked_out == Some(r.primary.as_path());
    let where_text = |p: &Path| {
        if p == r.primary {
            "the main checkout".to_owned()
        } else {
            abbreviate_home(p)
        }
    };
    let busy_why = checked_out.map(|p| format!("The branch is checked out in {}", where_text(p)));
    let mut acts = vec![RowAct {
        icon: ICON_GIT_BRANCH_PLUS,
        label: "Continue in worktree".to_owned(),
        tip: "Create a worktree on the parked branch and move its sessions into it",
        enabled: env.omp_available && checked_out.is_none(),
        why: busy_why
            .clone()
            .or_else(|| (!env.omp_available).then(|| "omp was not found".to_owned())),
        danger: false,
        action: SidebarAction::ContinueInWorktree(entry.clone()),
    }];
    if in_primary {
        acts.push(RowAct {
            icon: ICON_ARROW_LEFT_RIGHT,
            label: format!("Switch main checkout back to {}", r.base),
            tip: "Check the base branch out in the main checkout again; the parked branch stays",
            enabled: true,
            why: None,
            danger: false,
            action: SidebarAction::SwitchMainBack(entry.clone()),
        });
    } else {
        acts.push(RowAct {
            icon: ICON_FOLDER_GIT_2,
            label: "Continue in main checkout".to_owned(),
            tip: "Check the parked branch out in the main checkout and open this session there",
            enabled: checked_out.is_none(),
            why: busy_why,
            danger: false,
            action: SidebarAction::ContinueInMain(entry.clone()),
        });
    }
    acts
}

/// The worktree actions of the row of `entry`: its live worktree's, or its parked branch's.
fn row_acts(env: &MenuEnv<'_>, entry: &SessionEntry) -> Vec<RowAct> {
    if let Some(w) = env.worktree_of(&entry.cwd) {
        return live_acts(env, w);
    }
    match env.parked.record_of(&entry.id) {
        Some(r) => parked_acts(env, entry, r, env.parked.checked_out(r)),
        None => Vec::new(),
    }
}

fn act_item(ui: &mut Ui, a: &RowAct, out: &mut SidebarOutput) {
    let item = menu_item(ui, a.icon, &a.label, None, a.enabled, a.danger);
    let item = if a.enabled {
        item.on_hover_text(a.tip)
    } else {
        item.on_disabled_hover_text(a.why.as_deref().unwrap_or(a.tip))
    };
    if item.clicked() {
        out.actions.push(a.action.clone());
        out.interacted = true;
        ui.close();
    }
}

/// The worktree actions, "Open worktree folder" and "Copy worktree path" in the menu of a
/// session that runs in worktree `w`.
fn worktree_items(ui: &mut Ui, env: &MenuEnv<'_>, w: &Worktree, out: &mut SidebarOutput) {
    for a in live_acts(env, w) {
        act_item(ui, &a, out);
    }
    if menu_item(
        ui,
        ICON_FOLDER_OPEN,
        "Open worktree folder",
        None,
        true,
        false,
    )
    .clicked()
    {
        out.actions.push(SidebarAction::OpenFolder(w.path.clone()));
        ui.close();
    }
    if menu_item(ui, ICON_COPY, "Copy worktree path", None, true, false).clicked() {
        out.actions.push(SidebarAction::CopyText {
            text: w.path.to_string_lossy().into_owned(),
            what: "worktree path",
        });
        ui.close();
    }
}

/// Nothing under the scope yet: say so (the button above the list starts the first session).
fn scope_empty(ui: &mut Ui, dir: &Path) {
    let pal = Palette::of_ui(ui);
    empty_state(
        ui,
        ICON_FOLDER_OPEN,
        pal.accent,
        "No sessions in this folder",
        "Start one with the button above, or clear the folder to see all projects.",
    );
    ui.add_space(10.0);
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(dir.display().to_string())
                .monospace()
                .small()
                .color(pal.weak),
        );
    });
}

fn header_button(ui: &mut Ui, icon: Icon, tip: &str, spinning: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(
        vec2(theme::dens(28.0, 32.0), theme::dens(28.0, 36.0)),
        Sense::click(),
    );
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
    let p = ui.painter();
    let neutral = theme::flat();
    p.rect_filled(
        rect,
        if neutral { 8 } else { 10 },
        theme::hover_fill(pal).gamma_multiply(0.85 * hov),
    );
    if spinning {
        ui.put(
            Rect::from_center_size(rect.center(), vec2(16.0, 16.0)),
            crate::widgets::Spin::new(if neutral { 14.0 } else { 16.0 }, pal.accent_b),
        );
    } else {
        paint_icon(
            p,
            rect.center(),
            Align2::CENTER_CENTER,
            icon,
            if neutral { 15.0 } else { 19.0 },
            theme::mix(pal.weak, pal.text_strong, hov),
        );
    }
    resp.on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
}

struct HeaderResult {
    resp: egui::Response,
    /// The header body (not a button) was clicked.
    toggle: bool,
}

struct HeaderSpec<'a> {
    group: &'a ProjectGroup,
    /// Rows shown below the header (filtered).
    shown: usize,
    /// Rows of the project before filtering.
    total: usize,
    /// Offer the hover button that starts a session in a new worktree.
    wt_button: bool,
    default_view: SessionView,
}

/// The header's name and its faint hint: folder name and parent path.
fn header_text<'a>(spec: &'a HeaderSpec<'_>) -> (Cow<'a, str>, Cow<'a, str>) {
    let (parent, name) = split_display(&spec.group.display);
    (Cow::Borrowed(name), Cow::Borrowed(parent))
}

fn header_icon(open_t: f32) -> Icon {
    if open_t > 0.5 {
        ICON_FOLDER_OPEN
    } else {
        ICON_FOLDER
    }
}

/// Where a header puts its parts.
struct HeaderGeom {
    rect: Rect,
    plus: Rect,
    wt: Option<Rect>,
}

impl HeaderGeom {
    /// Width at the right of the name taken by buttons.
    fn reserved(&self, base: f32) -> f32 {
        base + 28.0 + if self.wt.is_some() { 28.0 } else { 0.0 }
    }
}

/// Hover animation of a header and its buttons (0..=1).
struct HeaderHover {
    hov: f32,
    plus: f32,
    wt: f32,
}

fn group_header(
    ui: &mut Ui,
    spec: &HeaderSpec<'_>,
    open_t: f32,
    out: &mut SidebarOutput,
) -> HeaderResult {
    let pal = Palette::of_ui(ui);
    let group = spec.group;
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), header_h()), Sense::click());
    let plus_rect =
        Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(24.0, 24.0));
    let plus = ui.interact(plus_rect, resp.id.with("new"), Sense::click());
    let wt_rect = plus_rect.translate(vec2(-28.0, 0.0));
    let wt_btn = spec
        .wt_button
        .then(|| ui.interact(wt_rect, resp.id.with("new-wt"), Sense::click()));
    let (view, _) = view_action(spec.default_view, None);
    let view_rect = plus_rect.translate(vec2(if spec.wt_button { -56.0 } else { -28.0 }, 0.0));
    let view_button = ui
        .interact(view_rect, resp.id.with("new-view"), Sense::click())
        .on_hover_text(if view == SessionView::Chat {
            "New chat session"
        } else {
            "New terminal session"
        });
    let over = |r: &Option<egui::Response>| r.as_ref().is_some_and(egui::Response::hovered);
    let hov_raw = resp.hovered()
        || plus.hovered()
        || view_button.hovered()
        || over(&wt_btn)
        || resp.context_menu_opened();
    let ctx = ui.ctx().clone();
    let hover = HeaderHover {
        hov: ctx.animate_bool_with_time(resp.id.with("hov"), hov_raw, 0.1),
        plus: ctx.animate_bool_with_time(plus.id.with("hov"), plus.hovered(), 0.1),
        wt: ctx.animate_bool_with_time(resp.id.with("wt-hov"), over(&wt_btn), 0.1),
    };
    let geom = HeaderGeom {
        rect,
        plus: plus_rect,
        wt: wt_btn.as_ref().map(|_| wt_rect),
    };

    let (name, hint) = header_text(spec);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if theme::flat() {
            paint_header_neutral(p, pal, spec, (&name, &hint), &geom, open_t, &hover);
        } else {
            paint_header_neon(p, pal, spec, (&name, &hint), &geom, open_t, &hover);
        }
        paint_header_extras(p, pal, &geom, &hover);
        if hover.hov > 0.01 {
            paint_icon(
                p,
                view_rect.center(),
                Align2::CENTER_CENTER,
                view_icon(view),
                16.0,
                pal.accent,
            );
        }
    }
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    group_hover(&resp, spec, &name, open_t);
    for (btn, tip) in [
        (&Some(plus.clone()), "New omp session in this project"),
        (&wt_btn, "New session in a new git worktree"),
    ] {
        if let Some(b) = btn {
            if b.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            let _ = b.clone().on_hover_text(tip);
        }
    }
    let none = HeaderResult {
        resp: resp.clone(),
        toggle: false,
    };
    if plus.clicked() {
        out.actions.push(SidebarAction::New(group.cwd.clone()));
        out.interacted = true;
        return none;
    }
    if wt_btn.as_ref().is_some_and(egui::Response::clicked) {
        out.actions
            .push(SidebarAction::NewWorktree(group.cwd.clone()));
        out.interacted = true;
        return none;
    }
    if view_button.clicked() {
        out.actions
            .push(SidebarAction::NewView(group.cwd.clone(), view));
        out.interacted = true;
        return none;
    }
    let toggle = resp.clicked() && !view_button.hovered() && !over(&wt_btn) && !plus.hovered();
    HeaderResult { resp, toggle }
}

/// Hover card of a project header: folder and session count.
fn group_hover(resp: &egui::Response, spec: &HeaderSpec<'_>, name: &str, open_t: f32) {
    const KEY_W: f32 = 100.0;
    let group = spec.group;
    let head = card::Header::new(header_icon(open_t), name);
    card::hover(
        resp,
        340.0,
        false,
        &head,
        |ui| {
            let n = spec.total;
            let text = if n == 1 {
                "1 session".to_owned()
            } else {
                format!("{n} sessions")
            };
            badge::show(ui, None, &text, Hue::Amber, tips::VALUE);
        },
        |ui| {
            card::kv(ui, KEY_W, "Folder", |ui| tips::path(ui, &group.cwd));
            card::footer_hints(
                ui,
                &[("click", "fold / unfold"), ("right-click", "actions")],
            );
        },
    );
}

/// Name (medium) and hint (faint) on one line, cut at `max_width`.
fn header_job(
    pal: &Palette,
    (name, hint): (&str, &str),
    (name_px, hint_px): (f32, f32),
    max_width: f32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(
        name,
        0.0,
        TextFormat {
            font_id: crate::theme::font(name_px),
            color: pal.text_strong,
            ..Default::default()
        },
    );
    if !hint.is_empty() {
        job.append(
            &format!("  {hint}"),
            0.0,
            TextFormat {
                font_id: crate::theme::font(hint_px),
                color: pal.weak,
                ..Default::default()
            },
        );
    }
    job.wrap.max_width = max_width.max(10.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job
}

/// The hover button that starts a session in a new worktree.
fn paint_header_extras(p: &egui::Painter, pal: &Palette, geom: &HeaderGeom, hover: &HeaderHover) {
    if let Some(r) = geom.wt
        && hover.hov > 0.01
    {
        if theme::flat() {
            p.rect_filled(r, 6, pal.raised.gamma_multiply(hover.hov * hover.wt));
        } else {
            p.rect_filled(
                r,
                8,
                theme::mix(pal.raised, pal.accent, hover.wt * 0.5).gamma_multiply(hover.hov),
            );
        }
        paint_icon(
            p,
            r.center(),
            Align2::CENTER_CENTER,
            ICON_GIT_BRANCH_PLUS,
            if theme::flat() { 15.0 } else { 17.0 },
            theme::mix(pal.weak, pal.text_strong, hover.wt).gamma_multiply(hover.hov),
        );
    }
}

/// Neon header: folder glyph, name + parent path, count chip that crossfades into the "+".
fn paint_header_neon(
    p: &egui::Painter,
    pal: &Palette,
    spec: &HeaderSpec<'_>,
    text: (&str, &str),
    geom: &HeaderGeom,
    open_t: f32,
    hover: &HeaderHover,
) {
    let rect = geom.rect;
    paint_header_chrome(p, pal, rect, open_t, hover.hov);
    paint_icon(
        p,
        pos2(rect.left() + 34.0, rect.center().y),
        Align2::CENTER_CENTER,
        header_icon(open_t),
        18.0,
        pal.accent,
    );
    let job = header_job(
        pal,
        text,
        (14.0, 11.5),
        rect.width() - 46.0 - geom.reserved(40.0),
    );
    let galley = p.layout_job(job);
    p.galley(
        pos2(rect.left() + 48.0, rect.center().y - galley.size().y / 2.0),
        galley,
        pal.text,
    );
    let g = p.layout_no_wrap(
        spec.shown.to_string(),
        crate::theme::font(11.5),
        pal.weak.gamma_multiply(1.0 - hover.hov),
    );
    let chip = Rect::from_center_size(
        pos2(rect.right() - 16.0, rect.center().y),
        vec2((g.size().x + 12.0).max(22.0), 18.0),
    );
    p.rect_filled(chip, 9, pal.surface.gamma_multiply(1.0 - hover.hov));
    p.galley(chip.center() - g.size() / 2.0, g, pal.weak);
    if hover.hov > 0.01 {
        p.rect_filled(
            geom.plus,
            8,
            theme::mix(pal.raised, pal.accent, hover.plus * 0.5).gamma_multiply(hover.hov),
        );
        paint_icon(
            p,
            geom.plus.center(),
            Align2::CENTER_CENTER,
            ICON_PLUS,
            17.0,
            theme::mix(pal.weak, pal.text_strong, hover.plus).gamma_multiply(hover.hov),
        );
    }
}

/// Neutral header: chevron, folder glyph, medium name + faint hint, plain count that turns
/// into the "+" on hover. No fills except the hover wash.
fn paint_header_neutral(
    p: &egui::Painter,
    pal: &Palette,
    spec: &HeaderSpec<'_>,
    text: (&str, &str),
    geom: &HeaderGeom,
    open_t: f32,
    hover: &HeaderHover,
) {
    let rect = geom.rect;
    paint_header_chrome(p, pal, rect, open_t, hover.hov);
    let y = rect.center().y;
    paint_icon(
        p,
        pos2(rect.left() + 31.0, y),
        Align2::CENTER_CENTER,
        header_icon(open_t),
        15.0,
        theme::mix(pal.glyph, pal.text_strong, open_t * 0.5),
    );
    let job = header_job(
        pal,
        text,
        (13.0, 11.0),
        rect.width() - 44.0 - geom.reserved(40.0),
    );
    let galley = p.layout_job(job);
    p.galley(
        pos2(rect.left() + 46.0, y - galley.size().y / 2.0),
        galley,
        pal.text,
    );
    let count = p.layout_no_wrap(
        spec.shown.to_string(),
        crate::theme::font(11.0),
        pal.weak.gamma_multiply(1.0 - hover.hov),
    );
    p.galley(
        pos2(
            rect.right() - 14.0 - count.size().x / 2.0,
            y - count.size().y / 2.0,
        ),
        count,
        pal.weak,
    );
    if hover.hov > 0.01 {
        p.rect_filled(
            geom.plus,
            6,
            pal.raised.gamma_multiply(hover.hov * hover.plus),
        );
        paint_icon(
            p,
            geom.plus.center(),
            Align2::CENTER_CENTER,
            ICON_PLUS,
            15.0,
            theme::mix(pal.weak, pal.text_strong, hover.plus).gamma_multiply(hover.hov),
        );
    }
}

/// Header hover wash and the open/closed chevron at the left (Neon: rotated by `open_t`).
fn paint_header_chrome(p: &egui::Painter, pal: &Palette, rect: Rect, open_t: f32, hov: f32) {
    let color = theme::mix(pal.weak, pal.text_strong, hov);
    if hov > 0.01 {
        let radius = if theme::flat() { 6 } else { 9 };
        p.rect(
            rect,
            radius,
            theme::hover_fill(pal).gamma_multiply(0.75 * hov),
            Stroke::new(1.0, theme::hover_stroke(pal).gamma_multiply(hov)),
            StrokeKind::Inside,
        );
    }
    if theme::flat() {
        let chev = if open_t > 0.5 {
            ICON_CHEVRON_DOWN
        } else {
            ICON_CHEVRON_RIGHT
        };
        let c = pos2(rect.left() + 12.0, rect.center().y);
        paint_icon(p, c, Align2::CENTER_CENTER, chev, 14.0, color);
        return;
    }
    // Rotated by the open animation (0 = right, 1 = down).
    let c = pos2(rect.left() + 14.0, rect.center().y);
    let a = open_t * std::f32::consts::FRAC_PI_2;
    let rot = |x: f32, y: f32| c + vec2(x * a.cos() - y * a.sin(), x * a.sin() + y * a.cos());
    p.add(Shape::line(
        vec![rot(-2.5, -4.0), rot(2.5, 0.0), rot(-2.5, 4.0)],
        Stroke::new(1.7, color),
    ));
}

/// Most rows the "Open tabs" list shows before it scrolls.
const OPEN_TABS_MAX_ROWS: f32 = 6.0;
/// Icon slot of the status indicators in sidebar rows.
const STATUS_SIZE: f32 = 14.0;

/// Header of a pinned section ("Open tabs", "Running sessions"): chevron, title in the UI font,
/// the count and, while `waiting > 0` sessions need the user, the attention badge (the body may
/// be collapsed). A click toggles; the returned state shows the body. `default_open` applies
/// until the user toggles (egui memory is not persisted, so it applies on every launch).
fn section_header(
    ui: &mut Ui,
    out: &mut SidebarOutput,
    (salt, title): (&str, &str),
    count: usize,
    waiting: usize,
    default_open: bool,
) -> CollapsingState {
    let pal = Palette::of_ui(ui);
    let id = ui.make_persistent_id(salt);
    let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, default_open);
    let open_t = state.openness(ui.ctx());
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), header_h()), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        paint_header_chrome(p, pal, rect, open_t, hov);
        let x = rect.left() + if theme::flat() { 26.0 } else { 28.0 };
        let y = rect.center().y;
        let size = if theme::flat() { 13.0 } else { 14.0 };
        p.text(
            pos2(x, y),
            Align2::LEFT_CENTER,
            title,
            theme::font(size),
            pal.text_strong,
        );
        let mut right = rect.right() - 10.0;
        let count = p.text(
            pos2(right, y),
            Align2::RIGHT_CENTER,
            count.to_string(),
            theme::font(size - 2.0),
            pal.weak,
        );
        right = count.left() - 10.0;
        if waiting > 0 {
            let c = pos2(right - STATUS_SIZE / 2.0, y);
            tabs::paint_status(ui, c, STATUS_SIZE, TabStatus::Attention);
        }
    }
    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
        state.toggle(ui);
        out.interacted = true;
    }
    state.store(ui.ctx());
    state
}

/// "Open tabs": every tab in strip order with its status indicator; a click activates, the ✕ or a
/// middle click closes. Pinned above the session list (its own capped scroll area).
fn open_tabs_section(ui: &mut Ui, input: &SidebarInput<'_>, out: &mut SidebarOutput) {
    if input.tabs.is_empty() {
        return;
    }
    let pal = Palette::of_ui(ui);
    let waiting = input
        .tabs
        .iter()
        .enumerate()
        .filter(|(i, t)| t.status(*i == input.active_tab) == TabStatus::Attention)
        .count();
    let mut state = section_header(
        ui,
        out,
        ("open-tabs", "Open tabs"),
        input.tabs.len(),
        waiting,
        false,
    );
    state.show_body_unindented(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt("open-tabs-list")
            .max_height((row_h() + 2.0) * OPEN_TABS_MAX_ROWS)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for (i, tab) in input.tabs.iter().enumerate() {
                    open_tab_row(ui, input, i, tab, out);
                }
            });
    });
    ui.add_space(theme::dens(6.0, 8.0));
    let sep = ui.available_rect_before_wrap();
    ui.painter()
        .hline(sep.x_range(), sep.top(), Stroke::new(1.0, pal.stroke));
    ui.add_space(theme::dens(6.0, 8.0));
}

fn open_tab_row(
    ui: &mut Ui,
    input: &SidebarInput<'_>,
    i: usize,
    tab: &Tab,
    out: &mut SidebarOutput,
) {
    let pal = Palette::of_ui(ui);
    let active = i == input.active_tab;
    let status = tab.status(active);
    let label = tab.label(input.labels);
    let folder = tab.cwd.file_name().and_then(|n| n.to_str());
    let neutral = theme::flat();

    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h()), Sense::click());
    let close_rect =
        Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(20.0, 20.0));
    let close = ui.interact(close_rect, resp.id.with("close"), Sense::click());
    let (view, view_label) = view_action(input.default_view, Some(tab.view()));
    let view_why = view_switch_reason(tab, view, input.host_available);
    let view_rect = close_rect.translate(vec2(-26.0, 0.0));
    let view_button = ui
        .interact(view_rect, resp.id.with("session-view"), Sense::click())
        .on_hover_text(view_why.unwrap_or(view_label));
    let ctx = ui.ctx().clone();
    let hov = ctx.animate_bool_with_time(
        resp.id.with("hov"),
        resp.hovered() || close.hovered() || view_button.hovered() || resp.context_menu_opened(),
        0.12,
    );
    let sel = ctx.animate_bool_with_time(resp.id.with("sel"), active, 0.18);
    if active && sel < 1.0 {
        // Just activated (from anywhere): keep its row in view.
        ui.scroll_to_rect(rect, None);
    }
    if ui.is_rect_visible(rect) {
        let pill = rect.shrink2(vec2(2.0, 0.0));
        let p = ui.painter();
        paint_row_bg(p, pal, pill, (hov, sel));
        if status == TabStatus::Attention {
            tabs::paint_attention_pill(p, pal, pill, if neutral { 6 } else { 10 });
        }
        let ic = pos2(
            pill.left() + if neutral { 15.0 } else { 19.0 },
            pill.center().y,
        );
        if !tabs::paint_status(ui, ic, STATUS_SIZE, status) {
            let (icon, col) = tab.kind_icon(pal, sel);
            paint_icon(p, ic, Align2::CENTER_CENTER, icon, STATUS_SIZE, col);
        }
        let color = theme::mix(pal.text, pal.text_strong, sel.max(hov * 0.6));
        let mut job = LayoutJob::default();
        job.append(
            label,
            0.0,
            TextFormat {
                font_id: theme::font(if neutral { 13.0 } else { 13.5 }),
                color,
                ..Default::default()
            },
        );
        if let Some(folder) = folder {
            job.append(
                &format!("  {folder}"),
                0.0,
                TextFormat {
                    font_id: theme::font(11.0),
                    color: pal.weak,
                    ..Default::default()
                },
            );
        }
        let text_x = pill.left() + if neutral { 28.0 } else { 32.0 };
        job.wrap.max_width = (view_rect.left() - 4.0 - text_x).max(10.0);
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let galley = p.layout_job(job);
        p.galley(
            pos2(text_x, pill.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
        if hov > 0.01 {
            paint_icon(
                p,
                view_rect.center(),
                Align2::CENTER_CENTER,
                view_icon(view),
                14.0,
                if view_why.is_none() {
                    pal.accent
                } else {
                    pal.weak
                },
            );
            let ch = ctx.animate_bool_with_time(close.id.with("hov"), close.hovered(), 0.1);
            p.circle_filled(
                close_rect.center(),
                10.0,
                theme::hover_fill(pal).gamma_multiply(ch),
            );
            paint_icon(
                p,
                close_rect.center(),
                Align2::CENTER_CENTER,
                ICON_X,
                14.0,
                theme::mix(pal.weak, pal.text_strong, ch).gamma_multiply(hov),
            );
        }
    }
    let close = close.on_hover_text("Close tab");
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    tabs::hover_card(&resp, pal, tab, label, status);
    if close.clicked() || resp.middle_clicked() {
        out.actions.push(SidebarAction::CloseTab(i));
    } else if view_button.clicked() {
        if view_why.is_none() {
            out.actions
                .push(SidebarAction::SwitchView { uid: tab.uid, view });
            out.interacted = true;
        }
    } else if resp.clicked() {
        out.actions.push(SidebarAction::ActivateTab(i));
        out.interacted = true;
    }
    let env = input.menu_env();
    theme::context_menu(&resp, 230.0, |ui| tab_menu(ui, i, tab, &env, out));
}

/// The branch chip of a session that runs in a worktree of the project.
struct RowChip<'a> {
    wt: RowWorktree<'a>,
    /// The worktree is being finished: the chip says so instead of naming the branch.
    finishing: bool,
    /// This window started the finish, so it can cancel it.
    own_finish: bool,
}

impl RowChip<'_> {
    /// Longest branch name shown in the chip (characters); a parked chip is longer ("parked ·")
    /// and shorter in its name to leave the title room.
    fn max_chars(&self) -> usize {
        match self.wt {
            RowWorktree::Parked(..) => 8,
            _ => 14,
        }
    }

    /// The chip, its branch name cut further while it is wider than `max_w` (the title keeps room).
    fn badge(&self, p: &egui::Painter, pal: &Palette, size: f32, max_w: f32) -> badge::Badge {
        let mut chars = self.max_chars();
        loop {
            let b = self.badge_with(p, pal, size, chars);
            if b.width() <= max_w || chars <= 3 {
                return b;
            }
            chars -= 1;
        }
    }

    fn badge_with(
        &self,
        p: &egui::Painter,
        pal: &Palette,
        size: f32,
        chars: usize,
    ) -> badge::Badge {
        let label = self.wt.chip_label();
        let label: std::borrow::Cow<'_, str> = match label.char_indices().nth(chars) {
            Some((at, _)) => {
                let keep = label.char_indices().nth(chars - 1).map_or(at, |(i, _)| i);
                std::borrow::Cow::Owned(format!("{}…", &label[..keep]))
            }
            None => label,
        };
        match (self.wt, self.finishing) {
            (_, true) => badge::Badge::single(
                p,
                Some(ICON_GIT_MERGE),
                "Finishing…",
                Hue::Amber.color(pal),
                size,
            ),
            (RowWorktree::Live(_), false) => {
                badge::Badge::single(p, Some(ICON_GIT_BRANCH), &label, Hue::Teal.color(pal), size)
            }
            (RowWorktree::Parked(..), false) => badge::Badge::single(
                p,
                Some(ICON_GIT_BRANCH),
                &format!("parked · {label}"),
                Hue::Violet.color(pal),
                size,
            ),
            (RowWorktree::Removed(..), false) => {
                let dim = Hue::Teal.color(pal).gamma_multiply(0.55);
                badge::Badge::new(
                    p,
                    Some(ICON_GIT_BRANCH),
                    &[(&label, dim), ("removed", pal.weak)],
                    dim,
                    size,
                )
            }
        }
    }
}

/// What one session row shows besides its entry.
struct RowCtx<'a> {
    running: bool,
    selected: bool,
    chip: Option<RowChip<'a>>,
}

fn session_row(
    ui: &mut Ui,
    entry: &SessionEntry,
    age_text: &str,
    row: &RowCtx<'_>,
    input: &SidebarInput<'_>,
    out: &mut SidebarOutput,
) {
    let pal = Palette::of_ui(ui);
    let (running, selected) = (row.running, row.selected);
    // A delete/move/copy of this session is in flight on a worker.
    let busy = input.busy.contains(&entry.path);
    // The live omp state of a tab showing this session (replaces the LED).
    let live = input
        .tab_activity
        .get(entry.path.as_path())
        .copied()
        .filter(|_| !busy);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h()), Sense::click());
    let env = input.menu_env();
    // The worktree actions: a hover button each (the same actions as the context menu's).
    let slots: Vec<Slot> = row
        .chip
        .as_ref()
        .map(|c| {
            slots(c, input.omp_available)
                .into_iter()
                .flatten()
                .collect()
        })
        .unwrap_or_default();
    let work_slots = slots.len();
    let view_offer = row_view_action(
        input.default_view,
        input
            .tabs
            .iter()
            .find(|t| t.session_path() == Some(entry.path.as_path()))
            .map(Tab::view),
        running,
    );
    let (other_view, view_label) = view_offer.unwrap_or((input.default_view, "Open"));
    let view_why = input
        .tabs
        .iter()
        .find(|t| t.session_path() == Some(entry.path.as_path()))
        .and_then(|t| view_switch_reason(t, other_view, input.host_available));
    let mut slots = slots;
    if view_offer.is_some() {
        slots.push(Slot {
            icon: view_icon(other_view),
            enabled: view_why.is_none(),
        });
    }
    let pill_rect = rect.shrink2(vec2(2.0, 0.0));
    let btn_rects: Vec<Rect> = (0..slots.len())
        .map(|k| {
            Rect::from_center_size(
                pos2(pill_rect.right() - 14.0 - 28.0 * k as f32, rect.center().y),
                vec2(24.0, 24.0),
            )
        })
        .collect();
    let btns: Vec<egui::Response> = btn_rects
        .iter()
        .enumerate()
        .map(|(k, r)| ui.interact(*r, resp.id.with(("act", k)), Sense::click()))
        .collect();
    // Width at the right of the row the buttons take: the title and chip never move for them.
    let acts_w = if slots.is_empty() {
        0.0
    } else {
        14.0 + 28.0 * slots.len() as f32
    };
    let over_btn = btns.iter().any(egui::Response::hovered);
    // The buttons show (and the age hides) while the row is hovered or its menu is open.
    let show_btns = !slots.is_empty() && (resp.hovered() || over_btn || resp.context_menu_opened());
    // What the chip may take: the title keeps at least ~90 px beside the age / the buttons.
    let chip_budget = pill_rect.width() - 32.0 - 90.0 - acts_w.max(46.0);
    let ctx = ui.ctx().clone();
    let hov = ctx.animate_bool_with_time(
        resp.id.with("hov"),
        resp.hovered() || over_btn || resp.context_menu_opened(),
        0.12,
    );
    let sel = ctx.animate_bool_with_time(resp.id.with("sel"), selected, 0.18);
    let btn_hov: Vec<f32> = btns
        .iter()
        .map(|b| ctx.animate_bool_with_time(b.id.with("hov"), b.hovered(), 0.1))
        .collect();
    if ui.is_rect_visible(rect) {
        let pill = rect.shrink2(vec2(2.0, 0.0));
        let p = ui.painter();
        if theme::flat() {
            let badge = row
                .chip
                .as_ref()
                .map(|c| c.badge(p, pal, 10.5, chip_budget));
            paint_row_neutral(
                p,
                pal,
                pill,
                (entry.label(), age_text),
                (running, busy || live.is_some()),
                (hov, sel),
                (badge.as_ref(), acts_w, show_btns),
            );
        } else {
            let badge = row
                .chip
                .as_ref()
                .map(|c| c.badge(p, pal, 11.0, chip_budget));
            paint_row_bg(p, pal, pill, (hov, sel));
            // Status dot.
            let dot = pos2(pill.left() + 19.0, pill.center().y);
            if busy || live.is_some() {
                // The spinner / tab status is placed below, once the painter borrow ends.
            } else if running {
                theme::glow_dot(p, dot, 3.5, pal.ok, theme::GLOW_REST);
            } else {
                p.circle_filled(dot, 2.8, pal.weak.gamma_multiply(0.45));
            }
            // Age chip.
            let age = p.layout_no_wrap(age_text.to_owned(), crate::theme::font(11.5), pal.weak);
            let chip = Rect::from_center_size(
                pos2(
                    pill.right() - 10.0 - (age.size().x + 12.0) / 2.0,
                    pill.center().y,
                ),
                vec2(age.size().x + 12.0, 18.0),
            );
            if !show_btns {
                p.rect_filled(chip, 9, pal.bg.gamma_multiply(0.55 + 0.45 * sel));
                p.galley(chip.center() - age.size() / 2.0, age, pal.weak);
            }
            // Title.
            let color = theme::mix(pal.text, pal.text_strong, sel.max(hov * 0.5));
            let mut job = LayoutJob::simple_singleline(
                entry.label().to_owned(),
                // One step below the project header's 14 px name.
                crate::theme::font(13.0),
                color,
            );
            let mut text_right = chip.left().min(pill.right() - acts_w);
            if let Some(b) = &badge {
                text_right -= 6.0 + b.width();
                b.paint_at(p, text_right, pill.center().y);
            }
            job.wrap.max_width = (text_right - pill.left() - 32.0 - 8.0).max(10.0);
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            let galley = p.layout_job(job);
            p.galley(
                pos2(pill.left() + 32.0, pill.center().y - galley.size().y / 2.0),
                galley,
                color,
            );
        }
        if show_btns {
            paint_row_acts(p, pal, &slots, &btn_rects, &btn_hov, hov);
        }
    }
    let dot = pos2(
        rect.left() + 2.0 + if theme::flat() { 15.0 } else { 19.0 },
        rect.center().y,
    );
    if busy {
        ui.put(
            Rect::from_center_size(dot, vec2(12.0, 12.0)),
            crate::widgets::Spin::new(12.0, pal.accent_b),
        );
    } else if let Some(status) = live {
        tabs::paint_status(ui, dot, STATUS_SIZE, status);
    }
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    for (k, btn) in btns.iter().enumerate() {
        if btn.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        let _ = btn.clone().on_hover_ui(|ui| {
            if k == work_slots {
                ui.label(view_why.unwrap_or(view_label));
            } else if let Some(act) = row_acts(&env, entry).get(k) {
                ui.label(act.button_tip());
            }
        });
    }
    let clicked = btns
        .iter()
        .enumerate()
        .find(|(k, b)| b.clicked() && slots[*k].enabled)
        .map(|(k, _)| k);
    if let Some(k) = clicked {
        if k == work_slots {
            out.actions.push(
                match input
                    .tabs
                    .iter()
                    .find(|t| t.session_path() == Some(entry.path.as_path()))
                {
                    Some(t) => SidebarAction::SwitchView {
                        uid: t.uid,
                        view: other_view,
                    },
                    None => SidebarAction::OpenView(entry.clone(), other_view),
                },
            );
            out.interacted = true;
        } else if let Some(act) = row_acts(&env, entry).into_iter().nth(k) {
            out.actions.push(act.action);
            out.interacted = true;
        }
    } else if resp.clicked() && !over_btn {
        out.actions.push(SidebarAction::Open(entry.clone()));
        out.interacted = true;
    }
    theme::context_menu(&resp, 230.0, |ui| {
        session_menu(ui, entry, running, true, &env, out)
    });
    const KEY_W: f32 = 76.0;
    let tone = live.map_or(Color32::TRANSPARENT, |s| tips::status_tone(pal, s));
    let mut head = card::Header::new(ICON_MESSAGES_SQUARE, entry.label()).tone(tone);
    if let Some(hint) = live.and_then(TabStatus::hint) {
        head = head.subtitle(hint);
    } else if let Some(chip) = &row.chip {
        if chip.finishing && !chip.own_finish {
            head = head.subtitle("Finishing: another device or window started it");
        } else if chip.finishing {
            head = head.subtitle("Finishing: waiting for the agent to merge");
        } else if matches!(chip.wt, RowWorktree::Removed(..)) {
            head = head.subtitle("Its worktree was removed by OmpDeck");
        } else if matches!(chip.wt, RowWorktree::Parked(..)) {
            head = head.subtitle("Parked — local branch kept");
        }
    }
    // The row's card would cover the buttons' tooltips.
    if over_btn {
        return;
    }
    card::hover(
        &resp,
        340.0,
        false,
        &head,
        |ui| {
            if busy {
                badge::show(ui, None, "working on file", Hue::Blue, tips::VALUE);
            } else if let Some(s) = live {
                tips::status_badge(ui, s, None);
            } else if running {
                badge::show(ui, None, "running", Hue::Green, tips::VALUE);
            }
        },
        |ui| {
            card::kv(ui, KEY_W, "Id", |ui| tips::id_badge(ui, &entry.id));
            card::kv(ui, KEY_W, "Folder", |ui| tips::path(ui, &entry.cwd));
            card::kv(ui, KEY_W, "File", |ui| tips::path(ui, &entry.path));
            if let Some(chip) = &row.chip {
                card::kv(ui, KEY_W, "Branch", |ui| match chip.wt {
                    RowWorktree::Live(w) => {
                        let branch = w.branch.as_deref().unwrap_or("detached HEAD");
                        badge::show(ui, Some(ICON_GIT_BRANCH), branch, Hue::Teal, tips::VALUE);
                    }
                    RowWorktree::Removed(_, r) => {
                        let branch = r.branch.as_deref().unwrap_or("detached HEAD");
                        badge::show(ui, Some(ICON_GIT_BRANCH), branch, Hue::Rose, tips::VALUE);
                    }
                    RowWorktree::Parked(r, _) => {
                        badge::show(
                            ui,
                            Some(ICON_GIT_BRANCH),
                            &r.branch,
                            Hue::Violet,
                            tips::VALUE,
                        );
                    }
                });
                if let RowWorktree::Parked(r, checked_out) = chip.wt {
                    card::kv(ui, KEY_W, "Base", |ui| {
                        badge::show(ui, Some(ICON_GIT_BRANCH), &r.base, Hue::Blue, tips::VALUE);
                    });
                    card::kv(ui, KEY_W, "State", |ui| {
                        badge::show(
                            ui,
                            None,
                            "Parked — local branch kept",
                            Hue::Violet,
                            tips::VALUE,
                        );
                    });
                    if let Some(at) = checked_out {
                        card::kv(ui, KEY_W, "Checkout", |ui| {
                            let text = if at == r.primary {
                                "checked out in the main checkout".to_owned()
                            } else {
                                format!("checked out in {}", abbreviate_home(at))
                            };
                            badge::show(ui, None, &text, Hue::Amber, tips::VALUE);
                        });
                    }
                }
                if let Some(path) = chip.wt.path() {
                    card::kv(ui, KEY_W, "Worktree", |ui| tips::path(ui, path));
                }
            }
            card::kv(ui, KEY_W, "Updated", |ui| {
                let ago = if age_text == "now" {
                    "just now".to_owned()
                } else {
                    format!("{age_text} ago")
                };
                badge::show(ui, Some(ICON_HISTORY), &ago, Hue::Amber, tips::VALUE);
            });
            card::footer_hints(ui, &[("click", "open"), ("right-click", "actions")]);
        },
    );
}

/// Neutral session row: status dot at the left, age at the right, the worktree chip left of the
/// age (background: [`paint_row_bg`]).
fn paint_row_neutral(
    p: &egui::Painter,
    pal: &Palette,
    pill: Rect,
    (label, age_text): (&str, &str),
    (running, placed): (bool, bool),
    (hov, sel): (f32, f32),
    (chip, acts_w, show_btns): (Option<&badge::Badge>, f32, bool),
) {
    paint_row_bg(p, pal, pill, (hov, sel));
    let y = pill.center().y;
    let dot = pos2(pill.left() + 15.0, y);
    if placed {
        // The spinner / tab status is placed by the caller.
    } else if running {
        p.circle_filled(dot, 3.0, pal.ok);
    } else {
        p.circle_filled(dot, 2.5, pal.weak.gamma_multiply(0.4));
    }
    let age = p.layout_no_wrap(age_text.to_owned(), crate::theme::font(11.0), pal.weak);
    let age_x = pill.right() - 10.0 - age.size().x;
    // With hover buttons the age gives way to them (it fades out while the row is hovered).
    let mut text_right = age_x.min(pill.right() - acts_w);
    if let Some(b) = chip {
        text_right -= 6.0 + b.width();
        b.paint_at(p, text_right, y);
    }
    let color = theme::mix(pal.text, pal.text_strong, sel.max(hov * 0.6));
    // One step below the project header's 13 px name.
    let mut job = LayoutJob::simple_singleline(label.to_owned(), crate::theme::font(12.0), color);
    job.wrap.max_width = (text_right - pill.left() - 28.0 - 8.0).max(10.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = p.layout_job(job);
    p.galley(
        pos2(pill.left() + 28.0, y - galley.size().y / 2.0),
        galley,
        color,
    );
    if !show_btns {
        p.galley(pos2(age_x, y - age.size().y / 2.0), age, pal.weak);
    }
}

/// The hover buttons of a session row, drawn like the project header's: a raised rounded square
/// with the action's icon, lifting on hover; disabled actions are dimmed.
fn paint_row_acts(
    p: &egui::Painter,
    pal: &Palette,
    slots: &[Slot],
    rects: &[Rect],
    btn_hov: &[f32],
    hov: f32,
) {
    if hov <= 0.01 {
        return;
    }
    for ((act, r), over) in slots.iter().zip(rects).zip(btn_hov) {
        let over = if act.enabled { *over } else { 0.0 };
        if theme::flat() {
            p.rect_filled(*r, 6, pal.raised.gamma_multiply(hov * over));
        } else {
            p.rect_filled(
                *r,
                8,
                theme::mix(pal.raised, pal.accent, over * 0.5).gamma_multiply(hov),
            );
        }
        let tint = if act.enabled {
            theme::mix(pal.weak, pal.text_strong, over)
        } else {
            pal.weak.gamma_multiply(0.45)
        };
        paint_icon(
            p,
            r.center(),
            Align2::CENTER_CENTER,
            act.icon,
            if theme::flat() { 15.0 } else { 17.0 },
            tint.gamma_multiply(hov),
        );
    }
}

/// List row hover and selection. Hover lifts the row (raised fill + a bright outline, also on the
/// active row); selection: Neutral raised fill + 2 px accent edge, Neon accent-tinted card with a
/// gradient edge.
fn paint_row_bg(p: &egui::Painter, pal: &Palette, pill: Rect, (hov, sel): (f32, f32)) {
    let neutral = theme::flat();
    let lk = theme::look();
    let radius = if neutral { theme::ui_rad(6) } else { 10 };
    p.rect_filled(
        pill,
        radius,
        theme::hover_fill(pal).gamma_multiply(0.75 * hov * (1.0 - sel)),
    );
    if sel > 0.01 {
        if neutral {
            if lk.sel == theme::Sel::Tint {
                p.rect(
                    pill,
                    radius,
                    theme::skin::alpha(pal.accent, (28.0 * sel) as u8),
                    Stroke::new(1.0, theme::skin::alpha(pal.accent, (110.0 * sel) as u8)),
                    StrokeKind::Inside,
                );
            } else {
                p.rect_filled(pill, radius, pal.raised.gamma_multiply(sel));
                let bar = Rect::from_center_size(
                    pos2(pill.left() + 1.0, pill.center().y),
                    vec2(2.0, 14.0 * sel),
                );
                if lk.sel == theme::Sel::GradBar {
                    theme::skin::gradient_card(
                        p,
                        egui::Id::unique("row-bar"),
                        bar,
                        1.0,
                        (pal.accent_a, pal.accent_b),
                        theme::skin::Dir::Down,
                    );
                } else {
                    p.rect_filled(bar, 1, pal.accent);
                }
            }
        } else {
            p.rect(
                pill,
                radius,
                theme::mix(pal.surface, pal.accent, 0.20).gamma_multiply(sel),
                Stroke::new(1.0, pal.accent.gamma_multiply(0.5 * sel)),
                StrokeKind::Inside,
            );
            let bar = Rect::from_center_size(
                pos2(pill.left() + 3.0, pill.center().y),
                vec2(3.0, 20.0 * sel),
            );
            let mut m = egui::epaint::Mesh::default();
            m.colored_vertex(bar.left_top(), pal.accent_a);
            m.colored_vertex(bar.right_top(), pal.accent_a);
            m.colored_vertex(bar.left_bottom(), pal.accent_b);
            m.colored_vertex(bar.right_bottom(), pal.accent_b);
            m.add_triangle(0, 1, 2);
            m.add_triangle(1, 3, 2);
            p.add(Shape::mesh(m));
        }
    }
    if hov > 0.01 {
        let w = if lk.card.hover == theme::Hover::Hard {
            2.0
        } else {
            1.0
        };
        p.rect_stroke(
            pill,
            radius,
            Stroke::new(w, theme::hover_stroke(pal).gamma_multiply(hov)),
            StrokeKind::Inside,
        );
    }
}

/// What the session and tab context menus need besides the row itself.
pub struct MenuEnv<'a> {
    pub groups: &'a [ProjectGroup],
    /// omp was found: copy/move/delete are offered.
    pub omp_available: bool,
    /// Sessions can be stopped and restarted (a session backend available).
    pub can_stop: bool,
    pub default_view: SessionView,
    pub host_available: bool,
    pub tabs: &'a [Tab],
    /// Session files with a move/copy/delete in flight.
    pub busy: &'a HashSet<PathBuf>,
    pub backup: BackupMenu<'a>,
    /// Session files open in a live tab (a move to a worktree types into that tab).
    pub live_tabs: &'a [&'a Path],
    /// Worktrees (by folder) that were sent the finish prompt and are being waited for.
    pub finishing: &'a HashMap<PathBuf, bool>,
    pub parked: ParkView<'a>,
}

impl MenuEnv<'_> {
    fn entry(&self, path: &Path) -> Option<&SessionEntry> {
        self.groups
            .iter()
            .flat_map(|g| &g.sessions)
            .find(|e| e.path == path)
    }

    /// The linked worktree a session whose header folder is `cwd` runs in.
    fn worktree_of(&self, cwd: &Path) -> Option<&Worktree> {
        self.groups
            .iter()
            .find(|g| g.cwd == cwd)
            .and_then(|g| g.worktree.as_ref())
    }
}

impl SidebarInput<'_> {
    fn menu_env(&self) -> MenuEnv<'_> {
        MenuEnv {
            groups: self.groups.unwrap_or(&[]),
            omp_available: self.omp_available,
            can_stop: self.can_stop,
            default_view: self.default_view,
            host_available: self.host_available,
            tabs: self.tabs,
            busy: self.busy,
            backup: self.backup,
            live_tabs: self.open_paths,
            finishing: self.finishing,
            parked: self.parked,
        }
    }
}

/// A tab's context menu (tab strip and "Open tabs" rows): the tab items, then its session's full
/// menu once the session file is listed.
pub fn tab_menu(ui: &mut Ui, i: usize, tab: &Tab, env: &MenuEnv<'_>, out: &mut SidebarOutput) {
    if menu_item(ui, ICON_X, "Close tab", None, true, false).clicked() {
        out.actions.push(SidebarAction::CloseTab(i));
        ui.close();
    }
    if tab.pane_uid().is_some() {
        split_items(ui, out, |side| SidebarAction::SplitTab { index: i, side });
    }
    let running = !tab.terminal().is_exited();
    let entry = tab.session_path().and_then(|p| env.entry(p));
    // The session menu's "Stop session process" covers this whenever it is offered.
    if (entry.is_none() || !env.can_stop)
        && menu_item(ui, ICON_POWER, "Kill session process", None, running, false).clicked()
    {
        out.actions.push(SidebarAction::KillTab(i));
        ui.close();
    }
    if entry.is_none() && tab.role == tabs::TabRole::Session {
        let (view, label) = view_action(env.default_view, Some(tab.view()));
        let why = view_switch_reason(tab, view, env.host_available);
        if menu_item(ui, view_icon(view), label, None, why.is_none(), false)
            .on_disabled_hover_text(why.unwrap_or(""))
            .clicked()
        {
            out.actions
                .push(SidebarAction::SwitchView { uid: tab.uid, view });
            ui.close();
        }
    }
    if let Some(entry) = entry {
        ui.separator();
        session_menu(ui, entry, running, false, env, out);
    }
}
pub fn view_action(
    default: SessionView,
    current: Option<SessionView>,
) -> (SessionView, &'static str) {
    match current {
        Some(SessionView::Terminal) => (SessionView::Chat, "Switch to chat view"),
        Some(SessionView::Chat) => (SessionView::Terminal, "Switch to terminal view"),
        None if default == SessionView::Terminal => (SessionView::Chat, "Open in chat view"),
        None => (SessionView::Terminal, "Open in terminal"),
    }
}

fn view_icon(view: SessionView) -> Icon {
    match view {
        SessionView::Chat => ICON_MESSAGES_SQUARE,
        SessionView::Terminal => ICON_TERMINAL,
    }
}

/// Detached live sessions attach in their actual mode; only tabs can restart to switch modes.
fn row_view_action(
    default: SessionView,
    current: Option<SessionView>,
    running: bool,
) -> Option<(SessionView, &'static str)> {
    if running && current.is_none() {
        None
    } else {
        Some(view_action(default, current))
    }
}

pub fn view_switch_reason(tab: &Tab, view: SessionView, host: bool) -> Option<&'static str> {
    switch_reason(
        tab.role == tabs::TabRole::Session && !tab.terminal().is_exited(),
        tab.session_path().is_some() && tab.mux_name.is_some(),
        tab.activity() == deck_service::statusfmt::Activity::Idle,
        view,
        host,
    )
}

fn switch_reason(
    session: bool,
    restartable: bool,
    idle: bool,
    view: SessionView,
    host: bool,
) -> Option<&'static str> {
    if !session {
        Some("Select a running session first")
    } else if !restartable {
        Some("Wait until the session has been saved on a session backend")
    } else if view == SessionView::Chat && !host {
        Some("Chat view requires the OmpDeck session host")
    } else if !idle {
        Some("omp is working; wait until the session is idle before switching views")
    } else {
        None
    }
}

/// "Open in split right/down" for the menu's target.
fn split_items(ui: &mut Ui, out: &mut SidebarOutput, action: impl Fn(Side) -> SidebarAction) {
    for (side, icon, label) in [
        (Side::Right, ICON_COLUMNS_2, "Open in split right"),
        (Side::Down, ICON_ROWS_2, "Open in split down"),
    ] {
        if menu_item(ui, icon, label, None, true, false).clicked() {
            out.actions.push(action(side));
            out.interacted = true;
            ui.close();
        }
    }
}

fn session_menu(
    ui: &mut Ui,
    entry: &SessionEntry,
    running: bool,
    split: bool,
    env: &MenuEnv<'_>,
    out: &mut SidebarOutput,
) {
    fn copy(ui: &mut Ui, out: &mut SidebarOutput, text: String, what: &'static str) {
        out.actions.push(SidebarAction::CopyText { text, what });
        ui.close();
    }
    let tab = env
        .tabs
        .iter()
        .find(|t| t.session_path() == Some(entry.path.as_path()));
    if let Some((view, label)) = row_view_action(env.default_view, tab.map(Tab::view), running) {
        let why = tab.and_then(|t| view_switch_reason(t, view, env.host_available));
        if menu_item(ui, view_icon(view), label, None, why.is_none(), false)
            .on_disabled_hover_text(why.unwrap_or(""))
            .clicked()
        {
            out.actions.push(match tab {
                Some(t) => SidebarAction::SwitchView { uid: t.uid, view },
                None => SidebarAction::OpenView(entry.clone(), view),
            });
            out.interacted = true;
            ui.close();
        }
    }
    if menu_item(ui, ICON_PLAY, "Open", None, true, false).clicked() {
        out.actions.push(SidebarAction::Open(entry.clone()));
        out.interacted = true;
        ui.close();
    }
    if split {
        split_items(ui, out, |side| SidebarAction::SplitOpen {
            entry: entry.clone(),
            side,
        });
    }
    if menu_item(ui, ICON_LIST_TREE, "Session tree…", None, true, false).clicked() {
        out.actions.push(SidebarAction::Tree(entry.clone()));
        ui.close();
    }
    ui.separator();
    if menu_item(ui, ICON_FILE_TEXT, "Copy session path", None, true, false).clicked() {
        copy(
            ui,
            out,
            entry.path.to_string_lossy().into_owned(),
            "session path",
        );
    }
    if menu_item(ui, ICON_KEY_ROUND, "Copy session id", None, true, false).clicked() {
        copy(ui, out, entry.id.clone(), "session id");
    }
    if menu_item(ui, ICON_TERMINAL, "Copy resume command", None, true, false).clicked() {
        copy(ui, out, resume_command(&entry.path), "resume command");
    }
    ui.separator();
    if menu_item(
        ui,
        ICON_FOLDER_OPEN,
        "Open project folder",
        None,
        true,
        false,
    )
    .clicked()
    {
        out.actions
            .push(SidebarAction::OpenFolder(entry.cwd.clone()));
        ui.close();
    }
    if menu_item(
        ui,
        ICON_EXTERNAL_LINK,
        "Show session file",
        None,
        true,
        false,
    )
    .clicked()
    {
        let dir = entry
            .path
            .parent()
            .map_or_else(|| entry.path.clone(), Path::to_path_buf);
        out.actions.push(SidebarAction::OpenFolder(dir));
        ui.close();
    }
    {
        let pal = Palette::of_ui(ui);
        ui.menu_button(
            (
                icon_text(ICON_CLOUD, 17.0, pal.glyph),
                RichText::new("Backup").color(pal.text),
            ),
            |ui| backup_items(ui, env.backup, &entry.cwd, false, out),
        );
    }
    let worktree = env.worktree_of(&entry.cwd);
    if let Some(w) = worktree {
        ui.separator();
        worktree_items(ui, env, w, out);
    }
    if worktree.is_none()
        && let Some(r) = env.parked.record_of(&entry.id)
    {
        ui.separator();
        for a in parked_acts(env, entry, r, env.parked.checked_out(r)) {
            act_item(ui, &a, out);
        }
    }
    if env.omp_available {
        ui.separator();
        let live = env.live_tabs.contains(&entry.path.as_path());
        let linked = worktree.is_some();
        let mut item = menu_item(
            ui,
            ICON_GIT_BRANCH_PLUS,
            "Move to new worktree…",
            None,
            live && !linked,
            false,
        );
        item = if linked {
            item.on_disabled_hover_text("This session already runs in a worktree")
        } else if !live {
            item.on_disabled_hover_text("Open the session in a tab first")
        } else {
            item.on_hover_text(
                "Carry this session and its uncommitted changes to a new git worktree",
            )
        };
        if item.clicked() {
            out.actions
                .push(SidebarAction::MoveToWorktree(entry.clone()));
            out.interacted = true;
            ui.close();
        }
        let idle = !env.busy.contains(&entry.path);
        for (kind, icon, label) in [
            (TransferKind::Copy, ICON_FILES, "Copy to project"),
            (TransferKind::Move, ICON_FOLDER_OUTPUT, "Move to project"),
        ] {
            transfer_menu(ui, entry, (kind, icon, label), idle, env, out);
        }
        ui.separator();
        if menu_item(ui, ICON_TRASH_2, "Delete session…", None, idle, true).clicked() {
            out.actions.push(SidebarAction::Delete(entry.clone()));
            ui.close();
        }
    }
    if running && env.can_stop {
        ui.separator();
        if menu_item(ui, ICON_ROTATE_CCW, "Restart session", None, true, false).clicked() {
            out.actions.push(SidebarAction::Restart(entry.clone()));
            ui.close();
        }
        if menu_item(
            ui,
            ICON_CIRCLE_STOP,
            "Stop session process",
            None,
            true,
            true,
        )
        .clicked()
        {
            out.actions.push(SidebarAction::Stop(entry.clone()));
            ui.close();
        }
    }
}

/// "Back up to {place}…" and "Restore from {place}…" for `cwd`.
fn backup_items(
    ui: &mut Ui,
    menu: BackupMenu<'_>,
    cwd: &Path,
    separator: bool,
    out: &mut SidebarOutput,
) {
    if separator {
        ui.separator();
    }
    for (icon, label, restore) in [
        (
            ICON_CLOUD_UPLOAD,
            format!("Back up to {}…", menu.place),
            false,
        ),
        (
            ICON_ARCHIVE_RESTORE,
            format!("Restore from {}…", menu.place),
            true,
        ),
    ] {
        let mut resp = menu_item(ui, icon, &label, None, menu.off.is_none(), false);
        if let Some(why) = menu.off {
            resp = resp.on_disabled_hover_text(why);
        }
        if resp.clicked() {
            out.actions.push(if restore {
                SidebarAction::RestoreProject(cwd.to_path_buf())
            } else {
                SidebarAction::BackupProject(cwd.to_path_buf())
            });
            out.interacted = true;
            ui.close();
        }
    }
}

/// "Copy to project" / "Move to project" submenu: every other project, then "Choose folder…".
fn transfer_menu(
    ui: &mut Ui,
    entry: &SessionEntry,
    spec: (TransferKind, Icon, &str),
    enabled: bool,
    env: &MenuEnv<'_>,
    out: &mut SidebarOutput,
) {
    let (kind, icon, label) = spec;
    let pal = Palette::of_ui(ui);
    let mut send = |ui: &mut Ui, dest: Option<PathBuf>| {
        out.actions.push(SidebarAction::Transfer {
            kind,
            entry: entry.clone(),
            dest,
        });
        out.interacted = true;
        ui.close();
    };
    ui.add_enabled_ui(enabled, |ui| {
        ui.menu_button(
            (
                icon_text(icon, 17.0, pal.glyph),
                RichText::new(label).color(pal.text),
            ),
            |ui| {
                ui.set_min_width(260.0);
                let targets = transfer::targets(env.groups, &entry.cwd);
                if targets.is_empty() {
                    ui.add_enabled(
                        false,
                        egui::Label::new(RichText::new("No other projects").color(pal.weak)),
                    );
                }
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .auto_shrink([true, true])
                    .show(ui, |ui| {
                        for g in targets {
                            if menu_item(ui, ICON_FOLDER, &g.display, None, true, false)
                                .on_hover_text(g.cwd.display().to_string())
                                .clicked()
                            {
                                send(ui, Some(g.cwd.clone()));
                            }
                        }
                    });
                ui.separator();
                if menu_item(ui, ICON_FOLDER_OPEN, "Choose folder…", None, true, false).clicked()
                {
                    send(ui, None);
                }
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_button_slots_mirror_the_menu_actions() {
        let busy = HashSet::new();
        let finishing = HashMap::new();
        let parkable = HashSet::new();
        let (records, states) = ([], []);
        let env = MenuEnv {
            groups: &[],
            omp_available: true,
            can_stop: true,
            default_view: SessionView::Terminal,
            host_available: true,
            tabs: &[],
            busy: &busy,
            backup: BackupMenu {
                place: "Google Drive",
                off: None,
            },
            live_tabs: &[],
            finishing: &finishing,
            parked: ParkView {
                records: &records,
                states: &states,
                parkable: &parkable,
            },
        };
        let rec = ParkedBranch {
            primary: PathBuf::from("/p/repo"),
            branch: "feat".into(),
            base: "main".into(),
            sessions: vec!["s".into()],
            parked_ms: 0,
        };
        let entry = SessionEntry {
            id: "s".into(),
            path: PathBuf::from("/s/s.jsonl"),
            cwd: PathBuf::from("/p/repo"),
            title: None,
            first_prompt: None,
            modified: SystemTime::UNIX_EPOCH,
        };
        for at in [
            None,
            Some(Path::new("/p/repo")),
            Some(Path::new("/wt/elsewhere")),
        ] {
            let chip = RowChip {
                wt: RowWorktree::Parked(&rec, at),
                finishing: false,
                own_finish: false,
            };
            let acts = parked_acts(&env, &entry, &rec, at);
            let sl = slots(&chip, true);
            assert_eq!(acts.len(), sl.iter().flatten().count());
            for (a, s) in acts.iter().zip(sl.iter().flatten()) {
                assert_eq!((a.enabled, a.icon.codepoint), (s.enabled, s.icon.codepoint));
            }
        }
    }

    #[test]
    fn cancel_finishing_is_only_enabled_for_own_jobs() {
        let wt = Worktree {
            path: PathBuf::from("/wt/a"),
            primary: PathBuf::from("/p/repo"),
            branch: Some("a".into()),
        };
        let busy = HashSet::new();
        let parkable = HashSet::new();
        let (records, states) = ([], []);
        for own in [true, false] {
            let finishing = HashMap::from([(wt.path.clone(), own)]);
            let env = MenuEnv {
                groups: &[],
                omp_available: true,
                can_stop: true,
                default_view: SessionView::Terminal,
                host_available: true,
                tabs: &[],
                busy: &busy,
                backup: BackupMenu {
                    place: "Google Drive",
                    off: None,
                },
                live_tabs: &[],
                finishing: &finishing,
                parked: ParkView {
                    records: &records,
                    states: &states,
                    parkable: &parkable,
                },
            };
            let acts = live_acts(&env, &wt);
            assert_eq!(acts[0].label, "Cancel finishing");
            assert_eq!(acts[0].enabled, own);
            assert_eq!(acts[0].why.is_some(), !own);
            let chip = RowChip {
                wt: RowWorktree::Live(&wt),
                finishing: true,
                own_finish: own,
            };
            assert_eq!(slots(&chip, true)[0].map(|s| s.enabled), Some(own));
        }
    }

    fn at(secs: u64) -> (SystemTime, SystemTime) {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000_000);
        (now, now - Duration::from_secs(secs))
    }

    fn age(secs: u64) -> String {
        let (n, m) = at(secs);
        format_age(n, m)
    }

    #[test]
    fn age_boundaries() {
        assert_eq!(age(0), "now");
        assert_eq!(age(59), "now");
        assert_eq!(age(60), "1m");
        assert_eq!(age(3599), "59m");
        assert_eq!(age(3600), "1h");
        assert_eq!(age(86_399), "23h");
        assert_eq!(age(86_400), "1d");
        assert_eq!(age(7 * 86_400 - 1), "6d");
        assert_eq!(age(7 * 86_400), "1w");
        assert_eq!(age(28 * 86_400), "4w");
        assert_eq!(age(365 * 86_400), "1y");
    }

    #[test]
    fn age_future_mtime_is_now() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        assert_eq!(format_age(now, now + Duration::from_secs(50)), "now");
    }

    fn entry(id: &str, title: Option<&str>, prompt: Option<&str>) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            path: PathBuf::from(format!("/s/{id}.jsonl")),
            cwd: PathBuf::from("/home/u/proj"),
            title: title.map(Into::into),
            first_prompt: prompt.map(Into::into),
            modified: SystemTime::UNIX_EPOCH,
        }
    }

    fn group(e: &SessionEntry) -> ProjectGroup {
        ProjectGroup {
            cwd: e.cwd.clone(),
            display: "~/proj".into(),
            sessions: vec![e.clone()],
            worktree: None,
        }
    }

    #[test]
    fn filter_cache_follows_query_and_data() {
        let a = entry("aaa", Some("alpha"), None);
        let b = entry("bbb", Some("beta"), None);
        let groups = vec![ProjectGroup {
            cwd: a.cwd.clone(),
            display: "~/proj".into(),
            sessions: vec![a, b],
            worktree: None,
        }];
        let mut cache = FilterCache::default();
        let stamp = (1, groups.as_ptr() as usize);

        assert!(
            cache.sync(stamp, "", &groups).is_empty(),
            "no query: nothing cached"
        );
        assert_eq!(cache.sync(stamp, "beta", &groups), [vec![1]]);
        assert_eq!(cache.sync(stamp, "alpha", &groups), [vec![0]]);
        // New data under the same query is picked up.
        assert_eq!(cache.sync((2, stamp.1), "alpha", &groups), [vec![0]]);
        assert_eq!(cache.key.as_ref().map(|k| k.0), Some((2, stamp.1)));
    }

    #[test]
    fn age_cache_refreshes_per_time_bucket() {
        let e = entry("aaa", None, None);
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut cache = AgeCache::default();
        assert_eq!(
            cache.get(((1, 0), 1), t0, &e),
            "11d".replace("11d", &format_age(t0, e.modified))
        );
        // Same bucket: the cached text is served even if `now` moved on.
        let later = t0 + Duration::from_secs(10 * 86_400);
        assert_eq!(
            cache.get(((1, 0), 1), later, &e),
            format_age(t0, e.modified)
        );
        assert_eq!(
            cache.get(((1, 0), 2), later, &e),
            format_age(later, e.modified)
        );
    }

    #[test]
    fn filter_matching() {
        let e = entry("AbC123", Some("Fix Parser"), Some("please help"));
        let g = group(&e);
        assert!(matches_filter("", &g, &e));
        assert!(matches_filter("parser", &g, &e)); // case-insensitive title
        assert!(matches_filter("~/pro", &g, &e)); // cwd display
        assert!(matches_filter("abc1", &g, &e)); // id prefix
        assert!(!matches_filter("c123", &g, &e)); // id infix is not a match
        assert!(!matches_filter("nomatch", &g, &e));
    }

    #[test]
    fn filter_uses_label_fallback() {
        let e = entry("x", None, None);
        assert!(matches_filter("(untitled)", &group(&e), &e));
    }

    #[test]
    fn resume_command_quotes_the_path() {
        assert_eq!(
            resume_command(Path::new("/h/my dir/s.jsonl")),
            "omp --resume '/h/my dir/s.jsonl'"
        );
    }

    #[test]
    fn display_split() {
        assert_eq!(split_display("~/dev/proj"), ("~/dev", "proj"));
        assert_eq!(split_display("~"), ("", "~"));
        assert_eq!(split_display("/"), ("", "/"));
        assert_eq!(split_display("/x"), ("", "x"));
    }

    fn proj(cwd: &str, ids: &[&str]) -> ProjectGroup {
        ProjectGroup {
            cwd: PathBuf::from(cwd),
            display: cwd.into(),
            sessions: ids
                .iter()
                .map(|id| SessionEntry {
                    id: (*id).into(),
                    path: PathBuf::from(format!("{cwd}/{id}.jsonl")),
                    cwd: PathBuf::from(cwd),
                    title: None,
                    first_prompt: None,
                    modified: SystemTime::UNIX_EPOCH,
                })
                .collect(),
            worktree: None,
        }
    }

    fn open_set(ex: &Expansion, groups: &[ProjectGroup]) -> Vec<bool> {
        groups.iter().map(|g| ex.is_open(&g.cwd, false)).collect()
    }

    fn group_at<'a>(groups: &'a [ProjectGroup], path: &str) -> Option<&'a Path> {
        active_group_of(groups, Some(Path::new(path)), None).map(|i| groups[i].cwd.as_path())
    }

    #[test]
    fn only_the_active_tabs_group_is_open_at_startup() {
        let groups = [
            proj("/a", &["a1"]),
            proj("/b", &["b1"]),
            proj("/c", &["c1"]),
        ];
        let active = group_at(&groups, "/b/b1.jsonl");
        assert_eq!(active, Some(Path::new("/b")));
        let mut ex = Expansion::default();
        ex.on_activation(active, None);
        assert_eq!(open_set(&ex, &groups), [false, true, false]);
    }

    #[test]
    fn new_session_tab_opens_its_cwd_group() {
        let groups = [proj("/a", &["a1"]), proj("/b", &["b1"])];
        assert_eq!(
            active_group_of(&groups, None, Some(Path::new("/a"))),
            Some(0)
        );
        assert_eq!(
            active_group_of(&groups, None, Some(Path::new("/zzz"))),
            None
        );
    }

    #[test]
    fn manual_open_survives_activating_it_then_another_group() {
        let groups = [
            proj("/a", &["a1"]),
            proj("/b", &["b1"]),
            proj("/c", &["c1"]),
        ];
        let mut ex = Expansion::default();
        ex.on_activation(group_at(&groups, "/a/a1.jsonl"), None);
        ex.set(Path::new("/c"), true); // user opens /c by hand
        assert_eq!(open_set(&ex, &groups), [true, false, true]);
        ex.on_activation(group_at(&groups, "/c/c1.jsonl"), None);
        ex.on_activation(group_at(&groups, "/b/b1.jsonl"), None);
        assert_eq!(open_set(&ex, &groups), [false, true, true]);
    }

    #[test]
    fn changing_group_reopens_a_collapsed_group_but_reclicking_does_not() {
        let groups = [proj("/a", &["a1"]), proj("/b", &["b1"])];
        let mut ex = Expansion::default();
        let a = group_at(&groups, "/a/a1.jsonl");
        ex.on_activation(a, None);
        ex.set(Path::new("/a"), false);
        ex.on_activation(a, None); // re-click the active tab: stays collapsed
        assert_eq!(open_set(&ex, &groups), [false, false]);
        ex.on_activation(group_at(&groups, "/b/b1.jsonl"), None);
        ex.set(Path::new("/a"), false);
        ex.on_activation(a, None); // group changed back: reopens
        assert_eq!(open_set(&ex, &groups), [true, false]);
    }

    #[test]
    fn a_tab_without_group_keeps_the_previous_group_open() {
        let groups = [proj("/a", &["a1"]), proj("/b", &["b1"])];
        let mut ex = Expansion::default();
        ex.on_activation(group_at(&groups, "/b/b1.jsonl"), None);
        ex.on_activation(None, None); // a tab without a group
        assert_eq!(open_set(&ex, &groups), [false, true]);
    }

    #[test]
    fn activating_a_worktree_opens_it_and_its_repository() {
        let mut ex = Expansion::default();
        ex.set(Path::new("/repo"), false);
        ex.on_activation(Some(Path::new("/wt/a")), Some(Path::new("/repo")));
        assert!(ex.is_open(Path::new("/wt/a"), false));
        assert!(ex.is_open(Path::new("/repo"), false));
        assert!(!ex.is_open(Path::new("/other"), false));
    }

    #[test]
    fn active_group_lookup_is_cached_per_key() {
        let groups = [proj("/a", &["a1"]), proj("/b", &["b1"])];
        let mut cache = ActiveGroupCache::default();
        let stamp = (1, groups.as_ptr() as usize);
        let b = Some(Path::new("/b/b1.jsonl"));
        assert_eq!(
            cache.get(stamp, &groups, b, None).map(|g| g.cwd.as_path()),
            Some(Path::new("/b"))
        );
        assert_eq!(cache.group, Some(1));
        assert_eq!(
            cache
                .get(stamp, &groups, Some(Path::new("/a/a1.jsonl")), None)
                .map(|g| g.cwd.as_path()),
            Some(Path::new("/a"))
        );
    }

    #[test]
    fn filtering_shows_everything_and_collapse_expand_all_set_overrides() {
        let groups = [proj("/a", &["a1"]), proj("/b", &["b1"])];
        let mut ex = Expansion::default();
        ex.on_activation(Some(Path::new("/a")), None);
        assert!(groups.iter().all(|g| ex.is_open(&g.cwd, true)));
        ex.set_all(groups.iter().map(|g| g.cwd.as_path()), false);
        assert_eq!(open_set(&ex, &groups), [false, false]);
        ex.set_all(groups.iter().map(|g| g.cwd.as_path()), true);
        assert_eq!(open_set(&ex, &groups), [true, true]);
    }
}

#[cfg(test)]
mod view_tests {
    use super::{SessionView, switch_reason, view_action};

    #[test]
    fn non_default_open_and_current_tab_switch_are_distinct() {
        assert_eq!(
            view_action(SessionView::Terminal, None),
            (SessionView::Chat, "Open in chat view")
        );
        assert_eq!(
            view_action(SessionView::Chat, None),
            (SessionView::Terminal, "Open in terminal")
        );
        for default in [SessionView::Terminal, SessionView::Chat] {
            assert_eq!(
                view_action(default, Some(SessionView::Terminal)),
                (SessionView::Chat, "Switch to chat view")
            );
            assert_eq!(
                view_action(default, Some(SessionView::Chat)),
                (SessionView::Terminal, "Switch to terminal view")
            );
        }
    }

    #[test]
    fn menu_and_palette_share_idle_restart_gating() {
        for view in [SessionView::Terminal, SessionView::Chat] {
            assert!(switch_reason(false, true, true, view, true).is_some());
            assert!(switch_reason(true, false, true, view, true).is_some());
            assert!(switch_reason(true, true, false, view, true).is_some());
            assert_eq!(switch_reason(true, true, true, view, true), None);
        }
        assert!(switch_reason(true, true, true, SessionView::Chat, false).is_some());
        assert_eq!(
            switch_reason(true, true, true, SessionView::Terminal, false),
            None
        );
    }
}

#[cfg(test)]
mod detached_view_tests {
    use super::{SessionView, row_view_action, view_action};

    #[test]
    fn detached_running_sessions_never_promise_a_different_launch_mode() {
        for default in [SessionView::Terminal, SessionView::Chat] {
            assert_eq!(row_view_action(default, None, true), None);
            assert_eq!(
                row_view_action(default, None, false),
                Some(view_action(default, None))
            );
            for current in [SessionView::Terminal, SessionView::Chat] {
                assert_eq!(
                    row_view_action(default, Some(current), true),
                    Some(view_action(default, Some(current)))
                );
            }
        }
    }
}
