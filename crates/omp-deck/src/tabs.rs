//! Tab model, pure tab helpers, and the tab-strip widget (drawn inside the title bar).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use deck_core::config::SessionView;
use deck_core::mux::is_noise_title;
use deck_core::omp::UpdateKind;
use deck_service::controller::{ActionFacts, SessionTarget};
use deck_service::gateway::protocol::{BackendRef, ControllerInfo, SwitchInfo};
use deck_service::session_actions::ActionCtx;
use deck_service::statusfmt::Activity;
use deck_term::Terminal;

use crate::badge::{self, Hue};
use crate::icons::{
    ICON_CIRCLE_CHECK, ICON_CIRCLE_X, ICON_DOWNLOAD, ICON_MESSAGES_SQUARE, ICON_SPARKLES,
    ICON_TERMINAL, ICON_X, Icon,
};
use crate::titlebar::strip_omp_prefix;
use crate::{card, tips};
use deck_service::status::TabStatus;
use egui::{
    Align2, Color32, CursorIcon, Painter, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::sidebar::{self, MenuEnv, SidebarAction, SidebarOutput};
use crate::theme::{self, Palette, skin};
use crate::widgets::paint_icon;

fn session_icon(chat: bool) -> Icon {
    if chat {
        ICON_MESSAGES_SQUARE
    } else {
        ICON_TERMINAL
    }
}

/// What a terminal tab runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabRole {
    /// An omp session (resumed, new or forked).
    Session,
    /// `omp update …` (never persisted).
    Update(UpdateKind),
    /// omp's official installer, offered while omp is not found (never persisted).
    InstallOmp,
}

/// A terminal tab: a view of one session the gateway controls, rendered by deck-term.
pub struct Tab {
    /// This window's own view; dropping it only detaches (the session lives on in the gateway).
    term: Terminal,
    /// The session this window's view is attached to. A new generation (omp moved to another
    /// file, a restart) ends the view: [`Tab::needs_reattach`] tells the app to attach anew.
    target: SessionTarget,
    info: Option<ControllerInfo>,
    /// A session file this window resolved and sent to the gateway, until the record shows it.
    path_hint: Option<PathBuf>,
    /// The file the tab was launched on.
    launch_path: Option<PathBuf>,
    /// Session name in the backend; `None` for direct tabs (update/installer, or no backend).
    pub mux_name: Option<String>,
    pub cwd: PathBuf,
    /// Label used until the sidebar knows this session (restored state / click time).
    pub fallback_title: String,
    pub bell: bool,
    pub role: TabRole,
    /// The exit of an update tab has been acted upon.
    pub exit_handled: bool,
    /// Set while this tab runs `omp --fork` and the gateway has not reported the copy's file.
    pub copy: Option<CopyPending>,
    /// Stable identity of the tab (indices shift when tabs close).
    pub uid: u64,
    /// The control socket was connected at least once.
    pub ctl_seen: bool,
    /// When the tab was spawned (a freshly launched omp needs a moment to open its socket).
    pub spawned: Instant,
    /// The view the launch (or the last restart) reported; the gateway's record, once it
    /// describes the attached session, takes over (see [`Tab::view`]).
    view: SessionView,
}

/// A copy in progress: the tab runs `omp --fork <src>`; the gateway finds the new session file
/// and reports it.
pub struct CopyPending {
    /// Where the copy goes (shown in the success toast).
    pub dest: PathBuf,
}

impl Tab {
    /// A tab on the gateway's session `target`, shown through `term`.
    pub fn new(term: Terminal, target: SessionTarget, uid: u64, launch: TabLaunch) -> Tab {
        Tab {
            term,
            target,
            info: None,
            path_hint: None,
            launch_path: launch.session_path,
            mux_name: launch.mux_name,
            cwd: launch.cwd,
            fallback_title: launch.title,
            bell: false,
            role: launch.role,
            exit_handled: false,
            copy: None,
            uid,
            ctl_seen: false,
            spawned: Instant::now(),
            view: launch.view,
        }
    }
}

/// What a launch told the tab about itself.
pub struct TabLaunch {
    pub mux_name: Option<String>,
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,
    pub title: String,
    pub role: TabRole,
    /// The view the session runs in (as the gateway reported it at launch).
    pub view: SessionView,
}

