//! The `dock` region: the floating composer (omp's `editor`, drawn natively: text, caret, selection,
//! placeholder, ghost), its context meter and model/extras bar, the working line, the queue, the
//! ask dialog and the approval strip, and omp's buttons.

use egui::text::LayoutJob;
use egui::text_selection::CCursorRange;
use egui::{
    Align, Color32, CursorIcon, Id, Layout, Rect, Response, RichText, Sense, Stroke, TextFormat,
    Ui, UiBuilder, pos2, text::CCursor, vec2,
};
use serde_json::Value;

use super::ctx::{Claim, Cx, Tx};
use super::glyphs;
use super::paint;
use super::render::{self, blank};
use super::select::{self, ComposerSel};
use super::snap::SNode;
use super::style::{LABEL, PROSE, PROSE_LINE};
use super::text::{self, Base};
use super::transcript::measure_size;

/// Padding inside the composer panel.
const PAD_X: f32 = 16.0;
const PAD_Y: f32 = 12.0;

// ---- UTF-16 <-> char indices ----------------------------------------------------------------

/// Number of UTF-16 code units of `s`.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Char index of UTF-16 offset `u` in `s`. An offset inside a surrogate pair belongs to the char
/// that owns the pair; past the end clamps to the end.
pub fn utf16_to_char(s: &str, u: usize) -> usize {
    let mut units = 0;
    for (i, c) in s.chars().enumerate() {
        let next = units + c.len_utf16();
        if u < next {
            return i;
        }
        units = next;
    }
    s.chars().count()
}

/// UTF-16 offset of char index `c` in `s` (past the end clamps to the length).
pub fn char_to_utf16(s: &str, c: usize) -> usize {
    s.chars().take(c).map(char::len_utf16).sum()
}

// ---- entry points ---------------------------------------------------------------------------

/// The whole dock region, top to bottom.
pub fn show(cx: &Cx, ui: &mut Ui) {
    let Some(dock) = cx.snap.region("dock") else {
        return;
    };
    ui.spacing_mut().item_spacing.y = 8.0;
    let tx = Tx {
        font: cx.ink.label(12.0),
        color: cx.ink.dim(),
        line: None,
    };
    cx.with_tx(tx, || render::children(cx, ui, dock));
}

/// Dock roles; `true` when handled.
pub fn role_node(cx: &Cx, ui: &mut Ui, n: &SNode) -> bool {
    match n.role() {
        "omp.working" => working(cx, ui, n),
        "omp.queue" => queue(cx, ui, n),
        "omp.editor" if n.kind == "col" => composer(cx, ui, n),
        "omp.editor.ask" => {
            panel(cx, ui, Panel::Ask, &|ui| ask_body(cx, ui, n));
        }
        "omp.editor.ask.approval" => {
            panel(cx, ui, Panel::Approval, &|ui| ask_body(cx, ui, n));
        }
        "omp.editor.ask.hidden" => {
            panel(cx, ui, Panel::Ask, &|ui| ask_body(cx, ui, n));
        }
        "omp.hud.pill" => hud_pill(cx, ui, n),
        "omp.btn" => btn(cx, ui, n),
        "omp.composer.send" => send_button(cx, ui, n),
        "omp.composer.stop" => stop_button(cx, ui, n),
        "omp.composer.context" => context_bar(cx, ui, n),
        _ => return false,
    }
    true
}

fn ask_body(cx: &Cx, ui: &mut Ui, n: &SNode) {
    ui.spacing_mut().item_spacing.y = 10.0;
    let tx = Tx {
        font: cx.ink.prose(14.0),
        color: cx.ink.text(),
        line: Some(21.0),
    };
    cx.with_tx(tx, || render::children(cx, ui, n));
}

/// Wording of the banner of an exited process (the terminal tab shows the same).
pub fn exit_banner(code: i32) -> String {
    format!("[process exited: {code}]")
}

