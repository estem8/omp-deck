//! The session tree window: a git-graph style picker over [`crate::tree_layout`]. Rows are
//! virtualized; the layout is rebuilt only when the tree revision, the leaf or the filter
//! changes.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use crate::icons::Icon;
use crate::icons::{
    ICON_ALIGN_LEFT, ICON_ARROW_RIGHT, ICON_BOOKMARK, ICON_COINS, ICON_COPY,
    ICON_CORNER_DOWN_RIGHT, ICON_CROSSHAIR, ICON_FOLD_VERTICAL, ICON_GIT_FORK, ICON_KEY_ROUND,
    ICON_LIST_TREE, ICON_MESSAGES_SQUARE, ICON_SCROLL_TEXT, ICON_SPARKLES, ICON_USER, ICON_WRENCH,
    ICON_X,
};
use deck_core::config::UiFont;
use deck_core::ctl::BranchSummary;
use deck_core::tree::{NodeKind, RecordedCost, SessionTree};
use egui::Galley;
use egui::epaint::CubicBezierShape;
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Color32, CursorIcon, FontId, Frame, Id, Key, Margin, Modifiers, Order, Painter,
    Pos2, Rect, RichText, Sense, Sides, Stroke, StrokeKind, TextEdit, Ui, pos2, vec2,
};

use crate::badge::{Badge, Hue};
use crate::card;
use crate::confirm::{self, Dialog, Reply};
use crate::theme::{self, Palette};
use crate::tree_layout::{self, Filter, FuzzySearch, Layout, Row, RowClass, SearchIndex};
use crate::widgets::{DraftButton, DraftKind, empty_state, menu_item, paint_icon};

/// Rows per page before the list has been measured.
const DEFAULT_PAGE: usize = 12;
/// Uniform row height (virtualization relies on it).
pub const ROW_H: f32 = 38.0;
const LANE_W: f32 = 18.0;
const FULL_LANES: usize = 7;
const PAD_L: f32 = 18.0;
/// Right-hand column: the age, or the hover actions in its place (label, fork, go here).
const ACTIONS_W: f32 = 212.0;
/// The same on an `ask` result: one more button (re-answer).
const ACTIONS_W_ASK: f32 = 248.0;

fn actions_w(ask: bool) -> f32 {
    if ask { ACTIONS_W_ASK } else { ACTIONS_W }
}

pub enum TreeAction {
    Close,
    /// Fork the session at `entry` (already resolved: a user node forks at its parent).
    Fork {
        entry: String,
        what: String,
    },
    Navigate {
        entry: String,
        /// The target is a user prompt: omp moves to its parent and returns the text to the editor.
        user: bool,
        /// What omp does with the branch being left.
        summary: BranchSummary,
    },
    /// Set (`Some`) or remove (`None`) the label of `entry`.
    SetLabel {
        entry: String,
        label: Option<String>,
    },
    /// Re-answer the `ask` result `entry`: omp asks the question again in the session's tab.
    Reanswer {
        entry: String,
    },
    Copy {
        text: String,
        what: &'static str,
    },
    /// A keyboard shortcut that cannot run right now.
    Notice(String),
}

pub struct TreeInput<'a> {
    pub tree: Option<&'a Arc<SessionTree>>,
    pub revision: u64,
    pub error: Option<&'a str>,
    /// The live leaf entry id, when the session has a control link.
    pub live_leaf: Option<&'a str>,
    /// `Ok` = Go here is available; `Err` = why it is not.
    pub navigate: Result<(), &'a str>,
    /// `Ok` = Fork is available; `Err` = why it is not.
    pub fork: Result<(), &'a str>,
    /// omp's `branchSummary.enabled`: Go here asks whether to summarize the branch it leaves.
    pub summary_prompt: bool,
    /// `Ok` = labels can be edited; `Err` = why not.
    pub label: Result<(), &'a str>,
    /// `Ok` = an `ask` result can be re-answered in the session's tab; `Err` = why not.
    pub reanswer: Result<(), &'a str>,
    /// What the session is busy with (shown as progress), if anything.
    pub busy: Option<&'a str>,
    pub now: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    None,
    /// Center the leaf row.
    Leaf,
    /// Bring the selected row into view.
    Selected,
}

struct Cached {
    revision: u64,
    leaf: Option<usize>,
    filter: Filter,
    layout: Layout,
}

struct Hits {
    query: String,
    revision: u64,
    filter: Filter,
    leaf: Option<usize>,
    tokens: Vec<String>,
    rows: Vec<bool>,
}

pub struct TreeState {
    pub title: String,
    filter: Filter,
    search: String,
    cache: Option<Cached>,
    hits: Option<Hits>,
    /// Selected node id.
    selected: Option<String>,
    want: Want,
    /// The view follows the leaf while the session grows.
    pinned: bool,
    last_offset: f32,
    settle: u8,
    /// Rows the list shows at once (PgUp / PgDn / ← / → step); measured by the last frame.
    page: usize,
    /// Prepared search texts of the tree revision in `index_revision`.
    index: SearchIndex,
    index_revision: Option<u64>,
    focus_search: bool,
    /// The label being edited.
    label: Option<LabelEdit>,
    /// The "Summarize branch?" question of a Go here.
    summary: Option<SummaryDialog>,
    /// The search box had focus at the end of the previous frame: egui drops focus on Esc
    /// before the app sees the key, so this is what tells "Esc in the box" from "Esc".
    search_focused: bool,
    galleys: GalleyCache,
    search_id: Id,
    scratch: Vec<RowUi>,
    long: Vec<u32>,
}

impl TreeState {
    pub fn new(title: String) -> TreeState {
        TreeState {
            title,
            filter: Filter::default(),
            search: String::new(),
            cache: None,
            hits: None,
            selected: None,
            want: Want::Leaf,
            pinned: true,
            last_offset: 0.0,
            settle: 0,
            page: DEFAULT_PAGE,
            index: SearchIndex::default(),
            index_revision: None,
            focus_search: false,
            label: None,
            summary: None,
            search_focused: false,
            search_id: Id::unique("tree-search"),
            galleys: GalleyCache::default(),
            scratch: Vec::new(),
            long: Vec::new(),
        }
    }
}

/// What a row's text layout depends on besides the node and the width.
struct GalleyKey<'a> {
    revision: u64,
    ppp: u32,
    theme: [Color32; 6],
    font: UiFont,
    tokens: &'a [String],
}

/// Laid-out row texts, so a frame does not rebuild a `LayoutJob` per visible row. Valid for one
/// (tree revision, pixel ratio, theme, search tokens); keyed per row by (node, on path, width).
#[derive(Default)]
struct GalleyCache {
    valid: bool,
    revision: u64,
    ppp: u32,
    font: UiFont,
    theme: [Color32; 6],
    tokens: Vec<String>,
    map: HashMap<(usize, bool, u32), Arc<Galley>>,
}

/// More laid-out rows than a long scroll through a big tree needs.
const GALLEY_CAP: usize = 4096;

impl GalleyCache {
    fn sync(&mut self, k: GalleyKey<'_>) {
        if self.valid
            && self.revision == k.revision
            && self.ppp == k.ppp
            && self.font == k.font
            && self.theme == k.theme
            && self.tokens == k.tokens
        {
            return;
        }
        self.valid = true;
        self.revision = k.revision;
        self.ppp = k.ppp;
        self.font = k.font;
        self.theme = k.theme;
        self.tokens = k.tokens.to_vec();
        self.map.clear();
    }

    fn row(&mut self, key: (usize, bool, u32), build: impl FnOnce() -> Arc<Galley>) -> Arc<Galley> {
        if let Some(g) = self.map.get(&key) {
            return Arc::clone(g);
        }
        if self.map.len() >= GALLEY_CAP {
            self.map.clear();
        }
        let g = build();
        self.map.insert(key, Arc::clone(&g));
        g
    }
}

/// What the pointer did on one visible row this frame.
struct RowUi {
    row: usize,
    rect: Rect,
    hovered: bool,
}

/// Lane spacing and gutter width for `lanes` lanes: wide trees compress instead of growing.
pub fn gutter(lanes: u16) -> (f32, f32) {
    let n = usize::from(lanes.max(1));
    let step = if n <= FULL_LANES {
        LANE_W
    } else {
        (FULL_LANES as f32 * LANE_W / n as f32).max(6.0)
    };
    (step, n as f32 * step)
}

/// Selection after `delta` steps through `len` rows, clamped; no selection starts at the edge.
pub fn step_row(current: Option<usize>, delta: isize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = len - 1;
    Some(match current {
        Some(c) => c.saturating_add_signed(delta).min(last),
        None if delta < 0 => last,
        None => 0,
    })
}

// ------------------------------------------------------------------ colours

fn class_color(pal: &Palette, class: RowClass) -> Color32 {
    match class {
        RowClass::User if theme::flat() => pal.text_strong,
        RowClass::Assistant if theme::flat() => pal.text,
        RowClass::User => pal.accent_b,
        RowClass::Assistant => pal.accent,
        RowClass::Tool => pal.weak,
        RowClass::Compaction => pal.warn,
        RowClass::Summary => pal.ok,
        RowClass::Other => pal.weak,
    }
}

fn class_icon(class: RowClass) -> Icon {
    match class {
        RowClass::User => ICON_USER,
        RowClass::Assistant => ICON_SPARKLES,
        RowClass::Tool => ICON_WRENCH,
        RowClass::Compaction => ICON_FOLD_VERTICAL,
        RowClass::Summary => ICON_SCROLL_TEXT,
        RowClass::Other => ICON_ALIGN_LEFT,
    }
}

/// Muted tint of an inactive branch's line, by lane.
fn lane_color(pal: &Palette, lane: u16) -> Color32 {
    let hues = [pal.accent_b, pal.warn, pal.ok, pal.danger, pal.accent_a];
    theme::mix(pal.weak, hues[usize::from(lane) % hues.len()], 0.38).gamma_multiply(0.85)
}

// ------------------------------------------------------------------ show

