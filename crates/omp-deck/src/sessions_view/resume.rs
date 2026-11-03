//! Resume mode of the session manager (omp's `/resume` picker, natively): the sessions of the
//! current project — pinned first, then newest — or every project, or a Claude Code / Codex
//! source to import from. The primary action resumes the pick into the tab the manager was
//! opened from; the secondary one opens it in a tab of its own. Every row carries its actions as
//! hover buttons and in a context menu.
//!
//! State and rules are in [`crate::resume_model`]; this module lays out and paints and returns
//! [`SessionsAction`]s.

use std::path::Path;
use std::sync::Arc;

use deck_core::omp::ForeignSession;
use egui::epaint::text::Galley;
use egui::{
    Align, Align2, Color32, CursorIcon, Frame, Id, Key, Layout, Margin, Modifiers, Rect, Response,
    RichText, Sense, Sides, Stroke, StrokeKind, Ui, pos2, vec2,
};

use super::{FOOT_H, SessionsAction, ViewInput, ViewState, lay, pane, row_key, row_key_action};
use crate::icons::{
    ICON_CORNER_DOWN_RIGHT, ICON_EXTERNAL_LINK, ICON_HISTORY, ICON_INFO, ICON_PENCIL, ICON_PLAY,
    ICON_SEARCH, ICON_STAR, ICON_TRASH_2, Icon,
};
use crate::resume_model::{Flow, ForeignLoad, Pick, ResumeState, Source, Then};
use crate::sessions_model::{Model, Row, plural};
use crate::sidebar::format_age;
use crate::theme::{self, Palette};
use crate::usage_view::tab_chip;
use crate::widgets::{DraftButton, DraftKind, Spin, empty_state, icon_text, menu_item, paint_icon};
use deck_core::sessions::abbreviate_home;

const FILTER_ID: &str = "omp-deck-resume-filter";
const OMP_ROW_H: f32 = 52.0;
const FOREIGN_ROW_H: f32 = 66.0;
const BTN: f32 = 26.0;
const BTN_STEP: f32 = 30.0;
/// Left gutter of a row: the pin star.
const GUTTER: f32 = 34.0;

/// The picked list entry, resolved.
#[derive(Clone, Copy)]
enum Cur<'a> {
    Omp(&'a Row),
    Foreign(&'a ForeignSession),
}

/// What every row needs besides itself.
struct Env<'a> {
    pal: &'a Palette,
    input: &'a ViewInput<'a>,
    /// An action is in flight: nothing new can start.
    busy: bool,
    /// The list shows every project (rows name their project).
    all: bool,
    /// Cache key of laid-out row text.
    key: u64,
}

impl Env<'_> {
    fn here_ok(&self) -> bool {
        self.input.here.is_ok()
    }
}

// ------------------------------------------------------------------ picking

/// The entry the pick names, if the list still holds it.
fn current<'a>(model: &'a Model, r: &'a ResumeState) -> Option<Cur<'a>> {
    match (&r.pick, r.source) {
        (Some(Pick::Session(p)), Source::Omp) => model.row_by_path(p).map(Cur::Omp),
        (Some(Pick::Foreign(id)), Source::Foreign(f)) => match r.foreign(f) {
            ForeignLoad::Ready(l) => l.iter().find(|s| s.id == *id).map(Cur::Foreign),
            _ => None,
        },
        _ => None,
    }
}

/// Position of the pick in the list.
fn pick_pos(model: &Model, r: &ResumeState) -> Option<usize> {
    let order = r.order();
    match (&r.pick, r.source) {
        (Some(Pick::Session(p)), Source::Omp) => order
            .iter()
            .position(|&i| model.rows().get(i).is_some_and(|row| row.path() == p)),
        (Some(Pick::Foreign(id)), Source::Foreign(f)) => match r.foreign(f) {
            ForeignLoad::Ready(l) => order
                .iter()
                .position(|&i| l.get(i).is_some_and(|s| s.id == *id)),
            _ => None,
        },
        _ => None,
    }
}

/// The entry at list position `pos`, as a pick.
fn pick_at(model: &Model, r: &ResumeState, pos: usize) -> Option<Pick> {
    let i = *r.order().get(pos)?;
    match r.source {
        Source::Omp => model
            .rows()
            .get(i)
            .map(|row| Pick::Session(row.path().to_path_buf())),
        Source::Foreign(f) => match r.foreign(f) {
            ForeignLoad::Ready(l) => l.get(i).map(|s| Pick::Foreign(s.id.clone())),
            _ => None,
        },
    }
}

