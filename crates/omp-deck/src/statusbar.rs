//! The bottom status bar: what omp's own status line shows for the active session (activity,
//! model + thinking, path, git, context meter, tokens, cost) plus the session action buttons.
//!
//! Strings are formatted once per status snapshot ([`BarView::update`]); drawing only paints.

use std::path::Path;
use std::sync::Arc;

use crate::icons::ICON_GAUGE;
use crate::icons::{
    ICON_ACTIVITY, ICON_ARROW_DOWN, ICON_ARROW_UP, ICON_BRAIN, ICON_CHART_PIE,
    ICON_CHEVRONS_UP_DOWN, ICON_CIRCLE_ALERT, ICON_CIRCLE_CHECK, ICON_CIRCLE_STOP, ICON_COPY,
    ICON_DOWNLOAD, ICON_ELLIPSIS, ICON_FOLD_VERTICAL, ICON_FOLDER, ICON_FOLDER_GIT_2,
    ICON_GIT_BRANCH, ICON_GIT_FORK, ICON_GIT_MERGE, ICON_HANDSHAKE, ICON_HISTORY, ICON_LIST_TREE,
    ICON_MAP, ICON_NETWORK, ICON_PENCIL, ICON_ROTATE_CCW, ICON_SPARKLES, ICON_SQUARE, ICON_VIBRATE,
};
use deck_core::ctl::{ContextBreakdown, LiveAccount, LiveModel};
use deck_core::omp::ModelInfo;
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

use crate::model_picker::{self, ModelsCache, PickerAction, PickerInput, PickerState};
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

pub const BAR_H: f32 = 41.0;
const CHIP_H: f32 = 29.0;
const PATH_CHARS: usize = 44;
const CHIP_PAD: f32 = 7.0;
/// Horizontal space a chip adds around its content: both pads plus the one item gap measured
/// on screen (so a chip asked to be N px wide really is N px wide).
const CHIP_CHROME_W: f32 = 2.0 * CHIP_PAD + 6.0;
/// The model chip's width in every session (a 213 px baseline widened by 30 %); it does not follow
/// the path chip, so a long path (a worktree) cannot push the other segments out.
const MODEL_CHIP_W: f32 = 213.0 * 1.3;
const PIE_BOX: f32 = 18.0;
const CARET_W: f32 = 16.8;

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
        self.path_text = statusfmt::display_path(&s.cwd, PATH_CHARS);

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
}

/// What the status bar's "Agents" chip shows.
#[derive(Clone, Copy, Debug, Default)]
pub struct AgentsChip {
    pub counts: Counts,
    /// The agents dashboard is open.
    pub open: bool,
}

// ------------------------------------------------------------------ drawing

/// Draws the bar as a bottom panel and returns what the user asked for.
pub fn show(
    ui: &mut Ui,
    view: &BarView,
    picker: &mut PickerState,
    input: &BarInput<'_>,
) -> Vec<BarAction> {
    let pal = Palette::of_ui(ui);
    let mut out = Vec::new();
    let panel = egui::Panel::bottom("status-bar")
        .resizable(false)
        .exact_size(BAR_H)
        .show_separator_line(false)
        .frame(
            Frame::NONE
                .fill(pal.bar)
                .inner_margin(Margin::symmetric(12, 0)),
        )
        .show(ui, |ui| {
            let full = ui.max_rect();
            let right = ui.scope_builder(
                UiBuilder::new()
                    .max_rect(full)
                    .layout(Layout::right_to_left(Align::Center)),
                |ui| actions_row(ui, input, &mut out),
            );
            let left_max = (right.response.rect.left() - 8.0).max(full.left());
            let left_rect = Rect::from_min_max(full.min, pos2(left_max, full.max.y));
            ui.scope_builder(
                UiBuilder::new()
                    .max_rect(left_rect)
                    .layout(Layout::left_to_right(Align::Center)),
                |ui| {
                    ui.set_clip_rect(left_rect.intersect(ui.clip_rect()));
                    info_row(ui, view, picker, input, &mut out);
                },
            );
        });
    let r = panel.response.rect;
    if theme::is_gallery() || theme::look().separated {
        theme::skin::paint_edge(ui.painter(), r, theme::skin::Edge::Top, pal);
    } else {
        ui.painter().hline(
            r.x_range(),
            r.top() + 0.5,
            Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
        );
    }
    out
}