pub fn show(ctx: &egui::Context, st: &mut TreeState, input: &TreeInput<'_>) -> Vec<TreeAction> {
    let mut out = Vec::new();
    let pal = Palette::of_ctx(ctx);
    let screen = ctx.content_rect();

    egui::Area::new(Id::unique("tree-scrim"))
        .order(Order::Background)
        .fixed_pos(screen.min)
        .interactable(true)
        .show(ctx, |ui| {
            let (rect, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
            let dark = ctx.global_style().visuals.dark_mode;
            ui.painter().rect_filled(rect, 0, theme::backdrop(dark));
            if resp.clicked() {
                out.push(TreeAction::Close);
            }
        });

    sync(st, input);
    keyboard(ctx, st, input, &mut out);

    let size = vec2(
        (screen.width() * 0.74)
            .clamp(620.0, 1500.0)
            .min(screen.width() - 24.0),
        (screen.height() * 0.82)
            .clamp(420.0, 1150.0)
            .min(screen.height() - 24.0),
    );

    egui::Window::new("tree-window")
        .id(Id::unique("tree-window"))
        .title_bar(false)
        .frame(Frame::NONE)
        .resizable(true)
        .movable(true)
        .collapsible(false)
        .constrain_to(screen)
        .min_size(vec2(560.0, 360.0))
        .default_size(size)
        .default_pos(screen.center() - size / 2.0)
        .show(ctx, |ui| {
            card::chrome(ui, Margin::ZERO, pal.bar, |ui| {
                body(ui, st, input, &mut out)
            });
        });
    st.search_focused = ctx.memory(|m| m.has_focus(st.search_id));
    summary_dialog(ctx, st, &mut out);
    out
}

/// Rebuild the cached layout and search hits when their inputs changed.
fn sync(st: &mut TreeState, input: &TreeInput<'_>) {
    let Some(tree) = input.tree else {
        st.cache = None;
        st.hits = None;
        return;
    };
    let leaf = input
        .live_leaf
        .and_then(|id| tree.index.get(id).copied())
        .or(tree.file_leaf);
    let stale = st
        .cache
        .as_ref()
        .is_none_or(|c| c.revision != input.revision || c.leaf != leaf || c.filter != st.filter);
    if stale {
        let grown = st
            .cache
            .as_ref()
            .is_some_and(|c| c.revision != input.revision);
        let layout = tree_layout::build(tree, leaf, st.filter);
        let first = st.cache.is_none();
        st.cache = Some(Cached {
            revision: input.revision,
            leaf,
            filter: st.filter,
            layout,
        });
        let Some(c) = &st.cache else {
            return;
        };
        // A filter that hides the selection leaves it on the nearest listed ancestor.
        let anchor = st
            .selected
            .as_ref()
            .and_then(|id| tree.index.get(id.as_str()).copied())
            .and_then(|i| c.layout.nearest_listed(tree, i));
        match anchor {
            Some(i) if !first => {
                let id = tree.nodes[i].id.as_str();
                if st.selected.as_deref() != Some(id) {
                    st.selected = Some(id.to_owned());
                }
            }
            _ => {
                // omp opens `/tree` on the leaf.
                st.selected = c
                    .layout
                    .leaf_row
                    .map(|r| tree.nodes[c.layout.rows[r].node].id.clone());
                if first {
                    st.want = Want::Leaf;
                }
            }
        }
        if grown && st.pinned && !first {
            st.want = Want::Leaf;
            if let Some(r) = c.layout.leaf_row {
                st.selected = Some(tree.nodes[c.layout.rows[r].node].id.clone());
            }
        } else if !first && st.want == Want::None {
            st.want = Want::Selected;
        }
    }
    let Some(c) = &st.cache else {
        return;
    };
    let current = st.hits.as_ref().is_some_and(|h| {
        h.query == st.search && h.revision == c.revision && h.filter == c.filter && h.leaf == c.leaf
    });
    if !current {
        if st.index_revision != Some(c.revision) {
            st.index.clear();
            st.index_revision = Some(c.revision);
        }
        let tokens = tree_layout::search_tokens(&st.search);
        let search = FuzzySearch::new(&st.search);
        let rows = if search.is_empty() {
            Vec::new()
        } else {
            tree_layout::search_matches(tree, &c.layout, &search, &mut st.index)
        };
        st.hits = Some(Hits {
            query: st.search.clone(),
            revision: c.revision,
            filter: c.filter,
            leaf: c.leaf,
            tokens,
            rows,
        });
        follow_matches(st, tree);
    }
}

fn selected_row(st: &TreeState, tree: &SessionTree) -> Option<usize> {
    let c = st.cache.as_ref()?;
    let i = *tree.index.get(st.selected.as_deref()?)?;
    c.layout.row_of_node(i)
}

fn keyboard(
    ctx: &egui::Context,
    st: &mut TreeState,
    input: &TreeInput<'_>,
    out: &mut Vec<TreeAction>,
) {
    let Some(tree) = input.tree else {
        if !egui::Popup::is_any_open(ctx)
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            out.push(TreeAction::Close);
        }
        return;
    };
    // The summary question owns the keys while it is open.
    if st.summary.is_some() {
        return;
    }
    // The label field: Enter saves, Esc cancels; everything else is typing.
    if st.label.is_some() {
        let (esc, enter) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::Enter),
            )
        });
        if esc {
            st.label = None;
        } else if enter {
            finish_label(st, input, out);
        }
        return;
    }
    let searching = st.search_focused || ctx.memory(|m| m.has_focus(st.search_id));
    let menu_open = egui::Popup::is_any_open(ctx);
    // Like omp, the label key is a command only while no search is typed.
    let label_ok = !searching && st.search.is_empty();
    let keys = ctx.input_mut(|i| read_keys(i, searching, menu_open, label_ok));
    let (esc, enter, fork, slash) = (keys.esc, keys.enter, keys.fork, keys.slash);
    if esc {
        if searching {
            // First Esc leaves the search box (clearing it); the next one closes the view.
            st.search.clear();
            st.search_focused = false;
            ctx.memory_mut(|m| m.surrender_focus(st.search_id));
        } else {
            out.push(TreeAction::Close);
        }
        return;
    }
    if slash {
        st.focus_search = true;
    }
    let next_filter = keys
        .filter
        .or_else(|| keys.cycle.map(|d| st.filter.cycled(d)));
    if let Some(f) = next_filter
        && f != st.filter
    {
        st.filter = f;
        st.want = Want::Selected;
        ctx.request_repaint();
    }
    let Some(c) = &st.cache else {
        return;
    };
    let cur = selected_row(st, tree);
    // While a query has matches only they can be selected, like omp's filtered list.
    let target = keys.mv.and_then(|mv| {
        search_rows(st)
            .and_then(|rows| match_target(rows, cur, mv, st.page))
            .or_else(|| move_target(tree, &c.layout, cur, mv, st.page))
    });
    if let Some(r) = target
        && Some(r) != cur
    {
        select_row(st, tree, r);
        st.want = Want::Selected;
    }
    if enter && let Some(r) = selected_row(st, tree) {
        go_here(st, tree, input, r, keys.summarize, out);
    }
    if fork && let Some(r) = selected_row(st, tree) {
        fork_here(st, tree, input, r, out);
    }
    if keys.label
        && let Some(r) = selected_row(st, tree)
    {
        label_here(st, tree, input, r, out);
    }
}

/// One selection move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Move {
    /// Up / down by `n` rows.
    Step(isize),
    /// A page up (-1) or down (+1).
    Page(isize),
    Home,
    End,
    /// Previous / next user or assistant message (Alt+Up / Alt+Down).
    Turn {
        forward: bool,
    },
    /// Previous / next branch (`[` / `]`).
    Branch {
        forward: bool,
    },
}

/// The row `mv` selects from row `cur`; `page` is the number of rows the list shows.
fn move_target(
    tree: &SessionTree,
    layout: &Layout,
    cur: Option<usize>,
    mv: Move,
    page: usize,
) -> Option<usize> {
    let len = layout.rows.len();
    match mv {
        Move::Step(n) => step_row(cur, n, len),
        Move::Page(n) => step_row(cur, n * page.max(1) as isize, len),
        Move::Home => step_row(Some(0), 0, len),
        Move::End => len.checked_sub(1),
        Move::Turn { forward } => tree_layout::turn_jump(tree, layout, cur?, forward),
        Move::Branch { forward } => tree_layout::branch_jump(tree, layout, cur?, forward),
    }
}

/// The per-row match flags of an active query that matches something (`None` otherwise: no
/// query, or nothing matches and the list is left as it is).
fn search_rows(st: &TreeState) -> Option<&[bool]> {
    st.hits
        .as_ref()
        .map(|h| &h.rows[..])
        .filter(|rows| rows.contains(&true))
}

/// Whether a query is active and row `r` is not among its matches.
fn excluded_by_search(st: &TreeState, r: usize) -> bool {
    st.hits
        .as_ref()
        .is_some_and(|h| !h.rows.is_empty() && !h.rows.get(r).copied().unwrap_or(false))
}

/// The match to rest on from row `from`: itself, else the first one after it, else the first.
fn nearest_match(rows: &[bool], from: Option<usize>) -> Option<usize> {
    let start = from.unwrap_or(0).min(rows.len());
    rows[start..]
        .iter()
        .position(|m| *m)
        .map(|i| start + i)
        .or_else(|| rows.iter().position(|m| *m))
}

/// `mv` among the matching rows only (they are the list while a query is typed); the moves that
/// do not walk the list (message and branch jumps) are left to [`move_target`].
fn match_target(rows: &[bool], cur: Option<usize>, mv: Move, page: usize) -> Option<usize> {
    let matches: Vec<usize> = (0..rows.len()).filter(|&r| rows[r]).collect();
    let last = matches.len().checked_sub(1)?;
    let steps = match mv {
        Move::Step(n) => n,
        Move::Page(n) => n * page.max(1) as isize,
        Move::Home => return matches.first().copied(),
        Move::End => return matches.last().copied(),
        Move::Turn { .. } | Move::Branch { .. } => return None,
    };
    // Matches before `cur`; a match at `cur` itself is where the walk starts.
    let Some(cur) = cur else {
        return Some(matches[if steps >= 0 { 0 } else { last }]);
    };
    let before = matches.partition_point(|&m| m < cur);
    let on_match = matches.get(before) == Some(&cur);
    // From a non-match the first step down lands on the next match, the first step up on the
    // previous one.
    let at = if on_match {
        before as isize + steps
    } else if steps > 0 {
        before as isize + steps - 1
    } else {
        before as isize + steps
    };
    Some(matches[at.clamp(0, last as isize) as usize])
}

/// After the query (or what it runs over) changed: a selection the query excludes moves to the
/// nearest match. Nothing moves when no row matches.
fn follow_matches(st: &mut TreeState, tree: &SessionTree) {
    let cur = selected_row(st, tree);
    let Some(rows) = search_rows(st) else {
        return;
    };
    if cur.is_some_and(|r| rows.get(r).copied().unwrap_or(false)) {
        return;
    }
    let Some(r) = nearest_match(rows, cur) else {
        return;
    };
    select_row(st, tree, r);
    st.want = Want::Selected;
}

/// What the keyboard asked for this frame.
#[derive(Default)]
struct Keys {
    esc: bool,
    enter: bool,
    /// Shift+Enter: go there with a summary of the branch left (omp pre-answers "Summarize").
    summarize: bool,
    fork: bool,
    /// `L`: edit the selected entry's label.
    label: bool,
    slash: bool,
    mv: Option<Move>,
    /// Alt+C / D / T / U / L / A.
    filter: Option<Filter>,
    /// Ctrl+O (+1) / Ctrl+Shift+O (-1).
    cycle: Option<isize>,
}

/// Consume the tree's keys. omp's chords go first because egui ignores extra Shift / Alt when it
/// matches a key: a plain `ArrowUp` pattern would also take Alt+Up, Ctrl+O would take Ctrl+Shift+O.
/// Letters and ← → belong to the search box while it has focus.
fn read_keys(i: &mut egui::InputState, searching: bool, menu_open: bool, label_ok: bool) -> Keys {
    let n = Modifiers::NONE;
    let mut k = Keys::default();
    let mut alt = false;
    for (letter, key) in [
        (Filter::Conversation, Key::C),
        (Filter::Default, Key::D),
        (Filter::NoTools, Key::T),
        (Filter::UserOnly, Key::U),
        (Filter::Labeled, Key::L),
        (Filter::All, Key::A),
    ] {
        if !menu_open && i.consume_key(Modifiers::ALT, key) {
            k.filter = Some(letter);
            alt = true;
        }
    }
    if !menu_open {
        if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::O) {
            k.cycle = Some(-1);
        } else if i.consume_key(Modifiers::COMMAND, Key::O) {
            k.cycle = Some(1);
        }
        for (key, forward) in [(Key::ArrowUp, false), (Key::ArrowDown, true)] {
            if i.consume_key(Modifiers::ALT, key) {
                k.mv = Some(Move::Turn { forward });
                alt = true;
            }
        }
    }
    if alt {
        // Linux delivers Alt+D as the text "d" as well; the search box must not type it.
        i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
    }
    k.esc = !menu_open && i.consume_key(n, Key::Escape);
    // Shift+Enter first: a plain Enter pattern would take it too.
    k.summarize = !menu_open && i.consume_key(Modifiers::SHIFT, Key::Enter);
    k.enter = !menu_open && (k.summarize || i.consume_key(n, Key::Enter));
    k.fork = !searching && !menu_open && i.consume_key(n, Key::F);
    k.label = label_ok && !menu_open && i.consume_key(n, Key::L);
    k.slash = !searching && !menu_open && i.consume_key(n, Key::Slash)
        || i.consume_key(Modifiers::COMMAND, Key::F);
    let mut plain = |key: Key, mv: Move| {
        if i.consume_key(n, key) && k.mv.is_none() {
            k.mv = Some(mv);
        }
    };
    plain(Key::ArrowUp, Move::Step(-1));
    plain(Key::ArrowDown, Move::Step(1));
    plain(Key::PageUp, Move::Page(-1));
    plain(Key::PageDown, Move::Page(1));
    if !searching {
        plain(Key::ArrowLeft, Move::Page(-1));
        plain(Key::ArrowRight, Move::Page(1));
        plain(Key::Home, Move::Home);
        plain(Key::End, Move::End);
        if !menu_open {
            plain(Key::OpenBracket, Move::Branch { forward: false });
            plain(Key::CloseBracket, Move::Branch { forward: true });
        }
    }
    k
}

fn select_row(st: &mut TreeState, tree: &SessionTree, r: usize) {
    let Some(c) = &st.cache else {
        return;
    };
    let Some(row) = c.layout.rows.get(r) else {
        return;
    };
    st.selected = Some(tree.nodes[row.node].id.clone());
    // Resting on the leaf re-engages the live follow; moving elsewhere stops it.
    st.pinned = row.is_leaf;
}