fn row_h(source: Source) -> f32 {
    match source {
        Source::Omp => OMP_ROW_H,
        Source::Foreign(_) => FOREIGN_ROW_H,
    }
}

/// Brings the list up to date and makes sure the pick names an entry of it (the first one when
/// it names none: omp's picker always has a highlighted row).
fn sync(model: &Model, st: &mut ViewState) {
    let r = &mut st.resume;
    match r.source {
        Source::Omp => r.ensure_order(model.cells_rev(), |scope, text| {
            model.resume_order(scope, text)
        }),
        Source::Foreign(f) => r.ensure_foreign_order(f),
    }
    if pick_pos(model, r).is_none() {
        r.pick = pick_at(model, r, 0);
    }
}

/// Moves the pick by `step` entries (clamped) and scrolls it into view.
fn move_pick(model: &Model, r: &mut ResumeState, step: isize) {
    let n = r.order().len();
    if n == 0 {
        return;
    }
    let cur = pick_pos(model, r).unwrap_or(0);
    let pos = cur.saturating_add_signed(step).min(n - 1);
    if let Some(p) = pick_at(model, r, pos) {
        r.pick = Some(p);
    }
    let h = row_h(r.source);
    let (top, view) = r.viewport;
    let (a, b) = (pos as f32 * h, (pos + 1) as f32 * h);
    if a < top {
        r.scroll_to = Some(a);
    } else if b > top + view {
        r.scroll_to = Some((b - view).max(0.0));
    }
}

// ------------------------------------------------------------------ actions

/// Resume `cur` into the origin tab (an imported session first).
fn resume_action(cur: Cur<'_>, input: &ViewInput<'_>) -> Option<SessionsAction> {
    if !input.omp || input.here.is_err() {
        return None;
    }
    Some(match cur {
        Cur::Omp(row) => SessionsAction::ResumeHere(row.path().to_path_buf()),
        Cur::Foreign(s) => SessionsAction::ImportForeign {
            source: s.source,
            id: s.id.clone(),
            cwd: s.cwd.clone(),
            then: Then::ResumeHere,
        },
    })
}

/// Open `cur` in a tab of its own (an imported session first).
fn open_action(cur: Cur<'_>, input: &ViewInput<'_>) -> Option<SessionsAction> {
    if !input.omp {
        return None;
    }
    Some(match cur {
        Cur::Omp(row) => SessionsAction::Open(row.path().to_path_buf()),
        Cur::Foreign(s) => SessionsAction::ImportForeign {
            source: s.source,
            id: s.id.clone(),
            cwd: s.cwd.clone(),
            then: Then::NewTab,
        },
    })
}

/// Enter / double-click: resume here; with nowhere to resume into, open in a new tab.
fn primary_action(cur: Cur<'_>, input: &ViewInput<'_>) -> Option<SessionsAction> {
    resume_action(cur, input).or_else(|| open_action(cur, input))
}

/// The tooltip of the Resume here button: what it does, or why it cannot.
fn resume_tip(input: &ViewInput<'_>) -> String {
    match input.here {
        Ok(title) => format!("Resume here — switch the tab “{title}” to this session (Enter)"),
        Err(why) => why.to_owned(),
    }
}

// --------------------------------------------------------------------- keys

/// Keyboard of resume mode: arrows move the pick, Enter resumes, Ctrl+R renames, Alt+K pins,
/// Alt+Shift+K unpins, Ctrl+D deletes, Esc clears the filter, then closes. The filter box keeps
/// its focus meanwhile; the keys it would use itself (arrows, Enter) are taken first.
pub(super) fn keys(
    ctx: &egui::Context,
    model: &Model,
    st: &mut ViewState,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    if input.blocked || egui::Popup::is_any_open(ctx) {
        return;
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        if st.resume.filter.is_empty() {
            out.push(SessionsAction::Close);
            return;
        }
        st.resume.filter.clear();
    }
    let step: isize = ctx.input_mut(|i| {
        [
            (Key::ArrowDown, 1),
            (Key::ArrowUp, -1),
            (Key::PageDown, 8),
            (Key::PageUp, -8),
        ]
        .into_iter()
        .find_map(|(k, s)| i.consume_key(Modifiers::NONE, k).then_some(s))
        .unwrap_or(0)
    });
    if step != 0 {
        move_pick(model, &mut st.resume, step);
    }
    let enter = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
    let key = row_key(ctx);
    if !st.resume.flow.is_idle() {
        return;
    }
    let Some(cur) = current(model, &st.resume) else {
        return;
    };
    if enter && let Some(a) = primary_action(cur, input) {
        out.push(a);
    }
    if let (Some(key), Cur::Omp(row)) = (key, cur)
        && let Some(a) = row_key_action(key, row, input)
    {
        out.push(a);
    }
}

