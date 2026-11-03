//! The session manager hub: a large modal window with a projects pane, a sortable table of every
//! persisted omp session (sizes, usage, status), a detail pane and the bulk-action footer.
//!
//! All data and rules live in [`crate::sessions_model`]; this module only lays out and paints,
//! mutating the model for view-local state (scope, filter, sort, selection, detail) and returning
//! [`SessionsAction`]s for everything that touches the outside world.

mod resume;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use egui::epaint::text::Galley;
use egui::{
    Align, Align2, Color32, CursorIcon, Frame, Id, Key, Layout, Margin, Modifiers, Order, Painter,
    Rect, RichText, Sense, Sides, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::app::human_size;
use crate::col_alloc::{self, CELL_PAD};
use crate::icons::{
    ICON_ACTIVITY, ICON_ARROW_DOWN, ICON_ARROW_UP, ICON_CHECK, ICON_CIRCLE_STOP, ICON_COINS,
    ICON_COPY, ICON_CPU, ICON_EXTERNAL_LINK, ICON_FOLDER, ICON_FOLDER_INPUT, ICON_FOLDER_OPEN,
    ICON_FOLDER_OUTPUT, ICON_FOLDER_X, ICON_HARD_DRIVE, ICON_INFO, ICON_MESSAGES_SQUARE,
    ICON_MINUS, ICON_PENCIL, ICON_REFRESH_CW, ICON_SEARCH, ICON_SPARKLES, ICON_STAR, ICON_TRASH_2,
    ICON_X, Icon,
};
use crate::resume_model::{ResumeState, Source, Then};
use crate::running_model::{HubMode, RunModel};
use crate::sessions_model::{
    Badge, Check, Column, Filter, Model, Project, Row, StatsSlot, compact, plural,
    split_project_label, utc_stamp,
};
use crate::sidebar::format_age;
use crate::theme::{self, Palette};
use crate::transfer::TransferKind;
use crate::usage_fmt::money;
use crate::usage_view::tab_chip;
use crate::widgets::{DraftButton, DraftKind};
use crate::widgets::{Spin, empty_state, icon_text, paint_icon};
use deck_core::omp::ForeignSource;

pub(crate) const ROW_H: f32 = 34.0;
pub(crate) const HEAD_H: f32 = 30.0;
const FOOT_H: f32 = 46.0;
const PROJECTS_W: f32 = 184.0;
const DETAIL_W: f32 = 318.0;
const GAP: f32 = 10.0;
const PANE_MARGIN: f32 = 8.0;
const FILTER_ID: &str = "omp-deck-sessions-filter";

/// View-local state that is not part of the data model.
#[derive(Default)]
pub struct ViewState {
    /// Focus the filter box on the next frame (`/` and Ctrl+F).
    focus_filter: bool,
    /// Drop keyboard focus held outside the hub (the terminal) on the next frame.
    pub release_focus: bool,
    /// Resume mode's own state.
    pub resume: ResumeState,
}

/// What the app tells the view each frame.
pub struct ViewInput<'a> {
    /// A scan is running.
    pub scanning: bool,
    /// At least one scan finished (before that the table shows a spinner).
    pub loaded: bool,
    /// Which view of the hub is showing.
    pub mode: HubMode,
    /// A bulk operation in flight.
    pub op: Option<&'a OpProgress>,
    /// A dialog is open above the hub: it owns Esc.
    pub blocked: bool,
    pub now: SystemTime,
    /// omp is installed (delete / copy / move need it).
    pub omp: bool,
    /// Stopping a running session is possible (a session backend exists).
    pub can_stop: bool,
    /// Resume mode: the title of the tab "Resume here" acts on, or why there is none.
    pub here: Result<&'a str, &'static str>,
}

pub struct OpProgress {
    /// "Deleting", "Copying", "Moving", "Stopping".
    pub verb: &'static str,
    pub done: usize,
    pub total: usize,
}

#[derive(Debug)]
pub enum SessionsAction {
    Close,
    Refresh,
    /// Open the session in a tab (the hub closes).
    Open(PathBuf),
    Delete(Vec<PathBuf>),
    /// Copy / move to a project (`dest: None` = ask for a folder first).
    Transfer {
        kind: TransferKind,
        paths: Vec<PathBuf>,
        dest: Option<PathBuf>,
    },
    Stop(Vec<PathBuf>),
    /// The Sessions | Running switch.
    SetMode(HubMode),
    /// Kill these backend sessions (by mux name) after a confirmation.
    KillRunning(Vec<String>),
    CopyText {
        text: String,
        what: &'static str,
    },
    OpenFolder(PathBuf),
    /// Reveal the session file in the file manager (opens its folder).
    Reveal(PathBuf),
    /// Pin or unpin the session `id` in omp.
    Pin {
        id: String,
        pinned: bool,
    },
    /// Ask for a new title of the session in this file.
    Rename(PathBuf),
    /// Resume the omp session in this file into the tab the manager was opened from.
    ResumeHere(PathBuf),
    /// Resume mode lists another source now.
    SetSource(Source),
    /// Read the Claude Code / Codex list again.
    ReloadForeign(ForeignSource),
    /// Import the foreign session `id` (it ran in `cwd`), then resume it here or open it.
    ImportForeign {
        source: ForeignSource,
        id: String,
        cwd: String,
        then: Then,
    },
}

// ------------------------------------------------------------------ window

pub fn show(
    ctx: &egui::Context,
    model: &mut Model,
    run: &mut RunModel,
    st: &mut ViewState,
    input: &ViewInput<'_>,
) -> Vec<SessionsAction> {
    let mut out = Vec::new();
    let pal = Palette::of_ctx(ctx);
    let screen = ctx.content_rect();
    model.prepare(input.now);
    run.prepare(input.now);

    egui::Area::new(Id::unique("sessions-scrim"))
        .order(Order::Background)
        .fixed_pos(screen.min)
        .interactable(true)
        .show(ctx, |ui| {
            let (rect, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
            let dark = ctx.global_style().visuals.dark_mode;
            ui.painter().rect_filled(rect, 0, theme::backdrop(dark));
            if resp.clicked() && !input.blocked {
                out.push(SessionsAction::Close);
            }
        });

    if std::mem::take(&mut st.release_focus)
        && let Some(id) = ctx.memory(|m| m.focused())
    {
        ctx.memory_mut(|m| m.surrender_focus(id));
    }
    // Only the hub's own text field counts as typing; the terminal's focus must not.
    let typing = ctx.memory(|m| m.has_focus(Id::unique(FILTER_ID)));
    if input.mode == HubMode::Resume {
        resume::keys(ctx, model, st, input, &mut out);
    } else if !input.blocked && !egui::Popup::is_any_open(ctx) && !typing {
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            out.push(SessionsAction::Close);
        }
        if input.mode == HubMode::Sessions {
            if ctx.input_mut(|i| {
                i.consume_key(Modifiers::NONE, Key::Slash)
                    || i.consume_key(Modifiers::COMMAND, Key::F)
            }) {
                st.focus_filter = true;
            }
            if input.omp
                && input.op.is_none()
                && !model.sel.is_empty()
                && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete))
            {
                out.push(SessionsAction::Delete(selected_paths(model)));
            }
            session_keys(ctx, model, input, &mut out);
        }
    }

    let size = vec2(
        (screen.width() * 0.97)
            .clamp(980.0, 1560.0)
            .min(screen.width() - 24.0),
        (screen.height() * 0.9)
            .clamp(520.0, 980.0)
            .min(screen.height() - 24.0),
    );
    let margin = Margin::symmetric(18, 14);
    let frame = Frame::NONE.inner_margin(margin);
    egui::Window::new("sessions-window")
        .id(Id::unique("sessions-window"))
        .title_bar(false)
        .frame(frame)
        .resizable(true)
        .movable(true)
        .collapsible(false)
        .constrain_to(screen)
        .min_size(vec2(720.0, 420.0))
        .default_size(size)
        .default_pos(screen.center() - size / 2.0)
        .show(ctx, |ui| {
            crate::card::chrome(ui, margin, pal.bar, |ui| {
                body(ui, model, run, st, input, &mut out);
            });
        });
    out
}

/// omp's resume-picker keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowKey {
    Rename,
    Pin,
    Unpin,
    Delete,
}