/// The dock of a chat whose process exited: a banner instead of the composer (the document
/// still shows the last state of a session that is gone, so nothing in it may stay live).
pub fn exited(cx: &Cx, ui: &mut Ui, code: i32) {
    let ink = cx.ink;
    panel(cx, ui, Panel::Exited, &|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(
            RichText::new(exit_banner(code))
                .font(ink.label(13.0))
                .color(ink.p.danger),
        );
        ui.label(
            RichText::new("The session has ended. Reopen it to continue; the conversation above stays readable.")
                .font(ink.prose(13.0))
                .color(ink.dim()),
        );
    });
}

// ---- panels ---------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Composer,
    Ask,
    Approval,
    /// The process behind the chat is gone.
    Exited,
}

/// A framed dock panel: measured first so the glass, shadow and outline sit behind the content.
fn panel(cx: &Cx, ui: &mut Ui, kind: Panel, body: &dyn Fn(&mut Ui)) {
    let ink = cx.ink;
    let w = ui.available_width();
    let inner_w = w - 2.0 * PAD_X;
    let size = measure_size(ui, inner_w, body);
    let h = size.y + 2.0 * PAD_Y;
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    if ui.is_rect_visible(rect.expand(40.0)) {
        let p = ui.painter();
        match kind {
            Panel::Approval | Panel::Exited => {
                let pts = paint::corners_rect(rect, 7.0, 0.0, 7.0, 0.0);
                let c = if kind == Panel::Exited {
                    ink.p.danger
                } else {
                    ink.p.warn
                };
                crate::deco::drop_shadow(p, rect, 7, 6, 16, ink.dark, 90);
                crate::deco::fill(p, pts.clone(), ink.wash(c));
                crate::deco::outline(p, pts, Stroke::new(1.0, c));
            }
            Panel::Composer | Panel::Ask => {
                let pts = paint::corners_rect(rect, 16.0, 4.0, 16.0, 4.0);
                let live = cx.focused || kind == Panel::Ask;
                crate::deco::drop_shadow(
                    p,
                    rect,
                    16,
                    12,
                    30,
                    ink.dark,
                    if ink.dark { 150 } else { 60 },
                );
                // A cool halo always, stronger while the composer has the keys.
                let halo = if live { 0.30 } else { 0.14 };
                crate::deco::glow(
                    p,
                    rect,
                    16,
                    ink.b
                        .gamma_multiply(if ink.dark { halo } else { halo * 0.6 }),
                    26,
                );
                let top =
                    crate::theme::mix(ink.p.surface, ink.a, if ink.dark { 0.07 } else { 0.05 });
                let bot =
                    crate::theme::mix(ink.p.surface, ink.b, if ink.dark { 0.03 } else { 0.02 });
                crate::deco::fill_vertical(p, &pts, top, bot);
                // Glass: a faint light edge along the top inside the frame.
                let hi = rect.shrink2(vec2(18.0, 0.0));
                crate::deco::gradient_rule(
                    p,
                    pos2(hi.left(), rect.top() + 1.5),
                    pos2(hi.right(), rect.top() + 1.5),
                    1.0,
                    &[
                        (0.0, Color32::TRANSPARENT),
                        (
                            0.5,
                            Color32::from_white_alpha(if ink.dark { 26 } else { 90 }),
                        ),
                        (1.0, Color32::TRANSPARENT),
                    ],
                );
                let k = if live { 1.0 } else { 0.85 };
                let stops = [
                    (0.0, ink.a.gamma_multiply(k)),
                    (1.0, ink.b.gamma_multiply(k)),
                ];
                crate::deco::gradient_outline(p, pts, 1.5, &stops, vec2(1.0, 0.15));
            }
        }
    }
    let inner = Rect::from_min_size(
        pos2(rect.left() + PAD_X, rect.top() + PAD_Y),
        vec2(inner_w, size.y),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt("panel")
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
    );
    body(&mut child);
}

// ---- composer -------------------------------------------------------------------------------

fn composer(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let body = |ui: &mut Ui| {
        ui.spacing_mut().item_spacing.y = 8.0;
        render::children(cx, ui, n);
    };
    panel(cx, ui, Panel::Composer, &body);
}