/// Why Go here is unavailable for row `r`, if it is.
fn go_blocked<'a>(layout: &Layout, input: &TreeInput<'a>, r: usize) -> Option<&'a str> {
    if let Err(why) = input.navigate {
        return Some(why);
    }
    let row = &layout.rows[r];
    (layout.leaf_node == Some(row.node)).then_some("Already at this point")
}

/// Why Fork is unavailable for row `r`, if it is.
fn fork_blocked<'a>(
    tree: &SessionTree,
    layout: &Layout,
    input: &TreeInput<'a>,
    r: usize,
) -> Option<&'a str> {
    let node = layout.rows[r].node;
    if !tree.is_forkable(node) {
        return Some(if matches!(tree.nodes[node].kind, NodeKind::User) {
            "Nothing comes before the first prompt, so there is nothing to fork"
        } else {
            "Only messages can be forked"
        });
    }
    input.fork.err()
}

/// What "Go here" does about a branch summary.
#[derive(Clone, Debug, PartialEq, Eq)]
enum GoPlan {
    /// Ask "No summary / Summarize / Summarize with custom prompt".
    Ask,
    Now(BranchSummary),
}

/// omp: Shift+Enter pre-answers "Summarize" and skips the question; otherwise the question is
/// asked only while `branchSummary.enabled`.
fn go_plan(prompt_enabled: bool, shift: bool) -> GoPlan {
    if shift {
        GoPlan::Now(BranchSummary::Summarize)
    } else if prompt_enabled {
        GoPlan::Ask
    } else {
        GoPlan::Now(BranchSummary::None)
    }
}

/// The three answers of omp's "Summarize branch?" selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SummaryChoice {
    None,
    Summarize,
    Custom,
}

impl SummaryChoice {
    const ALL: [SummaryChoice; 3] = [
        SummaryChoice::None,
        SummaryChoice::Summarize,
        SummaryChoice::Custom,
    ];

    fn title(self) -> &'static str {
        match self {
            SummaryChoice::None => "No summary",
            SummaryChoice::Summarize => "Summarize",
            SummaryChoice::Custom => "Summarize with custom prompt…",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            SummaryChoice::None => "Leave the branch behind as it is.",
            SummaryChoice::Summarize => "omp writes a summary of the branch and carries it over.",
            SummaryChoice::Custom => "Say what the summary should focus on.",
        }
    }

    /// `delta` steps through the list, stopping at both ends.
    fn step(self, delta: isize) -> SummaryChoice {
        let at = Self::ALL.iter().position(|c| *c == self).unwrap_or(0);
        Self::ALL[at.saturating_add_signed(delta).min(Self::ALL.len() - 1)]
    }
}

/// A Go here waiting for its answer to "Summarize branch?".
struct SummaryDialog {
    entry: String,
    user: bool,
    choice: SummaryChoice,
    instructions: String,
    /// The instructions field takes the caret the next time it is drawn.
    focus_editor: bool,
    /// The chosen answer takes the keyboard focus the next time it is drawn.
    refocus: bool,
    editor: Id,
}

impl SummaryDialog {
    fn new(entry: String, user: bool) -> SummaryDialog {
        SummaryDialog {
            entry,
            user,
            choice: SummaryChoice::None,
            instructions: String::new(),
            focus_editor: false,
            refocus: true,
            editor: Id::unique("tree-summary-instructions"),
        }
    }

    fn choose(&mut self, choice: SummaryChoice) {
        self.focus_editor = choice == SummaryChoice::Custom && self.choice != choice;
        self.refocus = true;
        self.choice = choice;
    }

    /// What to send. Blank instructions are omp's default summary prompt.
    fn summary(&self) -> BranchSummary {
        match self.choice {
            SummaryChoice::None => BranchSummary::None,
            SummaryChoice::Summarize => BranchSummary::Summarize,
            SummaryChoice::Custom => match self.instructions.trim() {
                "" => BranchSummary::Summarize,
                text => BranchSummary::Custom(text.to_owned()),
            },
        }
    }
}

/// The label field's state.
struct LabelEdit {
    entry: String,
    /// The label the entry has (blank = none).
    original: String,
    text: String,
    /// The field takes the caret the next time it is drawn.
    focus: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum LabelEnd {
    /// Nothing to send: the text is what the entry already has.
    Unchanged,
    /// The new label; `None` removes it.
    Set(Option<String>),
}

impl LabelEdit {
    fn new(entry: String, current: Option<&str>) -> LabelEdit {
        let current = current.unwrap_or("").to_owned();
        LabelEdit {
            entry,
            text: current.clone(),
            original: current,
            focus: true,
        }
    }

    /// Enter: a blank field removes the label, like omp.
    fn finish(&self) -> LabelEnd {
        let new = self.text.trim();
        if new == self.original.trim() {
            return LabelEnd::Unchanged;
        }
        LabelEnd::Set((!new.is_empty()).then(|| new.to_owned()))
    }
}

/// Enter in the label field (or Save): send the label, or just close when nothing changed.
fn finish_label(st: &mut TreeState, input: &TreeInput<'_>, out: &mut Vec<TreeAction>) {
    let Some(edit) = st.label.take() else {
        return;
    };
    let LabelEnd::Set(label) = edit.finish() else {
        return;
    };
    match input.label {
        Ok(()) => out.push(TreeAction::SetLabel {
            entry: edit.entry,
            label,
        }),
        Err(why) => {
            out.push(TreeAction::Notice(why.to_owned()));
            st.label = Some(edit);
        }
    }
}

/// Open the label field on row `r`.
fn label_here(
    st: &mut TreeState,
    tree: &SessionTree,
    input: &TreeInput<'_>,
    r: usize,
    out: &mut Vec<TreeAction>,
) {
    if let Err(why) = input.label {
        out.push(TreeAction::Notice(why.to_owned()));
        return;
    }
    let Some(row) = st.cache.as_ref().and_then(|c| c.layout.rows.get(r)) else {
        return;
    };
    let id = tree.nodes[row.node].id.clone();
    let current = tree.labels.get(&id).map(String::as_str);
    st.label = Some(LabelEdit::new(id.clone(), current));
}

/// Why "Re-answer…" is unavailable for row `r`, if it is.
fn reanswer_blocked<'a>(
    tree: &SessionTree,
    layout: &Layout,
    input: &TreeInput<'a>,
    r: usize,
) -> Option<&'a str> {
    if !tree_layout::is_ask_result(&tree.nodes[layout.rows[r].node]) {
        return Some("Only the answer to an ask can be re-answered");
    }
    input.reanswer.err()
}

fn reanswer_here(
    st: &TreeState,
    tree: &SessionTree,
    input: &TreeInput<'_>,
    r: usize,
    out: &mut Vec<TreeAction>,
) {
    let Some(c) = &st.cache else {
        return;
    };
    match reanswer_blocked(tree, &c.layout, input, r) {
        Some(why) => out.push(TreeAction::Notice(why.to_owned())),
        None => out.push(TreeAction::Reanswer {
            entry: tree.nodes[c.layout.rows[r].node].id.clone(),
        }),
    }
}

/// Go to row `r`: now, or after asking about a branch summary (`shift` = Shift+Enter).
fn go_here(
    st: &mut TreeState,
    tree: &SessionTree,
    input: &TreeInput<'_>,
    r: usize,
    shift: bool,
    out: &mut Vec<TreeAction>,
) {
    let Some(c) = &st.cache else {
        return;
    };
    // The row is dimmed out by the query: not part of the list.
    if excluded_by_search(st, r) {
        return;
    }
    if let Some(why) = go_blocked(&c.layout, input, r) {
        out.push(TreeAction::Notice(why.to_owned()));
        return;
    }
    let n = &tree.nodes[c.layout.rows[r].node];
    let (entry, user) = (n.id.clone(), matches!(n.kind, NodeKind::User));
    match go_plan(input.summary_prompt, shift) {
        GoPlan::Ask => st.summary = Some(SummaryDialog::new(entry, user)),
        GoPlan::Now(summary) => out.push(TreeAction::Navigate {
            entry,
            user,
            summary,
        }),
    }
}

fn fork_here(
    st: &TreeState,
    tree: &SessionTree,
    input: &TreeInput<'_>,
    r: usize,
    out: &mut Vec<TreeAction>,
) {
    let Some(c) = &st.cache else {
        return;
    };
    match fork_blocked(tree, &c.layout, input, r) {
        Some(why) => out.push(TreeAction::Notice(why.to_owned())),
        None => {
            let node = c.layout.rows[r].node;
            if let Some(entry) = tree.fork_target(node) {
                out.push(TreeAction::Fork {
                    entry: entry.to_owned(),
                    what: tree_layout::fork_hint(&tree.nodes[node].kind).to_owned(),
                });
            }
        }
    }
}

// ------------------------------------------------------------------ layout of the window

fn body(ui: &mut Ui, st: &mut TreeState, input: &TreeInput<'_>, out: &mut Vec<TreeAction>) {
    let pal = Palette::of_ui(ui);
    ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
    ui.spacing_mut().scroll = egui::style::ScrollStyle::floating();

    footer(ui, st, input);
    label_bar(ui, st, input, out);

    Frame::NONE
        .inner_margin(Margin {
            left: 22,
            right: 18,
            top: 16,
            bottom: 8,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            header(ui, st, input, out);
        });
    let y = ui.cursor().top();
    card::header_rule(
        ui,
        ui.max_rect().left(),
        ui.max_rect().right(),
        y,
        Color32::TRANSPARENT,
    );
    busy_banner(ui, input);

    let Some(tree) = input.tree else {
        centered_status(ui, input);
        return;
    };
    let empty = st.cache.as_ref().is_none_or(|c| c.layout.rows.is_empty());
    if empty {
        let entries = st.cache.as_ref().map_or(0, |c| c.layout.stats.entries);
        let (title, sub) = match st.filter {
            _ if entries == 0 => ("Nothing to show", "This session has no entries yet."),
            Filter::Labeled => (
                "No labeled entries",
                "Labels you add with /label show up here.",
            ),
            _ => (
                "Nothing matches this filter",
                "Alt+A shows every entry, Alt+D the default view.",
            ),
        };
        empty_state(ui, ICON_LIST_TREE, pal.weak, title, sub);
        return;
    }
    list(ui, st, tree, input, out);
}

fn centered_status(ui: &mut Ui, input: &TreeInput<'_>) {
    let pal = Palette::of_ui(ui);
    match input.error {
        Some(e) => empty_state(ui, ICON_LIST_TREE, pal.danger, "Cannot read the session", e),
        None => {
            ui.add_space(ui.available_height() * 0.35);
            ui.vertical_centered(|ui| {
                ui.add(crate::widgets::Spin::new(22.0, pal.accent));
                ui.add_space(8.0);
                ui.label(RichText::new("Reading the session…").color(pal.weak));
            });
        }
    }
}

fn header(ui: &mut Ui, st: &mut TreeState, input: &TreeInput<'_>, out: &mut Vec<TreeAction>) {
    let pal = Palette::of_ui(ui);
    Sides::new().height(32.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            card::icon_tile(ui, 30.0, ICON_LIST_TREE, Color32::TRANSPARENT, true);
            ui.label(
                theme::strong("Session tree")
                    .size(19.0)
                    .color(pal.text_strong),
            );
            if !st.title.is_empty() {
                ui.add(
                    egui::Label::new(RichText::new(&st.title).color(pal.weak).size(14.0))
                        .truncate(),
                );
            }
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if card::close_button_sized(ui, 30.0, 18.0).clicked() {
                out.push(TreeAction::Close);
            }
        },
    );
    ui.add_space(4.0);
    let mut filter = st.filter;
    let focus = std::mem::take(&mut st.focus_search);
    let search_id = st.search_id;
    let search = &mut st.search;
    Sides::new().height(30.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for f in Filter::ALL {
                if chip(ui, f, filter == f).clicked() {
                    filter = f;
                }
            }
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            search_box(ui, search, search_id, focus);
        },
    );
    if filter != st.filter {
        st.filter = filter;
        st.want = Want::Selected;
    }
    ui.add_space(2.0);
    if let Some(c) = &st.cache {
        let s = c.layout.stats;
        let plural = |n: usize, one: &str, many: &str| {
            format!("{} {}", fmt_count(n), if n == 1 { one } else { many })
        };
        let mut line = format!(
            "{} of {}",
            fmt_count(s.shown),
            plural(s.entries, "entry", "entries")
        );
        if s.tips > 1 {
            line.push_str(" · ");
            line.push_str(&plural(s.tips, "branch", "branches"));
        }
        if s.forks > 0 {
            line.push_str(" · ");
            line.push_str(&plural(s.forks, "fork point", "fork points"));
        }
        if input.live_leaf.is_some() {
            line.push_str(" · live");
        }
        ui.label(RichText::new(line).size(12.0).color(pal.weak));
    }
}

fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn icon(i: Icon, size: f32, color: Color32) -> RichText {
    crate::widgets::icon_text(i, size, color)
}

/// Filter pill; the tooltip names the filter's Alt shortcut.
fn chip(ui: &mut Ui, filter: Filter, on: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let g = ui.painter().layout_no_wrap(
        filter.title().to_owned(),
        crate::theme::font(13.0),
        pal.text,
    );
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 20.0, 28.0), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
    let p = ui.painter();
    let r = theme::rad(8, 14);
    if on {
        let fill = if theme::flat() {
            pal.raised
        } else {
            pal.accent.gamma_multiply(0.22)
        };
        p.rect_filled(rect, r, fill);
        p.rect_stroke(
            rect,
            r,
            Stroke::new(
                1.0,
                pal.accent
                    .gamma_multiply(if theme::flat() { 0.8 } else { 0.7 }),
            ),
            StrokeKind::Inside,
        );
    } else {
        p.rect_filled(rect, r, pal.raised.gamma_multiply(0.7 * hov));
        p.rect_stroke(
            rect,
            r,
            Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
            StrokeKind::Inside,
        );
    }
    let color = if on {
        pal.text_strong
    } else {
        theme::mix(pal.weak, pal.text_strong, hov)
    };
    p.galley(rect.center() - g.size() / 2.0, g, color);
    resp.on_hover_ui(|ui| {
        ui.label(filter.hint());
        ui.label(
            RichText::new(format!(
                "Alt+{} · Ctrl+O cycles, Ctrl+Shift+O cycles back",
                filter.shortcut()
            ))
            .size(12.0)
            .color(pal.weak),
        );
    })
    .on_hover_cursor(CursorIcon::PointingHand)
}

fn search_box(ui: &mut Ui, search: &mut String, id: Id, focus: bool) {
    let pal = Palette::of_ui(ui);
    if !search.is_empty() {
        let clear = ui.add(egui::Button::new(icon(ICON_X, 16.0, pal.weak)).frame(false));
        if clear.on_hover_text("Clear search").clicked() {
            search.clear();
        }
    }
    let edit = TextEdit::singleline(search)
        .id(id)
        .hint_text("Search prompts, replies, labels…  (/)")
        .desired_width(270.0)
        .margin(vec2(10.0, 6.0));
    let r = ui.add(edit);
    if focus {
        r.request_focus();
    }
}

fn footer(ui: &mut Ui, st: &TreeState, input: &TreeInput<'_>) {
    let pal = Palette::of_ui(ui);
    egui::Panel::bottom("tree-footer")
        .show_separator_line(false)
        .exact_size(100.0)
        .frame(Frame::NONE.inner_margin(Margin::symmetric(22, 8)))
        .show(ui, |ui| {
            // Below this the key labels would run past the window edge; the keycaps stay.
            let wide = ui.available_width() >= 780.0;
            let y = ui.max_rect().top();
            ui.painter().hline(
                ui.max_rect().left() - 22.0..=ui.max_rect().right() + 22.0,
                y - 8.0,
                Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
            );
            key_row(
                ui,
                wide,
                &[
                    ("↑ ↓", "select"),
                    ("PgUp PgDn ← →", "page"),
                    ("Home End", "ends"),
                    ("Alt+↑ Alt+↓", "message"),
                    ("[ ]", "branch"),
                ],
            );
            key_row(
                ui,
                wide,
                &[
                    ("Enter", "go here"),
                    ("Shift+Enter", "summarize"),
                    ("F", "fork"),
                    ("L", "label"),
                    ("Esc", "close"),
                ],
            );
            Sides::new().height(24.0).show(
                ui,
                |ui| {
                    ui.spacing_mut().item_spacing.x = 14.0;
                    for (key, what) in [
                        ("Ctrl+O", "filter"),
                        ("Alt+C D T U L A", "pick filter"),
                        ("/", "search"),
                    ] {
                        keycap(ui, key, what, wide);
                    }
                },
                |ui| {
                    let (text, color) = match (&input.navigate, st.cache.is_some()) {
                        (Err(why), true) => (format!("Go here is off: {why}"), pal.weak),
                        (Ok(()), true) => (
                            "Go here re-renders the tab at the chosen point".to_owned(),
                            pal.weak,
                        ),
                        _ => (String::new(), pal.weak),
                    };
                    ui.add(
                        egui::Label::new(RichText::new(text).size(12.0).color(color)).truncate(),
                    );
                    ui.add_space(14.0);
                },
            );
        });
}

fn key_row(ui: &mut Ui, wide: bool, keys: &[(&str, &str)]) {
    ui.horizontal(|ui| {
        ui.set_height(24.0);
        ui.spacing_mut().item_spacing.x = 14.0;
        for (key, what) in keys {
            keycap(ui, key, what, wide);
        }
    });
}

/// Progress of what the session is busy with (a summary can take a while).
fn busy_banner(ui: &mut Ui, input: &TreeInput<'_>) {
    let Some(text) = input.busy else {
        return;
    };
    let pal = Palette::of_ui(ui);
    Frame::NONE
        .fill(pal.surface)
        .inner_margin(Margin::symmetric(22, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                ui.add(crate::widgets::Spin::new(16.0, pal.accent));
                ui.label(RichText::new(text).size(13.0).color(pal.text));
            });
        });
}

/// The inline label editor above the footer.
fn label_bar(ui: &mut Ui, st: &mut TreeState, input: &TreeInput<'_>, out: &mut Vec<TreeAction>) {
    let Some(edit) = st.label.as_mut() else {
        return;
    };
    let pal = Palette::of_ui(ui);
    let mut save = false;
    let mut cancel = false;
    egui::Panel::bottom("tree-label")
        .show_separator_line(false)
        .exact_size(64.0)
        .frame(Frame::NONE.inner_margin(Margin::symmetric(22, 8)))
        .show(ui, |ui| {
            let y = ui.max_rect().top();
            ui.painter().hline(
                ui.max_rect().left() - 22.0..=ui.max_rect().right() + 22.0,
                y - 8.0,
                Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
            );
            ui.horizontal(|ui| {
                ui.set_min_height(crate::widgets::draft_height());
                ui.spacing_mut().item_spacing.x = 10.0;
                ui.label(icon(ICON_BOOKMARK, 16.0, pal.warn));
                ui.label(RichText::new("Label").size(13.0).color(pal.text_strong));
                let width = (ui.available_width() - 232.0).max(140.0);
                let field = ui.add(
                    TextEdit::singleline(&mut edit.text)
                        .hint_text("Label for this entry — empty removes it")
                        .desired_width(width)
                        .margin(vec2(10.0, 6.0)),
                );
                if std::mem::take(&mut edit.focus) {
                    field.request_focus();
                }
                let ready = input.label.is_ok();
                if DraftButton::new("Save")
                    .kind(DraftKind::Primary)
                    .enabled(ready)
                    .min_width(72.0)
                    .show(ui)
                    .clicked()
                {
                    save = true;
                }
                if DraftButton::new("Cancel")
                    .min_width(72.0)
                    .show(ui)
                    .clicked()
                {
                    cancel = true;
                }
            });
        });
    if cancel {
        st.label = None;
    } else if save {
        finish_label(st, input, out);
    }
}

/// "Summarize branch?" before a Go here, as a Drafted dialog.
fn summary_dialog(ctx: &egui::Context, st: &mut TreeState, out: &mut Vec<TreeAction>) {
    let Some(d) = st.summary.as_mut() else {
        return;
    };
    let editor = d.editor;
    // The arrow keys move the choice, unless the instructions field has the caret.
    if !ctx.memory(|m| m.has_focus(editor)) {
        let delta = ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                1
            } else if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                -1
            } else {
                0
            }
        });
        if delta != 0 {
            let next = d.choice.step(delta);
            d.choose(next);
        }
    }
    let custom = d.choice == SummaryChoice::Custom;
    let reply = confirm::show_dialog_reply(
        ctx,
        &Dialog {
            id: "tree-summary",
            title: "Summarize branch?",
            subtitle: Some("The branch you leave can be summarized and carried to the new point."),
            width: 480.0,
            confirm_label: "Go here",
            confirm_enabled: true,
            enter_confirms: !custom,
            ctrl_enter_confirms: custom,
            icon: Some(ICON_SCROLL_TEXT),
            ..Dialog::default()
        },
        "Cancel",
        |ui| summary_body(ui, d, editor),
    );
    match reply {
        Some(Reply::Confirmed) => {
            if let Some(d) = st.summary.take() {
                out.push(TreeAction::Navigate {
                    summary: d.summary(),
                    entry: d.entry,
                    user: d.user,
                });
            }
        }
        // The Cancel button, Esc or a click outside: back to the tree.
        Some(_) => st.summary = None,
        None => {}
    }
}

fn summary_body(ui: &mut Ui, d: &mut SummaryDialog, editor: Id) {
    let pal = Palette::of_ui(ui);
    ui.spacing_mut().item_spacing.y = 4.0;
    for choice in SummaryChoice::ALL {
        let on = d.choice == choice;
        let title = RichText::new(choice.title()).size(13.5).color(if on {
            pal.text_strong
        } else {
            pal.text
        });
        let radio = ui.radio(on, title);
        if radio.has_focus() {
            // The arrow keys belong to this list (they would move egui's focus otherwise).
            ui.ctx().memory_mut(|m| {
                m.set_focus_lock_filter(
                    radio.id,
                    egui::EventFilter {
                        vertical_arrows: true,
                        ..egui::EventFilter::default()
                    },
                );
            });
        }
        if on && std::mem::take(&mut d.refocus) && choice != SummaryChoice::Custom {
            radio.request_focus();
        }
        if radio.clicked() && !on {
            d.choose(choice);
        }
        ui.horizontal(|ui| {
            ui.add_space(26.0);
            ui.label(RichText::new(choice.hint()).size(12.0).color(pal.weak));
        });
        ui.add_space(4.0);
    }
    if d.choice == SummaryChoice::Custom {
        let field = ui.add(
            TextEdit::multiline(&mut d.instructions)
                .id(editor)
                .desired_rows(5)
                .desired_width(f32::INFINITY)
                .hint_text("What should the summary focus on? Empty uses omp's default prompt."),
        );
        if std::mem::take(&mut d.focus_editor) {
            field.request_focus();
        }
    }
    ui.add_space(8.0);
}

fn keycap(ui: &mut Ui, key: &str, what: &str, with_label: bool) {
    let pal = Palette::of_ui(ui);
    let g = ui
        .painter()
        .layout_no_wrap(key.to_owned(), crate::theme::font(11.5), pal.text);
    let (rect, _) = ui.allocate_exact_size(vec2(g.size().x + 14.0, 20.0), Sense::hover());
    ui.painter().rect_filled(rect, 6, pal.surface);
    ui.painter()
        .rect_stroke(rect, 6, Stroke::new(1.0, pal.stroke), StrokeKind::Inside);
    ui.painter()
        .galley(rect.center() - g.size() / 2.0, g, pal.text);
    if with_label {
        ui.label(RichText::new(what).size(12.0).color(pal.weak));
    }
}

// ------------------------------------------------------------------ the list

struct Geometry {
    left: f32,
    top: f32,
    step: f32,
    graph_w: f32,
}

impl Geometry {
    fn x(&self, lane: u16) -> f32 {
        self.left + PAD_L + (f32::from(lane) + 0.5) * self.step
    }
    fn y(&self, row: usize) -> f32 {
        self.top + (row as f32 + 0.5) * ROW_H
    }
    fn at(&self, row: usize, lane: u16) -> Pos2 {
        pos2(self.x(lane), self.y(row))
    }
    fn text_x(&self) -> f32 {
        self.left + PAD_L + self.graph_w + 10.0
    }
}