/// Ctrl+R renames, Alt+K pins, Alt+Shift+K unpins, Ctrl+D deletes (omp's defaults). The shifted
/// chord is taken first so Alt+Shift+K is never read as Alt+K.
fn row_key(ctx: &egui::Context) -> Option<RowKey> {
    ctx.input_mut(|i| {
        if i.consume_key(Modifiers::ALT | Modifiers::SHIFT, Key::K) {
            Some(RowKey::Unpin)
        } else if i.consume_key(Modifiers::ALT, Key::K) {
            Some(RowKey::Pin)
        } else if i.consume_key(Modifiers::COMMAND, Key::R) {
            Some(RowKey::Rename)
        } else if i.consume_key(Modifiers::COMMAND, Key::D) {
            Some(RowKey::Delete)
        } else {
            None
        }
    })
}

/// What `key` does to the session `row`.
fn row_key_action(key: RowKey, row: &Row, input: &ViewInput<'_>) -> Option<SessionsAction> {
    if !input.omp {
        return None;
    }
    let idle = input.op.is_none();
    match key {
        RowKey::Pin | RowKey::Unpin => Some(SessionsAction::Pin {
            id: row.entry.id.clone(),
            pinned: key == RowKey::Pin,
        }),
        RowKey::Rename => idle.then(|| SessionsAction::Rename(row.path().to_path_buf())),
        RowKey::Delete => idle.then(|| SessionsAction::Delete(vec![row.path().to_path_buf()])),
    }
}

/// The picker keys on the session whose details are open.
fn session_keys(
    ctx: &egui::Context,
    model: &Model,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    let Some(row) = model.detail.as_deref().and_then(|p| model.row_by_path(p)) else {
        return;
    };
    if let Some(key) = row_key(ctx)
        && let Some(a) = row_key_action(key, row, input)
    {
        out.push(a);
    }
}

fn selected_paths(model: &Model) -> Vec<PathBuf> {
    model
        .selected_rows()
        .iter()
        .map(|r| r.entry.path.clone())
        .collect()
}

fn body(
    ui: &mut Ui,
    model: &mut Model,
    run: &mut RunModel,
    st: &mut ViewState,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    header(ui, model, run, input, out);
    ui.add_space(8.0);
    if input.mode == HubMode::Resume {
        resume::body(ui, model, st, input, out);
        return;
    }
    let sessions = input.mode == HubMode::Sessions;
    if sessions {
        filter_row(ui, model, st);
        ui.add_space(8.0);
    }

    let avail = ui.available_size();
    let body_h = (avail.y - FOOT_H - 8.0).max(120.0);
    // Fixed for the frame: a click in the table opens the pane on the next one, never halfway.
    let detail_open = model.detail.is_some();
    if sessions {
        ui.allocate_ui_with_layout(
            vec2(avail.x, body_h),
            Layout::left_to_right(Align::Min),
            |ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                pane(ui, PROJECTS_W, body_h, |ui| projects_pane(ui, model));
                let detail_w = if detail_open { DETAIL_W + GAP } else { 0.0 };
                let table_w = (ui.available_width() - detail_w).max(300.0);
                pane(ui, table_w, body_h, |ui| {
                    table_pane(ui, model, input, out);
                });
                if detail_open {
                    let w = ui.available_width().max(200.0);
                    pane(ui, w, body_h, |ui| {
                        detail_pane(ui, model, input, body_h, out);
                    });
                }
            },
        );
        if model.detail.is_some() != detail_open {
            ui.ctx().request_repaint();
        }
        ui.add_space(8.0);
        footer(ui, model, input, out);
    } else {
        pane(ui, avail.x, body_h, |ui| {
            crate::running_view::table(ui, run, input, out);
        });
        ui.add_space(8.0);
        crate::running_view::footer(ui, run, input, out, FOOT_H);
    }
}

/// A bordered panel of exactly `w` × `h` points.
pub(crate) fn pane(ui: &mut Ui, w: f32, h: f32, add: impl FnOnce(&mut Ui)) {
    let pal = Palette::of_ui(ui);
    let margin = PANE_MARGIN;
    Frame::NONE
        .fill(pal.surface)
        .stroke(Stroke::new(1.0, pal.stroke))
        .corner_radius(theme::rad(8, 12))
        .inner_margin(Margin::same(margin as i8))
        .show(ui, |ui| {
            ui.set_width(w - 2.0 * margin - 2.0);
            ui.set_height(h - 2.0 * margin - 2.0);
            ui.with_layout(Layout::top_down(Align::Min), add);
        });
}

// ------------------------------------------------------------------ header

fn header(
    ui: &mut Ui,
    model: &Model,
    run: &RunModel,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    let mut set_mode: Option<HubMode> = None;
    let pal = Palette::of_ui(ui);
    Sides::new().height(32.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            crate::card::icon_tile(ui, 30.0, ICON_HARD_DRIVE, Color32::TRANSPARENT, true);
            ui.label(
                theme::strong("Session manager")
                    .size(19.0)
                    .color(pal.text_strong),
            );
            ui.spacing_mut().item_spacing.x = 6.0;
            if tab_chip(ui, "Sessions", input.mode == HubMode::Sessions)
                .on_hover_text("Every saved omp session")
                .clicked()
            {
                set_mode = Some(HubMode::Sessions);
            }
            let label = if run.loaded() {
                format!(
                    "Running {} · {}",
                    run.rows().len(),
                    human_size(run.total_bytes())
                )
            } else {
                "Running".to_owned()
            };
            if tab_chip(ui, &label, input.mode == HubMode::Running)
                .on_hover_text("Live omp sessions with the memory they use — kill them here")
                .clicked()
            {
                set_mode = Some(HubMode::Running);
            }
            if tab_chip(ui, "Resume…", input.mode == HubMode::Resume)
                .on_hover_text(
                    "Pick a session to resume — omp's /resume, with Claude Code and Codex",
                )
                .clicked()
            {
                set_mode = Some(HubMode::Resume);
            }
            ui.add_space(4.0);
            match input.mode {
                HubMode::Sessions => {
                    let t = model.scope_totals();
                    let mut text = format!("{} sessions · {}", t.count, human_size(t.bytes));
                    if model.filter.is_active() {
                        text.push_str(&format!(" · {} shown", model.visible().len()));
                    }
                    ui.label(RichText::new(text).size(13.0).color(pal.weak));
                    let pending = model.pending_stats();
                    if pending > 0 {
                        ui.label(
                            RichText::new(format!("· reading usage ({pending} left)"))
                                .size(12.0)
                                .color(pal.weak),
                        );
                    }
                }
                HubMode::Running => {}
                HubMode::Resume => {}
            }
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if crate::card::close_button_sized(ui, 30.0, 18.0).clicked() {
                out.push(SessionsAction::Close);
            }
            if input.scanning {
                let (rect, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
                Spin::new(17.0, pal.accent).paint_at(ui, rect.center());
            } else if ui
                .add_sized(
                    [30.0, 30.0],
                    egui::Button::new(icon_text(ICON_REFRESH_CW, 18.0, pal.weak)).frame(false),
                )
                .on_hover_text("Rescan the session store")
                .on_hover_cursor(CursorIcon::PointingHand)
                .clicked()
            {
                out.push(SessionsAction::Refresh);
            }
        },
    );
    let (left, right, y) = (
        ui.min_rect().left(),
        ui.min_rect().left() + ui.available_width(),
        ui.min_rect().bottom() + 3.0,
    );
    crate::card::header_rule(ui, left, right, y, Color32::TRANSPARENT);
    if let Some(m) = set_mode {
        out.push(SessionsAction::SetMode(m));
    }
}

// ----------------------------------------------------------------- filters