// --------------------------------------------------------------------- body

pub(super) fn body(
    ui: &mut Ui,
    model: &mut Model,
    st: &mut ViewState,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    sync(model, st);
    toolbar(ui, model, st, out);
    ui.add_space(6.0);
    origin_line(ui, input);
    ui.add_space(6.0);
    let avail = ui.available_size();
    let body_h = (avail.y - FOOT_H - 8.0).max(120.0);
    pane(ui, avail.x, body_h, |ui| list(ui, model, st, input, out));
    ui.add_space(8.0);
    footer(ui, model, st, input, out);
}

fn toolbar(ui: &mut Ui, model: &Model, st: &mut ViewState, out: &mut Vec<SessionsAction>) {
    let pal = Palette::of_ui(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for src in Source::ALL {
            let tip = match src {
                Source::Omp => "Sessions saved by omp",
                Source::Foreign(_) => "Sessions of another coding agent — omp imports them",
            };
            if tab_chip(ui, src.label(), st.resume.source == src)
                .on_hover_text(tip)
                .clicked()
                && st.resume.source != src
            {
                out.push(SessionsAction::SetSource(src));
            }
        }
        ui.add_space(8.0);
        let id = Id::unique(FILTER_ID);
        let focused = ui.memory(|m| m.has_focus(id));
        let resp = ui.add(
            egui::TextEdit::singleline(&mut st.resume.filter)
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
        if std::mem::take(&mut st.resume.focus_filter) {
            resp.request_focus();
        }
        ui.add_space(6.0);
        if let Some(project) = st.resume.project.as_deref() {
            let all = st.resume.all_projects;
            let label = model.project_label(project);
            if tab_chip(ui, "All projects", all)
                .on_hover_text("List the sessions of every project, not only the current one")
                .clicked()
            {
                st.resume.all_projects = !all;
            }
            if !all {
                ui.label(
                    RichText::new(format!("in {label}"))
                        .size(12.5)
                        .color(pal.weak),
                );
            }
        }
    });
}

/// Where Resume here goes (or why it cannot).
fn origin_line(ui: &mut Ui, input: &ViewInput<'_>) {
    let pal = Palette::of_ui(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        match input.here {
            Ok(title) => {
                ui.label(icon_text(ICON_CORNER_DOWN_RIGHT, 14.0, pal.accent));
                ui.label(RichText::new("Resume here switches the tab").color(pal.weak));
                ui.add(egui::Label::new(theme::strong(title).color(pal.text_strong)).truncate());
            }
            Err(why) => {
                ui.label(icon_text(ICON_INFO, 14.0, pal.warn));
                ui.label(RichText::new(why).color(pal.warn));
                ui.label(
                    RichText::new("Enter opens the session in a new tab instead.").color(pal.weak),
                );
            }
        }
    });
}

// --------------------------------------------------------------------- list

fn list(
    ui: &mut Ui,
    model: &Model,
    st: &mut ViewState,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    let pal = Palette::of_ui(ui);
    if let Some(e) = st.resume.error.clone() {
        ui.label(RichText::new(e).color(pal.danger));
        ui.add_space(4.0);
    }
    match st.resume.source {
        Source::Omp => omp_list(ui, model, st, input, out),
        Source::Foreign(src) => foreign_list(ui, st, src, input, out),
    }
}

fn spinner(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 2.0 - 30.0);
        ui.add(Spin::new(22.0, pal.accent));
        ui.add_space(8.0);
        ui.label(RichText::new(text).color(pal.weak));
    });
}