/// The context meter as a slim bar with its label at the right.
fn context_bar(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let value = n.num("value").map(|v| v.clamp(0.0, 1.0) as f32);
    let (warn, bad) = (
        n.obj("thresholds")
            .and_then(|t| t.get("warn"))
            .and_then(Value::as_f64)
            .unwrap_or(0.6) as f32,
        n.obj("thresholds")
            .and_then(|t| t.get("bad"))
            .and_then(Value::as_f64)
            .unwrap_or(0.85) as f32,
    );
    let color = match value {
        Some(v) if v >= bad => ink.p.danger,
        Some(v) if v >= warn => ink.p.warn,
        _ => ink.b,
    };
    let label = {
        let l = text::prop(n, "label").unwrap_or_default();
        match text::prop(n, "total") {
            Some(t) if !l.is_empty() => format!("{l} · {t}"),
            _ => l,
        }
    };
    let font = ink.label(LABEL);
    let g = ui.painter().layout_no_wrap(label, font, ink.dim());
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 12.0), Sense::hover());
    let resp = ui.interact(rect, ui.scope_id().with(("ctx", &n.id)), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
    let bar = Rect::from_min_max(
        pos2(rect.left(), rect.center().y - 2.0),
        pos2(
            (rect.right() - g.size().x - 10.0).max(rect.left() + 20.0),
            rect.center().y + 2.0,
        ),
    );
    render::meter_bar(cx, ui, bar, value, color, n);
    paint::pop(ui.painter(), bar.expand2(vec2(2.0, 3.0)), 4, ink, hov);
    ui.painter().galley(
        pos2(
            rect.right() - g.size().x,
            rect.center().y - g.size().y / 2.0,
        ),
        g,
        ink.dim(),
    );
    let resp = match n.str("title") {
        Some(t) => resp.on_hover_text(text::clean(t).into_owned()),
        None => resp,
    };
    if let Some(act) = n.click()
        && resp.on_hover_cursor(CursorIcon::PointingHand).clicked()
    {
        cx.click(n, act);
    }
}

// ---- buttons --------------------------------------------------------------------------------

/// Look of a [`button`].
#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    Neutral,
    Accent,
    Danger,
}

pub fn tone_of(tone: Option<&str>) -> Tone {
    match tone {
        Some("accent" | "info" | "success" | "user") => Tone::Accent,
        Some("error" | "warning") => Tone::Danger,
        _ => Tone::Neutral,
    }
}

