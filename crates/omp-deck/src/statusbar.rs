//! The bottom status bar: what omp's own status line shows for the active session (activity,
//! model + thinking, path, git, context meter, tokens, cost) plus the session action buttons.
//!
//! Strings are formatted once per status snapshot ([`BarView::update`]); drawing only paints.

use std::path::Path;
use std::sync::Arc;

use crate::chat_view::{Ink, hairline, pop};
use crate::icons::{
    ICON_ACTIVITY, ICON_ARROW_DOWN, ICON_ARROW_UP, ICON_BRAIN, ICON_CHART_PIE, ICON_CHEVRON_DOWN,
    ICON_CIRCLE_ALERT, ICON_CIRCLE_CHECK, ICON_CIRCLE_STOP, ICON_COPY, ICON_CPU, ICON_DOWNLOAD,
    ICON_ELLIPSIS, ICON_FOLD_VERTICAL, ICON_FOLDER, ICON_FOLDER_GIT_2, ICON_GAUGE, ICON_GIT_BRANCH,
    ICON_GIT_FORK, ICON_GIT_MERGE, ICON_HANDSHAKE, ICON_HISTORY, ICON_LIST_TREE, ICON_MAP,
    ICON_NETWORK, ICON_PENCIL, ICON_ROTATE_CCW, ICON_SPARKLES, ICON_SQUARE, ICON_VIBRATE,
};
use deck_core::ctl::{ContextBreakdown, LiveAccount, LiveModel};
use deck_core::omp::ModelInfo;
use deck_core::sessions::abbreviate_home;
use deck_core::status::{GitInfo, SessionStats, SessionStatus};
use egui::{
    Align, Align2, Color32, CursorIcon, Frame, Layout, Margin, Painter, Pos2, Rect, Response,
    RichText, Sense, Shape, Stroke, Ui, UiBuilder, pos2, vec2,
};

use crate::agent_model::Counts;
use crate::badge::{self, Badge, Hue};
use crate::cost_tip::{self, CostTip};
use crate::ctx_tip::{self, CtxTip};
use crate::usage_state::{CardView, Chip, PillKind, QuotaPill, Summary};
use crate::usage_view;
use crate::{card, tips};
use deck_core::usage::LimitStatus;

use crate::model_picker::{
    self, ModelsCache, PickerAction, PickerInput, PickerState, SessionExtras,
};
use crate::theme::{self, Palette};
use crate::widgets::{icon_text, menu_item, paint_icon};
use deck_service::gateway::protocol::SwitchInfo;
use deck_service::session_actions::{
    self, Action, ActionCtx, Availability, CompactMode, Kind, ShakeMode,
};
use deck_service::statusfmt::{
    self, Activity, ContextLevel, context_label, context_level, context_percent, fmt_cost,
    fmt_tokens, thinking_fill, thinking_short,
};

/// Height of the bar strip. The chat view's composer row sits on 12 px labels; the bar is a
/// full-width strip read from further away, so its chips run at 13 px (≈1.08×, was 14.4 = 1.2×)
/// inside 26 px pills (was 29) on a 40 px strip (was 41).
pub const BAR_H: f32 = 40.0;
const CHIP_H: f32 = 26.0;
/// The path chip never shrinks below its last component, ellipsized to this many characters.
const PATH_MIN_CHARS: usize = 20;
/// The path's available width is rounded down to a multiple of this (px) before it is elided, so
/// dragging the sidebar does not re-measure the path every pixel.
const PATH_STEP: f32 = 8.0;
const CHIP_PAD: f32 = 8.0;
/// Horizontal space a chip adds around its content: both pads plus the one item gap measured
/// on screen (so a chip asked to be N px wide really is N px wide).
const CHIP_CHROME_W: f32 = 2.0 * CHIP_PAD + 6.0;
/// Text size of chip content, of the secondary figures (git counts, effort label, PR tag) and of
/// icons, in px.
const TEXT_PX: f32 = 13.0;
const SMALL_PX: f32 = 12.0;
const ICON_PX: f32 = 15.0;
/// The model chip's width in every session (was 213 × 1.3 = 277); it does not follow the path
/// chip, so a long path (a worktree) cannot push the other segments out.
const MODEL_CHIP_W: f32 = 260.0;
/// The model chip never gets narrower than this, however narrow the bar (was 195).
const MODEL_CHIP_MIN: f32 = 176.0;
/// Share of the bar the model chip may take at most.
const MODEL_CHIP_SHARE: f32 = 0.42;
const PIE_BOX: f32 = 16.0;
const CARET_W: f32 = 14.0;
/// Side padding of the strip, and width of one icon button of the action group.
const BAR_PAD_X: i8 = 12;
const BTN_W: f32 = 28.0;

// ------------------------------------------------------------------ view (cached strings)

pub struct GitView {
    pub branch: String,
    pub dirty: bool,
    /// `↑2 ↓1`
    pub sync: String,
    pub unstaged: String,
    pub staged: String,
    pub untracked: String,
    /// The counts behind the chip, for its hover card.
    pub counts: GitCounts,
    pub pr: Option<(String, String)>,
    /// The cwd is a linked worktree of this primary checkout.
    pub worktree_of: Option<std::path::PathBuf>,
}

/// Upstream distance and working-tree counts of the git chip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GitCounts {
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
}

/// The model chip's hover card, formatted once per rebuild.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelTip {
    /// Taken from omp's settings: the session has not used a model yet.
    pub is_default: bool,
    /// Catalog name, else the selector, else `None` (nothing recorded yet).
    pub title: Option<String>,
    pub selector: Option<String>,
    /// `high`, `auto · high`, `auto`.
    pub thinking: Option<String>,
    /// Context window, `1M`.
    pub window: Option<String>,
}

/// The tokens chip's hover card: `group_digits` of input, output, cache read, cache write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokensTip {
    pub input: String,
    pub output: String,
    pub cache_read: String,
    pub cache_write: String,
}

/// Everything the bar shows, formatted. Rebuilt when the snapshot or the model catalog changes.
#[derive(Default)]
pub struct BarView {
    defaults: Defaults,
    /// omp's in-memory model state (live control), used while the file records no model.
    live_model: Option<LiveModel>,
    /// The OAuth account the session uses for its model's provider (live control): the usage
    /// chip and its hover show that account's quota.
    account: Option<LiveAccount>,
    snap: Option<Arc<SessionStatus>>,
    catalog_gen: u64,
    built: bool,
    /// Bumps on every rebuild (invalidates cached segment widths).
    pub rev: u64,

    pub model_name: String,
    pub model_selector: Option<String>,
    /// `model_selector`/`level` come from omp's settings (neither the session file nor live
    /// control report a model yet): shown dimmed with a tooltip, replaced by the first real one.
    pub model_is_default: bool,
    /// Effective thinking level (None = the model has none / unknown).
    pub level: Option<String>,
    pub configured: Option<String>,
    pub model_tip: ModelTip,

    /// The working directory, `~`-abbreviated and whole: the chip elides it to the room it gets.
    pub path_text: String,
    pub path_full: String,

    pub git: Option<GitView>,

    pub ctx_label: String,
    pub ctx_frac: Option<f32>,
    pub ctx_level: Option<ContextLevel>,
    pub ctx_tip: String,
    /// The live `/context` breakdown (keyed by what it was computed from), formatted once.
    ctx_detail: Option<((i64, u64, u64), Arc<CtxTip>)>,
    /// The jsonl view of the context (last reply's prompt tokens, model window): the chip falls
    /// back to it when no live breakdown exists.
    ctx_jsonl: (Option<u64>, Option<u64>),
    /// Quota summary behind the usage chip; `None` hides the chip.
    usage: Option<Arc<Summary>>,

    pub tokens_in: String,
    pub tokens_out: String,
    pub tokens_tip: TokensTip,
    pub show_tokens: bool,
    pub cost: String,
    pub cost_tip: CostTip,
    pub show_cost: bool,
    pub last_turn_failed: bool,
    pub has_reply: bool,
}

/// omp's configured default model and thinking level (`modelRoles.default`,
/// `defaultThinkingLevel`), shown for a fresh session that has no `model_change` entry yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Defaults {
    pub model: Option<String>,
    pub level: Option<String>,
}

/// The session's model and thinking level: the file's once it records a model, else omp's live
/// in-memory state (omp persists a session only with its first message, so model and thinking
/// changes before it never reach the file).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelState<'a> {
    pub model: Option<&'a str>,
    pub thinking: Option<&'a str>,
    pub configured: Option<&'a str>,
}

pub fn model_state<'a>(stats: &'a SessionStats, live: Option<&'a LiveModel>) -> ModelState<'a> {
    match live.filter(|_| stats.model.is_none()) {
        Some(l) => ModelState {
            model: Some(&l.selector),
            thinking: Some(&l.thinking),
            configured: l.thinking_configured.as_deref(),
        },
        None => ModelState {
            model: stats.model.as_deref(),
            thinking: stats.thinking.as_deref(),
            configured: stats.thinking_configured.as_deref(),
        },
    }
}

impl BarView {
    /// New defaults invalidate the built view (rebuilt by the next [`BarView::update`]).
    pub fn set_defaults(&mut self, defaults: &Defaults) {
        if self.defaults != *defaults {
            self.defaults = defaults.clone();
            self.built = false;
        }
    }

    /// A changed live model state invalidates the built view, like [`BarView::set_defaults`].
    pub fn set_live_model(&mut self, live: Option<&LiveModel>) {
        if self.live_model.as_ref() != live {
            self.live_model = live.cloned();
            self.built = false;
        }
    }

    /// A changed session account changes the usage chip's pills (and width): bumps `rev`.
    pub fn set_account(&mut self, account: Option<&LiveAccount>) {
        if self.account.as_ref() != account {
            self.account = account.cloned();
            self.rev += 1;
        }
    }

    /// The usage chip's data; a new summary changes the chip's width, so it bumps `rev`.
    pub fn set_usage(&mut self, summary: Option<&Arc<Summary>>) {
        let same = match (&self.usage, summary) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.usage = summary.cloned();
            self.rev += 1;
        }
    }

    /// The live context breakdown behind the context tooltip (`None`: no live control, or an
    /// older extension that does not send one).
    pub fn set_context(&mut self, breakdown: Option<&ContextBreakdown>) {
        let Some(b) = breakdown else {
            if self.ctx_detail.take().is_some() {
                self.apply_ctx();
            }
            return;
        };
        let key = (b.computed_at_ms, b.used_tokens, b.context_window);
        if self.ctx_detail.as_ref().is_none_or(|(k, _)| *k != key) {
            self.ctx_detail = Some((key, Arc::new(CtxTip::build(b))));
            self.apply_ctx();
        }
    }

    /// Chip label/fraction/level: the live breakdown's numbers when present (so chip and tooltip
    /// agree), else the jsonl ones.
    fn apply_ctx(&mut self) {
        let (tokens, window) =
            chip_numbers(self.ctx_detail.as_ref().map(|(k, _)| *k), self.ctx_jsonl);
        self.ctx_label = context_label(tokens, window);
        let pct = context_percent(tokens, window);
        self.ctx_frac = pct.map(|p| (p / 100.0).clamp(0.0, 1.0) as f32);
        self.ctx_level = pct.zip(window).map(|(p, w)| context_level(p, w));
        self.ctx_tip = context_tooltip(tokens, window, pct);
        self.rev += 1;
    }

    /// Rebuild when the snapshot (by identity) or the catalog changed; true when it did.
    pub fn update(
        &mut self,
        snap: Option<&Arc<SessionStatus>>,
        fallback_cwd: &Path,
        catalog: &ModelsCache,
    ) -> bool {
        let same_snap = match (&self.snap, snap) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if self.built && same_snap && self.catalog_gen == catalog.generation {
            return false;
        }
        self.snap = snap.cloned();
        self.rev += 1;
        self.catalog_gen = catalog.generation;
        self.built = true;
        match snap {
            Some(s) => self.build(s, catalog),
            None => self.build(
                &SessionStatus {
                    cwd: fallback_cwd.to_path_buf(),
                    ..SessionStatus::default()
                },
                catalog,
            ),
        }
        true
    }

    fn build(&mut self, s: &SessionStatus, catalog: &ModelsCache) {
        let st = &s.stats;
        let state = model_state(st, self.live_model.as_ref());
        self.model_is_default = state.model.is_none() && self.defaults.model.is_some();
        let selector = if self.model_is_default {
            self.defaults.model.clone()
        } else {
            state.model.map(str::to_owned)
        };
        let info: Option<&ModelInfo> = selector.as_deref().and_then(|m| catalog.get(m));
        self.model_selector = selector.clone();
        self.model_name = match (info, selector.as_deref()) {
            (Some(i), _) => i.name.clone(),
            (None, Some(sel)) => sel.rsplit('/').next().unwrap_or(sel).to_owned(),
            (None, None) => String::new(),
        };
        if self.model_is_default {
            // A model without thinking levels ignores the default level.
            let level = self
                .defaults
                .level
                .clone()
                .filter(|_| info.is_none_or(|i| !i.thinking.is_empty()));
            self.level = level.clone();
            self.configured = level;
        } else {
            self.level = state.thinking.map(str::to_owned);
            self.configured = state.configured.map(str::to_owned);
        }
        self.model_tip = model_tip(
            selector.as_deref(),
            info,
            self.level.as_deref(),
            self.configured.as_deref(),
            self.model_is_default,
        );

        self.path_full = s.cwd.to_string_lossy().into_owned();
        self.path_text = abbreviate_home(&s.cwd);

        self.git = s.git.as_ref().map(git_view);

        let window = info.and_then(|i| i.context_window);
        self.ctx_jsonl = (st.context_tokens, window);
        self.apply_ctx();

        self.tokens_in = fmt_tokens(st.input_tokens);
        self.tokens_out = fmt_tokens(st.output_tokens);
        self.show_tokens = st.input_tokens + st.output_tokens + st.cache_read + st.cache_write > 0;
        self.tokens_tip = TokensTip {
            input: group_digits(st.input_tokens),
            output: group_digits(st.output_tokens),
            cache_read: group_digits(st.cache_read),
            cache_write: group_digits(st.cache_write),
        };
        self.cost = fmt_cost(st.cost);
        self.show_cost = st.cost > 0.0;
        self.last_turn_failed = st.last_turn_failed;
        self.has_reply = st.last_assistant_text.is_some();
        self.cost_tip = CostTip::build(st.cost, &st.cost_detail);
    }

    /// Selector + level the dropdown highlights.
    pub fn current_level(&self) -> Option<&str> {
        self.configured.as_deref().or(self.level.as_deref())
    }
}