fn filter_row(ui: &mut Ui, model: &mut Model, st: &mut ViewState) {
    let pal = Palette::of_ui(ui);
    let before = model.filter.clone();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let id = Id::unique(FILTER_ID);
        let focused = ui.memory(|m| m.has_focus(id));
        let resp = ui.add(
            egui::TextEdit::singleline(&mut model.filter.text)
                .id(id)
                .prefix(icon_text(ICON_SEARCH, 16.0, pal.weak))
                .hint_text("Filter by title, prompt, folder or id…")
                .align(Align2::LEFT_CENTER)
                .min_size(vec2(0.0, 30.0))
                .desired_width(300.0)
                .margin(Margin::symmetric(10, 6))
                .frame(
                    Frame::new()
                        .fill(pal.bg)
                        .corner_radius(theme::rad(8, 11))
                        .stroke(Stroke::new(
                            1.0,
                            if focused { pal.accent } else { pal.stroke },
                        )),
                ),
        );
        if st.focus_filter {
            resp.request_focus();
            st.focus_filter = false;
        }
        ui.add_space(6.0);
        let f = &mut model.filter;
        let older = f.older_days.map_or_else(
            || "Older than…".to_owned(),
            |d| format!("Older than {d} days"),
        );
        if tab_chip(ui, &older, f.older_days.is_some())
            .on_hover_text("By last activity. Click to cycle 7 / 30 / 90 days / off")
            .clicked()
        {
            f.cycle_older();
        }
        let larger = f.larger_mb.map_or_else(
            || "Larger than…".to_owned(),
            |m| format!("Larger than {m} MB"),
        );
        if tab_chip(ui, &larger, f.larger_mb.is_some())
            .on_hover_text("Total size on disk. Click to cycle 10 / 100 MB / off")
            .clicked()
        {
            f.cycle_larger();
        }
        let was_running = before.running;
        for (label, on, tip) in [
            (
                "Empty stubs",
                &mut f.stubs,
                "Sessions without a title or a prompt",
            ),
            (
                "Folder missing",
                &mut f.missing,
                "The project folder no longer exists",
            ),
            (
                "Running",
                &mut f.running,
                "Only sessions whose omp process is running",
            ),
            (
                "Not running",
                &mut f.not_running,
                "Hide sessions whose omp process is running",
            ),
        ] {
            if tab_chip(ui, label, *on).on_hover_text(tip).clicked() {
                *on = !*on;
            }
        }
        if f.running && f.not_running {
            // Opposites: the chip just clicked wins.
            if was_running {
                f.running = false;
            } else {
                f.not_running = false;
            }
        }
        if f.is_active()
            && ui
                .add(egui::Button::new(RichText::new("Clear").color(pal.weak)).frame(false))
                .on_hover_cursor(CursorIcon::PointingHand)
                .clicked()
        {
            *f = Filter::default();
        }
    });
    if model.filter != before {
        model.mark_view_dirty();
    }
}

// ---------------------------------------------------------------- projects

fn projects_pane(ui: &mut Ui, model: &mut Model) {
    let pal = Palette::of_ui(ui);
    ui.label(theme::section_caption("Projects", pal.weak));
    ui.add_space(6.0);
    let all = model.rows().len();
    let all_bytes: u64 = model.projects().iter().map(|p| p.bytes).sum();
    let mut pick: Option<Option<PathBuf>> = None;
    egui::ScrollArea::vertical()
        .id_salt("sessions-projects")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            if project_row(
                ui,
                ICON_HARD_DRIVE,
                "All projects",
                all,
                all_bytes,
                model.scope.is_none(),
                false,
                None,
            )
            .clicked()
            {
                pick = Some(None);
            }
            for p in model.projects() {
                let on = model.scope.as_ref() == Some(&p.cwd);
                let icon = if p.missing {
                    ICON_FOLDER_X
                } else {
                    ICON_FOLDER
                };
                if project_row(
                    ui,
                    icon,
                    &p.label,
                    p.count,
                    p.bytes,
                    on,
                    p.missing,
                    Some(&p.cwd),
                )
                .clicked()
                {
                    pick = Some(Some(p.cwd.clone()));
                }
            }
        });
    if let Some(scope) = pick {
        model.scope = scope;
        model.mark_view_dirty();
    }
}

#[allow(clippy::too_many_arguments)]
fn project_row(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    count: usize,
    bytes: u64,
    on: bool,
    missing: bool,
    cwd: Option<&Path>,
) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 42.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let hov = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.08);
        let r = theme::rad(6, 10);
        let p = ui.painter();
        paint_row_bg(p, rect, r, pal, on, hov);
        let ink = if missing { pal.danger } else { pal.glyph };
        paint_icon(
            p,
            pos2(rect.left() + 16.0, rect.center().y),
            Align2::CENTER_CENTER,
            icon,
            17.0,
            ink,
        );
        let clip = Rect::from_min_max(pos2(rect.left() + 32.0, rect.top()), rect.right_bottom())
            .shrink2(vec2(6.0, 0.0));
        let tp = p.with_clip_rect(clip);
        let g = tp.layout_no_wrap(
            split_project_label(label).0.to_owned(),
            theme::font(13.5),
            if on { pal.text_strong } else { pal.text },
        );
        tp.galley(pos2(clip.left(), rect.top() + 6.0), g, pal.text);
        let sub = if missing {
            format!("{count} · {} · folder missing", human_size(bytes))
        } else {
            format!("{count} · {}", human_size(bytes))
        };
        let g = tp.layout_no_wrap(
            sub,
            theme::font(11.5),
            if missing { pal.danger } else { pal.weak },
        );
        tp.galley(pos2(clip.left(), rect.top() + 23.0), g, pal.weak);
    }
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    match cwd {
        Some(c) => resp.on_hover_text(format!("{label}\n{}", c.display())),
        None => resp,
    }
}

/// Row background: hover pops the outline, selection washes the accent.
pub(crate) fn paint_row_bg(p: &Painter, rect: Rect, radius: u8, pal: &Palette, on: bool, hov: f32) {
    if on {
        p.rect_filled(rect, radius, theme::accent_fill(pal, 0.16));
        p.rect_stroke(
            rect,
            radius,
            Stroke::new(1.0, pal.accent.gamma_multiply(0.8)),
            StrokeKind::Inside,
        );
    }
    if hov > 0.0 {
        p.rect_filled(
            rect,
            radius,
            theme::hover_fill(pal).gamma_multiply(0.8 * hov),
        );
        p.rect_stroke(
            rect,
            radius,
            Stroke::new(1.5, theme::hover_stroke(pal).gamma_multiply(hov)),
            StrokeKind::Inside,
        );
    }
}

// ------------------------------------------------------------------- table

struct Col {
    col: Column,
    label: &'static str,
    tip: &'static str,
    /// Minimum width: the whole width of a column whose cells are not measured (Project).
    w: f32,
    right: bool,
    /// Place in the order columns are dropped when the table is too narrow (`0` = never).
    drop: u8,
}

/// The columns that fit the table's width, with their measured widths.
struct Laid {
    cols: Vec<Col>,
    widths: Vec<f32>,
    total_w: f32,
}

pub(crate) const CHECK_W: f32 = 30.0;
const TITLE_MIN: f32 = 160.0;
/// The Model and Status columns stop growing here; their cells clip beyond it.
const MODEL_MAX: f32 = 130.0;
const STATUS_MAX: f32 = 190.0;
/// Width a badge pill adds around its label (6 on each side) plus the gap after it.
const PILL_PAD: f32 = 16.0;

fn columns(all_projects: bool) -> Vec<Col> {
    let c = |col, label, tip, w, right, drop| Col {
        col,
        label,
        tip,
        w,
        right,
        drop,
    };
    let mut v = vec![c(
        Column::Title,
        "Session",
        "Title, else the first prompt",
        0.0,
        false,
        0,
    )];
    if all_projects {
        v.push(c(
            Column::Project,
            "Project",
            "Folder the session ran in",
            118.0,
            false,
            5,
        ));
    }
    v.extend([
        c(
            Column::Size,
            "Size",
            "Session file + subagent transcripts + artifacts (what deleting frees)",
            0.0,
            true,
            0,
        ),
        c(
            Column::Ram,
            "RAM",
            "Memory of the running omp process tree",
            0.0,
            true,
            7,
        ),
        c(
            Column::Messages,
            "Msgs",
            "Your prompts · assistant turns (hover for subagents)",
            0.0,
            true,
            9,
        ),
        c(
            Column::Tokens,
            "Tokens",
            "Input / output tokens of the session itself",
            0.0,
            true,
            10,
        ),
        c(
            Column::Cost,
            "Cost",
            "Session + its subagents",
            0.0,
            true,
            0,
        ),
        c(
            Column::Model,
            "Model",
            "Most used model (hover a row for the full name)",
            0.0,
            false,
            2,
        ),
        c(
            Column::Created,
            "Created",
            "Age of the session",
            0.0,
            true,
            1,
        ),
        c(
            Column::Activity,
            "Active",
            "Time since the last entry",
            0.0,
            true,
            8,
        ),
        c(Column::Status, "Status", "Badges", 0.0, false, 6),
    ]);
    v
}