/// A drafted button: slanted outline, gradient fill and a hard base shadow for the primary tones.
pub fn button(
    cx: &Cx,
    ui: &mut Ui,
    id: Id,
    label: &str,
    icon: Option<&str>,
    tone: Tone,
) -> Response {
    let ink = cx.ink;
    let font = ink.label(12.0);
    let (fill, text_c) = match tone {
        Tone::Accent => ((ink.a, ink.b), crate::deco::ink_on_pair(ink.a, ink.b)),
        Tone::Danger => {
            let (a, b) = crate::deco::tone_pair(ink.p, ink.p.danger);
            ((a, b), crate::deco::ink_on_pair(a, b))
        }
        Tone::Neutral => ((ink.chip(), ink.chip()), ink.strong()),
    };
    let g = ui.painter().layout_no_wrap(label.to_owned(), font, text_c);
    let icon_w = if icon.is_some() { 18.0 } else { 0.0 };
    let size = vec2(g.size().x + icon_w + 30.0, 28.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let resp = ui.interact(rect, id, Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(id.with("h"), resp.hovered(), 0.1);
    let press = if resp.is_pointer_button_down_on() {
        1.5
    } else {
        0.0
    };
    let body = rect.translate(vec2(0.0, press));
    let p = ui.painter();
    let pts = paint::parallelogram(body, 7.0);
    if tone != Tone::Neutral {
        let base = paint::parallelogram(rect.translate(vec2(0.0, 2.5)), 7.0);
        crate::deco::fill(
            p,
            base,
            if ink.dark {
                Color32::from_black_alpha(130)
            } else {
                Color32::from_black_alpha(60)
            },
        );
    }
    let lift = |c: Color32| c.lerp_to_gamma(Color32::WHITE, 0.14 * hov);
    crate::deco::fill_linear(p, &pts, lift(fill.0), lift(fill.1), vec2(1.0, 0.0));
    let edge = match tone {
        Tone::Neutral => ink.p.stroke.lerp_to_gamma(ink.p.hover_line, hov),
        _ => Color32::from_white_alpha(50),
    };
    crate::deco::outline(p, pts, Stroke::new(1.0, edge));
    paint::pop(p, body, 4, ink, hov);
    let mut x = body.center().x - (g.size().x + icon_w) / 2.0;
    if let Some(ic) = icon {
        glyphs::paint(p, ic, pos2(x + 7.0, body.center().y), 13.0, text_c);
        x += icon_w;
    }
    p.galley(pos2(x, body.center().y - g.size().y / 2.0), g, text_c);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// omp's `omp.btn` rows: icon and text children, clicked through `actions.click`.
fn btn(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let mut label = String::new();
    let mut icon = None;
    for c in cx.snap.kids(n) {
        match c.kind.as_str() {
            "icon" => icon = icon.or(c.str("name")),
            "text" => {
                if !label.is_empty() {
                    label.push(' ');
                }
                label.push_str(text::node_plain(c).trim());
            }
            _ => {}
        }
    }
    if label.is_empty() {
        label = text::node_plain(n);
    }
    let resp = button(
        cx,
        ui,
        ui.scope_id().with(("btn", &n.id)),
        label.trim(),
        icon,
        tone_of(n.tone()),
    );
    let resp = match n.str("title") {
        Some(t) => resp.on_hover_text(text::clean(t).into_owned()),
        None => resp,
    };
    if resp.clicked()
        && let Some(act) = n.click()
    {
        cx.click(n, act);
    }
}

fn send_button(cx: &Cx, ui: &mut Ui, n: &SNode) {
    // A prompt may be sent only while omp says the editor is sendable and its hello advertised
    // `send`.
    let sendable = cx.snap.can_send && cx.snap.composer().is_some_and(|c| c.flag("sendable"));
    let ink = cx.ink;
    let id = ui.scope_id().with(("send", &n.id));
    let resp = if sendable {
        button(cx, ui, id, "Send", Some("send"), Tone::Accent)
    } else {
        // Not accepting a prompt yet: the same shape, quiet.
        let r = button(cx, ui, id, "Send", Some("send"), Tone::Neutral);
        ui.painter()
            .rect_filled(r.rect, 0.0, ink.p.bg.gamma_multiply(0.35));
        r
    };
    if resp.clicked()
        && sendable
        && let Some(act) = n.click()
    {
        cx.click(n, act);
    }
}

fn stop_button(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let resp = button(
        cx,
        ui,
        ui.scope_id().with(("stop", &n.id)),
        "Stop",
        Some("square"),
        Tone::Danger,
    );
    if resp.clicked()
        && let Some(act) = n.click()
    {
        cx.click(n, act);
    }
}

// ---- working line, queue, hud ---------------------------------------------------------------

fn working(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let w = ui.available_width();
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.set_width(w - 12.0);
            ui.spacing_mut().item_spacing.x = 8.0;
            let tx = Tx {
                font: ink.label(12.0),
                color: ink.dim(),
                line: None,
            };
            cx.with_tx(tx, || {
                let kids: Vec<&SNode> = cx.snap.kids(n).filter(|c| !blank(cx, c)).collect();
                // Everything but the stop control on the left, the control at the right edge.
                let (stop, rest): (Vec<&SNode>, Vec<&SNode>) = kids
                    .into_iter()
                    .partition(|c| c.role() == "omp.working.stop");
                ui.horizontal(|ui| {
                    for c in &rest {
                        if c.kind == "spacer" {
                            continue;
                        }
                        render::node(cx, ui, c);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        for c in &stop {
                            render::node(cx, ui, c);
                        }
                    });
                });
            });
        });
}