fn omp_list(
    ui: &mut Ui,
    model: &Model,
    st: &mut ViewState,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    let pal = Palette::of_ui(ui);
    if !input.loaded {
        spinner(ui, "Scanning sessions…");
        return;
    }
    if model.rows().is_empty() {
        empty_state(
            ui,
            ICON_HISTORY,
            pal.accent,
            "No sessions found",
            "omp has not saved any session in this sessions folder yet.",
        );
        return;
    }
    if st.resume.order().is_empty() {
        empty_state(
            ui,
            ICON_HISTORY,
            pal.accent,
            "No sessions match",
            if st.resume.all_projects {
                "Nothing matches the filter."
            } else {
                "This project has no matching session — try “All projects”."
            },
        );
        return;
    }
    let env = Env {
        pal,
        input,
        busy: !st.resume.flow.is_idle(),
        all: st.resume.all_projects,
        key: crate::paint_cache::with(
            crate::paint_cache::style_key(ui.ctx(), pal),
            (model.cells_rev(), 0xA11_u16, st.resume.all_projects),
        ),
    };
    let scroll = st.resume.scroll_to.take();
    let mut area = egui::ScrollArea::vertical()
        .id_salt("resume-list")
        .auto_shrink([false, false]);
    if let Some(y) = scroll {
        area = area.vertical_scroll_offset(y);
    }
    let n = st.resume.order().len();
    let r = &st.resume;
    let mut picked: Option<Pick> = None;
    let shown = area.show_rows(ui, OMP_ROW_H, n, |ui, range| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        for pos in range {
            let Some(row) = r.order().get(pos).and_then(|&i| model.rows().get(i)) else {
                continue;
            };
            let selected = r.pick.as_ref() == Some(&Pick::Session(row.path().to_path_buf()));
            if omp_row(ui, row, selected, &env, out) {
                picked = Some(Pick::Session(row.path().to_path_buf()));
            }
        }
    });
    st.resume.viewport = (shown.state.clamped_offset().y, shown.inner_rect.height());
    if picked.is_some() {
        st.resume.pick = picked;
    }
}

fn foreign_list(
    ui: &mut Ui,
    st: &mut ViewState,
    src: deck_core::omp::ForeignSource,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    let pal = Palette::of_ui(ui);
    let name = Source::Foreign(src).label();
    let scroll = st.resume.scroll_to.take();
    let list = match st.resume.foreign(src) {
        ForeignLoad::Idle | ForeignLoad::Loading => {
            spinner(ui, &format!("Reading the {name} sessions…"));
            return;
        }
        ForeignLoad::Failed(e) => {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(theme::strong(format!("Could not read the {name} sessions")));
                ui.label(RichText::new(e).color(pal.danger));
                ui.add_space(8.0);
                if DraftButton::new("Try again").show(ui).clicked() {
                    out.push(SessionsAction::ReloadForeign(src));
                }
            });
            return;
        }
        ForeignLoad::Ready(l) => l,
    };
    if list.is_empty() {
        empty_state(
            ui,
            ICON_HISTORY,
            pal.accent,
            &format!("No {name} sessions found"),
            &format!("omp found no {name} session on this machine to import."),
        );
        return;
    }
    if st.resume.order().is_empty() {
        empty_state(
            ui,
            ICON_HISTORY,
            pal.accent,
            "No sessions match",
            if st.resume.all_projects {
                "Nothing matches the filter."
            } else {
                "No session of this project — try “All projects”."
            },
        );
        return;
    }
    let env = Env {
        pal,
        input,
        busy: !st.resume.flow.is_idle(),
        all: st.resume.all_projects,
        key: 0,
    };
    let mut area = egui::ScrollArea::vertical()
        .id_salt("resume-list-foreign")
        .auto_shrink([false, false]);
    if let Some(y) = scroll {
        area = area.vertical_scroll_offset(y);
    }
    let n = st.resume.order().len();
    let r = &st.resume;
    let mut picked: Option<Pick> = None;
    let shown = area.show_rows(ui, FOREIGN_ROW_H, n, |ui, range| {
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        for pos in range {
            let Some(s) = r.order().get(pos).and_then(|&i| list.get(i)) else {
                continue;
            };
            let selected = r.pick.as_ref() == Some(&Pick::Foreign(s.id.clone()));
            if foreign_row(ui, s, selected, &env, out) {
                picked = Some(Pick::Foreign(s.id.clone()));
            }
        }
    });
    st.resume.viewport = (shown.state.clamped_offset().y, shown.inner_rect.height());
    if picked.is_some() {
        st.resume.pick = picked;
    }
}

// --------------------------------------------------------------------- rows

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Plain,
    /// A toggled-on state (a pinned star).
    Lit,
    Primary,
    Danger,
}

struct Slot<'a> {
    icon: Icon,
    tip: &'a str,
    enabled: bool,
    tone: Tone,
}