fn git_view(g: &GitInfo) -> GitView {
    let sync = match (g.ahead, g.behind) {
        (0, 0) => String::new(),
        (a, 0) => format!("↑{a}"),
        (0, b) => format!("↓{b}"),
        (a, b) => format!("↑{a} ↓{b}"),
    };
    let count = |prefix: char, n: u32| {
        if n > 0 {
            format!("{prefix}{n}")
        } else {
            String::new()
        }
    };
    GitView {
        branch: g.branch.clone(),
        dirty: g.staged + g.unstaged + g.untracked > 0,
        sync,
        unstaged: count('*', g.unstaged),
        staged: count('+', g.staged),
        untracked: count('?', g.untracked),
        counts: GitCounts {
            ahead: g.ahead,
            behind: g.behind,
            staged: g.staged,
            unstaged: g.unstaged,
            untracked: g.untracked,
        },
        pr: g
            .pr
            .as_ref()
            .map(|p| (format!("#{}", p.number), p.url.clone())),
        worktree_of: g.worktree_of.clone(),
    }
}

/// `1234567` → `1,234,567`.
pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn model_tip(
    selector: Option<&str>,
    info: Option<&ModelInfo>,
    level: Option<&str>,
    configured: Option<&str>,
    is_default: bool,
) -> ModelTip {
    let thinking = match (configured, level) {
        (Some("auto"), Some(l)) => Some(format!("auto · {l}")),
        (Some("auto"), None) => Some("auto".to_owned()),
        (_, Some(l)) => Some(l.to_owned()),
        _ => None,
    };
    ModelTip {
        is_default,
        title: info
            .map(|i| i.name.clone())
            .or_else(|| selector.map(str::to_owned)),
        selector: selector.map(str::to_owned),
        thinking,
        window: info.and_then(|i| i.context_window).map(fmt_tokens),
    }
}

/// What the chip shows: the live breakdown's `(computed_at, used_tokens, context_window)` when
/// it has a window, else the jsonl numbers.
fn chip_numbers(
    live: Option<(i64, u64, u64)>,
    jsonl: (Option<u64>, Option<u64>),
) -> (Option<u64>, Option<u64>) {
    match live {
        Some((_, used, window)) if window > 0 => (Some(used), Some(window)),
        _ => jsonl,
    }
}

fn context_tooltip(tokens: Option<u64>, window: Option<u64>, pct: Option<f64>) -> String {
    match (tokens, window, pct) {
        (Some(t), Some(w), Some(p)) => format!(
            "Context window: {} of {} tokens ({p:.1}%)\nPrompt tokens of the last reply; omp also counts text added since",
            group_digits(t),
            group_digits(w)
        ),
        (Some(t), None, _) => format!(
            "Context: {} tokens\nThe model's window size is unknown",
            group_digits(t)
        ),
        (None, Some(w), _) => format!("Context window: {} tokens\nNo reply yet", group_digits(w)),
        _ => "Context usage appears after the first reply".to_owned(),
    }
}

// ------------------------------------------------------------------ actions out

#[derive(Debug, PartialEq, Eq)]
pub enum BarAction {
    Picker(PickerAction),
    /// Task mode of the model picker: set (`Some(provider/id)`) or clear (`None`) the session's
    /// task-agent model override.
    TaskModel(Option<String>),
    Run(Action),
    AskHandoff,
    AskCompactFocus,
    AskRename,
    CopyPath(String),
    OpenUrl(String),
    ToggleAgents,
    OpenTree,
    OpenUsage,
    ToggleChanges,
}

pub struct BarInput<'a> {
    pub catalog: &'a ModelsCache,
    pub favorites: &'a [String],
    pub provider: Option<&'a str>,
    pub activity: Activity,
    /// Optimistic switch awaiting confirmation.
    pub pending: Option<&'a SwitchInfo>,
    /// Last accepted switch of a session file that does not record models yet.
    pub assumed: Option<&'a SwitchInfo>,
    pub actions: ActionCtx,
    pub agents: AgentsChip,
    /// What the model picker needs of the active tab beyond its model (task agents, roles).
    pub session: SessionExtras<'a>,
}

/// What the status bar's "Agents" chip shows.
#[derive(Clone, Copy, Debug, Default)]
pub struct AgentsChip {
    pub counts: Counts,
    /// The agents dashboard is open.
    pub open: bool,
}

// ------------------------------------------------------------------ drawing

/// Draws the bar as a bottom panel of the space that is left. The app shows the sidebar first, so
/// the strip spans exactly the tab area (the sidebar keeps the window's full height). Returns
/// what the user asked for.
pub fn show(
    ui: &mut Ui,
    view: &BarView,
    picker: &mut PickerState,
    input: &BarInput<'_>,
) -> Vec<BarAction> {
    let ink = Ink::new(ui, TEXT_PX);
    let mut out = Vec::new();
    egui::Panel::bottom("status-bar")
        .resizable(false)
        .exact_size(BAR_H)
        .show_separator_line(false)
        .frame(
            Frame::NONE
                .fill(ink.p.bar)
                .inner_margin(Margin::symmetric(BAR_PAD_X, 0)),
        )
        .show(ui, |ui| {
            let full = ui.max_rect();
            paint_strip(
                ui.painter(),
                full.expand2(vec2(f32::from(BAR_PAD_X), 0.0)),
                &ink,
            );
            let right = ui.scope_builder(
                UiBuilder::new()
                    .max_rect(full)
                    .layout(Layout::right_to_left(Align::Center)),
                |ui| actions_row(ui, &ink, input, &mut out),
            );
            let left_max = (right.response.rect.left() - 8.0).max(full.left());
            let left_rect = Rect::from_min_max(full.min, pos2(left_max, full.max.y));
            ui.scope_builder(
                UiBuilder::new()
                    .max_rect(left_rect)
                    .layout(Layout::left_to_right(Align::Center)),
                |ui| {
                    ui.set_clip_rect(left_rect.intersect(ui.clip_rect()));
                    info_row(ui, &ink, view, picker, input, &mut out);
                },
            );
        });
    out
}

/// The strip behind the bar, in the composer card's language: its vertical glass gradient
/// (surface warmed toward the first accent at the top, toward the second at the bottom), the
/// style's region separator, the accent-pair hairline of the card's outline along the top edge
/// and the faint light edge just below it.
fn paint_strip(p: &Painter, rect: Rect, ink: &Ink) {
    let (k_top, k_bot) = if ink.dark { (0.07, 0.03) } else { (0.05, 0.02) };
    theme::gradient_rect(
        p,
        rect,
        theme::mix(ink.p.surface, ink.a, k_top),
        theme::mix(ink.p.surface, ink.b, k_bot),
        false,
    );
    theme::skin::paint_edge(p, rect, theme::skin::Edge::Top, ink.p);
    hairline(
        p,
        ink,
        rect.left(),
        rect.right(),
        rect.top(),
        if ink.dark { 0.75 } else { 0.6 },
    );
    let glass = Color32::from_white_alpha(if ink.dark { 26 } else { 90 });
    crate::deco::gradient_rule(
        p,
        pos2(rect.left() + 18.0, rect.top() + 1.5),
        pos2(rect.right() - 18.0, rect.top() + 1.5),
        1.0,
        &[
            (0.0, Color32::TRANSPARENT),
            (0.5, glass),
            (1.0, Color32::TRANSPARENT),
        ],
    );
}

/// The droppable segments of the info row in priority order (the discriminant is the index into
/// [`SEGS`] and [`SegWidths::w`]): the least important goes first when the bar narrows. Path comes
/// last: it keeps only its last folder when nothing else is dropped, and grows into the room the
/// rest leaves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seg {
    Ctx,
    Usage,
    Git,
    Cost,
    Tokens,
    Path,
}

const SEGS: [Seg; 6] = [
    Seg::Ctx,
    Seg::Usage,
    Seg::Git,
    Seg::Cost,
    Seg::Tokens,
    Seg::Path,
];

type SegKey = (
    u64,
    Option<usize>,
    u32,
    deck_core::config::UiFont,
    &'static str,
);

/// Real widths (the chip, without the spacing before it) of every segment for one view
/// revision, usage chip page and pixel density.
#[derive(Clone, Copy)]
struct SegWidths {
    key: SegKey,
    /// The path's entry is its smallest chip (last folder only).
    w: [f32; 6],
    /// What the path chip adds around its text, and the width of the whole path as text.
    path_chrome: f32,
    path_text_w: f32,
}

/// Lays `f` out once in an invisible child and returns the width it took.
fn measure(ui: &mut Ui, f: impl FnOnce(&mut Ui)) -> f32 {
    let r = ui.max_rect();
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt("sb-measure")
            .max_rect(Rect::from_min_size(r.min, vec2(4000.0, r.height())))
            .layout(Layout::left_to_right(Align::Center))
            .invisible(),
    );
    f(&mut child);
    child.min_rect().width()
}

/// Width of `s` as chip text.
fn text_width(ui: &Ui, s: &str) -> f32 {
    ui.painter()
        .layout_no_wrap(
            s.to_owned(),
            theme::label_font(TEXT_PX),
            Color32::PLACEHOLDER,
        )
        .size()
        .x
}

fn seg_widths(ui: &mut Ui, ink: &Ink, v: &BarView) -> SegWidths {
    let key = (
        v.rev,
        // The pager's pick lives in egui memory, outside `rev`.
        usage_chip_card(ui.ctx(), v).map(|(_, i)| i),
        ui.ctx().pixels_per_point().to_bits(),
        theme::ui_font(),
        // The label face (mono or the UI font) follows the style.
        theme::style().key(),
    );
    let id = ui.scope_id().with("seg-widths");
    if let Some(c) = ui.data(|d| d.get_temp::<SegWidths>(id))
        && c.key == key
    {
        return c;
    }
    let mut w = [0.0; 6];
    let mut sink = Vec::new();
    for (i, s) in SEGS.into_iter().enumerate() {
        w[i] = if s == Seg::Path {
            let min = elide_path(&v.path_text, |_| false);
            measure(ui, |ui| segment(ui, ink, v, s, &min, &mut sink))
        } else {
            measure(ui, |ui| segment(ui, ink, v, s, "", &mut sink))
        };
    }
    let path_text_w = text_width(ui, &v.path_text);
    let path_full = measure(ui, |ui| {
        segment(ui, ink, v, Seg::Path, &v.path_text, &mut sink);
    });
    let c = SegWidths {
        key,
        w,
        path_chrome: path_full - path_text_w,
        path_text_w,
    };
    ui.data_mut(|d| d.insert_temp(id, c));
    c
}