fn queue(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let tx = Tx {
        font: ink.label(12.0),
        color: ink.dim(),
        line: None,
    };
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 3.0;
            cx.with_tx(tx, || render::children(cx, ui, n));
        });
}

fn hud_pill(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let tone = n.tone().map_or(ink.tone("accent"), |t| ink.tone(t));
    egui::Frame::new()
        .fill(ink.wash(tone))
        .stroke(Stroke::new(1.0, ink.edge(tone)))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(9, 2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let tx = Tx {
                font: ink.label(12.0),
                color: tone,
                line: None,
            };
            ui.horizontal(|ui| cx.with_tx(tx, || render::children(cx, ui, n)));
        });
}

// ---- editor ---------------------------------------------------------------------------------

/// Read-only `editor`/`input` nodes outside the composer (ask notes, hook inputs, previews).
pub fn editor_readonly(cx: &Cx, ui: &mut Ui, n: &SNode) {
    editor_box(cx, ui, n);
}

/// Builds the editor's text job: text with `decor` ranges and the `ghost`, else the placeholder.
pub fn editor_job(cx: &Cx, n: &SNode, width: f32) -> LayoutJob {
    let ink = cx.ink;
    let font = ink.prose(PROSE);
    let line = PROSE * PROSE_LINE;
    let mut base = Base::new(font.clone(), ink.strong(), ink);
    base.line_height = Some(line);
    let mut job = LayoutJob::default();
    job.wrap.max_width = width;
    let t = text::clean(n.text());
    if t.is_empty() {
        let mut faint = base.clone();
        faint.color = ink.faint();
        match n.get("placeholder") {
            Some(Value::String(s)) => job.append(&text::clean(s), 0.0, faint.format_plain()),
            Some(Value::Array(a)) => text::push_spans(&mut job, ink, &faint, a),
            _ => {}
        }
        if job.is_empty() {
            job.append("", 0.0, faint.format_plain());
        }
        return job;
    }
    // Decorations are UTF-16 ranges of the original text; the cleaned text has the same chars
    // unless private-use glyphs were removed (then decor is skipped to stay aligned).
    let aligned = t.len() == n.text().len();
    let mut ranges: Vec<(usize, usize, TextFormat)> = Vec::new();
    if aligned {
        for d in n.arr("decor") {
            let (Some(from), Some(to)) = (
                d.get("from").and_then(Value::as_u64),
                d.get("to").and_then(Value::as_u64),
            ) else {
                continue;
            };
            let st = text::span_style(ink, d.get("s").and_then(Value::as_str).unwrap_or_default());
            let mut b = base.clone();
            if let Some(c) = st.color {
                b.color = c;
            }
            let mut f = b.format_plain();
            if st.mono {
                f.font_id = b.mono.clone();
            }
            f.italics = st.italic;
            if let Some(bg) = st.background {
                f.background = bg;
            }
            ranges.push((
                utf16_to_char(&t, from as usize),
                utf16_to_char(&t, to as usize),
                f,
            ));
        }
    }
    ranges.sort_by_key(|r| r.0);
    let chars: Vec<char> = t.chars().collect();
    let mut at = 0;
    let push = |job: &mut LayoutJob, a: usize, b: usize, f: &TextFormat| {
        if b > a {
            job.append(&chars[a..b].iter().collect::<String>(), 0.0, f.clone());
        }
    };
    for (a, b, f) in &ranges {
        if *a < at {
            continue;
        }
        push(&mut job, at, *a, &base.format_plain());
        push(&mut job, *a, (*b).min(chars.len()), f);
        at = (*b).min(chars.len());
    }
    push(&mut job, at, chars.len(), &base.format_plain());
    if let Some(g) = n.str("ghost").filter(|g| !g.is_empty()) {
        let mut faint = base.clone();
        faint.color = ink.faint();
        job.append(&text::clean(g), 0.0, faint.format_plain());
    }
    job
}