impl Tab {
    /// The terminal, for rendering, user input and read-only queries.
    pub fn terminal(&self) -> &Terminal {
        &self.term
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal {
        &mut self.term
    }

    /// The session this tab shows now: the newest generation known (the gateway starts a new one
    /// when another file takes over or the session restarts).
    pub fn target(&self) -> &SessionTarget {
        match &self.info {
            Some(i) if i.target.generation >= self.target.generation => &i.target,
            _ => &self.target,
        }
    }

    /// The gateway's latest record of the session, `None` until it appeared in the cache.
    pub fn info(&self) -> Option<&ControllerInfo> {
        self.info.as_ref()
    }

    /// The view the session runs in: the gateway's record of the attached generation, until that
    /// arrives the one the launch or restart reported.
    pub fn view(&self) -> SessionView {
        match &self.info {
            Some(i)
                if i.target.key == self.target.key
                    && i.target.generation >= self.target.generation =>
            {
                i.view
            }
            _ => self.view,
        }
    }

    /// A restart reported the view the relaunched session runs in.
    pub fn set_view(&mut self, view: SessionView) {
        self.view = view;
    }

    /// Takes the gateway's record. A recorded file catches up with a hint sent by this window; a
    /// record of an older generation than the attached view is ignored. The backend session name
    /// follows the record.
    pub fn set_info(&mut self, info: &ControllerInfo) {
        if info.target.key == self.target.key && info.target.generation < self.target.generation {
            return;
        }
        if self.path_hint.is_some() && self.path_hint == info.session_path {
            self.path_hint = None;
        }
        if info.live_control {
            self.ctl_seen = true;
        }
        if let BackendRef::Mux { name, .. } = &info.backend {
            self.mux_name = Some(name.clone());
        }
        self.info = Some(info.clone());
    }

    /// The gateway started a newer generation of the session than the one this view is attached
    /// to: the view ended and must be replaced.
    pub fn needs_reattach(&self) -> bool {
        self.info.as_ref().is_some_and(|i| {
            i.target.key == self.target.key && i.target.generation > self.target.generation
        })
    }

    /// Replaces the view with one on `target`, the session's current generation (same tab).
    pub fn reattach(&mut self, target: SessionTarget, term: Terminal) {
        self.target = target;
        self.term = term;
        self.exit_handled = false;
    }

    /// This launch created the session with the control extension, so its socket is expected
    /// (attaching to an existing session ignores the argv).
    pub fn ctl_expect(&self) -> bool {
        self.info.as_ref().is_some_and(|i| i.ctl_expect)
    }

    /// The session file behind the tab: what this window asked for until the gateway's record
    /// shows it.
    pub fn session_path(&self) -> Option<&Path> {
        self.path_hint
            .as_deref()
            .or_else(|| self.info.as_ref().and_then(|i| i.session_path.as_deref()))
            .or(self.launch_path.as_deref())
    }

    pub fn session_path_owned(&self) -> Option<PathBuf> {
        self.session_path().map(Path::to_path_buf)
    }

    /// Remembers a session file this window resolved; the caller sends it to the gateway.
    pub fn hint_session_path(&mut self, path: Option<PathBuf>) {
        self.path_hint.clone_from(&path);
        if path.is_none() {
            self.launch_path = None;
        }
    }

    /// The backend session name, if any.
    pub fn mux_name(&self) -> Option<&str> {
        self.mux_name.as_deref()
    }

    /// The leading indicator: the gateway's record, else the terminal's own state; the bell only
    /// counts on a background tab.
    pub fn status(&self, active: bool) -> TabStatus {
        TabStatus::of(self.term.exit_code(), self.activity(), self.bell && !active)
    }

    /// The uid of a tab that may sit in a pane: only session tabs do (update and installer
    /// tabs always take the full width).
    pub fn pane_uid(&self) -> Option<u64> {
        (self.role == TabRole::Session).then_some(self.uid)
    }

    /// The title without omp's `π <glyph>` prefix: the status indicator shows that state.
    pub fn label<'a>(&'a self, labels: &'a HashMap<PathBuf, String>) -> &'a str {
        // Restored tabs fall back to the stored (raw) title until omp sets one.
        strip_omp_prefix(self.pick_title(labels, self.terminal().title().map(strip_omp_prefix)))
    }

    /// The idle icon and its accent (`sel` = selection 0..=1, mixing from `weak`).
    pub fn kind_icon(&self, pal: &Palette, sel: f32) -> (Icon, Color32) {
        if matches!(self.role, TabRole::Update(_) | TabRole::InstallOmp) {
            (
                ICON_DOWNLOAD,
                theme::mix(pal.weak, pal.accent_a, sel.max(0.6)),
            )
        } else {
            (
                session_icon(self.term.tsp().live()),
                theme::mix(pal.weak, pal.accent_b, sel),
            )
        }
    }

    pub fn activity(&self) -> Activity {
        self.info
            .as_ref()
            .map_or_else(|| Activity::of_terminal(&self.term), |i| i.activity)
    }

    pub fn pending_switch(&self) -> Option<&SwitchInfo> {
        self.info.as_ref().and_then(|i| i.pending_switch.as_ref())
    }

    pub fn assumed_switch(&self) -> Option<&SwitchInfo> {
        self.info.as_ref().and_then(|i| i.assumed_switch.as_ref())
    }

    /// The model the session runs as far as the gateway knows.
    pub fn current_model(&self) -> Option<String> {
        self.info.as_ref().and_then(|i| i.current_model.clone())
    }

    /// The availability inputs of the session actions.
    pub fn action_ctx(&self, facts: ActionFacts) -> ActionCtx {
        let activity = self.activity();
        ActionCtx {
            busy: activity.is_busy(),
            idle: activity == Activity::Idle,
            has_session_file: self.session_path().is_some(),
            last_turn_failed: facts.last_turn_failed,
            has_last_reply: facts.has_last_reply,
            exited: self.term.is_exited(),
            omp_found: facts.omp_found,
        }
    }

    /// OSC title if non-empty, else the session label, else the stored title, else cwd basename.
    pub fn title<'a>(&'a self, labels: &'a HashMap<PathBuf, String>) -> &'a str {
        self.pick_title(labels, self.terminal().title())
    }

    fn pick_title<'a>(
        &'a self,
        labels: &'a HashMap<PathBuf, String>,
        osc: Option<&'a str>,
    ) -> &'a str {
        match self.role {
            TabRole::Update(kind) => return crate::update::tab_title(kind),
            TabRole::InstallOmp => return "Install omp",
            TabRole::Session => {}
        }
        osc.filter(|t| !is_noise_title(t))
            .or_else(|| {
                self.session_path()
                    .and_then(|p| labels.get(p))
                    .map(String::as_str)
            })
            .or_else(|| Some(self.fallback_title.as_str()).filter(|t| !t.is_empty()))
            .or_else(|| self.cwd.file_name().and_then(|n| n.to_str()))
            .unwrap_or("omp")
    }
}