/// Widths of the columns shown for the filtered rows: each fixed column is as wide as its widest
/// cell or header (plus the sort arrow), the title takes the rest and columns are dropped by
/// [`col_alloc::allocate`] when the table is too narrow. Recomputed only when the cells, the
/// filter, the sort, the style or the width change; otherwise the cached result is returned.
fn layout(ui: &Ui, model: &Model, pal: &Palette, avail: f32) -> Arc<Laid> {
    let any_error = model
        .visible()
        .iter()
        .any(|&i| model.error(model.rows()[i].path()).is_some());
    let key = crate::paint_cache::with(
        crate::paint_cache::style_key(ui.ctx(), pal),
        (
            model.cells_rev(),
            model.scope.is_none(),
            slot(model.sort.col),
            avail.to_bits(),
            any_error,
            model.visible(),
        ),
    );
    let id = Id::unique("sessions-col-layout");
    if let Some((k, laid)) = ui.data(|d| d.get_temp::<(u64, Arc<Laid>)>(id))
        && k == key
    {
        return laid;
    }
    let laid = Arc::new(measure_layout(ui.painter(), model, pal, avail));
    ui.data_mut(|d| d.insert_temp(id, (key, Arc::clone(&laid))));
    laid
}

fn measure_layout(p: &Painter, model: &Model, pal: &Palette, avail: f32) -> Laid {
    let mut cols = columns(model.scope.is_none());
    let sorted = model.sort.col;
    let mut nat: Vec<f32> = cols
        .iter()
        .map(|c| {
            let arrow = if c.col == sorted { 16.0 } else { 0.0 };
            (lay(p, c.label, 12.5, pal.weak).size().x + arrow + CELL_PAD).max(c.w)
        })
        .collect();
    let (mut ram_used, mut status_w) = (false, 0.0_f32);
    let failed = lay(p, "Failed", 11.0, pal.danger).size().x + PILL_PAD;
    for &i in model.visible() {
        let row = &model.rows()[i];
        let cells = &row.cells;
        ram_used |= !cells.ram.is_empty();
        for (c, n) in cols.iter().zip(&mut nat) {
            let (text, size) = match c.col {
                Column::Size => (&cells.size, 13.0),
                Column::Ram => (&cells.ram, 12.5),
                Column::Messages => (&cells.messages, 12.5),
                Column::Tokens => (&cells.tokens, 12.5),
                Column::Cost => (&cells.cost, 12.5),
                Column::Model => (&cells.model, 12.0),
                Column::Created => (&cells.created, 12.5),
                Column::Activity => (&cells.activity, 12.5),
                Column::Title | Column::Project | Column::Status => continue,
            };
            if !text.is_empty() {
                *n = n.max(lay(p, text, size, pal.text).size().x + CELL_PAD);
            }
        }
        let mut pills: f32 = row
            .badges
            .iter()
            .map(|b| lay(p, b.short(), 11.0, pal.text).size().x + PILL_PAD)
            .sum();
        if model.error(row.path()).is_some() {
            pills += failed;
        }
        status_w = status_w.max(pills);
    }
    for (c, n) in cols.iter_mut().zip(&mut nat) {
        match c.col {
            Column::Model => *n = n.min(MODEL_MAX + CELL_PAD),
            Column::Status => {
                *n = n.max(status_w.min(STATUS_MAX) + CELL_PAD);
                if status_w == 0.0 {
                    c.drop = 4;
                }
            }
            Column::Ram if !ram_used => c.drop = 3,
            _ => {}
        }
    }
    let specs: Vec<col_alloc::Spec> = cols
        .iter()
        .zip(&nat)
        .map(|(c, &natural)| col_alloc::Spec {
            natural,
            drop: c.drop,
        })
        .collect();
    let alloc = col_alloc::allocate(&specs, 0, CHECK_W, avail, TITLE_MIN);
    let mut out = Laid {
        cols: Vec::with_capacity(cols.len()),
        widths: Vec::with_capacity(cols.len()),
        total_w: alloc.total,
    };
    for (c, w) in cols.into_iter().zip(alloc.widths) {
        if w > 0.0 {
            out.cols.push(c);
            out.widths.push(w);
        }
    }
    out
}

fn table_pane(
    ui: &mut Ui,
    model: &mut Model,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    let pal = Palette::of_ui(ui);
    if !input.loaded {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() / 2.0 - 30.0);
            ui.add(Spin::new(22.0, pal.accent));
            ui.add_space(8.0);
            ui.label(RichText::new("Scanning sessions…").color(pal.weak));
        });
        return;
    }
    if model.rows().is_empty() {
        empty_state(
            ui,
            ICON_HARD_DRIVE,
            pal.accent,
            "No sessions found",
            "omp has not saved any session in this sessions folder yet.",
        );
        return;
    }
    let laid = layout(ui, model, pal, ui.available_width());
    let (cols, widths, total_w) = (&laid.cols, &laid.widths, laid.total_w);

    egui::ScrollArea::horizontal()
        .id_salt("sessions-table-h")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_min_width(total_w);
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            table_header(ui, model, cols, widths, total_w);
            ui.add_space(4.0);
            if model.visible().is_empty() {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No sessions match").color(pal.weak));
                    if model.filter.is_active() && ui.link("Clear the filters").clicked() {
                        model.filter = Filter::default();
                        model.mark_view_dirty();
                    }
                });
                return;
            }
            table_rows(ui, model, cols, widths, total_w, input, out);
        });
}

fn table_header(ui: &mut Ui, model: &mut Model, cols: &[Col], widths: &[f32], total_w: f32) {
    let pal = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(total_w, HEAD_H), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, theme::rad(6, 9), pal.raised);
    p.rect_stroke(
        rect,
        theme::rad(6, 9),
        Stroke::new(1.0, pal.stroke),
        StrokeKind::Inside,
    );
    // Select-all-visible checkbox.
    let check_rect = Rect::from_min_size(rect.min, vec2(CHECK_W, HEAD_H));
    let resp = ui.interact(check_rect, Id::unique("sessions-check-all"), Sense::click());
    paint_checkbox(
        ui.painter(),
        check_rect.center(),
        model.header_check(),
        resp.hovered(),
        pal,
    );
    if resp
        .on_hover_text("Select / clear every visible session")
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
    {
        model.toggle_all_visible();
    }
    let mut x = rect.left() + CHECK_W;
    for (c, &w) in cols.iter().zip(widths) {
        let cell = Rect::from_min_size(pos2(x, rect.top()), vec2(w, HEAD_H));
        x += w;
        let resp = ui.interact(cell, Id::unique(("sessions-head", c.label)), Sense::click());
        let active = model.sort.col == c.col;
        let hov = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.08);
        let p = ui.painter();
        if hov > 0.0 {
            p.rect_stroke(
                cell.shrink(1.0),
                theme::rad(5, 8),
                Stroke::new(1.5, theme::hover_stroke(pal).gamma_multiply(hov)),
                StrokeKind::Inside,
            );
        }
        let ink = if active { pal.text_strong } else { pal.weak };
        let arrow_w = if active { 16.0 } else { 0.0 };
        let inner = cell.shrink2(vec2(8.0, 0.0));
        let tp = p.with_clip_rect(inner);
        let g = tp.layout_no_wrap(c.label.to_owned(), theme::font(12.5), ink);
        let text_w = g.size().x;
        let start_x = if c.right {
            inner.right() - text_w - arrow_w
        } else {
            inner.left()
        };
        tp.galley(pos2(start_x, cell.center().y - g.size().y / 2.0), g, ink);
        if active {
            paint_icon(
                p,
                pos2(start_x + text_w + 9.0, cell.center().y),
                Align2::CENTER_CENTER,
                if model.sort.desc {
                    ICON_ARROW_DOWN
                } else {
                    ICON_ARROW_UP
                },
                13.0,
                pal.accent,
            );
        }
        if resp
            .on_hover_text(format!("{} — click to sort", c.tip))
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
        {
            model.sort.click(c.col);
            model.mark_view_dirty();
        }
    }
}