/// Draws an editor node: selection, text and caret. Returns its response.
pub fn editor_box(cx: &Cx, ui: &mut Ui, n: &SNode) -> Response {
    let ink = cx.ink;
    let width = ui.available_width().max(40.0);
    let line = PROSE * PROSE_LINE;
    let max_lines = n.num("maxLines").map_or(18.0, |m| m.max(1.0)) as f32;
    let job = editor_job(cx, n, width);
    let galley = ui.painter().layout_job(job);
    let text = n.text();
    let max_h = max_lines * line;
    let h = galley.size().y.max(line).min(max_h);
    let readonly = n.flag("readonly");
    // Dragged to select, never focusable: the chat view owns the keyboard focus.
    let sense = if readonly {
        Sense::hover()
    } else {
        Sense::click_and_drag() - Sense::FOCUSABLE
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), sense);
    let is_focus = cx.focused
        && (cx.snap.focus.as_deref() == Some(n.id.as_str())
            || cx.snap.composer().is_some_and(|c| c.id == n.id));
    let cursor16 = n.num("cursor").map_or(utf16_len(text), |c| c as usize);
    let anchor16 = n.num("anchor").map_or(cursor16, |c| c as usize);
    let (cur, anc) = (utf16_to_char(text, cursor16), utf16_to_char(text, anchor16));
    // The galley is laid out over the cleaned text (no private-use characters), `text` is omp's:
    // everything omp tells or is told is a char index of `text`, the galley wants shown ones.
    let pua = text.chars().any(text::is_pua);
    let shown_at = |raw: usize| if pua { shown_index(text, raw) } else { raw };
    let caret_rect = galley.pos_from_cursor(CCursor::new(shown_at(cur)));
    // Show the end of a taller-than-allowed text, caret in view.
    let off = (caret_rect.max.y - h)
        .max(0.0)
        .min((galley.size().y - h).max(0.0));
    let origin = rect.min - vec2(0.0, off);
    let p = ui
        .painter()
        .with_clip_rect(rect.expand2(vec2(2.0, 0.0)).intersect(ui.clip_rect()));
    // The reader's selection (a drag here), else omp's own (vim's visual mode).
    let picked = cx.loc(|l| {
        l.sel
            .as_ref()
            .filter(|s| s.is_for(&n.id, text))
            .map(|s| (s.anchor, s.head))
    });
    let (anc, cur_sel) = picked.unwrap_or((anc, cur));
    let mut shown = galley.clone();
    if cur_sel != anc && !text.is_empty() {
        egui::text_selection::visuals::paint_text_selection(
            &mut shown,
            ui.visuals(),
            &CCursorRange::two(CCursor::new(shown_at(anc)), CCursor::new(shown_at(cur_sel))),
            None,
        );
    }
    p.galley(origin, shown, ink.strong());
    if is_focus && !n.flag("readonly") {
        // Only a caret that has the keys of a focused window blinks (and wakes the frame loop),
        // and only for BLINK_FOR after the last keystroke; then it rests solid, so an idle chat
        // costs no repaints at all.
        let t = cx
            .now
            .saturating_duration_since(cx.loc(|l| *l.blink_epoch.get_or_insert(cx.now)));
        let blink = !ink.reduce_motion && cx.focused && ui.input(|i| i.focused) && t < BLINK_FOR;
        let phase = (t.as_millis() % 1060) as u64;
        let on = !blink || phase < 640;
        if blink {
            let left =
                std::time::Duration::from_millis(if on { 640 - phase } else { 1060 - phase } + 8);
            // The last wake-up lands on the end of the blinking, which draws the caret solid.
            cx.animate_in(left.min(BLINK_FOR - t));
        }
        if on {
            let x = origin.x + caret_rect.min.x;
            let r = Rect::from_min_max(
                pos2(x, origin.y + caret_rect.min.y + 2.0),
                pos2(x + 2.0, origin.y + caret_rect.max.y - 2.0),
            );
            p.rect_filled(r, 1.0, ink.b);
        }
    }
    if is_focus {
        let r = Rect::from_min_max(
            pos2(origin.x + caret_rect.min.x, origin.y + caret_rect.min.y),
            pos2(
                origin.x + caret_rect.min.x + 2.0,
                origin.y + caret_rect.max.y,
            ),
        );
        cx.loc(|l| l.caret = Some(r));
        if !n.flag("readonly") {
            // The input method follows the caret, and its composition shows there, underlined,
            // until it commits (omp only ever sees the committed text).
            let pre = cx.loc(|l| (!l.preedit.is_empty()).then(|| l.preedit.clone()));
            if let Some(pre) = pre {
                let g = ui
                    .painter()
                    .layout_no_wrap(pre, ink.prose(PROSE), ink.strong());
                let at = r.min;
                let under = Rect::from_min_size(at, g.size());
                p.rect_filled(under, 2.0, ink.p.bg);
                p.line_segment(
                    [under.left_bottom(), under.right_bottom()],
                    Stroke::new(1.0, ink.b),
                );
                p.galley(at, g, ink.strong());
            }
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    purpose: egui::IMEPurpose::Normal,
                    rect,
                    cursor_rect: r,
                    should_interrupt_composition: false,
                })
            });
        }
    }
    if !readonly && ui.is_visible() {
        select_by_pointer(cx, ui, n, &resp, &galley, origin, (anc, cur_sel));
    }
    if !readonly {
        resp.on_hover_cursor(CursorIcon::Text)
    } else {
        resp
    }
}