/// Paints `status` centred at `c` in an icon slot of `size` px; returns false for
/// [`TabStatus::Idle`], which the caller paints itself. Shared by the tab strip and the sidebar.
pub fn paint_status(ui: &Ui, c: Pos2, size: f32, status: TabStatus) -> bool {
    let pal = Palette::of_ui(ui);
    let p = ui.painter();
    match status {
        TabStatus::Exited { clean } => {
            let (icon, col) = if clean {
                (ICON_CIRCLE_CHECK, pal.ok)
            } else {
                (ICON_CIRCLE_X, pal.danger)
            };
            paint_icon(p, c, Align2::CENTER_CENTER, icon, size, col);
        }
        TabStatus::Attention => paint_attention_badge(p, pal, c, size),
        TabStatus::Busy => crate::widgets::Spin::new(size * 0.8, pal.accent_b).paint_at(ui, c),
        TabStatus::Bell => theme::glow_dot(p, c, size * 0.22, pal.warn, theme::GLOW_REST),
        TabStatus::Idle => return false,
    }
    true
}

/// Solid warn disc with a drawn `!` (font independent, legible at a glance).
fn paint_attention_badge(p: &Painter, pal: &Palette, c: Pos2, size: f32) {
    let r = size * 0.44;
    theme::glow_dot(p, c, r, pal.warn, theme::GLOW_REST);
    let ink = Stroke::new((size * 0.12).max(1.6), pal.bg);
    p.line_segment([c + vec2(0.0, -r * 0.5), c + vec2(0.0, r * 0.12)], ink);
    p.circle_filled(c + vec2(0.0, r * 0.5), ink.width * 0.6, pal.bg);
}