/// The hover buttons, right to left from the end of `rect`. Always interactive (so hovering them
/// keeps the row's highlight) but painted — and clickable — only while `show` holds; returns the
/// clicked slot and whether the pointer is over a button.
fn hover_buttons(
    ui: &mut Ui,
    resp: &Response,
    rect: Rect,
    slots: &[Slot<'_>],
) -> (Option<usize>, bool, bool) {
    let pal = Palette::of_ui(ui);
    let n = slots.len();
    let rects: Vec<Rect> = (0..n)
        .map(|k| {
            Rect::from_center_size(
                pos2(
                    rect.right() - 10.0 - BTN / 2.0 - BTN_STEP * (n - 1 - k) as f32,
                    rect.center().y,
                ),
                vec2(BTN, BTN),
            )
        })
        .collect();
    let btns: Vec<Response> = rects
        .iter()
        .enumerate()
        .map(|(k, r)| ui.interact(*r, resp.id.with(("act", k)), Sense::click()))
        .collect();
    let over = btns.iter().any(Response::hovered);
    let show = resp.hovered() || over || resp.context_menu_opened();
    if !show {
        return (None, over, false);
    }
    let mut clicked = None;
    for (k, ((slot, r), b)) in slots.iter().zip(&rects).zip(&btns).enumerate() {
        let hov =
            ui.ctx()
                .animate_bool_with_time(b.id.with("hov"), b.hovered() && slot.enabled, 0.1);
        let p = ui.painter();
        let radius = theme::rad(6, 9);
        let (fill, ink) = match slot.tone {
            Tone::Primary => (
                pal.accent.gamma_multiply(0.85 + 0.15 * hov),
                theme::on_accent(pal),
            ),
            Tone::Danger => (pal.danger.gamma_multiply(0.14 * (1.0 + hov)), pal.danger),
            Tone::Lit => (pal.accent.gamma_multiply(0.18), pal.accent),
            Tone::Plain => (
                pal.raised.gamma_multiply(0.5 + 0.5 * hov),
                theme::mix(pal.weak, pal.text_strong, hov),
            ),
        };
        let ink = if slot.enabled {
            ink
        } else {
            ink.gamma_multiply(0.35)
        };
        p.rect_filled(*r, radius, fill);
        p.rect_stroke(
            *r,
            radius,
            Stroke::new(
                1.0,
                if hov > 0.0 {
                    theme::hover_stroke(pal)
                } else {
                    pal.stroke
                },
            ),
            StrokeKind::Inside,
        );
        paint_icon(p, r.center(), Align2::CENTER_CENTER, slot.icon, 15.0, ink);
        if b.hovered() {
            ui.ctx().set_cursor_icon(if slot.enabled {
                CursorIcon::PointingHand
            } else {
                CursorIcon::NotAllowed
            });
        }
        if b.clicked() && slot.enabled {
            clicked = Some(k);
        }
    }
    for (slot, b) in slots.iter().zip(&btns) {
        // Tooltips last: the buttons are laid out already.
        let _ = b.clone().on_hover_text(slot.tip);
    }
    (clicked, over, true)
}

/// Background of a list row: the pick's accent fill and outline, the hover pop.
fn row_bg(ui: &mut Ui, resp: &Response, inner: Rect, selected: bool, lit: bool) {
    let pal = Palette::of_ui(ui);
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), lit, 0.1);
    let r = theme::rad(6, 10);
    let p = ui.painter();
    if selected {
        p.rect_filled(inner, r, theme::accent_fill(pal, 0.10));
    }
    super::paint_row_bg(p, inner, r, pal, selected, hov);
    if !selected && hov == 0.0 {
        p.hline(
            inner.x_range().shrink(6.0),
            resp.rect.bottom(),
            Stroke::new(1.0, theme::mix(pal.surface, pal.stroke, 0.5)),
        );
    }
}

/// First line of `text`, for a one-line preview.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