fn list(
    ui: &mut Ui,
    st: &mut TreeState,
    tree: &Arc<SessionTree>,
    input: &TreeInput<'_>,
    out: &mut Vec<TreeAction>,
) {
    let pal = Palette::of_ui(ui);
    let Some(layout) = st.cache.as_ref().map(|c| &c.layout) else {
        return;
    };
    let rows = layout.rows.len();
    let (step, graph_w) = gutter(layout.lanes);
    let total_h = rows as f32 * ROW_H + 12.0;
    let want = std::mem::replace(&mut st.want, Want::None);
    let sel = selected_row(st, tree);
    let leaf_row = layout.leaf_row;
    let target = match want {
        Want::Leaf => leaf_row.or(sel),
        Want::Selected => sel,
        Want::None => None,
    };
    let dim_hits = st.hits.as_ref().filter(|h| !h.tokens.is_empty());
    st.galleys.sync(GalleyKey {
        revision: input.revision,
        ppp: ui.ctx().pixels_per_point().to_bits(),
        theme: [
            pal.text,
            pal.text_strong,
            pal.weak,
            pal.warn,
            pal.ok,
            pal.accent,
        ],
        font: crate::theme::ui_font(),
        tokens: dim_hits.map_or(&[], |h| &h.tokens[..]),
    });

    let mut clicked: Option<usize> = None;
    let mut actions: Vec<(usize, RowCommand)> = Vec::new();
    let area = egui::ScrollArea::vertical()
        .id_salt("tree-scroll")
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            let (rect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), total_h), Sense::hover());
            let geo = Geometry {
                left: rect.left(),
                top: rect.top(),
                step,
                graph_w,
            };
            if let Some(r) = target {
                let row = Rect::from_min_size(
                    pos2(rect.left(), geo.top + r as f32 * ROW_H),
                    vec2(rect.width(), ROW_H),
                );
                let align = if want == Want::Leaf {
                    Some(Align::Center)
                } else {
                    None
                };
                ui.scroll_to_rect_animation(
                    row.expand2(vec2(0.0, ROW_H)),
                    align,
                    egui::style::ScrollAnimation::none(),
                );
            }
            let first = ((viewport.min.y / ROW_H).floor().max(0.0) as usize).min(rows);
            let last = ((viewport.max.y / ROW_H).ceil().max(0.0) as usize + 1).min(rows);
            let hover_pos = ui.input(|i| i.pointer.hover_pos());
            st.scratch.clear();
            for r in first..last {
                let rr = Rect::from_min_size(
                    pos2(rect.left(), geo.top + r as f32 * ROW_H),
                    vec2(rect.width(), ROW_H),
                );
                let resp = ui.interact(rr, Id::unique("tree-row").with(r), Sense::click());
                let hovered = hover_pos
                    .is_some_and(|p| rr.contains(p) && ui.clip_rect().contains(p))
                    && !egui::Popup::is_any_open(ui.ctx());
                if resp.clicked() {
                    clicked = Some(r);
                }
                if let Some(c) = &st.cache {
                    let node = c.layout.rows[r].node;
                    row_menu(&resp, tree, &c.layout, input, r, node, &mut actions);
                    // The action buttons carry their own tooltip.
                    let actions_left = rr.right()
                        - 18.0
                        - actions_w(tree_layout::is_ask_result(&tree.nodes[node]));
                    let over_actions = hover_pos.is_some_and(|p| p.x >= actions_left);
                    if hovered && !over_actions && !resp.context_menu_opened() {
                        row_tip(&resp, tree, &c.layout.rows[r], input.now);
                    }
                }
                st.scratch.push(RowUi {
                    row: r,
                    rect: rr,
                    hovered,
                });
            }
            let Some(layout) = st.cache.as_ref().map(|c| &c.layout) else {
                return;
            };
            // Backgrounds, then the graph, then row content on top.
            let painter = ui.painter().clone();
            let hover_row = st.scratch.iter().find(|u| u.hovered).map(|u| u.row);
            for u in &st.scratch {
                paint_band(
                    &painter,
                    pal,
                    layout,
                    u,
                    sel == Some(u.row),
                    hover_row == Some(u.row),
                );
            }
            paint_graph(&painter, pal, layout, &geo, first, last, &mut st.long);
            let active = hover_row.or(sel);
            for u in &st.scratch {
                let row = &layout.rows[u.row];
                let dim = match dim_hits {
                    Some(h) if !h.rows.get(u.row).copied().unwrap_or(false) => 0.32,
                    _ => 1.0,
                };
                paint_node(
                    &painter,
                    pal,
                    row,
                    geo.at(u.row, row.lane),
                    row.children > 1,
                );
                let mut p = painter.clone();
                p.set_opacity(dim);
                let tokens: &[String] = dim_hits.map_or(&[], |h| &h.tokens);
                let cmd = paint_content(
                    ui,
                    &p,
                    pal,
                    &ContentCtx {
                        tree,
                        layout,
                        input,
                        geo: &geo,
                        tokens,
                        show_actions: active == Some(u.row),
                    },
                    u,
                    &mut st.galleys,
                );
                if let Some(cmd) = cmd {
                    actions.push((u.row, cmd));
                }
            }
        });
    let offset = area.state.clamped_offset().y;
    st.page = ((area.inner_rect.height() / ROW_H).floor() as usize).max(1);
    if want != Want::None {
        st.settle = 2;
    } else if st.settle > 0 {
        st.settle -= 1;
    } else if (offset - st.last_offset).abs() > 0.5 {
        // The user scrolled: stop following the leaf.
        st.pinned = false;
    }
    st.last_offset = offset;
    if let Some(r) = clicked {
        select_row(st, tree, r);
    }
    for (r, cmd) in actions {
        match cmd {
            RowCommand::Fork => {
                select_row(st, tree, r);
                fork_here(st, tree, input, r, out);
            }
            RowCommand::Go => {
                select_row(st, tree, r);
                go_here(st, tree, input, r, false, out);
            }
            RowCommand::Label => {
                select_row(st, tree, r);
                label_here(st, tree, input, r, out);
            }
            RowCommand::Reanswer => {
                select_row(st, tree, r);
                reanswer_here(st, tree, input, r, out);
            }
            RowCommand::CopyText(text) => out.push(TreeAction::Copy {
                text,
                what: "entry text",
            }),
            RowCommand::CopyId(text) => out.push(TreeAction::Copy {
                text,
                what: "entry id",
            }),
        }
    }
    current_button(ui, st, tree, area.inner_rect);
}

/// Floating "jump to the current point" button, shown when the leaf is off screen.
fn current_button(ui: &mut Ui, st: &mut TreeState, tree: &SessionTree, list: Rect) {
    let Some(leaf_row) = st.cache.as_ref().and_then(|c| c.layout.leaf_row) else {
        return;
    };
    let top = st.last_offset;
    let y = leaf_row as f32 * ROW_H;
    if y + ROW_H > top && y < top + list.height() {
        return;
    }
    let pal = Palette::of_ui(ui);
    let g = ui.painter().layout_no_wrap(
        "Current".to_owned(),
        crate::theme::font(12.5),
        theme::on_accent(pal),
    );
    let size = vec2(g.size().x + 40.0, 30.0);
    let rect = Rect::from_min_size(
        pos2(list.right() - size.x - 22.0, list.bottom() - size.y - 14.0),
        size,
    );
    let resp = ui.interact(rect, Id::unique("tree-current"), Sense::click());
    let hov = resp.hovered();
    let p = ui.painter();
    if !theme::flat() {
        p.rect_filled(
            rect.translate(vec2(0.0, 2.0)),
            15,
            Color32::from_black_alpha(60),
        );
    }
    theme::gradient_rounded(
        p,
        rect,
        15.0,
        pal.accent_a.gamma_multiply(if hov { 1.0 } else { 0.9 }),
        pal.accent_b.gamma_multiply(if hov { 1.0 } else { 0.9 }),
    );
    paint_icon(
        p,
        pos2(rect.left() + 16.0, rect.center().y),
        Align2::CENTER_CENTER,
        ICON_CROSSHAIR,
        16.0,
        theme::on_accent(pal),
    );
    p.galley(
        pos2(rect.left() + 28.0, rect.center().y - g.size().y / 2.0),
        g,
        theme::on_accent(pal),
    );
    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
        st.want = Want::Leaf;
        st.pinned = true;
        st.selected = st.cache.as_ref().and_then(|c| {
            c.layout
                .leaf_row
                .map(|r| tree.nodes[c.layout.rows[r].node].id.clone())
        });
    }
}

enum RowCommand {
    Fork,
    Go,
    Label,
    Reanswer,
    CopyText(String),
    CopyId(String),
}

fn row_menu(
    resp: &egui::Response,
    tree: &SessionTree,
    layout: &Layout,
    input: &TreeInput<'_>,
    r: usize,
    node: usize,
    out: &mut Vec<(usize, RowCommand)>,
) {
    theme::context_menu(resp, 230.0, |ui| {
        let n = &tree.nodes[node];
        let fork_off = fork_blocked(tree, layout, input, r);
        let it = menu_item(
            ui,
            ICON_GIT_FORK,
            tree_layout::fork_hint(&n.kind),
            None,
            fork_off.is_none(),
            false,
        );
        let it = match fork_off {
            Some(why) => it.on_disabled_hover_text(why),
            None => it,
        };
        if it.clicked() {
            out.push((r, RowCommand::Fork));
            ui.close();
        }
        let go_off = go_blocked(layout, input, r);
        let it = menu_item(
            ui,
            ICON_CORNER_DOWN_RIGHT,
            "Go here",
            Some("Enter"),
            go_off.is_none(),
            false,
        );
        let it = match go_off {
            Some(why) => it.on_disabled_hover_text(why),
            None => it,
        };
        if it.clicked() {
            out.push((r, RowCommand::Go));
            ui.close();
        }
        let label_off = input.label.err();
        let it = menu_item(
            ui,
            ICON_BOOKMARK,
            "Edit label…",
            Some("L"),
            label_off.is_none(),
            false,
        );
        let it = match label_off {
            Some(why) => it.on_disabled_hover_text(why),
            None => it,
        };
        if it.clicked() {
            out.push((r, RowCommand::Label));
            ui.close();
        }
        if tree_layout::is_ask_result(n) {
            let off = reanswer_blocked(tree, layout, input, r);
            let it = menu_item(
                ui,
                ICON_MESSAGES_SQUARE,
                "Re-answer…",
                None,
                off.is_none(),
                false,
            );
            let it = match off {
                Some(why) => it.on_disabled_hover_text(why),
                None => it,
            };
            if it.clicked() {
                out.push((r, RowCommand::Reanswer));
                ui.close();
            }
        }
        ui.separator();
        if menu_item(ui, ICON_COPY, "Copy text", None, !n.text.is_empty(), false).clicked() {
            out.push((r, RowCommand::CopyText(n.text.clone())));
            ui.close();
        }
        if menu_item(ui, ICON_KEY_ROUND, "Copy entry id", None, true, false).clicked() {
            out.push((r, RowCommand::CopyId(n.id.clone())));
            ui.close();
        }
    });
}

fn row_tip(resp: &egui::Response, tree: &SessionTree, row: &Row, now: i64) {
    let pal = Palette::of_ctx(&resp.ctx);
    let n = &tree.nodes[row.node];
    let kind = tree_layout::kind_name(&n.kind);
    let title = {
        let mut cs = kind.chars();
        cs.next()
            .map(|c| c.to_uppercase().chain(cs).collect::<String>())
            .unwrap_or_default()
    };
    let tip_icon = match row.class {
        RowClass::User => ICON_USER,
        RowClass::Assistant => ICON_SPARKLES,
        RowClass::Tool => ICON_WRENCH,
        RowClass::Compaction => ICON_FOLD_VERTICAL,
        RowClass::Summary => ICON_SCROLL_TEXT,
        RowClass::Other => ICON_LIST_TREE,
    };
    let head = card::Header::new(tip_icon, &title).tone(class_color(pal, row.class));
    card::hover(
        resp,
        460.0,
        false,
        &head,
        |ui| {
            if let Some(ts) = row.ts {
                crate::badge::show(
                    ui,
                    None,
                    &format!("{} ago", tree_layout::age(now, ts)),
                    Hue::Teal,
                    11.5,
                );
            }
        },
        |ui| {
            const KEY_W: f32 = 112.0;
            if !n.text.is_empty() {
                card::kv(ui, KEY_W, "Text", |ui| {
                    ui.label(RichText::new(&n.text).color(pal.text_strong));
                });
            }
            if let Some(l) = tree.labels.get(&n.id) {
                card::kv(ui, KEY_W, "Label", |ui| {
                    crate::badge::show(ui, Some(ICON_BOOKMARK), l, Hue::Amber, 11.5);
                });
            }
            card::kv(ui, KEY_W, "Cost before", |ui| {
                let hue = if n.cost_before.incomplete {
                    Hue::Amber
                } else {
                    Hue::Green
                };
                let amount = format!(
                    "{}${:.6}",
                    if n.cost_before.incomplete { "≥" } else { "" },
                    n.cost_before.total
                );
                ui.vertical(|ui| {
                    crate::badge::show(ui, Some(ICON_COINS), &amount, hue, 11.5);
                    ui.label(RichText::new("Journal order across all branches; excludes this entry and unrecorded live subagent spend.").color(pal.weak));
                    if n.cost_before.incomplete {
                        ui.label(
                            RichText::new("Some earlier usage could not be read or priced; this is the known subtotal.")
                                .color(pal.warn),
                        );
                    }
                });
            });
            if let Some(ts) = row.ts {
                card::kv(ui, KEY_W, "Time", |ui| {
                    ui.label(RichText::new(tree_layout::clock(ts)).color(pal.text_strong));
                });
            }
            card::kv(ui, KEY_W, "Entry id", |ui| {
                crate::badge::show(ui, Some(ICON_KEY_ROUND), &n.id, Hue::Teal, 11.5);
            });
        },
    );
}