/// The droppable segments of the info row, most important first.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seg {
    Ctx,
    Git,
    Path,
    Cost,
    Tokens,
    Usage,
}

const SEGS: [Seg; 6] = [
    Seg::Ctx,
    Seg::Git,
    Seg::Path,
    Seg::Cost,
    Seg::Tokens,
    Seg::Usage,
];

/// Real widths (divider + chip, without the spacing before them) of every segment for one view
/// revision, usage chip page and pixel density.
#[derive(Clone, Copy)]
struct SegWidths {
    key: (u64, Option<usize>, u32, deck_core::config::UiFont),
    w: [f32; 6],
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

fn seg_widths(ui: &mut Ui, v: &BarView) -> [f32; 6] {
    let key = (
        v.rev,
        // The pager's pick lives in egui memory, outside `rev`.
        usage_chip_card(ui.ctx(), v).map(|(_, i)| i),
        ui.ctx().pixels_per_point().to_bits(),
        theme::ui_font(),
    );
    let id = ui.scope_id().with("seg-widths");
    if let Some(c) = ui.data(|d| d.get_temp::<SegWidths>(id))
        && c.key == key
    {
        return c.w;
    }
    let mut w = [0.0; 6];
    let mut sink = Vec::new();
    for (i, s) in SEGS.into_iter().enumerate() {
        w[i] = measure(ui, |ui| segment(ui, v, s, &mut sink));
    }
    ui.data_mut(|d| d.insert_temp(id, SegWidths { key, w }));
    w
}

fn info_row(
    ui: &mut Ui,
    v: &BarView,
    picker: &mut PickerState,
    input: &BarInput<'_>,
    out: &mut Vec<BarAction>,
) {
    ui.spacing_mut().item_spacing.x = 6.0;

    activity_dot(ui, input.activity);
    ui.add_space(3.0);
    // The model chip has one width in every session, but never takes over the bar.
    let widths = seg_widths(ui, v);
    let gap = ui.spacing().item_spacing.x;
    let (chip_w, cap) = model_chip_width(ui.available_width());
    model_chip(ui, v, picker, input, (chip_w, cap), out);

    // Priority-based fit on measured widths: drop the least important segments first.
    let present = SEGS.map(|s| match s {
        Seg::Git => v.git.is_some(),
        Seg::Cost => v.show_cost,
        Seg::Tokens => v.show_tokens,
        Seg::Usage => v.usage.as_ref().is_some_and(|u| u.chip.is_some()),
        Seg::Ctx | Seg::Path => true,
    });
    let shown = fit_segments(&widths, &present, ui.available_width() - gap, gap);
    // Drawn in reading order: path, git, ctx, tokens, cost, usage.
    for s in [
        Seg::Path,
        Seg::Git,
        Seg::Ctx,
        Seg::Tokens,
        Seg::Cost,
        Seg::Usage,
    ] {
        if shown[SEGS.iter().position(|x| *x == s).unwrap_or(0)] {
            segment(ui, v, s, out);
        }
    }
}

/// Model chip `(width, cap)`: [`MODEL_CHIP_W`], capped at 52% of the bar so it never takes over a
/// narrow bar.
fn model_chip_width(bar_w: f32) -> (f32, f32) {
    let cap = (bar_w * 0.52).max(195.0);
    (MODEL_CHIP_W.min(cap), cap)
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

/// One segment with its leading divider.
fn segment(ui: &mut Ui, v: &BarView, seg: Seg, out: &mut Vec<BarAction>) {
    let pal = Palette::of_ui(ui);
    match seg {
        Seg::Path => {
            divider(ui);
            let r = chip(ui, "path", Sense::click(), |ui| {
                ui.label(icon_text(ICON_FOLDER, 14.4, pal.weak));
                ui.add(text(&v.path_text, 14.4, pal.text));
            });
            let r = r.on_hover_cursor(CursorIcon::PointingHand);
            path_hover(&r, &v.path_full);
            if r.clicked() {
                out.push(BarAction::CopyPath(v.path_full.clone()));
            }
        }
        Seg::Git => {
            if let Some(g) = &v.git {
                divider(ui);
                git_chip(ui, g, out);
            }
        }
        Seg::Ctx => {
            divider(ui);
            context_meter(ui, v);
        }
        Seg::Tokens => {
            divider(ui);
            let r = chip(ui, "tokens", Sense::hover(), |ui| {
                ui.add(text("↑", 14.4, pal.weak));
                ui.add(text(&v.tokens_in, 14.4, pal.text));
                ui.add_space(4.0);
                ui.add(text("↓", 14.4, pal.weak));
                ui.add(text(&v.tokens_out, 14.4, pal.text));
            });
            tokens_hover(&r, &v.tokens_tip);
        }
        Seg::Cost => {
            divider(ui);
            let r = chip(ui, "cost", Sense::hover(), |ui| {
                ui.label(RichText::new(&v.cost).size(14.4).color(pal.ok));
            });
            cost_tip::hover(&r, &v.cost_tip);
        }
        Seg::Usage => {
            if let Some(u) = &v.usage
                && let Some(c) = &u.chip
            {
                divider(ui);
                let r = chip(ui, "usage", Sense::click(), |ui| {
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
    ui.label(icon_text(ICON_GAUGE, 15.6, pal.weak));
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
    Badge::new(ui.painter(), None, parts, tint, 13.0).show(ui);
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
        13.0,
    )
    .show(ui);
}

fn text(s: &str, size: f32, color: Color32) -> egui::Label {
    egui::Label::new(RichText::new(s).size(size).color(color)).selectable(false)
}

fn chip_radius() -> u8 {
    if theme::flat() { 6 } else { 10 }
}

/// A rounded segment with a hover plate; `add` lays out its contents on one line. The response
/// is registered after the contents, so it wins hit-testing and labels never swallow clicks.
fn chip(ui: &mut Ui, id: &'static str, sense: Sense, add: impl FnOnce(&mut Ui)) -> Response {
    let pal = Palette::of_ui(ui);
    let bg = ui.painter().add(Shape::Noop);
    let rect = ui
        .allocate_ui_with_layout(
            vec2(ui.available_width(), CHIP_H),
            // `ui.horizontal` would reserve only `interact_size.y` (24 px in Compact) centred in
            // the row and let the 29 px chip grow downwards from there, sitting 2.5 px low.
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
    if sense != Sense::hover() && r.hovered() {
        ui.painter().set(
            bg,
            Shape::rect_filled(
                r.rect,
                chip_radius(),
                theme::hover_fill(pal).gamma_multiply(0.85),
            ),
        );
    }
    r
}

fn divider(ui: &mut Ui) {
    let pal = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(1.0, 16.8), Sense::hover());
    ui.painter()
        .rect_filled(rect, 0, pal.stroke.gamma_multiply(0.8));
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
    v: &BarView,
    picker: &mut PickerState,
    input: &BarInput<'_>,
    (min_w, max_w): (f32, f32),
    out: &mut Vec<BarAction>,
) {
    let pal = Palette::of_ui(ui);
    let pending = input.pending;
    let assumed = input
        .assumed
        .filter(|_| v.model_selector.is_none() || v.model_is_default);
    // An assumed switch overrides only what it changed (a model, a level or both).
    let assumed_model = assumed.and_then(|a| a.selector.as_deref());
    let assumed_level = assumed.and_then(|a| a.level.as_deref());
    let resp = chip(ui, "model", Sense::click(), |ui| {
        if let Some(p) = pending {
            ui.add(crate::widgets::Spin::new(16.8, pal.accent));
            ui.add(text(
                &format!(
                    "switching to {}…",
                    match (&p.selector, &p.level) {
                        (Some(s), _) => input
                            .catalog
                            .get(s)
                            .map_or_else(|| s.clone(), |m| m.name.clone()),
                        (None, Some(l)) => format!("thinking {l}"),
                        (None, None) => String::new(),
                    }
                ),
                15.2,
                pal.accent,
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
        let col = if (v.model_name.is_empty() || v.model_is_default) && assumed.is_none() {
            pal.weak
        } else {
            pal.text_strong
        };
        // Effort glyph + label and the caret sit at the right edge of a fixed-width chip; the
        // model name takes the rest and ellipsizes.
        let effort = configured.filter(|c| *c == "auto").or(level.or(configured));
        let label_w = effort.map_or(0.0, |l| {
            ui.painter()
                .layout_no_wrap(
                    thinking_short(l).to_owned(),
                    crate::theme::font(14.4),
                    pal.accent,
                )
                .size()
                .x
        });
        let spacing = ui.spacing().item_spacing.x;
        let right_w = CARET_W + effort.map_or(0.0, |_| spacing + PIE_BOX + spacing + label_w);
        let name_text = name.as_str();
        let name_nat = ui
            .painter()
            .layout_no_wrap(name_text.to_owned(), crate::theme::font(15.0), col)
            .size()
            .x
            + 8.0;
        let inner_w = (min_w - CHIP_CHROME_W)
            .max((name_nat + spacing + right_w).min(max_w - CHIP_CHROME_W))
            .min(ui.available_width())
            .max(0.0);
        let name_w = (inner_w - right_w - spacing).max(0.0);
        ui.allocate_ui_with_layout(
            vec2(name_w, CHIP_H),
            Layout::left_to_right(Align::Center),
            |ui| {
                // allocate_ui_with_layout only keeps the content's rect: pin the floor.
                ui.set_min_size(vec2(name_w, CHIP_H));
                ui.add(
                    egui::Label::new(
                        RichText::new(name_text)
                            .size(15.0)
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
                7.2,
                thinking_fill(l),
                pal.accent,
            );
            ui.add(text(thinking_short(l), 14.4, pal.accent));
        }
        ui.label(icon_text(ICON_CHEVRONS_UP_DOWN, 16.8, pal.weak));
    });
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    if !egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp)) {
        model_hover(&resp, &v.model_tip);
    }
    let acts = model_picker::show(
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
    );
    out.extend(acts.into_iter().map(BarAction::Picker));
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

fn git_chip(ui: &mut Ui, g: &GitView, out: &mut Vec<BarAction>) {
    let pal = Palette::of_ui(ui);
    let r = chip(ui, "git", Sense::click(), |ui| {
        ui.label(icon_text(
            if g.worktree_of.is_some() {
                ICON_FOLDER_GIT_2
            } else {
                ICON_GIT_BRANCH
            },
            18.0,
            if g.dirty { pal.warn } else { pal.ok },
        ));
        ui.add(text(&g.branch, 14.4, pal.text));
        if !g.sync.is_empty() {
            ui.add(text(&g.sync, 13.8, pal.weak));
        }
        if !g.unstaged.is_empty() {
            ui.add(text(&g.unstaged, 13.8, pal.warn));
        }
        if !g.staged.is_empty() {
            ui.add(text(&g.staged, 13.8, pal.ok));
        }
        if !g.untracked.is_empty() {
            ui.add(text(&g.untracked, 13.8, pal.weak));
        }
    });
    let r = r.on_hover_cursor(CursorIcon::PointingHand);
    git_hover(&r, g);
    if r.clicked() {
        out.push(BarAction::ToggleChanges);
    }
    if let Some((label, url)) = &g.pr {
        let pr = chip(ui, "pr", Sense::click(), |ui| {
            ui.add(text("PR", 13.2, pal.weak));
            ui.label(
                RichText::new(label)
                    .size(14.4)
                    .color(pal.accent)
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

fn context_meter(ui: &mut Ui, v: &BarView) {
    let pal = Palette::of_ui(ui);
    let color = level_color(pal, v.ctx_level);
    let r = chip(ui, "ctx", Sense::hover(), |ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(81.0, 16.8), Sense::hover());
        let bar = Rect::from_center_size(rect.center(), vec2(65.0, 5.0));
        let p = ui.painter();
        p.rect_filled(bar, 3, pal.surface);
        if let Some(f) = v.ctx_frac {
            let w = (bar.width() * f).max(if f > 0.0 { 3.6 } else { 0.0 });
            let fill = Rect::from_min_size(bar.min, vec2(w, bar.height()));
            p.rect_filled(fill, 3, color);
        }
        let label_color = if v.ctx_level.is_some_and(|l| l != ContextLevel::Normal) {
            color
        } else {
            pal.text
        };
        ui.add(text(&v.ctx_label, 14.4, label_color));
    });
    ctx_tip::hover(&r, v.ctx_detail.as_ref().map(|(_, t)| &**t), &v.ctx_tip);
}

/// The glyph of one count in the Agents chip.
enum Glyph {
    Spinner,
    Icon(crate::icons::Icon),
}

/// `⟳ 2 · ✓ 5 · ✕ 1` — live counts of the active session's subagents; click toggles the dashboard.
fn agents_chip(ui: &mut Ui, v: &AgentsChip, out: &mut Vec<BarAction>) {
    let pal = Palette::of_ui(ui);
    let c = v.counts;
    let total = c.total();
    // The action row lays out right → left and `horizontal` inherits that: contents are emitted
    // in reverse reading order (last count first, number before its glyph, dashboard icon last).
    let r = chip(ui, "agents", Sense::click(), |ui| {
        let lit = v.open || c.running > 0;
        let icon_color = if lit {
            pal.accent
        } else if total == 0 {
            pal.weak.gamma_multiply(0.7)
        } else {
            pal.weak
        };
        if total == 0 {
            ui.add(text("Agents", 14.4, pal.weak.gamma_multiply(0.85)));
        } else {
            let mut first = true;
            let mut seg = |ui: &mut Ui, n: usize, glyph: Glyph, color: Color32| {
                if n == 0 {
                    return;
                }
                if !first {
                    ui.add(text("·", 14.4, pal.weak));
                }
                first = false;
                ui.add(text(&n.to_string(), 14.4, pal.text));
                match glyph {
                    Glyph::Spinner => {
                        ui.add(crate::widgets::Spin::new(15.6, color));
                    }
                    Glyph::Icon(i) => {
                        ui.label(icon_text(i, 16.8, color));
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
        ui.label(icon_text(ICON_NETWORK, 19.2, icon_color));
    });
    if v.open {
        let p = ui.painter();
        p.rect_filled(r.rect, chip_radius(), pal.accent.gamma_multiply(0.14));
        p.rect_stroke(
            r.rect,
            chip_radius(),
            Stroke::new(1.0, pal.accent.gamma_multiply(0.55)),
            egui::StrokeKind::Inside,
        );
    }
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

fn actions_row(ui: &mut Ui, input: &BarInput<'_>, out: &mut Vec<BarAction>) {
    let c = &input.actions;
    ui.spacing_mut().item_spacing.x = 3.0;

    // Added right → left: the Agents chip sits at the far right, under the dashboard.
    agents_chip(ui, &input.agents, out);
    ui.add_space(6.0);

    // Added right → left, so this reads: Stop/Retry · Plan · Handoff · Compact ▾ · Shake ▾ · Fork · Tree · ⋯
    let more = bar_button(ui, ICON_ELLIPSIS, "More actions", true, None, false);
    theme::dropup_menu(&more, 220.0, |ui| overflow_menu(ui, c, out));

    let tree = Kind::Tree;
    if bar_button(
        ui,
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
            if menu_item(ui, ICON_FOLD_VERTICAL, "Compact (soft)", None, true, false).clicked() {
                out.push(BarAction::Run(Action::Compact(CompactMode::Soft)));
            }
            if menu_item(ui, ICON_FOLD_VERTICAL, "Snapcompact", None, true, false).clicked() {
                out.push(BarAction::Run(Action::Compact(CompactMode::Snapcompact)));
            }
            ui.separator();
            if menu_item(ui, ICON_PENCIL, "Compact with focus…", None, true, false).clicked() {
                out.push(BarAction::AskCompactFocus);
            }
        });
    }

    let handoff = bar_button(
        ui,
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

    if session_actions::visible(Kind::Retry, c)
        && bar_button(
            ui,
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

/// Compact icon button; disabled ones dim and explain why in the tooltip. `emphasis` tints it.
fn bar_button(
    ui: &mut Ui,
    icon: crate::icons::Icon,
    tooltip: &str,
    enabled: bool,
    disabled_reason: Option<&str>,
    emphasis: bool,
) -> Response {
    let pal = Palette::of_ui(ui);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, CHIP_H), sense);
    if resp.clicked() {
        resp.surrender_focus();
    }
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered() && enabled, 0.1);
    let base = if emphasis { pal.warn } else { pal.weak };
    let (fg_rest, fg_hot) = if enabled {
        (base, if emphasis { pal.warn } else { pal.text_strong })
    } else {
        (pal.weak.gamma_multiply(0.4), pal.weak.gamma_multiply(0.4))
    };
    let p = ui.painter();
    p.rect_filled(
        rect,
        chip_radius(),
        theme::hover_fill(pal).gamma_multiply(0.9 * hov),
    );
    paint_icon(
        p,
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        21.6,
        theme::mix(fg_rest, fg_hot, hov),
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
        // ctx is narrow, path wide: greedy skipping would show ctx but not path at some width,
        // and path but not ctx at another.
        let widths = [60.0, 90.0, 300.0, 70.0, 80.0];
        let all = [true; 5];
        let mut prev = [true; 5];
        let mut w = 800.0;
        while w >= 0.0 {
            let now = fit_segments(&widths, &all, w, 6.0);
            for i in 0..5 {
                assert!(prev[i] || !now[i], "segment {i} came back at {w}px");
            }
            // Never a lower-priority segment without every higher-priority one.
            for i in 1..5 {
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
    fn model_chip_is_as_wide_in_every_session_and_segments_still_fit_monotonically() {
        assert!((model_chip_width(1400.0).0 - 276.9).abs() < 0.1);
        assert_eq!(model_chip_width(400.0).0, 208.0);
        // A worktree's long path chip leaves the model chip alone, so every segment still fits
        // a 1100 px bar (sized from the path, the chip took 546 px and usage was dropped).
        let widths = [60.0, 90.0, 420.0, 70.0, 80.0];
        let all = [true; 5];
        let (chip, _) = model_chip_width(1100.0);
        assert_eq!(fit_segments(&widths, &all, 1100.0 - chip - 6.0, 6.0), all);
        let mut prev = [true; 5];
        let mut bar = 1400.0;
        while bar >= 0.0 {
            let (chip, _) = model_chip_width(bar);
            let now = fit_segments(&widths, &all, bar - chip - 6.0, 6.0);
            for i in 0..5 {
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
        // Path no longer fits: tokens (lower priority) goes too.
        assert_eq!(
            fit_segments(&widths, &present, 150.0, 6.0),
            [true, false, false, false, false]
        );
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
}