fn omp_row(
    ui: &mut Ui,
    row: &Row,
    selected: bool,
    env: &Env<'_>,
    out: &mut Vec<SessionsAction>,
) -> bool {
    let pal = env.pal;
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), OMP_ROW_H), Sense::click());
    let inner = rect.shrink2(vec2(0.0, 2.0));
    let can = env.input.omp && !env.busy;
    let idle_op = env.input.op.is_none();
    let resume_tip = resume_tip(env.input);
    let slots = [
        Slot {
            icon: ICON_TRASH_2,
            tip: "Delete this session… (Ctrl+D)",
            enabled: can && idle_op,
            tone: Tone::Danger,
        },
        Slot {
            icon: ICON_STAR,
            tip: if row.pinned {
                "Unpin (Alt+Shift+K)"
            } else {
                "Pin to the top (Alt+K)"
            },
            enabled: can,
            tone: if row.pinned { Tone::Lit } else { Tone::Plain },
        },
        Slot {
            icon: ICON_PENCIL,
            tip: "Rename… (Ctrl+R)",
            enabled: can && idle_op,
            tone: Tone::Plain,
        },
        Slot {
            icon: ICON_EXTERNAL_LINK,
            tip: "Open in a new tab",
            enabled: can,
            tone: Tone::Plain,
        },
        Slot {
            icon: ICON_PLAY,
            tip: &resume_tip,
            enabled: can && env.here_ok(),
            tone: Tone::Primary,
        },
    ];
    let (clicked, over, show) = hover_buttons(ui, &resp, inner, &slots);
    if ui.is_rect_visible(rect) {
        row_bg(
            ui,
            &resp,
            inner,
            selected,
            resp.hovered() || over || resp.context_menu_opened(),
        );
        let g = {
            let mut cache = row.paint.borrow_mut();
            let galleys = cache.get(env.key, || omp_galleys(ui.painter(), row, env));
            [
                Arc::clone(&galleys[0]),
                Arc::clone(&galleys[1]),
                Arc::clone(&galleys[2]),
            ]
        };
        let p = ui.painter();
        if row.pinned {
            paint_icon(
                p,
                pos2(inner.left() + GUTTER / 2.0 + 2.0, inner.center().y),
                Align2::CENTER_CENTER,
                ICON_STAR,
                15.0,
                pal.accent,
            );
        }
        let right = if show {
            inner.right() - 10.0 - BTN_STEP * slots.len() as f32
        } else {
            inner.right() - 12.0 - g[2].size().x - 8.0
        };
        let text = Rect::from_min_max(
            pos2(inner.left() + GUTTER, inner.top()),
            pos2(right.max(inner.left() + GUTTER + 40.0), inner.bottom()),
        );
        let tp = p.with_clip_rect(text);
        let gap = 3.0;
        let h = g[0].size().y + g[1].size().y + gap;
        let top = inner.center().y - h / 2.0;
        tp.galley(
            pos2(text.left(), top),
            Arc::clone(&g[0]),
            Color32::PLACEHOLDER,
        );
        tp.galley(
            pos2(text.left(), top + g[0].size().y + gap),
            Arc::clone(&g[1]),
            Color32::PLACEHOLDER,
        );
        if !show {
            p.galley(
                pos2(
                    inner.right() - 12.0 - g[2].size().x,
                    inner.center().y - g[2].size().y / 2.0,
                ),
                Arc::clone(&g[2]),
                Color32::PLACEHOLDER,
            );
        }
        if row.live.running {
            let at = pos2(inner.left() + GUTTER - 10.0, top + g[0].size().y / 2.0);
            if !row.pinned {
                p.circle_filled(at, 3.0, pal.ok);
            }
        }
    }
    let path = row.path().to_path_buf();
    let action = clicked.and_then(|k| match k {
        0 => Some(SessionsAction::Delete(vec![path.clone()])),
        1 => Some(SessionsAction::Pin {
            id: row.entry.id.clone(),
            pinned: !row.pinned,
        }),
        2 => Some(SessionsAction::Rename(path.clone())),
        3 => Some(SessionsAction::Open(path.clone())),
        4 => Some(SessionsAction::ResumeHere(path.clone())),
        _ => None,
    });
    let mut selected_now = false;
    if let Some(a) = action {
        out.push(a);
        selected_now = true;
    } else if !over {
        if resp.double_clicked() {
            if !env.busy
                && let Some(a) = primary_action(Cur::Omp(row), env.input)
            {
                out.push(a);
            }
            selected_now = true;
        } else if resp.clicked() || resp.secondary_clicked() {
            selected_now = true;
        }
    }
    let _ = resp.clone().on_hover_cursor(CursorIcon::PointingHand);
    theme::context_menu(&resp, 250.0, |ui| omp_menu(ui, row, env, out));
    selected_now
}

fn omp_galleys(p: &egui::Painter, row: &Row, env: &Env<'_>) -> Vec<Arc<Galley>> {
    let pal = env.pal;
    let title_ink = if row.entry.is_stub() {
        pal.weak
    } else {
        pal.text_strong
    };
    let prompt = row
        .entry
        .first_prompt
        .as_deref()
        .map_or("", first_line)
        .trim();
    // A session titled by its first prompt would show the same text twice.
    let prompt = if prompt == row.cells.title {
        ""
    } else {
        prompt
    };
    let sub = if env.all {
        format!("{} · {}", row.cells.project, prompt)
    } else {
        prompt.to_owned()
    };
    vec![
        lay(p, &row.cells.title, 13.5, title_ink),
        lay(p, &sub, 12.0, pal.weak),
        lay(p, &row.cells.activity, 12.5, pal.weak),
    ]
}