// ------------------------------------------------------------------ painting

fn paint_band(
    p: &Painter,
    pal: &Palette,
    layout: &Layout,
    u: &RowUi,
    selected: bool,
    hovered: bool,
) {
    let row = &layout.rows[u.row];
    let inner = u.rect.shrink2(vec2(8.0, 2.0));
    let r = theme::rad(8, 10);
    if row.is_leaf {
        let fill = if theme::flat() {
            pal.surface
        } else {
            pal.accent.gamma_multiply(0.11)
        };
        p.rect_filled(inner, r, fill);
    }
    if hovered {
        p.rect_filled(inner, r, pal.raised.gamma_multiply(0.55));
    }
    if selected {
        let fill = if theme::flat() {
            pal.raised
        } else {
            pal.accent.gamma_multiply(0.16)
        };
        p.rect_filled(inner, r, fill);
        p.rect_stroke(
            inner,
            r,
            Stroke::new(1.0, pal.accent.gamma_multiply(0.55)),
            StrokeKind::Inside,
        );
    }
}

/// Edges of the rows in `first..last`, plus the long edges crossing that range. Inactive
/// branches first so the active path draws on top.
fn paint_graph(
    p: &Painter,
    pal: &Palette,
    layout: &Layout,
    geo: &Geometry,
    first: usize,
    last: usize,
    scratch: &mut Vec<u32>,
) {
    scratch.clear();
    let from = first.saturating_sub(1);
    scratch.extend((from..last).map(|r| r as u32));
    // Long edges whose child row lies outside the window but whose span crosses it.
    for &r in &layout.long_edges {
        let r_us = r as usize;
        if (from..last).contains(&r_us) {
            continue;
        }
        if let Some(parent) = layout.rows[r_us].parent
            && (parent as usize) < last
            && r_us >= from
        {
            scratch.push(r);
        }
    }
    for pass in [false, true] {
        for &r in scratch.iter() {
            let row = &layout.rows[r as usize];
            if row.on_path != pass {
                continue;
            }
            let Some(parent) = row.parent else {
                continue;
            };
            let prow = &layout.rows[parent as usize];
            let (a, b) = (
                geo.at(parent as usize, prow.lane),
                geo.at(r as usize, row.lane),
            );
            let stroke = if row.on_path {
                Stroke::new(2.6, pal.accent)
            } else {
                Stroke::new(1.6, lane_color(pal, row.lane))
            };
            edge(p, a, b, stroke);
        }
    }
}

/// Parent `a` → child `b`: straight in a lane, or a smooth S-curve into the child's lane over the
/// first row, then straight down to the child.
fn edge(p: &Painter, a: Pos2, b: Pos2, stroke: Stroke) {
    if (a.x - b.x).abs() < 0.5 {
        p.line_segment([a, b], stroke);
        return;
    }
    let end = pos2(b.x, (a.y + ROW_H).min(b.y));
    let h = end.y - a.y;
    p.add(CubicBezierShape::from_points_stroke(
        [
            a,
            pos2(a.x, a.y + h * 0.62),
            pos2(b.x, end.y - h * 0.62),
            end,
        ],
        false,
        Color32::TRANSPARENT,
        stroke,
    ));
    if b.y > end.y {
        p.line_segment([end, b], stroke);
    }
}

fn paint_node(p: &Painter, pal: &Palette, row: &Row, c: Pos2, fork_point: bool) {
    let base = pal.bar;
    if row.is_leaf && !theme::flat() {
        p.circle_filled(c, 15.0, pal.accent.gamma_multiply(0.14));
        p.circle_filled(c, 10.5, pal.accent.gamma_multiply(0.26));
    }
    if fork_point {
        p.circle_stroke(c, 8.5, Stroke::new(1.4, pal.accent_b.gamma_multiply(0.8)));
    }
    if row.on_path {
        p.circle_filled(c, 6.2, base);
        p.circle_filled(c, 4.6, pal.accent);
        if row.is_leaf {
            p.circle_filled(c, 2.2, Color32::WHITE);
        }
    } else {
        let col = lane_color(pal, row.lane);
        p.circle_filled(c, 6.0, base);
        p.circle_filled(c, 4.4, col);
        p.circle_filled(c, 2.1, base);
    }
}

struct ContentCtx<'a> {
    tree: &'a SessionTree,
    layout: &'a Layout,
    input: &'a TreeInput<'a>,
    geo: &'a Geometry,
    tokens: &'a [String],
    show_actions: bool,
}

/// Glyph, text, pills, age or action buttons of one row.
fn paint_content(
    ui: &mut Ui,
    p: &Painter,
    pal: &Palette,
    cx: &ContentCtx<'_>,
    u: &RowUi,
    gc: &mut GalleyCache,
) -> Option<RowCommand> {
    let row = &cx.layout.rows[u.row];
    let node = &cx.tree.nodes[row.node];
    let cy = u.rect.center().y;
    let off_path = !row.on_path;
    let class_col = class_color(pal, row.class);
    let tone = |c: Color32| {
        if off_path {
            theme::mix(c, pal.weak, 0.3)
        } else {
            c
        }
    };

    // Role glyph.
    let gx = cx.geo.text_x() + 11.0;
    p.circle_filled(
        pos2(gx, cy),
        11.0,
        class_col.gamma_multiply(if off_path { 0.12 } else { 0.2 }),
    );
    paint_icon(
        p,
        pos2(gx, cy),
        Align2::CENTER_CENTER,
        class_icon(row.class),
        14.0,
        tone(class_col),
    );
    let text_left = gx + 11.0 + 10.0;

    // Right column and pills.
    let right = u.rect.right() - 18.0;
    let actions_left = right - actions_w(tree_layout::is_ask_result(node));
    let mut pill_right = actions_left - 6.0;
    let mut pills_w = 0.0;
    if row.is_leaf {
        let w = pill(
            p,
            pal,
            pill_right,
            cy,
            "you are here",
            Some(ICON_CROSSHAIR),
            PillStyle::Solid,
        );
        pill_right -= w + 6.0;
        pills_w += w + 6.0;
    }
    if row.children > 1 {
        let txt = format!("{} branches", row.children);
        let w = pill(
            p,
            pal,
            pill_right,
            cy,
            &txt,
            Some(ICON_LIST_TREE),
            PillStyle::Soft(if theme::flat() {
                pal.text
            } else {
                pal.accent_b
            }),
        );
        pill_right -= w + 6.0;
        pills_w += w + 6.0;
    }
    if let Some(l) = cx.tree.labels.get(&node.id) {
        let txt = clip(l, 24);
        let w = pill(
            p,
            pal,
            pill_right,
            cy,
            &txt,
            Some(ICON_BOOKMARK),
            PillStyle::Soft(pal.warn),
        );
        pills_w += w + 6.0;
    }

    // Text.
    let max_w = (actions_left - 8.0 - pills_w - text_left).max(40.0);
    let (font, color) = match row.class {
        RowClass::User => (crate::theme::font(14.0), pal.text_strong),
        RowClass::Assistant => (crate::theme::font(13.5), pal.text),
        RowClass::Tool => (FontId::monospace(12.0), pal.weak),
        RowClass::Compaction => (crate::theme::font(13.0), pal.warn),
        RowClass::Summary => (crate::theme::font(13.0), pal.ok),
        RowClass::Other => (crate::theme::font(13.0), pal.weak),
    };
    let color = if row.class == RowClass::Assistant && node.text.starts_with('(') {
        pal.weak
    } else {
        tone(color)
    };
    let shown = if node.text.is_empty() {
        tree_layout::kind_name(&node.kind)
    } else {
        &node.text
    };
    let g = gc.row((row.node, row.on_path, max_w.to_bits()), || {
        let spans = if cx.tokens.is_empty() {
            Vec::new()
        } else {
            tree_layout::highlight_spans(&node.text, cx.tokens)
        };
        p.layout_job(text_job(shown, font, color, pal, &spans, max_w))
    });
    p.galley(pos2(text_left, cy - 8.0 - g.size().y / 2.0), g, color);

    // Recorded cost before this entry: its own badge (amber with `≥` when incomplete).
    let hue = tone(
        if node.cost_before.incomplete {
            Hue::Amber
        } else {
            Hue::Green
        }
        .color(pal),
    );
    let amount = cost_amount(node.cost_before);
    Badge::new(
        p,
        Some(ICON_COINS),
        &[("before", tone(pal.weak)), (&amount, hue)],
        hue,
        10.5,
    )
    .paint_at(p, text_left, cy + 10.0);

    // Age, or the hover actions in its place.
    if cx.show_actions {
        return actions(ui, p, pal, cx, u, right);
    }
    if let Some(ts) = row.ts {
        let age = tree_layout::age(cx.input.now, ts);
        p.text(
            pos2(right, cy),
            Align2::RIGHT_CENTER,
            age,
            crate::theme::font(12.0),
            pal.weak,
        );
    }
    None
}