/// Warn wash and outline over a row/pill whose tab waits for the user.
pub fn paint_attention_pill(p: &Painter, pal: &Palette, rect: Rect, radius: u8) {
    let (fill, stroke) = attention_tone(pal);
    p.rect(rect, radius, fill, stroke, StrokeKind::Inside);
}

/// Fill and outline of the attention wash.
fn attention_tone(pal: &Palette) -> (Color32, Stroke) {
    (
        pal.warn.gamma_multiply(0.13),
        Stroke::new(1.0, pal.warn.gamma_multiply(0.7)),
    )
}

/// The tab that was open on a session being moved.
pub struct MovedTab {
    pub tab: ClosedTab,
    /// Where it sat in the strip; it is reopened there.
    pub anchor: Anchor,
    /// `Some` when it was the active tab: the activation counter after it closed, so the
    /// reopened tab only takes focus if the user has not switched since.
    pub refocus: Option<u64>,
}

/// What to relaunch when a closed tab is reopened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosedTab {
    pub mux_name: Option<String>,
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,
    pub title: String,
    /// The view to open it in again.
    pub view: SessionView,
}

impl ClosedTab {
    pub fn session_path(&self) -> Option<&Path> {
        self.session_path.as_deref()
    }

    pub fn session_path_owned(&self) -> Option<PathBuf> {
        self.session_path.clone()
    }
}

/// Where a relaunched tab goes, remembered as its neighbours' uids (not an index): tabs that
/// close or open while the relaunch is prepared do not displace it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Anchor {
    /// The nearest terminal tab on the left: the new tab goes right after it.
    pub after: Option<u64>,
    /// The nearest terminal tab on the right: used when the left neighbour is gone.
    pub before: Option<u64>,
}

impl Anchor {
    /// The anchor of strip position `slot`; `uids` lists the strip.
    pub fn at(uids: &[u64], slot: usize) -> Anchor {
        let slot = slot.min(uids.len());
        Anchor {
            after: uids[..slot].last().copied(),
            before: uids[slot..].first().copied(),
        }
    }

    /// The strip position to insert at: after the left neighbour, else before the right one,
    /// else at the end.
    pub fn index(&self, uids: &[u64]) -> usize {
        let find = |want: Option<u64>| want.and_then(|w| uids.iter().position(|u| *u == w));
        match (find(self.after), find(self.before)) {
            (Some(i), _) => i + 1,
            (None, Some(j)) => j,
            (None, None) => uids.len(),
        }
    }
}

/// Bounded LIFO of closed tabs.
#[derive(Debug)]
pub struct ReopenStack<T> {
    items: Vec<T>,
    cap: usize,
}

impl<T> ReopenStack<T> {
    pub fn new(cap: usize) -> Self {
        ReopenStack {
            items: Vec::new(),
            cap,
        }
    }

    pub fn push(&mut self, item: T) {
        if self.items.len() == self.cap {
            self.items.remove(0);
        }
        self.items.push(item);
    }

    pub fn pop(&mut self) -> Option<T> {
        self.items.pop()
    }