fn table_rows(
    ui: &mut Ui,
    model: &mut Model,
    cols: &[Col],
    widths: &[f32],
    total_w: f32,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    let pal = Palette::of_ui(ui);
    let shift = ui.input(|i| i.modifiers.shift);
    let n = model.visible().len();
    let key = crate::paint_cache::with(
        crate::paint_cache::style_key(ui.ctx(), pal),
        model.cells_rev(),
    );
    let mut check_click: Option<usize> = None;
    let mut detail_click: Option<PathBuf> = None;
    egui::ScrollArea::vertical()
        .id_salt("sessions-table-v")
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, n, |ui, range| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            ui.set_min_width(total_w);
            for pos in range {
                let Some(&idx) = model.visible().get(pos) else {
                    continue;
                };
                let row = &model.rows()[idx];
                let selected = model.sel.contains(row.path());
                let detail_on = model.detail.as_deref() == Some(row.path());
                let (rect, resp) = ui.allocate_exact_size(vec2(total_w, ROW_H), Sense::click());
                let inner = rect.shrink2(vec2(0.0, 1.0));
                let check_rect = Rect::from_min_size(inner.min, vec2(CHECK_W, inner.height()));
                let check = ui.interact(check_rect, resp.id.with("check"), Sense::click());
                let hov = ui.ctx().animate_bool_with_time(
                    resp.id.with("hov"),
                    resp.hovered() || check.hovered(),
                    0.08,
                );
                if ui.is_rect_visible(rect) {
                    let p = ui.painter();
                    let r = theme::rad(5, 8);
                    if selected && !detail_on {
                        p.rect_filled(inner, r, theme::accent_fill(pal, 0.10));
                    }
                    paint_row_bg(p, inner, r, pal, detail_on, hov);
                    if !detail_on && !selected && hov == 0.0 {
                        p.hline(
                            inner.x_range().shrink(6.0),
                            rect.bottom(),
                            Stroke::new(1.0, theme::mix(pal.surface, pal.stroke, 0.5)),
                        );
                    }
                    let state = if selected { Check::All } else { Check::None };
                    paint_checkbox(p, check_rect.center(), state, check.hovered(), pal);
                    paint_row(
                        ui,
                        row,
                        inner,
                        cols,
                        widths,
                        model.error(row.path()),
                        key,
                        pal,
                    );
                }
                if check.clicked() {
                    check_click = Some(pos);
                } else if resp.clicked() {
                    detail_click = Some(row.path().to_path_buf());
                }
                if resp.double_clicked() && input.omp {
                    out.push(SessionsAction::Open(row.path().to_path_buf()));
                }
                let pointer = ui.input(|i| i.pointer.hover_pos());
                if let Some(at) = pointer
                    && inner.contains(at)
                {
                    let col = column_at(inner.left() + CHECK_W, cols, widths, at.x);
                    row_tip(&resp, row, col, model.error(row.path()));
                }
            }
        });
    if let Some(pos) = check_click {
        model.click_row(pos, shift);
    }
    if let Some(path) = detail_click {
        model.detail = if model.detail.as_deref() == Some(path.as_path()) {
            None
        } else {
            Some(path)
        };
    }
}

fn column_at(mut x: f32, cols: &[Col], widths: &[f32], at: f32) -> Option<Column> {
    for (c, &w) in cols.iter().zip(widths) {
        if at >= x && at < x + w {
            return Some(c.col);
        }
        x += w;
    }
    None
}

const SLOTS: usize = 10;

fn slot(c: Column) -> usize {
    match c {
        Column::Title => 0,
        Column::Project => 1,
        Column::Size => 2,
        Column::Ram => 3,
        Column::Messages => 4,
        Column::Tokens => 5,
        Column::Cost => 6,
        Column::Model => 7,
        Column::Created => 8,
        Column::Activity => 9,
        Column::Status => usize::MAX,
    }
}

/// Every cell of the row laid out once (`SLOTS` text cells, then one galley per badge).
fn row_galleys(p: &Painter, row: &Row, pal: &Palette) -> Vec<Arc<Galley>> {
    let c = &row.cells;
    let title_ink = if row.entry.is_stub() {
        pal.weak
    } else {
        pal.text_strong
    };
    let mut v = vec![
        lay(p, &c.title, 13.5, title_ink),
        lay(p, split_project_label(&c.project).0, 12.5, pal.text),
        lay(p, &c.size, 13.0, pal.text_strong),
        lay(p, &c.ram, 12.5, pal.text_strong),
        lay(p, &c.messages, 12.5, pal.text),
        lay(p, &c.tokens, 12.5, pal.text),
        lay(p, &c.cost, 12.5, pal.text),
        lay(p, &c.model, 12.0, pal.weak),
        lay(p, &c.created, 12.5, pal.weak),
        lay(p, &c.activity, 12.5, pal.text),
    ];
    v.extend(
        row.badges
            .iter()
            .map(|b| lay(p, b.short(), 11.0, badge_color(b, pal))),
    );
    v
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    ui: &Ui,
    row: &Row,
    rect: Rect,
    cols: &[Col],
    widths: &[f32],
    error: Option<&str>,
    key: u64,
    pal: &Palette,
) {
    let p = ui.painter();
    let mut cache = row.paint.borrow_mut();
    let g = cache.get(key, || row_galleys(p, row, pal));
    let mut x = rect.left() + CHECK_W;
    for (c, &w) in cols.iter().zip(widths) {
        let cell = Rect::from_min_size(pos2(x, rect.top()), vec2(w, rect.height()));
        x += w;
        let inner = cell.shrink2(vec2(8.0, 0.0));
        match c.col {
            Column::Status => badges_cell(p, inner, &g[SLOTS..], &row.badges, error.is_some(), pal),
            Column::Project => {
                project_cell(p, inner, &g[slot(Column::Project)], &row.cells.project, pal)
            }
            col => {
                let right = !matches!(col, Column::Title | Column::Project | Column::Model);
                galley_cell(p, inner, &g[slot(col)], right);
            }
        }
    }
}

/// Lay out `text` in the UI font (callers cache the result).
pub(crate) fn lay(p: &Painter, text: &str, size: f32, color: Color32) -> Arc<Galley> {
    p.layout_no_wrap(text.to_owned(), theme::font(size), color)
}

/// The Project cell: the folder name (`name`, laid out by the caller) with the folder holding it
/// as a weak second line, elided from the left so the part next to the name survives.
pub(crate) fn project_cell(
    p: &Painter,
    rect: Rect,
    name: &Arc<Galley>,
    label: &str,
    pal: &Palette,
) {
    let tp = p.with_clip_rect(rect);
    let Some(parent) = split_project_label(label).1 else {
        galley_cell(p, rect, name, false);
        return;
    };
    let sub = elide_left(&tp, parent, 10.5, pal.weak, rect.width());
    let h = name.size().y + sub.size().y;
    let top = rect.center().y - h / 2.0;
    tp.galley(
        pos2(rect.left(), top),
        Arc::clone(name),
        Color32::PLACEHOLDER,
    );
    tp.galley(
        pos2(rect.left(), top + name.size().y),
        sub,
        Color32::PLACEHOLDER,
    );
}

/// `text` laid out in at most `max_w` points, losing its head (`…/PersProject`) when too long.
pub(crate) fn elide_left(
    p: &Painter,
    text: &str,
    size: f32,
    color: Color32,
    max_w: f32,
) -> Arc<Galley> {
    let mut g = lay(p, text, size, color);
    let chars: Vec<char> = text.chars().collect();
    let mut keep = chars.len();
    for _ in 0..6 {
        let w = g.size().x;
        if w <= max_w || keep <= 2 {
            break;
        }
        keep = ((keep as f32 * max_w / w) as usize)
            .saturating_sub(2)
            .max(1);
        let tail: String = chars[chars.len() - keep.min(chars.len())..]
            .iter()
            .collect();
        g = lay(p, &format!("…{tail}"), size, color);
    }
    g
}

pub(crate) fn galley_cell(p: &Painter, rect: Rect, g: &Arc<Galley>, right: bool) {
    let tp = p.with_clip_rect(rect);
    let x = if right {
        (rect.right() - g.size().x).max(rect.left())
    } else {
        rect.left()
    };
    tp.galley(
        pos2(x, rect.center().y - g.size().y / 2.0),
        Arc::clone(g),
        Color32::PLACEHOLDER,
    );
}

fn badge_color(b: &Badge, pal: &Palette) -> Color32 {
    match b {
        Badge::Running => pal.ok,
        Badge::InTab | Badge::Pinned => pal.accent,
        Badge::Busy | Badge::Aborted => pal.warn,
        Badge::Error | Badge::FolderMissing => pal.danger,
        Badge::Fork(_) | Badge::Stub => pal.weak,
    }
}

/// One badge pill starting at `x` (`tp` is clipped to the cell); `g` is the laid-out label.
/// Returns where the next pill starts.
pub(crate) fn pill(
    tp: &Painter,
    rect: Rect,
    pal: &Palette,
    x: f32,
    g: &Arc<Galley>,
    color: Color32,
) -> f32 {
    let size = vec2(g.size().x + 12.0, 18.0);
    let r = Rect::from_min_size(pos2(x, rect.center().y - size.y / 2.0), size);
    theme::skin::paint_chip(tp, r, theme::rad(4, 9), pal, color.gamma_multiply(0.18));
    tp.rect_stroke(
        r,
        theme::rad(4, 9),
        Stroke::new(1.0, color.gamma_multiply(0.55)),
        StrokeKind::Inside,
    );
    tp.galley(
        r.min + vec2(6.0, (size.y - g.size().y) / 2.0),
        Arc::clone(g),
        Color32::PLACEHOLDER,
    );
    x + size.x + 4.0
}