/// `$1.23`, or `≥$1.23` when some earlier usage could not be priced.
fn cost_amount(cost: RecordedCost) -> String {
    format!(
        "{}{}",
        if cost.incomplete { "≥" } else { "" },
        deck_service::statusfmt::fmt_cost(cost.total),
    )
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn text_job(
    text: &str,
    font: FontId,
    color: Color32,
    pal: &Palette,
    spans: &[Range<usize>],
    max_w: f32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let fmt = |c: Color32, bg: Color32| TextFormat {
        font_id: font.clone(),
        color: c,
        background: bg,
        ..TextFormat::default()
    };
    let mut at = 0;
    for s in spans {
        if s.start > at {
            job.append(&text[at..s.start], 0.0, fmt(color, Color32::TRANSPARENT));
        }
        job.append(
            &text[s.clone()],
            0.0,
            fmt(pal.text_strong, pal.accent.gamma_multiply(0.38)),
        );
        at = s.end;
    }
    if at < text.len() {
        job.append(&text[at..], 0.0, fmt(color, Color32::TRANSPARENT));
    }
    job.wrap.max_width = max_w;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job
}

#[derive(Clone, Copy)]
enum PillStyle {
    Solid,
    Soft(Color32),
}

/// Right-aligned pill ending at `right`; returns its width.
fn pill(
    p: &Painter,
    pal: &Palette,
    right: f32,
    cy: f32,
    text: &str,
    icon: Option<Icon>,
    style: PillStyle,
) -> f32 {
    let fg = match style {
        PillStyle::Solid => theme::on_accent(pal),
        PillStyle::Soft(c) => c,
    };
    let g = p.layout_no_wrap(text.to_owned(), crate::theme::font(11.5), fg);
    let icon_w = if icon.is_some() { 16.0 } else { 0.0 };
    let w = g.size().x + 20.0 + icon_w;
    let rect = Rect::from_min_size(pos2(right - w, cy - 10.0), vec2(w, 20.0));
    match style {
        PillStyle::Solid => theme::gradient_rounded(p, rect, 10.0, pal.accent_a, pal.accent_b),
        PillStyle::Soft(c) => {
            p.rect_filled(rect, 10, c.gamma_multiply(0.15));
            p.rect_stroke(
                rect,
                10,
                Stroke::new(1.0, c.gamma_multiply(0.45)),
                StrokeKind::Inside,
            );
        }
    }
    let mut x = rect.left() + 9.0;
    if let Some(i) = icon {
        paint_icon(p, pos2(x + 6.0, cy), Align2::CENTER_CENTER, i, 13.0, fg);
        x += 16.0;
    }
    p.galley(pos2(x, cy - g.size().y / 2.0), g, fg);
    w
}

/// Label / re-answer / Fork / Go here buttons of the active row.
fn actions(
    ui: &mut Ui,
    p: &Painter,
    pal: &Palette,
    cx: &ContentCtx<'_>,
    u: &RowUi,
    right: f32,
) -> Option<RowCommand> {
    let row = &cx.layout.rows[u.row];
    let node = &cx.tree.nodes[row.node];
    let cy = u.rect.center().y;
    let fork_off = fork_blocked(cx.tree, cx.layout, cx.input, u.row);
    let go_off = go_blocked(cx.layout, cx.input, u.row);
    let go_tip = if matches!(node.kind, NodeKind::User) {
        "Go to just before this prompt; its text goes back to the editor"
    } else {
        "Continue from here in this tab"
    };
    let label_tip = if cx.tree.labels.contains_key(&node.id) {
        "Edit this entry's label (L)"
    } else {
        "Add a label to this entry (L)"
    };
    let mut cmd = None;
    let mut x = right;
    x -= action_button(
        ui,
        p,
        pal,
        (x, cy),
        ("Go here", ICON_ARROW_RIGHT),
        (go_off, go_tip),
        u,
        "go",
        &mut cmd,
        RowCommand::Go,
    ) + 6.0;
    x -= action_button(
        ui,
        p,
        pal,
        (x, cy),
        ("Fork", ICON_GIT_FORK),
        (fork_off, tree_layout::fork_hint(&node.kind)),
        u,
        "fork",
        &mut cmd,
        RowCommand::Fork,
    ) + 6.0;
    if tree_layout::is_ask_result(node) {
        x -= action_button(
            ui,
            p,
            pal,
            (x, cy),
            ("", ICON_MESSAGES_SQUARE),
            (
                reanswer_blocked(cx.tree, cx.layout, cx.input, u.row),
                "Re-answer: omp asks the question again in the tab and continues from your new answer",
            ),
            u,
            "reanswer",
            &mut cmd,
            RowCommand::Reanswer,
        ) + 6.0;
    }
    action_button(
        ui,
        p,
        pal,
        (x, cy),
        ("", ICON_BOOKMARK),
        (cx.input.label.err(), label_tip),
        u,
        "label",
        &mut cmd,
        RowCommand::Label,
    );
    cmd
}

#[allow(clippy::too_many_arguments)]
fn action_button(
    ui: &mut Ui,
    p: &Painter,
    pal: &Palette,
    (right, cy): (f32, f32),
    (label, icon): (&str, Icon),
    (off, tip): (Option<&str>, &str),
    u: &RowUi,
    id: &str,
    cmd: &mut Option<RowCommand>,
    on_click: RowCommand,
) -> f32 {
    let enabled = off.is_none();
    let g = p.layout_no_wrap(label.to_owned(), crate::theme::font(12.5), pal.text_strong);
    let w = if label.is_empty() {
        30.0
    } else {
        g.size().x + 40.0
    };
    let rect = Rect::from_min_size(pos2(right - w, cy - 13.0), vec2(w, 26.0));
    let resp = ui.interact(
        rect,
        Id::unique("tree-act").with((u.row, id)),
        // Sensed even when unavailable, so the button (not the row under it) owns the hover and
        // shows why; the click is ignored below.
        Sense::click(),
    );
    let hov = resp.hovered() && enabled;
    let fg = if enabled {
        theme::mix(pal.text, pal.text_strong, f32::from(u8::from(hov)))
    } else {
        pal.weak.gamma_multiply(0.55)
    };
    if enabled {
        p.rect_filled(
            rect,
            13,
            if hov {
                theme::accent_fill(pal, 0.3)
            } else {
                pal.surface
            },
        );
        p.rect_stroke(
            rect,
            13,
            Stroke::new(1.0, if hov { pal.accent } else { pal.stroke }),
            StrokeKind::Inside,
        );
    } else {
        // Unavailable: no fill, faint outline; the reason shows on hover.
        p.rect_stroke(
            rect,
            13,
            Stroke::new(1.0, pal.stroke.gamma_multiply(0.5)),
            StrokeKind::Inside,
        );
    }
    paint_icon(
        p,
        pos2(rect.left() + 15.0, cy),
        Align2::CENTER_CENTER,
        icon,
        15.0,
        if enabled { pal.accent } else { fg },
    );
    p.galley(pos2(rect.left() + 28.0, cy - g.size().y / 2.0), g, fg);
    let resp = match off {
        Some(why) => resp.on_hover_text(why),
        None => resp
            .on_hover_text(tip)
            .on_hover_cursor(CursorIcon::PointingHand),
    };
    if resp.clicked() && enabled {
        *cmd = Some(on_click);
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gutter_compresses_wide_trees_only() {
        assert_eq!(gutter(1), (LANE_W, LANE_W));
        assert_eq!(gutter(7), (LANE_W, 7.0 * LANE_W));
        let (step, width) = gutter(14);
        assert!(step < LANE_W && (width - 7.0 * LANE_W).abs() < 0.01);
        assert!(gutter(400).0 >= 6.0);
    }

    #[test]
    fn selection_steps_clamp_at_both_ends() {
        assert_eq!(step_row(Some(3), 1, 5), Some(4));
        assert_eq!(step_row(Some(4), 1, 5), Some(4));
        assert_eq!(step_row(Some(1), -12, 5), Some(0));
        assert_eq!(step_row(Some(1), 12, 5), Some(4));
        assert_eq!(step_row(None, 1, 5), Some(0));
        assert_eq!(step_row(None, -1, 5), Some(4));
        assert_eq!(step_row(Some(0), 1, 0), None);
    }

    /// A chain of `n` alternating user / assistant nodes.
    fn chain(n: usize) -> SessionTree {
        let nodes: Vec<Arc<deck_core::tree::TreeNode>> = (0..n)
            .map(|i| {
                Arc::new(deck_core::tree::TreeNode {
                    id: format!("n{i}"),
                    parent: i.checked_sub(1).map(|p| format!("n{p}")),
                    kind: if i % 2 == 0 {
                        NodeKind::User
                    } else {
                        NodeKind::Assistant
                    },
                    timestamp: None,
                    text: String::new(),
                    has_text: true,
                    stop_reason: None,
                    active_leaf: None,
                    cost_before: Default::default(),
                    search_text: String::new(),
                    attribution: None,
                    user_request: i % 2 == 0,
                    tool_name: None,
                })
            })
            .collect();
        SessionTree {
            index: nodes
                .iter()
                .enumerate()
                .map(|(i, n)| (Arc::from(n.id.as_str()), i))
                .collect(),
            children: (0..n)
                .map(|i| Arc::new(if i + 1 < n { vec![i + 1] } else { Vec::new() }))
                .collect(),
            roots: vec![0],
            nodes,
            ..SessionTree::default()
        }
    }

    #[test]
    fn page_keys_move_by_the_visible_row_count_and_clamp() {
        let t = chain(30);
        let l = tree_layout::build(&t, Some(29), Filter::Conversation);
        let go = |cur, mv| move_target(&t, &l, Some(cur), mv, 7);
        assert_eq!(go(10, Move::Page(1)), Some(17));
        assert_eq!(go(10, Move::Page(-1)), Some(3));
        assert_eq!(go(25, Move::Page(1)), Some(29));
        assert_eq!(go(2, Move::Page(-1)), Some(0));
        assert_eq!(go(10, Move::Step(1)), Some(11));
        assert_eq!(go(10, Move::Home), Some(0));
        assert_eq!(go(10, Move::End), Some(29));
        // A list that has not been measured (page 0) still moves by one row.
        assert_eq!(move_target(&t, &l, Some(10), Move::Page(1), 0), Some(11));
        // Jumps need a selection.
        assert_eq!(
            move_target(&t, &l, None, Move::Turn { forward: true }, 7),
            None
        );
        assert_eq!(go(10, Move::Turn { forward: true }), Some(11));
        assert_eq!(go(10, Move::Branch { forward: true }), None);
    }

    #[test]
    fn going_to_the_current_leaf_says_so_and_a_blocked_session_says_why() {
        let t = chain(3);
        let l = tree_layout::build(&t, Some(2), Filter::Conversation);
        let input = |navigate| TreeInput {
            navigate,
            ..plain_input()
        };
        assert_eq!(
            go_blocked(&l, &input(Ok(())), 2),
            Some("Already at this point")
        );
        // The leaf here is a user prompt: omp still says "Already at this point" for it, and
        // switches only for another entry.
        assert!(matches!(t.nodes[l.rows[2].node].kind, NodeKind::User));
        assert_eq!(go_blocked(&l, &input(Ok(())), 1), None);
        assert_eq!(
            go_blocked(&l, &input(Err("Busy")), 2),
            Some("Busy"),
            "the session's own reason wins"
        );
    }

    fn plain_input<'a>() -> TreeInput<'a> {
        TreeInput {
            tree: None,
            revision: 1,
            error: None,
            live_leaf: None,
            navigate: Ok(()),
            fork: Ok(()),
            summary_prompt: false,
            label: Ok(()),
            reanswer: Ok(()),
            busy: None,
            now: 0,
        }
    }

    /// The window over a tree, driven frame by frame.
    struct Window {
        ctx: egui::Context,
        st: TreeState,
        tree: Arc<SessionTree>,
    }

    impl Window {
        fn new(tree: SessionTree) -> Window {
            let ctx = egui::Context::default();
            theme::apply(
                &ctx,
                deck_core::config::UiStyle::Neutral,
                deck_core::config::Density::Compact,
                UiFont::Auto,
                true,
            );
            let mut w = Window {
                ctx,
                st: TreeState::new("t".to_owned()),
                tree: Arc::new(tree),
            };
            w.frame(Vec::new(), |_| {});
            w
        }

        fn frame(
            &mut self,
            events: Vec<egui::Event>,
            tweak: impl FnOnce(&mut TreeInput<'_>),
        ) -> Vec<TreeAction> {
            let tree = Arc::clone(&self.tree);
            let mut input = plain_input();
            input.tree = Some(&tree);
            tweak(&mut input);
            let raw = egui::RawInput {
                events,
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 800.0))),
                ..Default::default()
            };
            let mut out = Vec::new();
            let st = &mut self.st;
            let mut full = self.ctx.run_ui(raw, |ui| {
                out = show(ui.ctx(), st, &input);
            });
            full.textures_delta.clear();
            out
        }

        fn key(
            &mut self,
            key: Key,
            modifiers: Modifiers,
            tweak: impl FnOnce(&mut TreeInput<'_>),
        ) -> Vec<TreeAction> {
            let event = egui::Event::Key {
                key,
                physical_key: Some(key),
                pressed: true,
                repeat: false,
                modifiers,
            };
            self.frame(vec![event], tweak)
        }

        /// Selects the first row (the tree has no leaf, so nothing is selected at first).
        fn select_first(&mut self) {
            self.key(Key::ArrowDown, Modifiers::NONE, |_| {});
        }
    }

    #[test]
    fn go_here_asks_about_a_summary_only_when_omp_does_and_shift_enter_skips_the_question() {
        use BranchSummary::*;
        assert_eq!(go_plan(false, false), GoPlan::Now(None));
        assert_eq!(go_plan(true, false), GoPlan::Ask);
        assert_eq!(go_plan(false, true), GoPlan::Now(Summarize));
        assert_eq!(go_plan(true, true), GoPlan::Now(Summarize));
    }

    #[test]
    fn the_summary_answer_maps_to_what_is_sent() {
        let mut d = SummaryDialog::new("e".to_owned(), false);
        assert_eq!(d.summary(), BranchSummary::None);
        d.choose(SummaryChoice::Summarize);
        assert_eq!(d.summary(), BranchSummary::Summarize);
        d.choose(SummaryChoice::Custom);
        assert!(
            d.focus_editor,
            "picking the custom prompt moves to its field"
        );
        // Blank instructions are omp's default prompt.
        d.instructions = "  \n ".to_owned();
        assert_eq!(d.summary(), BranchSummary::Summarize);
        d.instructions = "  focus on the tests \n".to_owned();
        assert_eq!(
            d.summary(),
            BranchSummary::Custom("focus on the tests".to_owned())
        );
        // The arrow keys stop at both ends.
        assert_eq!(SummaryChoice::None.step(-1), SummaryChoice::None);
        assert_eq!(SummaryChoice::None.step(1), SummaryChoice::Summarize);
        assert_eq!(SummaryChoice::Custom.step(1), SummaryChoice::Custom);
    }

    #[test]
    fn enter_goes_straight_away_and_the_summary_question_is_answered_in_its_dialog() {
        let mut w = Window::new(chain(3));
        w.select_first();
        let a = w.key(Key::Enter, Modifiers::NONE, |_| {});
        assert!(
            matches!(&a[..], [TreeAction::Navigate { entry, user: true, summary: BranchSummary::None }] if entry == "n0"),
            "{}",
            a.len()
        );

        // omp asks (setting on): nothing is sent until the dialog is answered.
        let on = |i: &mut TreeInput<'_>| i.summary_prompt = true;
        assert!(w.key(Key::Enter, Modifiers::NONE, on).is_empty());
        assert!(w.st.summary.is_some());
        // Down picks "Summarize"; Enter confirms it.
        assert!(w.key(Key::ArrowDown, Modifiers::NONE, on).is_empty());
        let a = w.key(Key::Enter, Modifiers::NONE, on);
        assert!(
            matches!(&a[..], [TreeAction::Navigate { entry, summary: BranchSummary::Summarize, .. }] if entry == "n0")
        );
        assert!(w.st.summary.is_none());
    }

    #[test]
    fn shift_enter_summarizes_without_asking_whatever_the_setting() {
        let mut w = Window::new(chain(3));
        w.select_first();
        for on in [false, true] {
            let a = w.key(Key::Enter, Modifiers::SHIFT, |i| i.summary_prompt = on);
            assert!(
                matches!(
                    &a[..],
                    [TreeAction::Navigate {
                        summary: BranchSummary::Summarize,
                        ..
                    }]
                ),
                "setting {on}"
            );
            assert!(w.st.summary.is_none());
        }
    }

    #[test]
    fn esc_in_the_summary_dialog_cancels_the_dialog_not_the_tree() {
        let mut w = Window::new(chain(3));
        w.select_first();
        let on = |i: &mut TreeInput<'_>| i.summary_prompt = true;
        w.key(Key::Enter, Modifiers::NONE, on);
        assert!(w.st.summary.is_some());
        let a = w.key(Key::Escape, Modifiers::NONE, on);
        assert!(a.is_empty(), "no Close, no Navigate");
        assert!(w.st.summary.is_none());
        // A second Esc is the tree's own: it closes.
        let a = w.key(Key::Escape, Modifiers::NONE, on);
        assert!(matches!(&a[..], [TreeAction::Close]));
    }

    #[test]
    fn the_label_editor_saves_a_trimmed_label_removes_on_blank_and_cancels_on_esc() {
        let mut t = chain(3);
        t.labels.insert("n0".to_owned(), "old".to_owned());
        let mut w = Window::new(t);
        w.select_first();
        let edit = |w: &mut Window, text: &str| {
            assert!(w.key(Key::L, Modifiers::NONE, |_| {}).is_empty());
            let e = w.st.label.as_mut().expect("the editor opens");
            text.clone_into(&mut e.text);
        };

        // Opens prefilled with the current label.
        assert!(w.key(Key::L, Modifiers::NONE, |_| {}).is_empty());
        let e = w.st.label.as_ref().expect("the editor opens");
        assert_eq!((e.entry.as_str(), e.text.as_str()), ("n0", "old"));
        w.key(Key::Escape, Modifiers::NONE, |_| {});
        assert!(w.st.label.is_none());

        edit(&mut w, "  new  ");
        let a = w.key(Key::Enter, Modifiers::NONE, |_| {});
        assert!(
            matches!(&a[..], [TreeAction::SetLabel { entry, label: Some(l) }] if entry == "n0" && l == "new")
        );
        assert!(w.st.label.is_none());

        // Blank removes.
        edit(&mut w, "   ");
        let a = w.key(Key::Enter, Modifiers::NONE, |_| {});
        assert!(matches!(&a[..], [TreeAction::SetLabel { entry, label: None }] if entry == "n0"));

        // Esc discards the typing and is not the tree's Esc.
        edit(&mut w, "discarded");
        let a = w.key(Key::Escape, Modifiers::NONE, |_| {});
        assert!(a.is_empty());
        assert!(w.st.label.is_none());

        // Saving what is already there sends nothing.
        assert!(w.key(Key::L, Modifiers::NONE, |_| {}).is_empty());
        assert!(w.key(Key::Enter, Modifiers::NONE, |_| {}).is_empty());
        assert!(w.st.label.is_none());
    }

    #[test]
    fn the_label_key_waits_for_an_empty_search_and_an_available_session() {
        let mut w = Window::new(chain(3));
        w.select_first();
        // omp types into its search; here a typed query keeps `L` out of the way.
        w.st.search = "n".to_owned();
        assert!(w.key(Key::L, Modifiers::NONE, |_| {}).is_empty());
        assert!(w.st.label.is_none());
        w.st.search.clear();
        // A refused gate says why instead of opening the editor.
        let a = w.key(Key::L, Modifiers::NONE, |i| {
            i.label = Err("Another tree action is still running");
        });
        assert!(
            matches!(&a[..], [TreeAction::Notice(t)] if t == "Another tree action is still running")
        );
        assert!(w.st.label.is_none());
    }

    #[test]
    fn label_edit_states() {
        let mut e = LabelEdit::new("x".to_owned(), None);
        assert_eq!(e.finish(), LabelEnd::Unchanged);
        e.text = "  ".to_owned();
        assert_eq!(
            e.finish(),
            LabelEnd::Unchanged,
            "blank over nothing is nothing"
        );
        e.text = " tag ".to_owned();
        assert_eq!(e.finish(), LabelEnd::Set(Some("tag".to_owned())));
        let mut e = LabelEdit::new("x".to_owned(), Some("tag"));
        e.text = "tag ".to_owned();
        assert_eq!(e.finish(), LabelEnd::Unchanged);
        e.text.clear();
        assert_eq!(e.finish(), LabelEnd::Set(None));
    }

    #[test]
    fn re_answer_is_offered_only_on_an_ask_result_and_needs_a_live_idle_tab() {
        let mut t = chain(3);
        {
            let n = Arc::make_mut(&mut t.nodes[1]);
            n.kind = NodeKind::ToolResult;
            n.text = "[ask: {\"questions\":[…]}]".to_owned();
            n.tool_name = Some("ask".to_owned());
        }
        let l = tree_layout::build(&t, Some(2), Filter::All);
        let live = plain_input();
        // Row 0 is a prompt, row 1 the ask result.
        assert!(reanswer_blocked(&t, &l, &live, 0).is_some());
        assert_eq!(reanswer_blocked(&t, &l, &live, 1), None);
        let mut off = plain_input();
        off.reanswer = Err("omp is busy — wait for it to finish");
        assert_eq!(
            reanswer_blocked(&t, &l, &off, 1),
            Some("omp is busy — wait for it to finish")
        );

        // The action carries the entry id, or the reason.
        let mut w = Window::new(t);
        w.st.filter = Filter::All;
        w.frame(Vec::new(), |_| {});
        let tree = Arc::clone(&w.tree);
        let mut out = Vec::new();
        reanswer_here(&w.st, &tree, &live, 1, &mut out);
        assert!(matches!(&out[..], [TreeAction::Reanswer { entry }] if entry == "n1"));
        out.clear();
        reanswer_here(&w.st, &tree, &off, 1, &mut out);
        assert!(matches!(&out[..], [TreeAction::Notice(_)]));
        // The whole list is shown: row 1 is the ask result, row 0 is not.
        out.clear();
        reanswer_here(&w.st, &tree, &live, 0, &mut out);
        assert!(matches!(&out[..], [TreeAction::Notice(_)]));
    }

    #[test]
    fn the_nearest_match_is_the_selection_itself_then_the_next_one_then_the_first() {
        let rows = [false, true, false, true, false];
        assert_eq!(nearest_match(&rows, Some(1)), Some(1));
        assert_eq!(nearest_match(&rows, Some(2)), Some(3));
        assert_eq!(
            nearest_match(&rows, Some(4)),
            Some(1),
            "nothing after it: the first"
        );
        assert_eq!(nearest_match(&rows, None), Some(1));
        assert_eq!(nearest_match(&[false, false], Some(0)), None);
    }

    #[test]
    fn moves_skip_the_rows_a_query_excludes_and_stop_at_the_ends() {
        let rows = [false, true, false, true, true, false];
        let go = |cur, mv| match_target(&rows, cur, mv, 2);
        assert_eq!(go(Some(1), Move::Step(1)), Some(3));
        assert_eq!(go(Some(3), Move::Step(1)), Some(4));
        assert_eq!(go(Some(4), Move::Step(1)), Some(4), "no match below");
        assert_eq!(go(Some(3), Move::Step(-1)), Some(1));
        assert_eq!(go(Some(1), Move::Step(-1)), Some(1), "no match above");
        // From an excluded row the first step lands on the neighbouring match.
        assert_eq!(go(Some(2), Move::Step(1)), Some(3));
        assert_eq!(go(Some(2), Move::Step(-1)), Some(1));
        assert_eq!(go(Some(0), Move::Step(-1)), Some(1));
        // A page walks that many matches; the ends are the first and last match.
        assert_eq!(go(Some(1), Move::Page(1)), Some(4));
        assert_eq!(go(Some(4), Move::Page(-1)), Some(1));
        assert_eq!(go(Some(3), Move::Home), Some(1));
        assert_eq!(go(Some(3), Move::End), Some(4));
        assert_eq!(go(None, Move::Step(1)), Some(1));
        assert_eq!(go(None, Move::Step(-1)), Some(4));
        // Jumps are not list walks; nothing matching means no move at all.
        assert_eq!(go(Some(1), Move::Turn { forward: true }), None);
        assert_eq!(
            match_target(&[false, false], Some(0), Move::Step(1), 2),
            None
        );
    }

    #[test]
    fn a_query_confines_selection_and_go_here_to_its_matches() {
        let mut t = chain(6);
        for (i, n) in t.nodes.iter_mut().enumerate() {
            Arc::make_mut(n).search_text = if i % 2 == 1 { "alpha" } else { "zeta" }.to_owned();
        }
        let mut w = Window::new(t);
        w.select_first();
        assert_eq!(w.st.selected.as_deref(), Some("n0"));
        let nothing = |_: &mut TreeInput<'_>| {};

        // Typing a query that excludes the selection moves it to the first match after it.
        w.st.search = "alpha".to_owned();
        w.frame(Vec::new(), nothing);
        assert_eq!(w.st.selected.as_deref(), Some("n1"));

        // Up / Down only visit matches.
        w.key(Key::ArrowDown, Modifiers::NONE, nothing);
        assert_eq!(w.st.selected.as_deref(), Some("n3"));
        w.key(Key::ArrowUp, Modifiers::NONE, nothing);
        w.key(Key::ArrowUp, Modifiers::NONE, nothing);
        assert_eq!(w.st.selected.as_deref(), Some("n1"));

        // Enter acts on the match.
        let a = w.key(Key::Enter, Modifiers::NONE, nothing);
        assert!(matches!(&a[..], [TreeAction::Navigate { entry, .. }] if entry == "n1"));

        // A row the query excludes (a click on a dimmed row) is not a place to go.
        w.st.selected = Some("n2".to_owned());
        assert!(w.key(Key::Enter, Modifiers::NONE, nothing).is_empty());
        assert!(w.key(Key::Enter, Modifiers::SHIFT, nothing).is_empty());
        assert_eq!(
            w.st.selected.as_deref(),
            Some("n2"),
            "the click's selection stays"
        );

        // A new query that excludes the selection moves it on, forward first.
        w.st.selected = Some("n3".to_owned());
        w.st.search = "zeta".to_owned();
        w.frame(Vec::new(), nothing);
        assert_eq!(w.st.selected.as_deref(), Some("n4"));

        // Nothing matches: the selection stays and Enter does nothing.
        w.st.search = "qqqq".to_owned();
        w.frame(Vec::new(), nothing);
        assert_eq!(w.st.selected.as_deref(), Some("n4"));
        assert!(w.key(Key::Enter, Modifiers::NONE, nothing).is_empty());
        assert_eq!(w.st.selected.as_deref(), Some("n4"));

        // Without a query every row is a place to go again.
        w.st.search.clear();
        w.frame(Vec::new(), nothing);
        let a = w.key(Key::Enter, Modifiers::NONE, nothing);
        assert!(matches!(&a[..], [TreeAction::Navigate { entry, .. }] if entry == "n4"));
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_000), "1,000");
        assert_eq!(fmt_count(22_691), "22,691");
        assert_eq!(fmt_count(1_234_567), "1,234,567");
    }

    #[test]
    fn long_labels_are_clipped_with_an_ellipsis() {
        assert_eq!(clip("short", 24), "short");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
    }
}