fn omp_menu(ui: &mut Ui, row: &Row, env: &Env<'_>, out: &mut Vec<SessionsAction>) {
    let path = row.path().to_path_buf();
    let can = env.input.omp && !env.busy;
    let idle_op = env.input.op.is_none();
    let mut send = |ui: &mut Ui, a: SessionsAction| {
        out.push(a);
        ui.close();
    };
    if menu_item(
        ui,
        ICON_PLAY,
        "Resume here",
        Some("Enter"),
        can && env.here_ok(),
        false,
    )
    .on_disabled_hover_text(resume_tip(env.input))
    .clicked()
    {
        send(ui, SessionsAction::ResumeHere(path.clone()));
    }
    if menu_item(
        ui,
        ICON_EXTERNAL_LINK,
        "Open in a new tab",
        None,
        can,
        false,
    )
    .clicked()
    {
        send(ui, SessionsAction::Open(path.clone()));
    }
    ui.separator();
    let (label, shortcut) = if row.pinned {
        ("Unpin", "Alt+Shift+K")
    } else {
        ("Pin to the top", "Alt+K")
    };
    if menu_item(ui, ICON_STAR, label, Some(shortcut), can, false).clicked() {
        send(
            ui,
            SessionsAction::Pin {
                id: row.entry.id.clone(),
                pinned: !row.pinned,
            },
        );
    }
    if menu_item(
        ui,
        ICON_PENCIL,
        "Rename…",
        Some("Ctrl+R"),
        can && idle_op,
        false,
    )
    .clicked()
    {
        send(ui, SessionsAction::Rename(path.clone()));
    }
    ui.separator();
    if menu_item(
        ui,
        ICON_TRASH_2,
        "Delete…",
        Some("Ctrl+D"),
        can && idle_op,
        true,
    )
    .clicked()
    {
        send(ui, SessionsAction::Delete(vec![path]));
    }
}

fn foreign_row(
    ui: &mut Ui,
    s: &ForeignSession,
    selected: bool,
    env: &Env<'_>,
    out: &mut Vec<SessionsAction>,
) -> bool {
    let pal = env.pal;
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), FOREIGN_ROW_H), Sense::click());
    let inner = rect.shrink2(vec2(0.0, 2.0));
    let can = env.input.omp && !env.busy;
    let resume_tip = resume_tip(env.input);
    let slots = [
        Slot {
            icon: ICON_EXTERNAL_LINK,
            tip: "Import and open in a new tab",
            enabled: can,
            tone: Tone::Plain,
        },
        Slot {
            icon: ICON_PLAY,
            tip: &resume_tip,
            enabled: can && env.here_ok(),
            tone: Tone::Primary,
        },
    ];
    let (clicked, over, show) = hover_buttons(ui, &resp, inner, &slots);
    if ui.is_rect_visible(rect) {
        row_bg(
            ui,
            &resp,
            inner,
            selected,
            resp.hovered() || over || resp.context_menu_opened(),
        );
        let now = std::time::SystemTime::now();
        let modified = std::time::UNIX_EPOCH
            + std::time::Duration::from_millis(u64::try_from(s.modified).unwrap_or(0));
        let age = format_age(now, modified);
        let title = s
            .title
            .as_deref()
            .map(first_line)
            .filter(|t| !t.is_empty())
            .or_else(|| s.first_message.as_deref().map(first_line))
            .filter(|t| !t.is_empty())
            .unwrap_or("(untitled)");
        let messages = s.message_count.map_or_else(String::new, |n| {
            format!(" · {}", plural(n as usize, "message", "messages"))
        });
        let folder = abbreviate_home(Path::new(&s.cwd));
        let p = ui.painter();
        let g_title = lay(p, title, 13.5, pal.text_strong);
        let g_cwd = lay(p, &format!("{folder}{messages}"), 12.0, pal.text);
        let first = s.first_message.as_deref().map_or("", first_line);
        let g_first = lay(p, first, 12.0, pal.weak);
        let g_age = lay(p, &age, 12.5, pal.weak);
        let right = if show {
            inner.right() - 10.0 - BTN_STEP * slots.len() as f32
        } else {
            inner.right() - 12.0 - g_age.size().x - 8.0
        };
        let text = Rect::from_min_max(
            pos2(inner.left() + 12.0, inner.top()),
            pos2(right.max(inner.left() + 80.0), inner.bottom()),
        );
        let tp = p.with_clip_rect(text);
        let h = g_title.size().y + g_cwd.size().y + g_first.size().y;
        let mut y = inner.center().y - h / 2.0;
        for g in [&g_title, &g_cwd, &g_first] {
            tp.galley(pos2(text.left(), y), Arc::clone(g), Color32::PLACEHOLDER);
            y += g.size().y;
        }
        if !show {
            p.galley(
                pos2(
                    inner.right() - 12.0 - g_age.size().x,
                    inner.center().y - g_age.size().y / 2.0,
                ),
                g_age,
                Color32::PLACEHOLDER,
            );
        }
    }
    let then = |k: usize| {
        if k == 0 {
            Then::NewTab
        } else {
            Then::ResumeHere
        }
    };
    let mut selected_now = false;
    if let Some(k) = clicked {
        out.push(SessionsAction::ImportForeign {
            source: s.source,
            id: s.id.clone(),
            cwd: s.cwd.clone(),
            then: then(k),
        });
        selected_now = true;
    } else if !over {
        if resp.double_clicked() {
            if !env.busy
                && let Some(a) = primary_action(Cur::Foreign(s), env.input)
            {
                out.push(a);
            }
            selected_now = true;
        } else if resp.clicked() || resp.secondary_clicked() {
            selected_now = true;
        }
    }
    let _ = resp.clone().on_hover_cursor(CursorIcon::PointingHand);
    theme::context_menu(&resp, 250.0, |ui| {
        let mut send = |ui: &mut Ui, then: Then| {
            out.push(SessionsAction::ImportForeign {
                source: s.source,
                id: s.id.clone(),
                cwd: s.cwd.clone(),
                then,
            });
            ui.close();
        };
        if menu_item(
            ui,
            ICON_PLAY,
            "Resume here",
            Some("Enter"),
            can && env.here_ok(),
            false,
        )
        .on_disabled_hover_text(resume_tip.clone())
        .clicked()
        {
            send(ui, Then::ResumeHere);
        }
        if menu_item(
            ui,
            ICON_EXTERNAL_LINK,
            "Import and open in a new tab",
            None,
            can,
            false,
        )
        .clicked()
        {
            send(ui, Then::NewTab);
        }
    });
    selected_now
}