/// `path` shortened from the front until `fits`: the whole path, then `…/` and fewer and fewer
/// of its trailing folders, down to the last one (itself ellipsized to [`PATH_MIN_CHARS`]), which
/// is returned when nothing fits.
fn elide_path(path: &str, mut fits: impl FnMut(&str) -> bool) -> String {
    if fits(path) {
        return path.to_owned();
    }
    // A separator that starts a component (not the leading or the trailing one) is a cut point.
    let cuts = path.char_indices().filter_map(|(i, c)| {
        (i > 0 && matches!(c, '/' | '\\') && i + c.len_utf8() < path.len()).then_some(i)
    });
    let mut last = String::new();
    for i in cuts {
        last = format!("…{}", &path[i..]);
        if fits(&last) {
            return last;
        }
    }
    if last.is_empty() {
        last = path.to_owned();
    }
    statusfmt::ellipsize_left(&last, PATH_MIN_CHARS)
}

type PathKey = (u64, u32, u32, deck_core::config::UiFont, &'static str);

/// The path chip's text for a chip `chip_w` wide, elided once per view revision and width step
/// (the cache holds a shared string, so a frame that hits it allocates nothing).
fn path_text(ui: &Ui, v: &BarView, w: &SegWidths, chip_w: f32) -> Arc<str> {
    let room = chip_w - w.path_chrome;
    let step = if room >= w.path_text_w {
        u32::MAX
    } else {
        (room.max(0.0) / PATH_STEP) as u32
    };
    let key: PathKey = (
        v.rev,
        step,
        ui.ctx().pixels_per_point().to_bits(),
        theme::ui_font(),
        theme::style().key(),
    );
    let id = ui.scope_id().with("path-text");
    if let Some((k, text)) = ui.data(|d| d.get_temp::<(PathKey, Arc<str>)>(id))
        && k == key
    {
        return text;
    }
    let limit = if step == u32::MAX {
        f32::INFINITY
    } else {
        step as f32 * PATH_STEP
    };
    let text: Arc<str> = elide_path(&v.path_text, |t| text_width(ui, t) <= limit).into();
    ui.data_mut(|d| d.insert_temp(id, (key, text.clone())));
    text
}

fn info_row(
    ui: &mut Ui,
    ink: &Ink,
    v: &BarView,
    picker: &mut PickerState,
    input: &BarInput<'_>,
    out: &mut Vec<BarAction>,
) {
    ui.spacing_mut().item_spacing.x = 6.0;

    activity_dot(ui, input.activity);
    ui.add_space(2.0);
    let widths = seg_widths(ui, ink, v);
    let gap = ui.spacing().item_spacing.x;
    let present = SEGS.map(|s| match s {
        Seg::Git => v.git.is_some(),
        Seg::Cost => v.show_cost,
        Seg::Tokens => v.show_tokens,
        Seg::Usage => v.usage.as_ref().is_some_and(|u| u.chip.is_some()),
        Seg::Ctx | Seg::Path => true,
    });
    // The model chip has one width in every session, but never takes over the bar: it yields
    // down to its minimum for the context meter and the usage chip.
    let keep: f32 = [Seg::Ctx, Seg::Usage]
        .into_iter()
        .filter(|s| present[*s as usize])
        .map(|s| widths.w[s as usize] + gap)
        .sum();
    let (chip_w, cap) = model_chip_width(ui.available_width(), keep + gap);
    model_chip(ui, ink, v, picker, input, (chip_w, cap), out);

    // Priority-based fit on measured widths: drop the least important segments first.
    let budget = ui.available_width() - gap;
    let shown = fit_segments(&widths.w, &present, budget, gap);
    // The path chip grows into what the others leave.
    let spare = budget - spent(&widths.w, &shown, gap);
    let path = shown[Seg::Path as usize]
        .then(|| path_text(ui, v, &widths, widths.w[Seg::Path as usize] + spare));
    // Drawn in reading order: path, git, ctx, tokens, cost, usage.
    for s in [
        Seg::Path,
        Seg::Git,
        Seg::Ctx,
        Seg::Tokens,
        Seg::Cost,
        Seg::Usage,
    ] {
        if shown[s as usize] {
            segment(ui, ink, v, s, path.as_deref().unwrap_or_default(), out);
        }
    }
}

/// Model chip `(width, cap)`: [`MODEL_CHIP_W`], capped at [`MODEL_CHIP_SHARE`] of the bar (never
/// below [`MODEL_CHIP_MIN`]) so it never takes over a narrow bar. `reserved` is the width the
/// segments that must stay (and their gaps) need: the chip gives way to them down to
/// [`MODEL_CHIP_MIN`].
fn model_chip_width(bar_w: f32, reserved: f32) -> (f32, f32) {
    let cap = (bar_w * MODEL_CHIP_SHARE).max(MODEL_CHIP_MIN);
    // A pixel of slack: layout rounds, and a segment must never miss its room by a hair.
    let room = (bar_w - reserved - 1.0).max(MODEL_CHIP_MIN);
    (MODEL_CHIP_W.min(cap).min(room), cap.min(room))
}

/// Which segments fit `budget`, in priority order: the first present one that does not fit ends
/// the list, so a narrower bar never shows a segment a wider one dropped.
fn fit_segments<const N: usize>(
    widths: &[f32; N],
    present: &[bool; N],
    mut budget: f32,
    gap: f32,
) -> [bool; N] {
    let mut shown = [false; N];
    for i in 0..N {
        if !present[i] {
            continue;
        }
        let cost = widths[i] + gap;
        if budget < cost {
            break;
        }
        budget -= cost;
        shown[i] = true;
    }
    shown
}

/// What the `shown` segments take, each with its gap.
fn spent<const N: usize>(widths: &[f32; N], shown: &[bool; N], gap: f32) -> f32 {
    widths
        .iter()
        .zip(shown)
        .filter(|(_, s)| **s)
        .map(|(w, _)| w + gap)
        .sum()
}

/// One segment: a pill (the pills are separated by the row's gap alone). `path` is the path
/// chip's text.
fn segment(ui: &mut Ui, ink: &Ink, v: &BarView, seg: Seg, path: &str, out: &mut Vec<BarAction>) {
    let pal = ink.p;
    match seg {
        Seg::Path => {
            let r = chip(ui, ink, "path", Sense::click(), Tone::Plain, |ui| {
                ui.label(icon_text(ICON_FOLDER, ICON_PX, pal.weak));
                ui.add(text(path, TEXT_PX, pal.text));
            });
            let r = r.on_hover_cursor(CursorIcon::PointingHand);
            path_hover(&r, &v.path_full);
            if r.clicked() {
                out.push(BarAction::CopyPath(v.path_full.clone()));
            }
        }
        Seg::Git => {
            if let Some(g) = &v.git {
                git_chip(ui, ink, g, out);
            }
        }
        Seg::Ctx => {
            context_meter(ui, ink, v);
        }
        Seg::Tokens => {
            let r = chip(ui, ink, "tokens", Sense::hover(), Tone::Plain, |ui| {
                ui.add(text("↑", TEXT_PX, pal.weak));
                ui.add(text(&v.tokens_in, TEXT_PX, pal.text));
                ui.add_space(4.0);
                ui.add(text("↓", TEXT_PX, pal.weak));
                ui.add(text(&v.tokens_out, TEXT_PX, pal.text));
            });
            tokens_hover(&r, &v.tokens_tip);
        }
        Seg::Cost => {
            let r = chip(ui, ink, "cost", Sense::hover(), Tone::Plain, |ui| {
                ui.add(text(&v.cost, TEXT_PX, pal.ok));
            });
            cost_tip::hover(&r, &v.cost_tip);
        }
        Seg::Usage => {
            if let Some(u) = &v.usage
                && let Some(c) = &u.chip
            {
                let r = chip(ui, ink, "usage", Sense::click(), Tone::Plain, |ui| {
                    usage_chip_content(ui, pal, v, c);
                });
                let r = r.on_hover_cursor(CursorIcon::PointingHand);
                usage_view::hover(&r, u, v.account.as_ref());
                if r.clicked() {
                    out.push(BarAction::OpenUsage);
                }
            }
        }
    }
}

/// The selected model's provider card and the page the usage chip shows
/// ([`usage_view::shown_page`]: the session's account, else the Usage view's pick).
fn usage_chip_card<'a>(ctx: &egui::Context, v: &'a BarView) -> Option<(&'a CardView, usize)> {
    let (provider, _) = v.model_selector.as_deref()?.split_once('/')?;
    let card = v.usage.as_deref()?.card_of(provider)?;
    Some((
        card,
        usage_view::shown_page(ctx, card, v.account.as_ref()).0,
    ))
}

/// The usage chip's content: the selected model's provider quota (for a provider with several
/// accounts, the one the session uses, as the hover) as one pill per window (5 h, weekly), each
/// in its own colour; a provider without a measurable window falls back to the tightest quota
/// overall, named so it is not mistaken for the selected provider's.
fn usage_chip_content(ui: &mut Ui, pal: &Palette, v: &BarView, c: &Chip) {
    ui.label(icon_text(ICON_GAUGE, ICON_PX, pal.weak));
    let pills = usage_chip_card(ui.ctx(), v)
        .and_then(|(card, i)| card.pages.get(i))
        .map(|p| p.pills.as_slice())
        .filter(|p| !p.is_empty());
    if let Some(pills) = pills {
        for p in pills {
            quota_pill(ui, pal, p);
        }
        return;
    }
    let tint = match c.status {
        LimitStatus::Ok => Hue::Teal.color(pal),
        s => usage_view::status_color(pal, s),
    };
    let who = c.tight.split(" · ").next().unwrap_or_default();
    let parts: &[(&str, Color32)] = if who.is_empty() {
        &[(&c.text, tint)]
    } else {
        &[(who, pal.weak), (&c.text, tint)]
    };
    Badge::new(ui.painter(), None, parts, tint, SMALL_PX).show(ui);
}

/// `5h 63% left`: the window tag in the window's hue, the figure strong; a warning or exhausted
/// window turns the whole pill amber or red.
fn quota_pill(ui: &mut Ui, pal: &Palette, p: &QuotaPill) {
    let hue = match p.kind {
        PillKind::FiveHour => Hue::Blue,
        PillKind::Weekly => Hue::Violet,
        PillKind::Other => Hue::Teal,
    }
    .color(pal);
    let (tint, figure) = match p.status {
        LimitStatus::Warning => (pal.warn, pal.warn),
        LimitStatus::Exhausted => (pal.danger, pal.danger),
        _ => (hue, pal.text_strong),
    };
    Badge::new(
        ui.painter(),
        None,
        &[(&p.tag, tint), (&p.text, figure)],
        tint,
        SMALL_PX,
    )
    .show(ui);
}

/// Chip text in the chat view's label face (mono where the style sets labels in mono).
fn text(s: &str, size: f32, color: Color32) -> egui::Label {
    egui::Label::new(RichText::new(s).font(theme::label_font(size)).color(color)).selectable(false)
}

fn chip_radius() -> u8 {
    theme::rad(8, 10)
}

/// What a pill stands for; picks its fill and outline.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    /// A quiet chip: the chat view's chip fill and hairline.
    Plain,
    /// The primary control (model): washed and outlined in the second accent.
    Accent,
    /// A switched-on toggle (the open dashboard): washed and outlined in the solid accent.
    Lit,
    /// Retry / Stop: amber.
    Alert,
}

/// Fill and outline of a pill at hover progress `hot` (0..=1). Hovering moves a plain chip's
/// outline to the style's hover line and an accent pill's toward its accent, as the chat view's
/// pills do; [`pop`] adds the outer ring.
fn plate_colors(ink: &Ink, tone: Tone, hot: f32) -> (Color32, Color32) {
    let p = ink.p;
    let (fill, line, lit, k) = match tone {
        Tone::Plain => (ink.chip(), ink.line(), p.hover_line, 1.0),
        Tone::Accent => (ink.wash(ink.b), ink.edge(ink.b), ink.b, 0.7),
        Tone::Lit => (ink.wash(p.accent), ink.edge(p.accent), p.accent, 0.7),
        Tone::Alert => (ink.wash(p.warn), ink.edge(p.warn), p.warn, 0.7),
    };
    (
        theme::mix(fill, p.hover_bg, 0.55 * hot),
        theme::mix(line, lit, k * hot),
    )
}