fn badges_cell(
    p: &Painter,
    rect: Rect,
    galleys: &[Arc<Galley>],
    badges: &[Badge],
    failed: bool,
    pal: &Palette,
) {
    let tp = p.with_clip_rect(rect);
    let mut x = rect.left();
    if failed {
        // Tinted like the other pills with danger-coloured text: readable in every style.
        let g = lay(&tp, "Failed", 11.0, pal.danger);
        x = pill(&tp, rect, pal, x, &g, pal.danger);
    }
    for (b, g) in badges.iter().zip(galleys) {
        if x > rect.right() {
            break;
        }
        x = pill(&tp, rect, pal, x, g, badge_color(b, pal));
    }
}

/// Hover card of a table row; the rows depend on the column under the pointer.
fn row_tip(resp: &egui::Response, row: &Row, col: Option<Column>, error: Option<&str>) {
    use crate::badge::{self, Hue};
    use crate::card::{self, Header};
    const KEY_W: f32 = 132.0;
    let e = &row.entry;
    let (icon, title, sub) = match col {
        Some(Column::Size) => (ICON_HARD_DRIVE, "Disk usage", row.label()),
        Some(Column::Ram) => (ICON_CPU, "Memory", row.label()),
        Some(Column::Messages) => (ICON_MESSAGES_SQUARE, "Conversation", row.label()),
        Some(Column::Tokens) => (ICON_ACTIVITY, "Tokens", row.label()),
        Some(Column::Cost) => (ICON_COINS, "Cost", row.label()),
        Some(Column::Model) => (ICON_SPARKLES, "Models", row.label()),
        Some(Column::Status) => (ICON_INFO, "Status", row.label()),
        _ => (ICON_FOLDER, row.label(), "Session"),
    };
    let head = Header::new(icon, title).subtitle(sub);
    card::hover(
        resp,
        380.0,
        true,
        &head,
        |ui| {
            if row.live.running {
                badge::show(ui, None, "Running", Hue::Green, 12.0);
            } else if error.is_some() {
                badge::show(ui, None, "Error", Hue::Rose, 12.0);
            }
        },
        |ui| {
            let pal = Palette::of_ui(ui);
            let val = |ui: &mut Ui, t: &str| {
                ui.label(RichText::new(t).color(pal.text_strong));
            };
            let note = |ui: &mut Ui, t: &str| {
                ui.label(RichText::new(t).size(12.0).color(pal.weak));
            };
            let tag = |ui: &mut Ui, t: &str, hue: Hue| {
                badge::show(ui, None, t, hue, 12.0);
            };
            match col {
                Some(Column::Size) => {
                    card::kv(ui, KEY_W, "Session file", |ui| {
                        tag(ui, &human_size(e.jsonl_bytes), Hue::Amber);
                    });
                    card::kv(ui, KEY_W, "Artifacts", |ui| {
                        ui.horizontal(|ui| {
                            tag(ui, &human_size(e.artifact_bytes), Hue::Teal);
                            note(
                                ui,
                                &format!(
                                    "in {} files (incl. subagent transcripts)",
                                    e.artifact_files
                                ),
                            );
                        });
                    });
                    card::kv(ui, KEY_W, "Deleting frees", |ui| {
                        tag(ui, &human_size(e.total_bytes()), Hue::Green);
                    });
                }
                Some(Column::Ram) => match row.ram {
                    Some(m) => {
                        card::kv(ui, KEY_W, "Memory", |ui| {
                            tag(ui, &human_size(m.bytes), Hue::Amber);
                        });
                        card::kv(ui, KEY_W, "Processes", |ui| {
                            val(
                                ui,
                                &format!(
                                    "{} in the process tree",
                                    plural(m.processes as usize, "process", "processes")
                                ),
                            );
                        });
                        note(ui, "Shared pages are split between their users");
                    }
                    None => note(ui, "Not running, or no reading yet"),
                },
                Some(Column::Messages) => match row.stats() {
                    Some(s) => {
                        card::kv(ui, KEY_W, "Prompts", |ui| {
                            val(ui, &format!("{} from you", s.user_messages));
                        });
                        card::kv(ui, KEY_W, "Turns", |ui| {
                            val(ui, &format!("{} assistant turns", s.assistant_turns));
                        });
                        card::kv(ui, KEY_W, "Subagents", |ui| {
                            val(ui, &plural(e.subagents as usize, "subagent", "subagents"));
                        });
                        if s.compactions > 0 {
                            card::kv(ui, KEY_W, "Compactions", |ui| {
                                val(ui, &format!("{} compactions", s.compactions));
                            });
                        }
                    }
                    None => note(ui, "Still reading…"),
                },
                Some(Column::Tokens) => match row.stats() {
                    Some(s) => {
                        card::kv(ui, KEY_W, "Input", |ui| {
                            tag(ui, &compact(s.input_tokens), Hue::Blue);
                        });
                        card::kv(ui, KEY_W, "Output", |ui| {
                            tag(ui, &compact(s.output_tokens), Hue::Teal);
                        });
                        card::kv(ui, KEY_W, "Cache", |ui| {
                            val(
                                ui,
                                &format!(
                                    "read {} · write {}",
                                    compact(s.cache_read_tokens),
                                    compact(s.cache_write_tokens)
                                ),
                            );
                        });
                    }
                    None => note(ui, "Still reading…"),
                },
                Some(Column::Cost) => match row.stats() {
                    Some(s) => {
                        card::kv(ui, KEY_W, "Session", |ui| {
                            tag(ui, &money(s.cost), Hue::Amber);
                        });
                        card::kv(ui, KEY_W, "Subagents", |ui| {
                            val(ui, &money(s.subagent_cost));
                        });
                    }
                    None => note(ui, "Still reading…"),
                },
                Some(Column::Model) => match row.stats() {
                    Some(s) if !s.models.is_empty() => {
                        card::kv(ui, KEY_W, "Models", |ui| {
                            ui.vertical(|ui| {
                                for m in &s.models {
                                    tag(ui, m, Hue::Violet);
                                }
                            });
                        });
                    }
                    Some(_) => note(ui, "No model recorded"),
                    None => note(ui, "Still reading…"),
                },
                Some(Column::Status) => {
                    if let Some(err) = error {
                        card::kv(ui, KEY_W, "Error", |ui| {
                            ui.label(RichText::new(err).color(pal.danger));
                        });
                    }
                    if !row.badges.is_empty() {
                        card::kv(ui, KEY_W, "Flags", |ui| {
                            ui.horizontal_wrapped(|ui| {
                                for b in &row.badges {
                                    tag(ui, &b.text(), badge_hue(b));
                                }
                            });
                        });
                    }
                    if row.badges.is_empty() && error.is_none() {
                        note(ui, "No flags");
                    }
                }
                _ => {
                    let c = &row.cells;
                    if !c.project.is_empty() {
                        card::kv(ui, KEY_W, "Project", |ui| val(ui, &c.project));
                    }
                    if !c.size.is_empty() {
                        card::kv(ui, KEY_W, "Size", |ui| tag(ui, &c.size, Hue::Amber));
                    }
                    if !c.model.is_empty() {
                        card::kv(ui, KEY_W, "Model", |ui| tag(ui, &c.model, Hue::Violet));
                    }
                    if !c.activity.is_empty() {
                        card::kv(ui, KEY_W, "Updated", |ui| val(ui, &c.activity));
                    }
                    card::kv(ui, KEY_W, "Id", |ui| tag(ui, &e.id, Hue::Teal));
                    card::kv(ui, KEY_W, "Folder", |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(e.cwd.display().to_string()).color(pal.weak),
                            )
                            .truncate(),
                        );
                    });
                    card::kv(ui, KEY_W, "File", |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(e.path.display().to_string()).color(pal.weak),
                            )
                            .truncate(),
                        );
                    });
                }
            }
        },
    );
}

/// Badge colour family of a status flag.
fn badge_hue(b: &Badge) -> crate::badge::Hue {
    use crate::badge::Hue;
    match b {
        Badge::Running => Hue::Green,
        Badge::InTab => Hue::Blue,
        Badge::Busy => Hue::Amber,
        Badge::Error => Hue::Rose,
        Badge::Aborted => Hue::Orange,
        Badge::Fork(_) => Hue::Violet,
        Badge::FolderMissing => Hue::Orange,
        Badge::Pinned => Hue::Amber,
        Badge::Stub => Hue::Teal,
    }
}