// ------------------------------------------------------------------- footer

fn footer(
    ui: &mut Ui,
    model: &Model,
    st: &ViewState,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    let pal = Palette::of_ui(ui);
    let r = &st.resume;
    let cur = current(model, r);
    let busy = !r.flow.is_idle();
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), FOOT_H),
        Layout::left_to_right(Align::Center),
        |ui| {
            Sides::new().show(
                ui,
                |ui| {
                    if busy {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        ui.add(Spin::new(17.0, pal.accent));
                        ui.label(theme::strong(flow_text(r)).color(pal.text_strong));
                    } else if r.source == Source::Omp {
                        crate::card::footer_hints(
                            ui,
                            &[
                                ("Enter", "resume"),
                                ("Ctrl+R", "rename"),
                                ("Alt+K", "pin"),
                                ("Alt+Shift+K", "unpin"),
                                ("Ctrl+D", "delete"),
                            ],
                        );
                    } else {
                        crate::card::footer_hints(ui, &[("Enter", "import and resume")]);
                    }
                },
                |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    // Right to left: the primary action is the rightmost.
                    let pick = cur.filter(|_| !busy);
                    let resume = pick.and_then(|c| resume_action(c, input));
                    let tip = resume_tip(input);
                    let b = DraftButton::new("Resume here")
                        .kind(DraftKind::Primary)
                        .icon(ICON_PLAY)
                        .enabled(resume.is_some());
                    if b.show(ui).on_disabled_hover_text(tip).clicked()
                        && let Some(a) = resume
                    {
                        out.push(a);
                    }
                    let open = pick.and_then(|c| open_action(c, input));
                    if DraftButton::new("Open in new tab")
                        .icon(ICON_EXTERNAL_LINK)
                        .enabled(open.is_some())
                        .show(ui)
                        .on_disabled_hover_text("Pick a session first")
                        .clicked()
                        && let Some(a) = open
                    {
                        out.push(a);
                    }
                },
            );
        },
    );
}

fn flow_text(r: &ResumeState) -> String {
    match &r.flow {
        Flow::Idle => String::new(),
        Flow::Resuming { force: false, .. } => "Resuming the session in the tab…".to_owned(),
        Flow::Resuming { force: true, .. } => "Stopping the turn, then resuming…".to_owned(),
        Flow::AskingForce { .. } => "Waiting for your answer…".to_owned(),
        Flow::Importing { .. } => format!("Importing from {}…", r.source.label()),
        Flow::Listing { .. } => "Opening the imported session…".to_owned(),
    }
}