/// Paints a pill's plate into the slot reserved for it before its content and returns the hover
/// progress (0..=1) so the content can follow it.
fn paint_plate(
    ui: &Ui,
    slot: egui::layers::ShapeIdx,
    ink: &Ink,
    rect: Rect,
    id: egui::Id,
    hovered: bool,
    tone: Tone,
) -> f32 {
    let hot = ui
        .ctx()
        .animate_bool_with_time(id.with("hot"), hovered, 0.1);
    let (fill, line) = plate_colors(ink, tone, hot);
    ui.painter().set(
        slot,
        Shape::Rect(egui::epaint::RectShape::new(
            rect,
            chip_radius(),
            fill,
            Stroke::new(1.0, line),
            egui::StrokeKind::Inside,
        )),
    );
    pop(ui.painter(), rect, chip_radius(), ink, hot);
    hot
}

/// A pill with an outline and a hover pop; `add` lays out its contents on one line. The response
/// is registered after the contents, so it wins hit-testing and labels never swallow clicks.
fn chip(
    ui: &mut Ui,
    ink: &Ink,
    id: &'static str,
    sense: Sense,
    tone: Tone,
    add: impl FnOnce(&mut Ui),
) -> Response {
    let slot = ui.painter().add(Shape::Noop);
    let rect = ui
        .allocate_ui_with_layout(
            vec2(ui.available_width(), CHIP_H),
            // `ui.horizontal` would reserve only `interact_size.y` (24 px in Compact) centred in
            // the row and let the chip grow downwards from there, sitting low.
            if ui.layout().prefer_right_to_left() {
                Layout::right_to_left(Align::Center)
            } else {
                Layout::left_to_right(Align::Center)
            },
            |ui| {
                ui.set_min_height(CHIP_H);
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.add_space(CHIP_PAD);
                add(ui);
                ui.add_space(CHIP_PAD);
            },
        )
        .response
        .rect;
    let r = ui.interact(rect, ui.scope_id().with(("bar-chip", id)), sense);
    if r.clicked() {
        // Keep keyboard focus with the terminal, not the chip.
        r.surrender_focus();
    }
    paint_plate(ui, slot, ink, r.rect, r.id, r.hovered(), tone);
    r
}

fn activity_dot(ui: &mut Ui, activity: Activity) {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
    match activity {
        Activity::Busy => {
            // Spinner only while a turn runs; it schedules its own repaints.
            let spinner = crate::widgets::Spin::new(15.6, pal.accent);
            ui.put(
                Rect::from_center_size(rect.center(), vec2(16.0, 16.0)),
                spinner,
            );
        }
        Activity::Unknown => {
            ui.painter().circle_stroke(
                rect.center(),
                4.2,
                Stroke::new(1.6, pal.weak.gamma_multiply(0.6)),
            );
        }
        Activity::Idle | Activity::Attention => {
            let color = if activity == Activity::Idle {
                pal.ok
            } else {
                pal.warn
            };
            theme::glow_dot(ui.painter(), rect.center(), 4.2, color, theme::GLOW_REST);
        }
    }
    activity_hover(&resp, activity);
}

/// Hover card of the activity dot: omp's state as a badge plus one sentence.
fn activity_hover(resp: &Response, activity: Activity) {
    let pal = Palette::of_ctx(&resp.ctx);
    let (badge_text, color, note) = match activity {
        Activity::Busy => ("working", Hue::Blue.color(pal), "omp is running a turn"),
        Activity::Idle => (
            "idle",
            Hue::Green.color(pal),
            "omp waits for your next prompt",
        ),
        Activity::Attention => (
            "needs input",
            Hue::Amber.color(pal),
            "omp needs your attention",
        ),
        Activity::Unknown => ("waiting", pal.weak, "Waiting for omp…"),
    };
    let tone = if activity == Activity::Attention {
        pal.warn
    } else {
        Color32::TRANSPARENT
    };
    card::hover(
        resp,
        240.0,
        false,
        &card::Header::new(ICON_ACTIVITY, "omp").tone(tone),
        |ui| {
            Badge::single(ui.painter(), None, badge_text, color, tips::VALUE).show(ui);
        },
        |ui| tips::note(ui, note),
    );
}

/// Hover card of the path chip: the folder's full path; a click copies it.
fn path_hover(resp: &Response, full: &str) {
    let name = Path::new(full)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(full);
    card::hover(
        resp,
        340.0,
        false,
        &card::Header::new(ICON_FOLDER, name).subtitle("Working directory"),
        |_| {},
        |ui| {
            card::kv(ui, 56.0, "Path", |ui| tips::mono(ui, full));
            card::footer_hints(ui, &[("click", "copy the path")]);
        },
    );
}

/// Hover card of the tokens chip: the whole session's token counts by type.
fn tokens_hover(resp: &Response, tip: &TokensTip) {
    card::hover(
        resp,
        260.0,
        false,
        &card::Header::new(ICON_CHART_PIE, "Tokens").subtitle("Whole session"),
        |_| {},
        |ui| {
            for (key, icon, value, hue) in [
                ("Input", ICON_ARROW_UP, &tip.input, Hue::Blue),
                ("Output", ICON_ARROW_DOWN, &tip.output, Hue::Green),
                ("Cache read", ICON_HISTORY, &tip.cache_read, Hue::Teal),
                ("Cache write", ICON_PENCIL, &tip.cache_write, Hue::Amber),
            ] {
                card::kv(ui, 104.0, key, |ui| {
                    badge::show(ui, Some(icon), value, hue, tips::VALUE);
                });
            }
        },
    );
}

/// Hover card of the model chip: name, selector, thinking level and context window.
fn model_hover(resp: &Response, tip: &ModelTip) {
    let mut head = card::Header::new(
        ICON_SPARKLES,
        tip.title.as_deref().unwrap_or("Model not recorded yet"),
    );
    if tip.is_default {
        head = head.subtitle("Default model (from omp's settings)");
    }
    card::hover(
        resp,
        370.0,
        false,
        &head,
        |ui| {
            if let Some(t) = &tip.thinking {
                badge::show(ui, Some(ICON_BRAIN), t, Hue::Blue, tips::VALUE);
            }
        },
        |ui| {
            const KEY_W: f32 = 84.0;
            if let Some(s) = &tip.selector {
                card::kv(ui, KEY_W, "Model", |ui| {
                    badge::show(ui, Some(ICON_SPARKLES), s, Hue::Violet, tips::VALUE);
                });
            }
            if let Some(t) = &tip.thinking {
                card::kv(ui, KEY_W, "Thinking", |ui| {
                    badge::show(ui, Some(ICON_BRAIN), t, Hue::Blue, tips::VALUE);
                });
            }
            if let Some(w) = &tip.window {
                card::kv(ui, KEY_W, "Context", |ui| {
                    badge::show(ui, None, w, Hue::Teal, tips::VALUE);
                });
            }
            if tip.is_default {
                tips::note(ui, "The session has not used a model yet.");
            }
            card::footer_hints(ui, &[("click", "switch model or thinking level")]);
        },
    );
}

/// Hover card of the git chip: branch, upstream distance, working-tree counts, worktree.
fn git_hover(resp: &Response, g: &GitView) {
    let pal = Palette::of_ctx(&resp.ctx);
    let c = g.counts;
    let icon = if g.worktree_of.is_some() {
        ICON_FOLDER_GIT_2
    } else {
        ICON_GIT_BRANCH
    };
    let tone = if g.dirty {
        pal.warn
    } else {
        Color32::TRANSPARENT
    };
    card::hover(
        resp,
        320.0,
        false,
        &card::Header::new(icon, &g.branch).tone(tone),
        |ui| {
            if g.dirty {
                badge::show(ui, None, "changes", Hue::Amber, tips::VALUE);
            } else {
                badge::show(
                    ui,
                    Some(ICON_CIRCLE_CHECK),
                    "clean",
                    Hue::Green,
                    tips::VALUE,
                );
            }
        },
        |ui| {
            const KEY_W: f32 = 100.0;
            if c.ahead > 0 || c.behind > 0 {
                card::kv(ui, KEY_W, "Upstream", |ui| {
                    if c.ahead > 0 {
                        let t = format!("↑{} ahead", c.ahead);
                        badge::show(ui, None, &t, Hue::Green, tips::VALUE);
                    }
                    if c.behind > 0 {
                        let t = format!("↓{} behind", c.behind);
                        badge::show(ui, None, &t, Hue::Amber, tips::VALUE);
                    }
                });
            }
            if g.dirty {
                card::kv(ui, KEY_W, "Working tree", |ui| {
                    for (n, what, hue) in [
                        (c.staged, "staged", Hue::Green),
                        (c.unstaged, "modified", Hue::Amber),
                        (c.untracked, "untracked", Hue::Teal),
                    ] {
                        if n > 0 {
                            badge::show(ui, None, &format!("{n} {what}"), hue, tips::VALUE);
                        }
                    }
                });
            } else {
                card::kv(ui, KEY_W, "Working tree", |ui| tips::strong(ui, "clean"));
            }
            if let Some(primary) = &g.worktree_of {
                card::kv(ui, KEY_W, "Worktree of", |ui| tips::path(ui, primary));
            }
            if let Some((label, _)) = &g.pr {
                card::kv(ui, KEY_W, "Pull request", |ui| {
                    badge::show(ui, Some(ICON_GIT_MERGE), label, Hue::Violet, tips::VALUE);
                });
            }
            card::footer_hints(
                ui,
                &[("click", "review changes"), ("Ctrl+Shift+G", "toggle")],
            );
        },
    );
}

/// Hover card of the PR chip: number and URL; a click opens it.
fn pr_hover(resp: &Response, label: &str, url: &str) {
    card::hover(
        resp,
        340.0,
        false,
        &card::Header::new(ICON_GIT_MERGE, "Pull request"),
        |ui| {
            badge::show(ui, None, label, Hue::Violet, tips::VALUE);
        },
        |ui| {
            card::kv(ui, 48.0, "URL", |ui| tips::mono(ui, url));
            card::footer_hints(ui, &[("click", "open in the browser")]);
        },
    );
}