pub(crate) fn paint_checkbox(
    p: &Painter,
    center: egui::Pos2,
    state: Check,
    hovered: bool,
    pal: &Palette,
) {
    let r = Rect::from_center_size(center, vec2(16.0, 16.0));
    let on = state != Check::None;
    let radius = theme::rad(4, 5);
    if on {
        p.rect_filled(r, radius, pal.accent);
        let ink = theme::on_accent(pal);
        match state {
            Check::All => paint_icon(p, r.center(), Align2::CENTER_CENTER, ICON_CHECK, 13.0, ink),
            _ => paint_icon(p, r.center(), Align2::CENTER_CENTER, ICON_MINUS, 13.0, ink),
        }
    } else {
        p.rect_filled(r, radius, pal.bg);
        p.rect_stroke(
            r,
            radius,
            Stroke::new(
                1.5,
                if hovered {
                    theme::hover_stroke(pal)
                } else {
                    theme::mix(pal.stroke, pal.weak, 0.5)
                },
            ),
            StrokeKind::Inside,
        );
    }
}

// ------------------------------------------------------------------ detail

fn detail_pane(
    ui: &mut Ui,
    model: &mut Model,
    input: &ViewInput<'_>,
    pane_h: f32,
    out: &mut Vec<SessionsAction>,
) {
    let pal = Palette::of_ui(ui);
    // The pane's inner height (see `pane`) ends here.
    let bottom = ui.cursor().top() + pane_h - 2.0 * PANE_MARGIN - 2.0;
    let Some(path) = model.detail.clone() else {
        return;
    };
    let mut select: Option<PathBuf> = None;
    let mut close = false;
    {
        let Some(row) = model.row_by_path(&path) else {
            return;
        };
        let parent = model.parent_of(row);
        let forks = model.forks_of(&path);
        let error = model.error(&path);
        Sides::new().show(
            ui,
            |ui| {
                ui.label(theme::section_caption("Details", pal.weak));
            },
            |ui| {
                if ui
                    .add(egui::Button::new(icon_text(ICON_X, 16.0, pal.weak)).frame(false))
                    .on_hover_text("Close the details")
                    .on_hover_cursor(CursorIcon::PointingHand)
                    .clicked()
                {
                    close = true;
                }
            },
        );
        ui.add_space(4.0);
        // The action buttons are pinned below the scroll area: it gets what they leave (their
        // height is measured each frame; a wrapped row shows up one frame late).
        let key = Id::unique("sessions-detail-buttons-h");
        let buttons_h: f32 = ui.data(|d| d.get_temp(key)).unwrap_or(70.0);
        let scroll_h = (bottom - ui.cursor().top() - buttons_h - 12.0).max(60.0);
        egui::ScrollArea::vertical()
            .id_salt("sessions-detail")
            .max_height(scroll_h)
            .min_scrolled_height(scroll_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 5.0;
                detail_body(ui, row, error, input.now, out);
                if let Some(parent) = parent {
                    kv_label(ui, "Fork of");
                    if link_row(ui, parent.label()) {
                        select = Some(parent.path().to_path_buf());
                    }
                }
                if !forks.is_empty() {
                    kv_label(ui, &format!("Forks ({})", forks.len()));
                    for f in forks {
                        if link_row(ui, f.label()) {
                            select = Some(f.path().to_path_buf());
                        }
                    }
                }
            });
        ui.add_space(6.0);
        let top = ui.cursor().top();
        detail_buttons(ui, row, model.projects(), input, out);
        let used = ui.min_rect().bottom() - top;
        ui.data_mut(|d| d.insert_temp(key, used));
    }
    if close {
        model.detail = None;
    }
    if let Some(p) = select {
        model.detail = Some(p);
    }
}

fn kv_label(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.add_space(4.0);
    ui.label(RichText::new(text).size(11.5).color(pal.weak));
}

fn link_row(ui: &mut Ui, text: &str) -> bool {
    let pal = Palette::of_ui(ui);
    ui.add(
        egui::Label::new(RichText::new(text).color(pal.accent))
            .truncate()
            .sense(Sense::click()),
    )
    .on_hover_cursor(CursorIcon::PointingHand)
    .clicked()
}

fn kv(ui: &mut Ui, key: &str, value: &str) {
    let pal = Palette::of_ui(ui);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add_sized(
            [92.0, 16.0],
            egui::Label::new(RichText::new(key).size(12.0).color(pal.weak)),
        );
        ui.add(
            egui::Label::new(RichText::new(value).size(12.5).color(pal.text))
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    });
}

fn small_copy(ui: &mut Ui, tip: &str) -> bool {
    let pal = Palette::of_ui(ui);
    ui.add(egui::Button::new(icon_text(ICON_COPY, 14.0, pal.weak)).frame(false))
        .on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
}

fn detail_body(
    ui: &mut Ui,
    row: &Row,
    error: Option<&str>,
    now: SystemTime,
    out: &mut Vec<SessionsAction>,
) {
    let pal = Palette::of_ui(ui);
    let e = &row.entry;
    ui.add(
        egui::Label::new(theme::strong(row.label()).size(15.0).color(pal.text_strong))
            .wrap_mode(egui::TextWrapMode::Wrap),
    );
    if let Some(err) = error {
        ui.add(
            egui::Label::new(RichText::new(err).color(pal.danger))
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    }
    if !row.badges.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
            for b in &row.badges {
                let c = badge_color(b, pal);
                ui.label(RichText::new(b.text()).size(11.5).color(c));
            }
        });
    }
    kv_label(ui, "Identity");
    Sides::new().shrink_left().truncate().spacing(4.0).show(
        ui,
        |ui| {
            ui.add(
                egui::Label::new(RichText::new(&e.id).monospace().size(11.5).color(pal.text))
                    .truncate(),
            );
        },
        |ui| {
            if small_copy(ui, "Copy the session id") {
                out.push(SessionsAction::CopyText {
                    text: e.id.clone(),
                    what: "session id",
                });
            }
        },
    );
    Sides::new().shrink_left().truncate().spacing(4.0).show(
        ui,
        |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(e.path.display().to_string())
                        .monospace()
                        .size(11.0)
                        .color(pal.weak),
                )
                .truncate(),
            );
        },
        |ui| {
            if ui
                .add(egui::Button::new(icon_text(ICON_FOLDER_OPEN, 14.0, pal.weak)).frame(false))
                .on_hover_text("Show the file in the file manager")
                .on_hover_cursor(CursorIcon::PointingHand)
                .clicked()
            {
                out.push(SessionsAction::Reveal(e.path.clone()));
            }
            if small_copy(ui, "Copy the file path") {
                out.push(SessionsAction::CopyText {
                    text: e.path.display().to_string(),
                    what: "session path",
                });
            }
        },
    );
    Sides::new().shrink_left().truncate().spacing(4.0).show(
        ui,
        |ui| {
            let c = if e.cwd_exists { pal.text } else { pal.danger };
            let resp = ui.add(
                egui::Label::new(
                    RichText::new(e.cwd.display().to_string())
                        .size(12.0)
                        .color(c),
                )
                .truncate(),
            );
            if !e.cwd_exists {
                resp.on_hover_text("This folder no longer exists");
            }
        },
        |ui| {
            if e.cwd_exists
                && ui
                    .add(
                        egui::Button::new(icon_text(ICON_EXTERNAL_LINK, 14.0, pal.weak))
                            .frame(false),
                    )
                    .on_hover_text("Open the project folder")
                    .on_hover_cursor(CursorIcon::PointingHand)
                    .clicked()
            {
                out.push(SessionsAction::OpenFolder(e.cwd.clone()));
            }
        },
    );
    kv_label(ui, "Time");
    kv(
        ui,
        "Created",
        &e.created.map_or_else(
            || "unknown".to_owned(),
            |c| format!("{} ({} ago)", utc_stamp(c), format_age(now, c)),
        ),
    );
    kv(
        ui,
        "Last activity",
        &format!(
            "{} ({} ago)",
            utc_stamp(row.activity()),
            format_age(now, row.activity())
        ),
    );
    if let Some(m) = row.ram {
        kv_label(ui, "Running");
        kv(
            ui,
            "Memory",
            &format!(
                "{} · {}",
                human_size(m.bytes),
                plural(m.processes as usize, "process", "processes")
            ),
        );
    }
    kv_label(ui, "Size on disk");
    kv(ui, "Total", &human_size(e.total_bytes()));
    kv(ui, "Session file", &human_size(e.jsonl_bytes));
    kv(
        ui,
        "Artifacts",
        &format!(
            "{} in {} files",
            human_size(e.artifact_bytes),
            e.artifact_files
        ),
    );
    kv(ui, "Subagents", &e.subagents.to_string());
    kv_label(ui, "Usage");
    match &row.stats {
        StatsSlot::Ready(s) => {
            kv(ui, "Prompts", &s.user_messages.to_string());
            kv(ui, "Turns", &s.assistant_turns.to_string());
            kv(
                ui,
                "Tokens",
                &format!(
                    "{} in · {} out",
                    compact(s.input_tokens),
                    compact(s.output_tokens)
                ),
            );
            kv(
                ui,
                "Cache",
                &format!(
                    "{} read · {} write",
                    compact(s.cache_read_tokens),
                    compact(s.cache_write_tokens)
                ),
            );
            kv(
                ui,
                "Cost",
                &format!(
                    "{} session + {} subagents",
                    money(s.cost),
                    money(s.subagent_cost)
                ),
            );
            kv(ui, "Compactions", &s.compactions.to_string());
            kv(
                ui,
                "Models",
                &if s.models.is_empty() {
                    "—".to_owned()
                } else {
                    s.models.join("\n")
                },
            );
        }
        StatsSlot::Pending => {
            ui.label(RichText::new("Reading the transcript…").color(pal.weak));
        }
        StatsSlot::Failed(err) => {
            ui.label(RichText::new(format!("Could not read it: {err}")).color(pal.danger));
        }
    }
}