/// Char index in the cleaned text (private-use characters dropped, as the editor draws it) of the
/// char index `raw` of `text`.
fn shown_index(text: &str, raw: usize) -> usize {
    text.chars().take(raw).filter(|c| !text::is_pua(*c)).count()
}

/// Char index in `text` of the char index `shown` of its cleaned form (the end for anything
/// past it, such as the ghost suggestion).
fn raw_index(text: &str, shown: usize) -> usize {
    let mut seen = 0;
    for (i, c) in text.chars().enumerate() {
        if text::is_pua(c) {
            continue;
        }
        if seen == shown {
            return i;
        }
        seen += 1;
    }
    text.chars().count()
}

/// Mouse selection in the composer. A press anchors a selection that follows the drag (double and
/// triple click take a word and a line, shift extends); a click without a drag places omp's caret.
/// Releasing a selection moves omp's caret to its moving end (`edit` with no text), which is what
/// typing, deleting and pasting over the selection then address (`select::plan`).
fn select_by_pointer(
    cx: &Cx,
    ui: &Ui,
    n: &SNode,
    resp: &Response,
    galley: &egui::Galley,
    origin: egui::Pos2,
    shown: (usize, usize),
) {
    let text = n.text();
    let at = |pos: egui::Pos2| raw_index(text, galley.cursor_from_pos(pos - origin).index.0);
    let ci = resp.interact_pointer_pos().map(at);
    let sel_of = |a: usize, b: usize| ComposerSel::new(&n.id, text, a, b);
    let caret_to = |ci: usize| {
        if cx.can_edit {
            cx.send(select::caret_edit(cx.sf(), &n.id, text, ci));
        }
        cx.loc(|l| l.blink_epoch = Some(cx.now));
    };
    if resp.hovered()
        && ui.input(|i| i.pointer.primary_pressed())
        && let Some(p) = resp.hover_pos()
    {
        let ci = at(p);
        let anchor = if ui.input(|i| i.modifiers.shift) {
            shown.0
        } else {
            ci
        };
        cx.loc(|l| l.sel = Some(sel_of(anchor, ci)));
    }
    let Some(ci) = ci else { return };
    if resp.dragged() {
        cx.loc(|l| {
            if let Some(s) = l.sel.as_mut().filter(|s| s.is_for(&n.id, text)) {
                s.head = ci.min(text.chars().count());
            }
        });
        ui.ctx().request_repaint();
    }
    let (word, line) = (resp.double_clicked(), resp.triple_clicked());
    if line || word {
        let (a, b) = if line {
            select::line_at(text, ci)
        } else {
            select::word_at(text, ci)
        };
        cx.loc(|l| l.sel = Some(sel_of(a, b)));
        caret_to(b);
    } else if resp.clicked() {
        // A click (no drag) is a caret move, not a selection.
        cx.loc(|l| l.sel = None);
        caret_to(ci);
    } else if resp.drag_stopped() {
        let head = cx.loc(|l| {
            l.sel
                .as_ref()
                .filter(|s| s.is_for(&n.id, text) && !s.is_empty())
                .map(|s| s.head)
        });
        match head {
            Some(h) => caret_to(h),
            None => cx.loc(|l| l.sel = None),
        }
    }
    cx.loc(|l| {
        if l.ctx_claim.is_none() && l.ctx_click.is_some_and(|p| resp.rect.contains(p)) {
            l.ctx_claim = Some(Claim::Composer(n.id.clone()));
        }
    });
}