fn model_chip(
    ui: &mut Ui,
    ink: &Ink,
    v: &BarView,
    picker: &mut PickerState,
    input: &BarInput<'_>,
    (min_w, max_w): (f32, f32),
    out: &mut Vec<BarAction>,
) {
    let pal = ink.p;
    let pending = input.pending;
    let assumed = input
        .assumed
        .filter(|_| v.model_selector.is_none() || v.model_is_default);
    // An assumed switch overrides only what it changed (a model, a level or both).
    let assumed_model = assumed.and_then(|a| a.selector.as_deref());
    let assumed_level = assumed.and_then(|a| a.level.as_deref());
    let resp = chip(ui, ink, "model", Sense::click(), Tone::Accent, |ui| {
        if let Some(p) = pending {
            ui.add(crate::widgets::Spin::new(16.0, ink.b));
            ui.add(text(
                &format!(
                    "switching to {}…",
                    match (&p.selector, &p.level) {
                        (Some(s), _) => input
                            .catalog
                            .get(s)
                            .map_or_else(|| s.clone(), |m| m.name.clone()),
                        (None, Some(l)) => format!("thinking {l}"),
                        // A role alias (`@smol`): omp resolves it, the deck cannot name the model.
                        (None, None) => "model".to_owned(),
                    }
                ),
                TEXT_PX,
                ink.accent_text(),
            ));
            return;
        }
        let name = match assumed_model {
            Some(s) => input
                .catalog
                .get(s)
                .map_or_else(|| s.to_owned(), |m| m.name.clone()),
            None if v.model_name.is_empty() => "model".to_owned(),
            None => v.model_name.clone(),
        };
        let level = assumed_level.or(v.level.as_deref());
        let configured = assumed_level.or(v.configured.as_deref());
        let unknown = (v.model_name.is_empty() || v.model_is_default) && assumed.is_none();
        let col = if unknown { pal.weak } else { pal.text_strong };
        // The chip glyph, the effort glyph + label and the caret frame a fixed-width chip; the
        // model name takes the rest and ellipsizes.
        let effort = configured.filter(|c| *c == "auto").or(level.or(configured));
        let label_w = effort.map_or(0.0, |l| {
            ui.painter()
                .layout_no_wrap(
                    thinking_short(l).to_owned(),
                    theme::label_font(SMALL_PX),
                    ink.accent_text(),
                )
                .size()
                .x
        });
        let spacing = ui.spacing().item_spacing.x;
        let lead_w = ICON_PX + spacing;
        let right_w = CARET_W + effort.map_or(0.0, |_| spacing + PIE_BOX + spacing + label_w);
        let name_text = name.as_str();
        let name_nat = ui
            .painter()
            .layout_no_wrap(name_text.to_owned(), theme::label_font(TEXT_PX), col)
            .size()
            .x
            + 8.0;
        let inner_w = (min_w - CHIP_CHROME_W)
            .max((lead_w + name_nat + spacing + right_w).min(max_w - CHIP_CHROME_W))
            .min(ui.available_width())
            .max(0.0);
        let name_w = (inner_w - lead_w - right_w - spacing).max(0.0);
        let (glyph, _) = ui.allocate_exact_size(vec2(ICON_PX, ICON_PX), Sense::hover());
        paint_icon(
            ui.painter(),
            glyph.center(),
            Align2::CENTER_CENTER,
            ICON_CPU,
            ICON_PX,
            if unknown { pal.weak } else { ink.accent_text() },
        );
        ui.allocate_ui_with_layout(
            vec2(name_w, CHIP_H),
            Layout::left_to_right(Align::Center),
            |ui| {
                // allocate_ui_with_layout only keeps the content's rect: pin the floor.
                ui.set_min_size(vec2(name_w, CHIP_H));
                ui.add(
                    egui::Label::new(
                        RichText::new(name_text)
                            .font(theme::label_font(TEXT_PX))
                            .color(col)
                            .variation("wght", 620.0),
                    )
                    .truncate()
                    .selectable(false),
                );
            },
        );
        if let Some(l) = effort {
            let (rect, _) = ui.allocate_exact_size(vec2(PIE_BOX, PIE_BOX), Sense::hover());
            paint_pie(
                ui.painter(),
                rect.center(),
                6.4,
                thinking_fill(l),
                ink.accent_text(),
            );
            ui.add(text(thinking_short(l), SMALL_PX, ink.accent_text()));
        }
        let (caret, _) = ui.allocate_exact_size(vec2(CARET_W, CARET_W), Sense::hover());
        paint_icon(
            ui.painter(),
            caret.center(),
            Align2::CENTER_CENTER,
            ICON_CHEVRON_DOWN,
            CARET_W,
            pal.weak,
        );
    });
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let popup_open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    if popup_open {
        // The dropdown is up: the chip stays outlined in the accent while the pointer is on it.
        ui.painter().rect_stroke(
            resp.rect,
            chip_radius(),
            Stroke::new(1.5, ink.b),
            egui::StrokeKind::Inside,
        );
    } else {
        model_hover(&resp, &v.model_tip);
    }
    let acts = model_picker::show_session(
        &resp,
        picker,
        &PickerInput {
            catalog: input.catalog,
            current: assumed_model.or(v.model_selector.as_deref()),
            level: assumed_level.or(v.current_level()),
            favorites: input.favorites,
            provider: input.provider,
            unset: "Current model unknown yet",
        },
        &input.session,
    );
    out.extend(acts.actions.into_iter().map(BarAction::Picker));
    out.extend(acts.task_model.map(BarAction::TaskModel));
}

/// Thinking glyph: ring + pie sector (clockwise from 12 o'clock); `None` = off (slashed ring).
fn paint_pie(p: &Painter, c: Pos2, r: f32, fill: Option<f32>, color: Color32) {
    let ring = Stroke::new(1.6, color);
    p.circle_stroke(c, r, ring);
    match fill {
        None => {
            let d = r * 0.7;
            p.line_segment([c + vec2(-d, d), c + vec2(d, -d)], ring);
        }
        Some(f) if f >= 0.995 => {
            p.circle_filled(c, r - 1.8, color);
        }
        Some(f) if f > 0.0 => {
            let steps = ((f * 24.0).ceil() as usize).max(2);
            let mut pts = Vec::with_capacity(steps + 2);
            pts.push(c);
            for i in 0..=steps {
                let a = -std::f32::consts::FRAC_PI_2
                    + f * std::f32::consts::TAU * i as f32 / steps as f32;
                pts.push(c + vec2(a.cos(), a.sin()) * (r - 1.8));
            }
            p.add(Shape::convex_polygon(pts, color, Stroke::NONE));
        }
        _ => {}
    }
}

fn git_chip(ui: &mut Ui, ink: &Ink, g: &GitView, out: &mut Vec<BarAction>) {
    let pal = ink.p;
    let r = chip(ui, ink, "git", Sense::click(), Tone::Plain, |ui| {
        ui.label(icon_text(
            if g.worktree_of.is_some() {
                ICON_FOLDER_GIT_2
            } else {
                ICON_GIT_BRANCH
            },
            ICON_PX + 1.0,
            if g.dirty { pal.warn } else { pal.ok },
        ));
        ui.add(text(&g.branch, TEXT_PX, pal.text));
        if !g.sync.is_empty() {
            ui.add(text(&g.sync, SMALL_PX, pal.weak));
        }
        if !g.unstaged.is_empty() {
            ui.add(text(&g.unstaged, SMALL_PX, pal.warn));
        }
        if !g.staged.is_empty() {
            ui.add(text(&g.staged, SMALL_PX, pal.ok));
        }
        if !g.untracked.is_empty() {
            ui.add(text(&g.untracked, SMALL_PX, pal.weak));
        }
    });
    let r = r.on_hover_cursor(CursorIcon::PointingHand);
    git_hover(&r, g);
    if r.clicked() {
        out.push(BarAction::ToggleChanges);
    }
    if let Some((label, url)) = &g.pr {
        let pr = chip(ui, ink, "pr", Sense::click(), Tone::Plain, |ui| {
            ui.add(text("PR", SMALL_PX, pal.weak));
            ui.label(
                RichText::new(label)
                    .font(theme::label_font(TEXT_PX))
                    .color(ink.accent_text())
                    .underline(),
            );
        });
        let pr = pr.on_hover_cursor(CursorIcon::PointingHand);
        pr_hover(&pr, label, url);
        if pr.clicked() {
            out.push(BarAction::OpenUrl(url.clone()));
        }
    }
}

/// Colour of the context meter / label at an escalation level.
pub(crate) fn level_color(pal: &Palette, level: Option<ContextLevel>) -> Color32 {
    match level {
        None | Some(ContextLevel::Normal) => pal.accent,
        Some(ContextLevel::Warn) => pal.warn,
        Some(ContextLevel::High) => theme::mix(pal.warn, pal.danger, 0.5),
        Some(ContextLevel::Critical) => pal.danger,
    }
}

fn context_meter(ui: &mut Ui, ink: &Ink, v: &BarView) {
    let pal = ink.p;
    let color = level_color(pal, v.ctx_level);
    let escalated = v.ctx_level.is_some_and(|l| l != ContextLevel::Normal);
    let r = chip(ui, ink, "ctx", Sense::hover(), Tone::Plain, |ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(70.0, 16.0), Sense::hover());
        let bar = Rect::from_center_size(rect.center(), vec2(64.0, 5.0));
        let p = ui.painter();
        // The chat view's meter: a quiet track, the accent pair as the fill while the context is
        // healthy, the escalation colour once it is not.
        p.rect_filled(bar, 3, ink.line().gamma_multiply(0.7));
        if let Some(f) = v.ctx_frac {
            let w = (bar.width() * f).max(if f > 0.0 { 3.6 } else { 0.0 });
            let fill = Rect::from_min_size(bar.min, vec2(w, bar.height()));
            if escalated {
                p.rect_filled(fill, 3, color);
            } else {
                theme::gradient_rounded(p, fill, 3.0, ink.a, ink.b);
            }
        }
        let label_color = if escalated { color } else { pal.text };
        ui.add(text(&v.ctx_label, TEXT_PX, label_color));
    });
    ctx_tip::hover(&r, v.ctx_detail.as_ref().map(|(_, t)| &**t), &v.ctx_tip);
}

/// The glyph of one count in the Agents chip.
enum Glyph {
    Spinner,
    Icon(crate::icons::Icon),
}

/// `⟳ 2 · ✓ 5 · ✕ 1` — live counts of the active session's subagents; click toggles the dashboard.
fn agents_chip(ui: &mut Ui, ink: &Ink, v: &AgentsChip, out: &mut Vec<BarAction>) {
    let pal = ink.p;
    let c = v.counts;
    let total = c.total();
    // The action row lays out right → left and `horizontal` inherits that: contents are emitted
    // in reverse reading order (last count first, number before its glyph, dashboard icon last).
    let tone = if v.open { Tone::Lit } else { Tone::Plain };
    let r = chip(ui, ink, "agents", Sense::click(), tone, |ui| {
        let lit = v.open || c.running > 0;
        let icon_color = if lit {
            pal.accent
        } else if total == 0 {
            pal.weak.gamma_multiply(0.7)
        } else {
            pal.weak
        };
        if c.total() == c.parked {
            ui.add(text("Agents", TEXT_PX, pal.weak.gamma_multiply(0.85)));
        } else {
            let mut first = true;
            let mut seg = |ui: &mut Ui, n: usize, glyph: Glyph, color: Color32| {
                if n == 0 {
                    return;
                }
                if !first {
                    ui.add(text("·", TEXT_PX, pal.weak));
                }
                first = false;
                ui.add(text(&n.to_string(), TEXT_PX, pal.text));
                match glyph {
                    Glyph::Spinner => {
                        ui.add(crate::widgets::Spin::new(ICON_PX, color));
                    }
                    Glyph::Icon(i) => {
                        ui.label(icon_text(i, ICON_PX + 1.0, color));
                    }
                }
            };
            seg(
                ui,
                c.aborted + c.interrupted,
                Glyph::Icon(ICON_CIRCLE_STOP),
                pal.warn,
            );
            seg(ui, c.failed, Glyph::Icon(ICON_CIRCLE_ALERT), pal.danger);
            seg(ui, c.done, Glyph::Icon(ICON_CIRCLE_CHECK), pal.ok);
            seg(ui, c.running, Glyph::Spinner, pal.accent_b);
        }
        ui.label(icon_text(ICON_NETWORK, ICON_PX + 2.0, icon_color));
    });
    let r = r.on_hover_cursor(CursorIcon::PointingHand);
    crate::card::hover(
        &r,
        260.0,
        false,
        &crate::card::Header::new(ICON_NETWORK, "Agents").subtitle("Subagents of this session"),
        |_| {},
        |ui| {
            if total == 0 {
                ui.label(RichText::new("No subagents in this session yet").color(pal.weak));
            }
            for (n, what, hue) in [
                (c.running, "running", Hue::Blue),
                (c.done, "done", Hue::Green),
                (c.failed, "failed", Hue::Rose),
                (c.aborted, "aborted", Hue::Amber),
                (c.interrupted, "interrupted", Hue::Amber),
            ] {
                if n > 0 {
                    crate::card::kv(ui, 104.0, what, |ui| {
                        crate::badge::show(ui, None, &n.to_string(), hue, 12.0);
                    });
                }
            }
            crate::card::footer_hints(ui, &[("Ctrl+Shift+A", "toggles the dashboard")]);
        },
    );
    if r.clicked() {
        out.push(BarAction::ToggleAgents);
    }
}

// ------------------------------------------------------------------ action buttons