    /// Drop every entry `keep` rejects.
    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.items.retain(keep);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Index after moving `delta` (±1) from `cur`, wrapping around `len`. `len == 0` → 0.
pub fn wrap_index(cur: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (cur as isize + delta).rem_euclid(len as isize) as usize
}

/// `omp` or `omp --resume <abs path>`.
pub fn omp_argv(session_path: Option<&Path>) -> Vec<String> {
    let mut argv = vec!["omp".to_owned()];
    if let Some(p) = session_path {
        argv.push("--resume".to_owned());
        argv.push(p.to_string_lossy().into_owned());
    }
    argv
}

/// Active index after removing tab `removed` from a list that had `len_before` tabs.
pub fn active_after_close(active: usize, removed: usize, len_before: usize) -> usize {
    let len = len_before.saturating_sub(1);
    let adjusted = if removed < active { active - 1 } else { active };
    adjusted.min(len.saturating_sub(1))
}

#[derive(Debug)]
pub enum TabAction {
    Activate(usize),
    Close(usize),
    /// Picked from the tab's context menu (the same menu as its "Open tabs" sidebar row).
    Menu(SidebarAction),
}

pub struct TabBarInput<'a> {
    pub tabs: &'a [Tab],
    pub labels: &'a HashMap<PathBuf, String>,
    pub active: usize,
    pub scroll_to_active: bool,
    pub menu: MenuEnv<'a>,
    /// An overlay owns the pointer: no hover tooltips on the strip.
    pub modal_open: bool,
    /// The tabs shown in the panes of a split (empty when not split): marked in the strip.
    pub shown: &'a [Option<u64>],
    /// The agents dashboard covers the terminals: no tab counts as seen.
    pub dashboard: bool,
}

/// Height of a tab's hit area (the full bar height); the pill is painted inset.
pub fn tab_hit_h() -> f32 {
    theme::dens(42.0, 46.0)
}

/// Gap between the tab row's edges and a tab pill (above and below).
const PILL_INSET_Y: f32 = 5.0;
const MAX_TEXT_W: f32 = 170.0;
const CLOSE_W: f32 = 20.0;
const ICON_W: f32 = 16.0;

/// Draws the tab strip (horizontal, scrollable); returns the requested action (at most one per frame).
/// `fade` is the bar background at the strip's left and right ends (for the overflow fades).
pub fn show_tab_strip(
    ui: &mut Ui,
    input: &TabBarInput<'_>,
    fade: (egui::Color32, egui::Color32),
) -> Option<TabAction> {
    let mut action = None;
    let strip = ui.available_rect_before_wrap();
    let hover_strip = ui.rect_contains_pointer(strip);
    let out = egui::ScrollArea::horizontal()
        .id_salt("tab-strip")
        .auto_shrink([true, false])
        // Never wider than the room left of the right-hand buttons, however narrow the window.
        .max_width(strip.width().max(0.0))
        .min_scrolled_width(0.0)
        .scroll_source(egui::scroll_area::ScrollSource {
            scroll_bar: false,
            drag: egui::scroll_area::DragScroll::Never,
            mouse_wheel: true,
        })
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            if hover_strip {
                // A plain vertical wheel scrolls the strip sideways.
                let dy = ui.input(|i| i.smooth_scroll_delta.y);
                if dy != 0.0 {
                    ui.scroll_with_delta(vec2(dy, 0.0));
                }
            }
            // The slanted outlines sit inside each tab's rect: the slant itself spaces them.
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                for (i, tab) in input.tabs.iter().enumerate() {
                    if let Some(a) = show_tab(ui, input, i, tab)
                        && action.is_none()
                    {
                        action = Some(a);
                    }
                }
            });
        });
    let view = out.inner_rect;
    let max_offset = (out.content_size.x - view.width()).max(0.0);
    if max_offset > 0.5 {
        let offset = out.state.clamped_offset().x;
        let p = ui.painter();
        let edge = vec2(26.0_f32.min(view.width() / 2.0), view.height());
        if offset > 0.5 {
            let r = egui::Rect::from_min_size(view.min, edge);
            theme::gradient_rect(p, r, fade.0, fade.0.gamma_multiply(0.0), true);
        }
        if offset < max_offset - 0.5 {
            let r = egui::Rect::from_min_size(pos2(view.right() - edge.x, view.top()), edge);
            theme::gradient_rect(p, r, fade.1.gamma_multiply(0.0), fade.1, true);
        }
    }
    action
}