/// How long the caret keeps blinking after the last keystroke or click.
const BLINK_FOR: std::time::Duration = std::time::Duration::from_secs(20);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_positions_map_between_the_drawn_and_the_raw_text() {
        // Two icons (private use) sit in omp's text; the editor draws "a b" without them.
        let raw = "a\u{e000} \u{f0001}b";
        assert_eq!(raw.chars().count(), 5);
        // drawn index -> char index of the raw text (the icons are skipped)
        let raw_of: Vec<usize> = (0..=3).map(|i| raw_index(raw, i)).collect();
        assert_eq!(raw_of, [0, 2, 4, 5]);
        // and back
        let drawn_of: Vec<usize> = (0..=5).map(|i| shown_index(raw, i)).collect();
        assert_eq!(drawn_of, [0, 1, 1, 2, 2, 3]);
        for drawn in 0..=3 {
            assert_eq!(shown_index(raw, raw_index(raw, drawn)), drawn);
        }
        // Text without icons is the identity; a ghost past the end clamps to the end.
        assert_eq!(raw_index("abc", 2), 2);
        assert_eq!(raw_index("abc", 9), 3);
        assert_eq!(shown_index("abc", 2), 2);
    }

    #[test]
    fn the_exit_banner_reads_like_the_terminals() {
        assert_eq!(exit_banner(0), "[process exited: 0]");
        assert_eq!(exit_banner(-1), "[process exited: -1]");
        assert_eq!(exit_banner(137), "[process exited: 137]");
    }

    #[test]
    fn utf16_offsets_survive_astral_characters() {
        let s = "a😀b";
        assert_eq!(utf16_len(s), 4);
        assert_eq!(utf16_to_char(s, 0), 0);
        assert_eq!(utf16_to_char(s, 1), 1);
        // Inside the surrogate pair: the emoji's own index, never between its halves.
        assert_eq!(utf16_to_char(s, 2), 1);
        assert_eq!(utf16_to_char(s, 3), 2);
        assert_eq!(utf16_to_char(s, 4), 3);
        assert_eq!(utf16_to_char(s, 99), 3);
        assert_eq!(char_to_utf16(s, 2), 3);
        assert_eq!(char_to_utf16(s, 3), 4);
        assert_eq!(char_to_utf16(s, 99), 4);
    }

    #[test]
    fn utf16_round_trips_at_every_char_boundary() {
        let s = "héllo 😀 wörld 🎉!";
        for c in 0..=s.chars().count() {
            assert_eq!(utf16_to_char(s, char_to_utf16(s, c)), c, "char {c}");
        }
    }

    #[test]
    fn tones_of_buttons() {
        assert!(tone_of(Some("accent")) == Tone::Accent);
        assert!(tone_of(Some("error")) == Tone::Danger);
        assert!(tone_of(None) == Tone::Neutral);
        assert!(tone_of(Some("muted")) == Tone::Neutral);
    }
}