fn actions_row(ui: &mut Ui, ink: &Ink, input: &BarInput<'_>, out: &mut Vec<BarAction>) {
    let c = &input.actions;
    ui.spacing_mut().item_spacing.x = 6.0;

    // Added right → left: the Agents chip sits at the far right, under the dashboard.
    agents_chip(ui, ink, &input.agents, out);

    // The icon buttons share one outlined group (the chat composer's button cluster), added
    // right → left, so it reads: Plan · Handoff · Compact ▾ · Shake ▾ · Fork · Tree · ⋯
    let slot = ui.painter().add(Shape::Noop);
    let group = ui
        .scope(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let more = bar_button(ui, ink, ICON_ELLIPSIS, "More actions", true, None, false);
            theme::dropup_menu(&more, 220.0, |ui| overflow_menu(ui, c, out));

            let tree = Kind::Tree;
            if bar_button(
                ui,
                ink,
                ICON_LIST_TREE,
                "Session tree (Ctrl+Shift+B)",
                session_actions::availability(tree, c).is_enabled(),
                reason(tree, c),
                false,
            )
            .clicked()
            {
                out.push(BarAction::OpenTree);
            }

            let fork = Kind::Fork;
            if bar_button(
                ui,
                ink,
                ICON_GIT_FORK,
                "Fork into a new tab",
                session_actions::availability(fork, c).is_enabled(),
                reason(fork, c),
                false,
            )
            .clicked()
            {
                out.push(BarAction::Run(Action::Fork));
            }

            let shake = bar_button(
                ui,
                ink,
                ICON_VIBRATE,
                "Shake heavy content out of the context…",
                session_actions::availability(Kind::Shake, c).is_enabled(),
                reason(Kind::Shake, c),
                false,
            );
            if session_actions::availability(Kind::Shake, c).is_enabled() {
                theme::dropup_menu(&shake, 250.0, |ui| {
                    for (text, mode) in [
                        ("Shake tool results + large blocks", ShakeMode::Elide),
                        ("Shake images", ShakeMode::Images),
                        ("Shake thinking", ShakeMode::Thinking),
                    ] {
                        if menu_item(ui, ICON_VIBRATE, text, None, true, false).clicked() {
                            out.push(BarAction::Run(Action::Shake(mode)));
                        }
                    }
                });
            }

            let compact = bar_button(
                ui,
                ink,
                ICON_FOLD_VERTICAL,
                "Compact context…",
                session_actions::availability(Kind::Compact, c).is_enabled(),
                reason(Kind::Compact, c),
                false,
            );
            if session_actions::availability(Kind::Compact, c).is_enabled() {
                theme::dropup_menu(&compact, 210.0, |ui| {
                    if menu_item(ui, ICON_FOLD_VERTICAL, "Compact", None, true, false).clicked() {
                        out.push(BarAction::Run(Action::Compact(CompactMode::Default)));
                    }
                    if menu_item(ui, ICON_FOLD_VERTICAL, "Compact (soft)", None, true, false)
                        .clicked()
                    {
                        out.push(BarAction::Run(Action::Compact(CompactMode::Soft)));
                    }
                    if menu_item(ui, ICON_FOLD_VERTICAL, "Snapcompact", None, true, false).clicked()
                    {
                        out.push(BarAction::Run(Action::Compact(CompactMode::Snapcompact)));
                    }
                    ui.separator();
                    if menu_item(ui, ICON_PENCIL, "Compact with focus…", None, true, false)
                        .clicked()
                    {
                        out.push(BarAction::AskCompactFocus);
                    }
                });
            }

            let handoff = bar_button(
                ui,
                ink,
                ICON_HANDSHAKE,
                "Hand off to a fresh context…",
                session_actions::availability(Kind::Handoff, c).is_enabled(),
                reason(Kind::Handoff, c),
                false,
            );
            if handoff.clicked() && session_actions::availability(Kind::Handoff, c).is_enabled() {
                out.push(BarAction::AskHandoff);
            }

            if bar_button(
                ui,
                ink,
                ICON_MAP,
                "Plan mode (toggle)",
                session_actions::availability(Kind::PlanMode, c).is_enabled(),
                reason(Kind::PlanMode, c),
                false,
            )
            .clicked()
            {
                out.push(BarAction::Run(Action::PlanMode));
            }
        })
        .response
        .rect;
    // The scope spans the bar's height; the box is only as tall as the pills beside it.
    let group = Rect::from_center_size(group.center(), vec2(group.width(), CHIP_H));
    ui.painter().set(
        slot,
        Shape::Rect(egui::epaint::RectShape::new(
            group,
            chip_radius(),
            ink.chip(),
            Stroke::new(1.0, ink.line()),
            egui::StrokeKind::Inside,
        )),
    );

    // Retry and Stop stand out of the group as amber pills, left of it.
    if session_actions::visible(Kind::Retry, c)
        && bar_button(
            ui,
            ink,
            ICON_ROTATE_CCW,
            "Retry the failed turn",
            session_actions::availability(Kind::Retry, c).is_enabled(),
            reason(Kind::Retry, c),
            true,
        )
        .clicked()
    {
        out.push(BarAction::Run(Action::Retry));
    }
    if session_actions::visible(Kind::Stop, c)
        && bar_button(
            ui,
            ink,
            ICON_SQUARE,
            "Stop the current turn (Esc)",
            true,
            None,
            true,
        )
        .clicked()
    {
        out.push(BarAction::Run(Action::Stop));
    }
}

fn reason(kind: Kind, c: &ActionCtx) -> Option<&'static str> {
    match session_actions::availability(kind, c) {
        Availability::Enabled => None,
        Availability::Disabled(r) => Some(r),
    }
}

fn overflow_menu(ui: &mut Ui, c: &ActionCtx, out: &mut Vec<BarAction>) {
    let mut item = |ui: &mut Ui, icon, text: &str, kind: Kind, act: BarAction| {
        let a = session_actions::availability(kind, c);
        let r = menu_item(ui, icon, text, None, a.is_enabled(), false);
        let r = match a {
            Availability::Disabled(why) => r.on_disabled_hover_text(why),
            Availability::Enabled => r,
        };
        if r.clicked() {
            out.push(act);
        }
    };
    item(
        ui,
        ICON_LIST_TREE,
        "Session tree",
        Kind::Tree,
        BarAction::OpenTree,
    );
    ui.separator();
    item(
        ui,
        ICON_PENCIL,
        "Rename…",
        Kind::Rename,
        BarAction::AskRename,
    );
    item(
        ui,
        ICON_SPARKLES,
        "Auto-name",
        Kind::AutoName,
        BarAction::Run(Action::AutoName),
    );
    ui.separator();
    item(
        ui,
        ICON_COPY,
        "Copy last response",
        Kind::CopyLast,
        BarAction::Run(Action::CopyLast),
    );
    item(
        ui,
        ICON_DOWNLOAD,
        "Export as HTML",
        Kind::Export,
        BarAction::Run(Action::ExportHtml),
    );
    ui.separator();
    if menu_item(ui, ICON_GAUGE, "Usage…", Some("Ctrl+Shift+U"), true, false).clicked() {
        out.push(BarAction::OpenUsage);
    }
}