fn show_tab(ui: &mut Ui, input: &TabBarInput<'_>, i: usize, tab: &Tab) -> Option<TabAction> {
    let pal = Palette::of_ui(ui);
    let title = tab.label(input.labels);
    let active = i == input.active;
    let shown = input.shown.contains(&Some(tab.uid));
    let status = tab.status((active || shown) && !input.dashboard);

    let text_color = if active { pal.text_strong } else { pal.text };
    let mut job = egui::text::LayoutJob::simple_singleline(
        title.to_owned(),
        crate::theme::font(13.5),
        text_color,
    );
    job.wrap.max_width = MAX_TEXT_W;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);

    let pill_h = tab_hit_h() - 2.0 * PILL_INSET_Y;
    // Content stays upright between the slanted edges: the slant widens the padding at both ends.
    let slant = skin::tab_slant(pill_h);
    let width = slant + 12.0 + ICON_W + 8.0 + galley.size().x + 6.0 + CLOSE_W + 6.0 + slant;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, tab_hit_h()), Sense::click());
    if input.scroll_to_active && active {
        ui.scroll_to_rect(rect.expand2(vec2(12.0, 0.0)), None);
    }
    let pill = rect.shrink2(vec2(0.0, PILL_INSET_Y));
    let inner = pill.shrink2(vec2(slant, 0.0));
    let close_rect = egui::Rect::from_center_size(
        pos2(inner.right() - 6.0 - CLOSE_W / 2.0, inner.center().y),
        vec2(CLOSE_W, CLOSE_W),
    );
    let close = ui.interact(close_rect, resp.id.with("close"), Sense::click());

    let ctx = ui.ctx().clone();
    // The ✕ is a widget on top of the tab and takes the hover from it: count either as the tab's.
    let hovered = resp.hovered() || close.hovered();
    let hov = ctx.animate_bool_with_time(resp.id.with("hov"), hovered, 0.12);
    let sel = ctx.animate_bool_with_time(resp.id.with("sel"), active, 0.18);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let outline = skin::paint_tab(
            p,
            pill,
            pal,
            skin::TabState {
                hov,
                sel,
                shown: shown && !active,
                dark: ui.visuals().dark_mode,
            },
        );
        if status == TabStatus::Attention {
            let (fill, stroke) = attention_tone(pal);
            crate::deco::fill(p, outline.clone(), fill);
            crate::deco::outline(p, outline, stroke);
        }
        // Leading icon / status.
        let ic = pos2(inner.left() + 12.0 + ICON_W / 2.0, inner.center().y);
        if !paint_status(ui, ic, ICON_W, status) {
            let (icon, col) = tab.kind_icon(pal, sel);
            paint_icon(p, ic, Align2::CENTER_CENTER, icon, 16.0, col);
        }
        p.galley(
            pos2(
                inner.left() + 12.0 + ICON_W + 8.0,
                inner.center().y - galley.size().y / 2.0,
            ),
            galley,
            text_color,
        );
        // Close ✕: visible on hover and for the active tab.
        let vis = hov.max(sel * 0.8);
        if vis > 0.01 {
            let ch = ctx.animate_bool_with_time(close.id.with("hov"), close.hovered(), 0.1);
            p.circle_filled(
                close_rect.center(),
                CLOSE_W / 2.0,
                theme::hover_fill(pal).gamma_multiply(ch),
            );
            paint_icon(
                p,
                close_rect.center(),
                Align2::CENTER_CENTER,
                ICON_X,
                14.0,
                theme::mix(pal.weak, pal.text_strong, ch).gamma_multiply(vis),
            );
        }
    }

    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    if !input.modal_open {
        hover_card(&resp, pal, tab, title, status);
    }

    let mut action = None;
    if close.clicked() || (resp.middle_clicked() && !close.hovered()) {
        action = Some(TabAction::Close(i));
    } else if resp.clicked() && !close.hovered() {
        action = Some(TabAction::Activate(i));
    }
    theme::context_menu(&resp, 230.0, |ui| {
        let mut out = SidebarOutput::default();
        sidebar::tab_menu(ui, i, tab, &input.menu, &mut out);
        if let Some(a) = out.actions.pop() {
            action = Some(TabAction::Menu(a));
        }
    });
    action
}