fn detail_buttons(
    ui: &mut Ui,
    row: &Row,
    projects: &[Project],
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    let path = row.path().to_path_buf();
    let idle = input.op.is_none();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
        if action_button(ui, ICON_EXTERNAL_LINK, "Open", input.omp, false)
            .on_hover_text("Open the session in a tab")
            .clicked()
        {
            out.push(SessionsAction::Open(path.clone()));
        }
        let pin_label = if row.pinned { "Unpin" } else { "Pin" };
        if action_button(ui, ICON_STAR, pin_label, input.omp, false)
            .on_hover_text("Pinned sessions come first (Alt+K pins, Alt+Shift+K unpins)")
            .clicked()
        {
            out.push(SessionsAction::Pin {
                id: row.entry.id.clone(),
                pinned: !row.pinned,
            });
        }
        if action_button(ui, ICON_PENCIL, "Rename…", input.omp && idle, false)
            .on_hover_text("Rename the session (Ctrl+R)")
            .clicked()
        {
            out.push(SessionsAction::Rename(path.clone()));
        }
        transfer_button(
            ui,
            TransferKind::Copy,
            input.omp && idle,
            std::slice::from_ref(&path),
            projects,
            out,
        );
        transfer_button(
            ui,
            TransferKind::Move,
            input.omp && idle,
            std::slice::from_ref(&path),
            projects,
            out,
        );
        if row.live.running
            && action_button(
                ui,
                ICON_CIRCLE_STOP,
                "Kill process",
                input.can_stop && idle,
                true,
            )
            .on_hover_text("Kill this session's omp process (the session file stays)")
            .clicked()
        {
            out.push(SessionsAction::Stop(vec![path.clone()]));
        }
        if action_button(ui, ICON_TRASH_2, "Delete…", input.omp && idle, true).clicked() {
            out.push(SessionsAction::Delete(vec![path]));
        }
    });
}

// ------------------------------------------------------------------ footer

fn footer(ui: &mut Ui, model: &mut Model, input: &ViewInput<'_>, out: &mut Vec<SessionsAction>) {
    let pal = Palette::of_ui(ui);
    let totals = model.selection_totals();
    let paths = selected_paths(model);
    let live: Vec<PathBuf> = model
        .selected_rows()
        .iter()
        .filter(|r| r.live.running)
        .map(|r| r.entry.path.clone())
        .collect();
    let running = live.len();
    let idle = input.op.is_none();
    let mut clear = false;
    let projects = model.projects();
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), FOOT_H),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            Sides::new().show(
                ui,
                |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    if let Some(op) = input.op {
                        ui.add(Spin::new(17.0, pal.accent));
                        ui.label(
                            theme::strong(format!("{} {} / {}…", op.verb, op.done, op.total))
                                .color(pal.text_strong),
                        );
                        let (rect, _) = ui.allocate_exact_size(vec2(160.0, 6.0), Sense::hover());
                        ui.painter()
                            .rect_filled(rect, 3, theme::mix(pal.surface, pal.weak, 0.28));
                        let frac = if op.total == 0 {
                            0.0
                        } else {
                            (op.done as f32 / op.total as f32).clamp(0.0, 1.0)
                        };
                        ui.painter().rect_filled(
                            Rect::from_min_size(rect.min, vec2(rect.width() * frac, 6.0)),
                            3,
                            pal.accent,
                        );
                    } else if totals.count == 0 {
                        ui.label(
                            RichText::new(
                                "Select sessions with the checkboxes (Shift for a range)",
                            )
                            .color(pal.weak),
                        );
                    } else {
                        ui.label(
                            theme::strong(format!(
                                "{} selected · frees {}",
                                totals.count,
                                human_size(totals.bytes)
                            ))
                            .color(pal.text_strong),
                        );
                        if running > 0 {
                            ui.label(
                                RichText::new(format!("{running} running"))
                                    .size(12.5)
                                    .color(pal.warn),
                            );
                        }
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Clear").color(pal.weak))
                                    .frame(false),
                            )
                            .on_hover_cursor(CursorIcon::PointingHand)
                            .clicked()
                        {
                            clear = true;
                        }
                    }
                },
                |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let on = input.omp && idle && totals.count > 0;
                    if action_button(ui, ICON_TRASH_2, "Delete…", on, true)
                        .on_hover_text("Delete the selected sessions and their artifacts (Del)")
                        .clicked()
                    {
                        out.push(SessionsAction::Delete(paths.clone()));
                    }
                    let can_stop = input.can_stop && idle && running > 0;
                    if action_button(ui, ICON_CIRCLE_STOP, "Kill process", can_stop, true)
                        .on_hover_text("Kill the omp process of the selected running sessions")
                        .clicked()
                    {
                        out.push(SessionsAction::Stop(live.clone()));
                    }
                    transfer_button(ui, TransferKind::Move, on, &paths, projects, out);
                    transfer_button(ui, TransferKind::Copy, on, &paths, projects, out);
                },
            );
        },
    );
    if clear {
        model.sel.clear();
    }
}

/// "Copy to…" / "Move to…": a menu of the other projects plus "Choose folder…".
fn transfer_button(
    ui: &mut Ui,
    kind: TransferKind,
    enabled: bool,
    paths: &[PathBuf],
    projects: &[Project],
    out: &mut Vec<SessionsAction>,
) {
    let (icon, label) = match kind {
        TransferKind::Copy => (ICON_FOLDER_INPUT, "Copy to…"),
        TransferKind::Move => (ICON_FOLDER_OUTPUT, "Move to…"),
    };
    let resp = action_button(ui, icon, label, enabled, false);
    let pal = Palette::of_ui(ui);
    let mut send = |dest: Option<PathBuf>| {
        out.push(SessionsAction::Transfer {
            kind,
            paths: paths.to_vec(),
            dest,
        });
    };
    theme::dropup_menu(&resp, 280.0, |ui| {
        ui.set_min_width(280.0);
        let mut dests: Vec<&Project> = projects.iter().filter(|p| !p.missing).collect();
        dests.sort_by_key(|p| p.label.to_lowercase());
        egui::ScrollArea::vertical()
            .max_height(300.0)
            .auto_shrink([true, true])
            .show(ui, |ui| {
                if dests.is_empty() {
                    ui.add_enabled(
                        false,
                        egui::Label::new(RichText::new("No other projects").color(pal.weak)),
                    );
                }
                for p in dests {
                    if crate::widgets::menu_item(ui, ICON_FOLDER, &p.label, None, true, false)
                        .on_hover_text(p.cwd.display().to_string())
                        .clicked()
                    {
                        send(Some(p.cwd.clone()));
                        ui.close();
                    }
                }
            });
        ui.separator();
        if crate::widgets::menu_item(ui, ICON_FOLDER_OPEN, "Choose folder…", None, true, false)
            .clicked()
        {
            send(None);
            ui.close();
        }
    });
}

/// Drafted button; `danger` makes it the destructive kind.
pub(crate) fn action_button(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    enabled: bool,
    danger: bool,
) -> egui::Response {
    DraftButton::new(label)
        .icon(icon)
        .kind(if danger {
            DraftKind::Danger
        } else {
            DraftKind::Secondary
        })
        .enabled(enabled)
        .show(ui)
}