/// Compact icon button; disabled ones dim and explain why in the tooltip. A plain button sits in
/// the action group and shows a plate only under the pointer; `emphasis` makes it an amber pill
/// of its own.
fn bar_button(
    ui: &mut Ui,
    ink: &Ink,
    icon: crate::icons::Icon,
    tooltip: &str,
    enabled: bool,
    disabled_reason: Option<&str>,
    emphasis: bool,
) -> Response {
    let pal = ink.p;
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let slot = ui.painter().add(Shape::Noop);
    let (rect, resp) = ui.allocate_exact_size(vec2(BTN_W, CHIP_H), sense);
    if resp.clicked() {
        resp.surrender_focus();
    }
    let hovered = resp.hovered() && enabled;
    let (fg_rest, fg_hot) = match (enabled, emphasis) {
        (false, _) => (pal.weak.gamma_multiply(0.4), pal.weak.gamma_multiply(0.4)),
        (true, true) => (pal.warn, pal.warn),
        (true, false) => (pal.weak, pal.text_strong),
    };
    let hot = if emphasis {
        paint_plate(ui, slot, ink, rect, resp.id, hovered, Tone::Alert)
    } else {
        // Inside the group's outline: the plate is inset so its own outline and the pop ring
        // stay within it.
        let plate = rect.shrink2(vec2(2.0, 3.0));
        let hot = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("hot"), hovered, 0.1);
        let (fill, line) = plate_colors(ink, Tone::Plain, hot);
        let radius = chip_radius().saturating_sub(2);
        let p = ui.painter();
        p.rect(
            plate,
            radius,
            fill.gamma_multiply(hot),
            Stroke::new(1.0, line.gamma_multiply(hot)),
            egui::StrokeKind::Inside,
        );
        pop(p, plate, radius, ink, hot);
        hot
    };
    paint_icon(
        ui.painter(),
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        17.0,
        theme::mix(fg_rest, fg_hot, hot),
    );
    let tip = match (enabled, disabled_reason) {
        (false, Some(why)) => format!("{tooltip}\n{why}"),
        _ => tooltip.to_owned(),
    };
    resp.on_hover_text(tip).on_hover_cursor(if enabled {
        CursorIcon::PointingHand
    } else {
        CursorIcon::NotAllowed
    })
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use deck_core::status::PrInfo;
    use std::path::PathBuf;

    fn catalog() -> ModelsCache {
        let mut c = ModelsCache::default();
        c.finish(Ok(vec![ModelInfo {
            selector: "anthropic/claude-opus-5-5".to_owned(),
            provider: "anthropic".to_owned(),
            id: "claude-opus-5-5".to_owned(),
            name: "Claude Opus 5.5".to_owned(),
            context_window: Some(1_000_000),
            thinking: vec!["low".into(), "high".into()],
            cost_input: Some(5.0),
            cost_output: Some(25.0),
            ..Default::default()
        }]));
        c
    }

    fn status() -> SessionStatus {
        SessionStatus {
            cwd: "/tmp/proj".into(),
            stats: SessionStats {
                model: Some("anthropic/claude-opus-5-5".into()),
                thinking: Some("high".into()),
                thinking_configured: Some("auto".into()),
                input_tokens: 50_000,
                output_tokens: 200,
                cache_read: 1_000,
                cache_write: 12_952,
                cost: 7.0,
                context_tokens: Some(173_000),
                ..SessionStats::default()
            },
            git: Some(GitInfo {
                branch: "feat/x".into(),
                ahead: 2,
                behind: 0,
                staged: 1,
                unstaged: 3,
                untracked: 2,
                pr: Some(PrInfo {
                    number: 12,
                    url: "https://example.com/pr/12".into(),
                }),
                worktree_of: None,
            }),
            ..SessionStatus::default()
        }
    }

    #[test]
    fn view_formats_every_segment() {
        let mut v = BarView::default();
        let snap = Arc::new(status());
        assert!(v.update(Some(&snap), Path::new("/x"), &catalog()));
        assert_eq!(v.model_name, "Claude Opus 5.5");
        assert_eq!(v.current_level(), Some("auto"));
        assert_eq!(v.ctx_label, "17.3%/1M");
        assert_eq!(v.ctx_level, Some(ContextLevel::Warn));
        assert_eq!(v.tokens_in, "50K");
        assert_eq!(v.tokens_out, "200");
        assert_eq!(v.cost, "$7.00");
        assert!(v.show_cost && v.show_tokens);
        let g = v.git.as_ref().expect("git");
        assert_eq!(g.sync, "↑2");
        assert_eq!(
            (g.unstaged.as_str(), g.staged.as_str(), g.untracked.as_str()),
            ("*3", "+1", "?2")
        );
        assert_eq!(
            g.pr,
            Some(("#12".to_owned(), "https://example.com/pr/12".to_owned()))
        );
        assert!(g.dirty);
        assert!(v.path_full.ends_with("proj"));
    }

    #[test]
    fn worktree_chip_names_the_primary_and_reads_clean() {
        let g = GitInfo {
            branch: "wt/x".into(),
            worktree_of: Some(PathBuf::from("/work/repo")),
            ..GitInfo::default()
        };
        let v = git_view(&g);
        assert_eq!(v.worktree_of.as_deref(), Some(Path::new("/work/repo")));
        assert!(!v.dirty);
        assert_eq!(v.counts, GitCounts::default());
        assert_eq!(git_view(&GitInfo::default()).worktree_of, None);
    }

    #[test]
    fn chip_follows_the_live_breakdown_and_falls_back_to_jsonl() {
        use deck_core::ctl::ContextBreakdown;
        let live = ContextBreakdown {
            model: None,
            context_window: 50_000,
            used_tokens: 1_900,
            anchored: false,
            categories: Vec::new(),
            auto_compact_buffer: 0,
            free_tokens: 0,
            threshold_tokens: None,
            snapcompact_saved: None,
            tools: Vec::new(),
            skills: 0,
            computed_at_ms: 1,
        };
        let mut v = BarView::default();
        let snap = Arc::new(status());
        v.update(Some(&snap), Path::new("/x"), &catalog());
        assert_eq!(v.ctx_label, "17.3%/1M");
        v.set_context(Some(&live));
        assert_eq!(v.ctx_label, "3.8%/50K");
        assert_eq!(v.ctx_level, Some(ContextLevel::Normal));
        // A rebuild keeps following the live numbers.
        let newer = Arc::new(status());
        v.update(Some(&newer), Path::new("/x"), &catalog());
        assert_eq!(v.ctx_label, "3.8%/50K");
        v.set_context(None);
        assert_eq!(v.ctx_label, "17.3%/1M");
    }

    #[test]
    fn view_rebuilds_only_on_new_snapshot_or_catalog() {
        let mut v = BarView::default();
        let snap = Arc::new(status());
        let cat = catalog();
        assert!(v.update(Some(&snap), Path::new("/x"), &cat));
        assert!(!v.update(Some(&snap), Path::new("/x"), &cat));
        let newer = Arc::new(status());
        assert!(v.update(Some(&newer), Path::new("/x"), &cat));
        let mut cat2 = catalog();
        cat2.finish(Ok(Vec::new()));
        assert!(v.update(Some(&newer), Path::new("/x"), &cat2));
        assert_eq!(v.model_name, "claude-opus-5-5");
        assert_eq!(v.ctx_label, "173K/?");
    }

    #[test]
    fn fresh_session_shows_omp_defaults_until_a_model_is_recorded() {
        let defaults = Defaults {
            model: Some("anthropic/claude-opus-5-5".into()),
            level: Some("high".into()),
        };
        let mut v = BarView::default();
        v.set_defaults(&defaults);
        assert!(v.update(None, Path::new("/work/dir"), &catalog()));
        assert!(v.model_is_default);
        assert_eq!(v.model_name, "Claude Opus 5.5");
        assert_eq!(v.current_level(), Some("high"));
        assert!(v.model_tip.is_default);
        assert_eq!(v.model_tip.title.as_deref(), Some("Claude Opus 5.5"));
        assert_eq!(v.model_tip.thinking.as_deref(), Some("high"));
        // The session's own model replaces the default.
        let snap = Arc::new(status());
        assert!(v.update(Some(&snap), Path::new("/x"), &catalog()));
        assert!(!v.model_is_default);
        assert_eq!(v.current_level(), Some("auto"));
        // Defaults loaded late rebuild an already built view.
        let mut late = BarView::default();
        assert!(late.update(None, Path::new("/w"), &catalog()));
        assert!(late.model_name.is_empty());
        late.set_defaults(&defaults);
        assert!(late.update(None, Path::new("/w"), &catalog()));
        assert_eq!(late.model_name, "Claude Opus 5.5");
    }

    #[test]
    fn unpersisted_session_shows_omp_live_model_over_defaults_until_the_file_records_one() {
        let defaults = Defaults {
            model: Some("openai/gpt-x".into()),
            level: Some("medium".into()),
        };
        // Picked before the first message: omp has not written the session file yet.
        let live = LiveModel {
            selector: "anthropic/claude-opus-5-5".into(),
            thinking: "low".into(),
            thinking_configured: Some("low".into()),
        };
        let fresh = Arc::new(SessionStatus {
            fresh: true,
            ..SessionStatus::default()
        });
        let mut v = BarView::default();
        v.set_defaults(&defaults);
        v.update(Some(&fresh), Path::new("/w"), &catalog());
        assert_eq!(v.model_name, "gpt-x");
        v.set_live_model(Some(&live));
        assert!(v.update(Some(&fresh), Path::new("/w"), &catalog()));
        assert!(!v.model_is_default);
        assert_eq!(
            v.model_selector.as_deref(),
            Some("anthropic/claude-opus-5-5")
        );
        assert_eq!(v.model_name, "Claude Opus 5.5");
        assert_eq!(v.level.as_deref(), Some("low"));
        assert_eq!(v.current_level(), Some("low"));
        assert_eq!(v.ctx_label, "?%/1M", "the live model's window");
        // A thinking change is a new state: rebuilt.
        let high = LiveModel {
            thinking: "high".into(),
            thinking_configured: Some("high".into()),
            ..live.clone()
        };
        v.set_live_model(Some(&high));
        assert!(v.update(Some(&fresh), Path::new("/w"), &catalog()));
        assert_eq!(v.current_level(), Some("high"));
        // Once the file records a model (first message on), the file wins.
        let persisted = Arc::new(status());
        assert!(v.update(Some(&persisted), Path::new("/w"), &catalog()));
        assert_eq!(v.level.as_deref(), Some("high"));
        assert_eq!(v.current_level(), Some("auto"));
        let mut file_model = status();
        file_model.stats.model = Some("openai/gpt-x".into());
        assert!(v.update(Some(&Arc::new(file_model)), Path::new("/w"), &catalog()));
        assert_eq!(v.model_name, "gpt-x");
    }

    #[test]
    fn model_state_prefers_the_file_once_it_records_a_model() {
        let live = LiveModel {
            selector: "p/live".into(),
            thinking: "off".into(),
            thinking_configured: Some("off".into()),
        };
        // A fresh file may carry a thinking_level_change without any model: live wins whole.
        let fresh = SessionStats {
            thinking: Some("max".into()),
            ..SessionStats::default()
        };
        assert_eq!(
            model_state(&fresh, Some(&live)),
            ModelState {
                model: Some("p/live"),
                thinking: Some("off"),
                configured: Some("off"),
            }
        );
        assert_eq!(model_state(&fresh, None).model, None);
        let stats = status().stats;
        assert_eq!(
            model_state(&stats, Some(&live)),
            ModelState {
                model: Some("anthropic/claude-opus-5-5"),
                thinking: Some("high"),
                configured: Some("auto"),
            }
        );
    }

    #[test]
    fn fresh_session_hides_tokens_and_cost() {
        let mut v = BarView::default();
        assert!(v.update(None, Path::new("/work/dir"), &catalog()));
        assert!(!v.show_tokens && !v.show_cost);
        assert!(v.git.is_none());
        assert_eq!(v.path_full, "/work/dir");
        assert_eq!(v.ctx_label, "?");
    }

    #[test]
    fn digits_are_grouped() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(1_234_567), "1,234,567");
    }

    #[test]
    fn segments_drop_monotonically_as_the_bar_narrows() {
        // In priority order: ctx, usage, git, cost, tokens, path (smallest chip). Greedy skipping
        // would show a narrow low-priority segment while a wider higher one is gone.
        let widths = [60.0, 90.0, 300.0, 70.0, 80.0, 130.0];
        let all = [true; 6];
        let mut prev = [true; 6];
        let mut w = 900.0;
        while w >= 0.0 {
            let now = fit_segments(&widths, &all, w, 6.0);
            for i in 0..6 {
                assert!(prev[i] || !now[i], "segment {i} came back at {w}px");
            }
            // Never a lower-priority segment without every higher-priority one.
            for i in 1..6 {
                assert!(
                    !now[i] || now[i - 1],
                    "segment {i} shown without {} at {w}",
                    i - 1
                );
            }
            prev = now;
            w -= 5.0;
        }
    }

    #[test]
    fn model_chip_is_as_wide_in_every_session_and_yields_to_ctx_and_usage_down_to_its_minimum() {
        assert_eq!(model_chip_width(1400.0, 0.0).0, MODEL_CHIP_W);
        assert_eq!(model_chip_width(400.0, 0.0).0, MODEL_CHIP_MIN);
        // Ctx (150) and usage (250) with their gaps need 418 px.
        let reserved = 150.0 + 6.0 + 250.0 + 6.0 + 6.0;
        assert_eq!(model_chip_width(1100.0, reserved).0, MODEL_CHIP_W);
        let (chip, cap) = model_chip_width(600.0, reserved);
        assert!(chip < MODEL_CHIP_W && chip > MODEL_CHIP_MIN, "{chip}");
        assert!(
            cap <= chip + 0.01,
            "the chip may not grow past what is left: {cap} vs {chip}"
        );
        assert_eq!(model_chip_width(300.0, reserved).0, MODEL_CHIP_MIN);

        // Usage (index 1) is dropped only once the chip is at its minimum and still leaves no
        // room; every segment still drops monotonically.
        let widths = [150.0, 250.0, 420.0, 70.0, 80.0, 130.0];
        let all = [true; 6];
        let fit = |bar: f32| {
            let (chip, _) = model_chip_width(bar, reserved);
            fit_segments(&widths, &all, bar - chip - 6.0, 6.0)
        };
        let least = MODEL_CHIP_MIN + reserved + 1.0;
        assert!(
            fit(least)[..2].iter().all(|s| *s),
            "ctx and usage at {least}px"
        );
        assert!(
            !fit(least - 20.0)[1],
            "usage goes once the chip cannot yield more"
        );
        let mut prev = [true; 6];
        let mut bar = 1400.0;
        while bar >= 0.0 {
            let now = fit(bar);
            for i in 0..6 {
                assert!(prev[i] || !now[i], "segment {i} came back at {bar}px");
            }
            prev = now;
            bar -= 5.0;
        }
    }

    #[test]
    fn absent_segments_do_not_block_the_rest() {
        let widths = [60.0, 90.0, 100.0, 70.0, 80.0];
        let present = [true, false, true, false, true];
        assert_eq!(
            fit_segments(&widths, &present, 400.0, 6.0),
            [true, false, true, false, true]
        );
        // The last one no longer fits: nothing after it comes back either.
        assert_eq!(
            fit_segments(&widths, &present, 150.0, 6.0),
            [true, false, false, false, false]
        );
    }

    /// Width of a path as characters: the elision's `fits` stand-in.
    fn elide(path: &str, chars: usize) -> String {
        elide_path(path, |t| t.chars().count() <= chars)
    }

    #[test]
    fn path_keeps_its_last_folders_behind_an_ellipsis() {
        let p = "~/a/PersProject/OmpDeck";
        assert_eq!(elide(p, 40), p, "room for all of it");
        assert_eq!(elide(p, 23), p);
        assert_eq!(elide(p, 22), "…/PersProject/OmpDeck");
        assert_eq!(
            elide(p, 20),
            "…/OmpDeck",
            "drops the folders from the front"
        );
        assert_eq!(elide(p, 9), "…/OmpDeck");
        assert_eq!(
            elide(p, 3),
            "…/OmpDeck",
            "the last folder stays whatever the room"
        );
        let w = "/work/OmpDeck-worktrees/bar-usage-fix";
        assert_eq!(elide(w, 34), "…/OmpDeck-worktrees/bar-usage-fix");
        assert_eq!(elide(w, 10), "…/bar-usage-fix");
    }

    #[test]
    fn path_elision_handles_roots_windows_separators_and_unicode() {
        assert_eq!(elide("~", 0), "~");
        assert_eq!(elide("/", 0), "/");
        assert_eq!(
            elide("/proj", 0),
            "/proj",
            "a root folder has no parent to cut"
        );
        assert_eq!(elide("C:\\Users\\me\\proj", 10), "…\\me\\proj");
        assert_eq!(elide("~/café/ünï", 8), "…/ünï");
        // A folder name longer than the minimum is ellipsized itself, never split mid-character.
        let long = elide("/a/b/ÀÉÎÕÜÀÉÎÕÜÀÉÎÕÜÀÉÎÕÜÀÉÎÕÜ", 0);
        assert_eq!(long.chars().count(), PATH_MIN_CHARS);
        assert!(long.starts_with('…') && long.ends_with('Ü'));
    }
}

#[cfg(test)]
mod centering_tests {
    use super::*;
    use deck_core::config::{Density, UiFont, UiStyle};
    use deck_core::status::{GitInfo, PrInfo, SessionStats, SessionStatus};