/// Hover card of a tab (the tab strip and the sidebar's "Open tabs" rows): title and live state
/// in the header, folder / session file / model / backend session in the body.
pub fn hover_card(resp: &egui::Response, pal: &Palette, tab: &Tab, title: &str, status: TabStatus) {
    const KEY_W: f32 = 76.0;
    let exit = tab.terminal().exit_code();
    let mut head =
        card::Header::new(tab.kind_icon(pal, 1.0).0, title).tone(tips::status_tone(pal, status));
    if let Some(hint) = status.hint() {
        head = head.subtitle(hint);
    }
    card::hover(
        resp,
        390.0,
        false,
        &head,
        |ui| {
            tips::status_badge(ui, status, exit);
        },
        |ui| {
            card::kv(ui, KEY_W, "Folder", |ui| tips::path(ui, &tab.cwd));
            if let Some(p) = tab.session_path() {
                card::kv(ui, KEY_W, "Session", |ui| tips::path(ui, p));
            }
            if let Some(m) = tab.info().and_then(|i| i.current_model.as_deref()) {
                card::kv(ui, KEY_W, "Model", |ui| {
                    let name = m.rsplit('/').next().unwrap_or(m);
                    badge::show(ui, Some(ICON_SPARKLES), name, Hue::Violet, tips::VALUE);
                });
            }
            if let Some(n) = &tab.mux_name {
                card::kv(ui, KEY_W, "tmux", |ui| tips::mono(ui, n));
            }
            card::footer_hints(
                ui,
                &[
                    ("click", "focus"),
                    ("middle-click", "close"),
                    ("right-click", "actions"),
                ],
            );
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_anchor_survives_a_tab_closing_in_front_of_it() {
        // Strip A B [slot] C D; the tab at the slot is being relaunched.
        let strip = [1, 2, 3, 4];
        let anchor = Anchor::at(&strip, 2);
        assert_eq!(
            anchor,
            Anchor {
                after: Some(2),
                before: Some(3)
            }
        );
        assert_eq!(anchor.index(&strip), 2);
        // A closes meanwhile: an absolute index 2 would now land after C.
        let closed_front = [2, 3, 4];
        assert_eq!(anchor.index(&closed_front), 1);
        // The left neighbour closes: fall back to the right one.
        assert_eq!(anchor.index(&[1, 3, 4]), 1);
        // Both neighbours gone: the end.
        assert_eq!(anchor.index(&[1, 4]), 2);
        // Edges.
        assert_eq!(Anchor::at(&[7], 1).after, Some(7));
        assert_eq!(
            Anchor::at(&[7], 0),
            Anchor {
                after: None,
                before: Some(7)
            }
        );
        assert_eq!(Anchor::at(&[], 0).index(&[]), 0);
    }

    #[test]
    fn wrap_switching() {
        assert_eq!(wrap_index(0, 3, 1), 1);
        assert_eq!(wrap_index(2, 3, 1), 0);
        assert_eq!(wrap_index(0, 3, -1), 2);
        assert_eq!(wrap_index(1, 3, -1), 0);
        assert_eq!(wrap_index(0, 1, 1), 0);
        assert_eq!(wrap_index(0, 0, 1), 0);
    }

    #[test]
    fn reopen_stack_lifo_and_cap() {
        let mut s = ReopenStack::new(2);
        assert!(s.is_empty());
        s.push(1);
        s.push(2);
        s.push(3);
        assert_eq!(s.pop(), Some(3));
        assert_eq!(s.pop(), Some(2));
        assert_eq!(s.pop(), None);
    }

    #[test]
    fn argv_forms() {
        assert_eq!(omp_argv(None), ["omp"]);
        assert_eq!(
            omp_argv(Some(Path::new("/a/b.jsonl"))),
            ["omp", "--resume", "/a/b.jsonl"]
        );
    }

    #[test]
    fn active_index_after_close() {
        assert_eq!(active_after_close(1, 0, 3), 0); // closed before active
        assert_eq!(active_after_close(1, 2, 3), 1); // closed after active
        assert_eq!(active_after_close(1, 1, 3), 1); // closed active, next slides in
        assert_eq!(active_after_close(2, 2, 3), 1); // closed last active
        assert_eq!(active_after_close(0, 0, 1), 0); // empty: clamped to 0
    }
}

#[cfg(test)]
mod session_icon_tests {
    #[test]
    fn live_chat_has_a_distinct_glyph() {
        assert_eq!(super::session_icon(true), super::ICON_MESSAGES_SQUARE);
        assert_eq!(super::session_icon(false), super::ICON_TERMINAL);
        assert_ne!(super::session_icon(true), super::session_icon(false));
    }
}