    /// Every text/icon painted by the bar is vertically centred on the bar (the chips used to sit
    /// `(CHIP_H - interact_size.y) / 2` low when the density's interact height was below the chip).
    /// An explicit `font` is installed with its real face, whose metrics differ from the default.
    fn assert_centered(style: UiStyle, density: Density, font: UiFont) {
        let ctx = egui::Context::default();
        crate::ui_fonts::install(&ctx, crate::ui_fonts::UiFonts::none());
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        crate::ui_fonts::finalize(&ctx);
        theme::apply(&ctx, style, density, font, true);
        let mut v = BarView::default();
        let st = SessionStatus {
            cwd: "/tmp/proj".into(),
            stats: SessionStats {
                model: Some("anthropic/claude-opus-5-5".into()),
                thinking: Some("high".into()),
                input_tokens: 50_000,
                output_tokens: 200,
                cost: 7.0,
                context_tokens: Some(173_000),
                ..SessionStats::default()
            },
            git: Some(GitInfo {
                branch: "feat/x".into(),
                ahead: 2,
                staged: 1,
                unstaged: 3,
                pr: Some(PrInfo {
                    number: 12,
                    url: "u".into(),
                }),
                ..GitInfo::default()
            }),
            ..SessionStatus::default()
        };
        let cat = ModelsCache::default();
        v.update(Some(&Arc::new(st)), Path::new("/x"), &cat);
        let mut picker = PickerState::default();
        let input = BarInput {
            catalog: &cat,
            favorites: &[],
            provider: None,
            activity: Activity::Idle,
            pending: None,
            assumed: None,
            actions: ActionCtx::default(),
            agents: AgentsChip::default(),
            session: SessionExtras {
                tab: 0,
                task_unavailable: None,
            },
        };
        let screen_h = 700.0;
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1400.0, screen_h))),
            ..Default::default()
        };
        let mut texts = 0;
        for i in 0..3 {
            let mut out = ctx.run_ui(raw.clone(), |ui| {
                show(ui, &v, &mut picker, &input);
            });
            out.textures_delta.clear();
            if i == 2 {
                for cs in &out.shapes {
                    if let Shape::Text(t) = &cs.shape {
                        let y = t.galley.rect.translate(t.pos.to_vec2()).center().y;
                        let off = y - (screen_h - BAR_H / 2.0);
                        assert!(
                            off.abs() <= 0.5,
                            "{style:?}/{density:?}/{font:?}: {:?} off by {off}",
                            t.galley.text()
                        );
                        texts += 1;
                    }
                }
            }
        }
        assert!(texts > 10, "bar painted {texts} texts");
    }

    #[test]
    fn bar_segments_are_vertically_centered() {
        assert_centered(UiStyle::Blueprint, Density::Compact, UiFont::Auto);
        assert_centered(UiStyle::Neutral, Density::Compact, UiFont::Auto);
        assert_centered(UiStyle::Neon, Density::Comfortable, UiFont::Auto);
        assert_centered(UiStyle::Blueprint, Density::Compact, UiFont::MartianMono);
        assert_centered(
            UiStyle::Neutral,
            Density::Comfortable,
            UiFont::BricolageGrotesque,
        );
        assert_centered(UiStyle::Neon, Density::Compact, UiFont::Inter);
        assert_centered(UiStyle::Phosphor, Density::Compact, UiFont::VictorMono);
    }

    /// The bar is a full-width strip of exactly `BAR_H` at the bottom of what the sidebar leaves:
    /// it never overlaps the sidebar, which keeps the whole window height.
    #[test]
    fn bar_is_a_bottom_strip_of_the_tab_area_and_leaves_the_sidebar_alone() {
        let ctx = egui::Context::default();
        crate::ui_fonts::install(&ctx, crate::ui_fonts::UiFonts::none());
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        crate::ui_fonts::finalize(&ctx);
        theme::apply(
            &ctx,
            UiStyle::Blueprint,
            Density::Compact,
            UiFont::Auto,
            true,
        );
        let v = BarView::default();
        let cat = ModelsCache::default();
        let mut picker = PickerState::default();
        let input = BarInput {
            catalog: &cat,
            favorites: &[],
            provider: None,
            activity: Activity::Idle,
            pending: None,
            assumed: None,
            actions: ActionCtx::default(),
            agents: AgentsChip::default(),
            session: SessionExtras {
                tab: 0,
                task_unavailable: None,
            },
        };
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1400.0, 700.0))),
            ..Default::default()
        };
        let (mut sidebar, mut before, mut after) = (Rect::NOTHING, Rect::NOTHING, Rect::NOTHING);
        for _ in 0..3 {
            ctx.run_ui(raw.clone(), |ui| {
                sidebar = egui::Panel::left("sidebar")
                    .exact_size(300.0)
                    .show(ui, |_| {})
                    .response
                    .rect;
                before = ui.available_rect_before_wrap();
                show(ui, &v, &mut picker, &input);
                after = ui.available_rect_before_wrap();
            })
            .textures_delta
            .clear();
        }
        assert_eq!(sidebar.height(), 700.0, "the sidebar keeps the full height");
        assert!(
            before.left() >= sidebar.right() - 0.5,
            "{before:?} vs {sidebar:?}"
        );
        assert_eq!(
            (after.left(), after.right()),
            (before.left(), before.right())
        );
        assert_eq!(after.top(), before.top());
        assert_eq!(before.bottom() - after.bottom(), BAR_H);
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use deck_core::config::{Density, UiFont, UiStyle};
    use deck_core::status::PrInfo;

    /// An Anthropic account with a 5 h and a weekly window.
    const USAGE: &str = r#"{"generatedAt":1000,"reports":[{"provider":"anthropic","fetchedAt":1000,
      "limits":[
       {"id":"a:5h","label":"Claude 5 Hour","scope":{"windowId":"5h"},"window":{"id":"5h","label":"5 Hour","resetsAt":8040000},
        "amount":{"usedFraction":0.37,"unit":"percent"},"status":"ok"},
       {"id":"a:7d","label":"Claude 7 Day","scope":{"windowId":"7d"},"window":{"id":"7d","label":"7 Day","resetsAt":277200000},
        "amount":{"usedFraction":0.815,"unit":"percent"},"status":"ok"}],
      "metadata":{"email":"me@example.test","planType":"pro"}}],
      "accountsWithoutUsage":[],"disabledCredentials":[]}"#;

    const FOLDER: &str = "bar-usage-fix";
    const WORKTREE: &str = "/work/OmpDeck-worktrees/bar-usage-fix";
    const BRANCH: &str = "feat/statusbar-usage";

    /// What of the bar's droppable segments a render painted.
    #[derive(Debug, Default, PartialEq)]
    struct Shown {
        ctx: bool,
        usage: bool,
        git: bool,
        cost: bool,
        tokens: bool,
        /// The path chip's text.
        path: Option<String>,
    }

    impl Shown {
        /// `[ctx, usage, git, cost, tokens, path]` in priority order.
        fn flags(&self) -> [bool; 6] {
            [
                self.ctx,
                self.usage,
                self.git,
                self.cost,
                self.tokens,
                self.path.is_some(),
            ]
        }
    }

    /// The bar of a session with everything to show, rendered in the user's setup (Lexend UI
    /// font, Neutral style, Compact density).
    struct Rig {
        ctx: egui::Context,
        view: BarView,
        cat: ModelsCache,
        picker: PickerState,
    }

    impl Rig {
        fn new(cwd: &str) -> Rig {
            let ctx = egui::Context::default();
            crate::ui_fonts::install(&ctx, crate::ui_fonts::UiFonts::none());
            ctx.run_ui(egui::RawInput::default(), |_| {})
                .textures_delta
                .clear();
            crate::ui_fonts::finalize(&ctx);
            theme::apply(
                &ctx,
                UiStyle::Neutral,
                Density::Compact,
                UiFont::Lexend,
                true,
            );
            let mut cat = ModelsCache::default();
            cat.finish(Ok(vec![ModelInfo {
                selector: "anthropic/claude-opus-5-5".to_owned(),
                provider: "anthropic".to_owned(),
                id: "claude-opus-5-5".to_owned(),
                name: "Claude Opus 5.5".to_owned(),
                context_window: Some(1_000_000),
                thinking: vec!["low".into(), "high".into()],
                ..Default::default()
            }]));
            let st = SessionStatus {
                cwd: cwd.into(),
                stats: SessionStats {
                    model: Some("anthropic/claude-opus-5-5".into()),
                    thinking: Some("high".into()),
                    input_tokens: 50_000,
                    output_tokens: 200,
                    cost: 7.0,
                    context_tokens: Some(173_000),
                    ..SessionStats::default()
                },
                git: Some(GitInfo {
                    branch: BRANCH.into(),
                    ahead: 2,
                    behind: 1,
                    staged: 1,
                    unstaged: 3,
                    untracked: 2,
                    pr: Some(PrInfo {
                        number: 12,
                        url: "u".into(),
                    }),
                    ..GitInfo::default()
                }),
                ..SessionStatus::default()
            };
            let mut view = BarView::default();
            view.update(Some(&Arc::new(st)), Path::new("/x"), &cat);
            let usage = deck_core::usage::UsageSnapshot::parse(USAGE).expect("fixture parses");
            view.set_usage(Some(&Arc::new(Summary::build(&usage, 0))));
            Rig {
                ctx,
                view,
                cat,
                picker: PickerState::default(),
            }
        }

        /// Renders the bar at `width` and reports which segments it painted.
        fn shown(&mut self, width: f32) -> Shown {
            let input = BarInput {
                catalog: &self.cat,
                favorites: &[],
                provider: None,
                activity: Activity::Idle,
                pending: None,
                assumed: None,
                actions: ActionCtx::default(),
                agents: AgentsChip::default(),
                session: SessionExtras {
                    tab: 0,
                    task_unavailable: None,
                },
            };
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(width, 700.0))),
                ..Default::default()
            };
            let mut texts = Vec::new();
            for i in 0..3 {
                let mut out = self.ctx.run_ui(raw.clone(), |ui| {
                    show(ui, &self.view, &mut self.picker, &input);
                });
                out.textures_delta.clear();
                if i == 2 {
                    for cs in &out.shapes {
                        if let Shape::Text(t) = &cs.shape {
                            let r = t.galley.rect.translate(t.pos.to_vec2());
                            texts.push((t.galley.text().to_owned(), r));
                        }
                    }
                }
            }
            // The info row is what lies left of the actions row (it starts at "Agents").
            let actions = texts
                .iter()
                .find(|(t, _)| t == "Agents")
                .map_or(width, |(_, r)| r.left());
            let info: Vec<&str> = texts
                .iter()
                .filter(|(_, r)| r.right() <= actions)
                .map(|(t, _)| t.as_str())
                .collect();
            let has = |s: &str| info.contains(&s);
            Shown {
                ctx: has("17.3%/1M"),
                usage: has("5h") && has("wk"),
                git: has(BRANCH),
                cost: has("$7.00"),
                tokens: has("50K"),
                path: info
                    .iter()
                    .find(|t| t.ends_with(FOLDER))
                    .map(|t| (*t).to_owned()),
            }
        }
    }

    #[test]
    fn usage_chip_shows_with_ctx_and_git_at_the_users_bar_width() {
        // 1920 px window, 425 px sidebar: the bar is about 1470 px.
        let mut rig = Rig::new(WORKTREE);
        let at_user = rig.shown(1470.0);
        assert!(
            at_user.ctx && at_user.usage && at_user.git,
            "1470 px shows ctx, usage and git: {at_user:?}"
        );
        let narrow = rig.shown(1100.0);
        assert!(
            narrow.ctx && narrow.usage,
            "1100 px keeps ctx and usage: {narrow:?}"
        );
        // The usage chip carries both windows.
        assert!(rig.view.usage.as_ref().is_some_and(|u| u.chip.is_some()));
    }

    #[test]
    fn path_elides_instead_of_pushing_usage_out() {
        let mut rig = Rig::new(WORKTREE);
        // A bar wide enough for everything shows the path whole.
        let wide = rig.shown(3200.0);
        assert_eq!(wide.path.as_deref(), Some(WORKTREE), "{wide:?}");
        assert!(wide.flags().iter().all(|s| *s), "{wide:?}");

        // Narrowing keeps usage and elides the path from the front until it goes.
        let mut elided = None;
        let mut width = 3200.0;
        let mut last_len = usize::MAX;
        while width >= 1100.0 {
            let s = rig.shown(width);
            assert!(s.usage, "usage dropped at {width}px for the path: {s:?}");
            if let Some(p) = &s.path {
                let len = p.chars().count();
                assert!(len <= last_len, "the path grew at {width}px: {p}");
                last_len = len;
                if p.starts_with('…') {
                    assert!(p.ends_with(FOLDER), "{p}");
                    elided.get_or_insert(p.clone());
                }
            }
            width -= 5.0;
        }
        assert!(
            elided.is_some(),
            "the path never elided between 3200 and 1100 px"
        );
        // The hover and click-to-copy keep the whole path.
        assert_eq!(rig.view.path_full, WORKTREE);
        assert_eq!(rig.view.path_text, WORKTREE);
    }

    #[test]
    fn segments_drop_in_priority_order_and_never_come_back_on_a_narrower_bar() {
        let mut rig = Rig::new(WORKTREE);
        let mut prev = [true; 6];
        let mut width = 3200.0;
        while width >= 500.0 {
            let now = rig.shown(width).flags();
            for i in 0..6 {
                assert!(prev[i] || !now[i], "segment {i} came back at {width}px");
                // ctx, usage, git, cost, tokens, path: never one without all before it.
                assert!(
                    i == 0 || !now[i] || now[i - 1],
                    "segment {i} shown without {} at {width}px",
                    i - 1
                );
            }
            prev = now;
            width -= 5.0;
        }
    }
}
