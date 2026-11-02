//! Generic node rendering: layout kinds, text, chips and every kind that is not part of a role-specific
//! surface (tool cards, the composer, overlays live in their own modules). Unknown kinds render their
//! text or children and never panic; `rows` falls back to its ANSI lines.

use std::sync::Arc;

use egui::text::LayoutJob;
use egui::{
    Align, Color32, CornerRadius, CursorIcon, FontId, Layout, Rect, Response, RichText, Sense,
    Stroke, StrokeKind, TextWrapMode, Ui, UiBuilder, pos2, vec2,
};
use serde_json::Value;

use super::ctx::{Cx, Tx};
use super::glyphs;
use super::paint;
use super::snap::SNode;
use super::style::LABEL;
use super::text::{self, Base};
use super::{ansi, code, dock, layer, md, tools, transcript};

/// Spacing step of `gap`/`size` props (px).
pub fn step(name: Option<&str>) -> f32 {
    match name {
        Some("none") => 0.0,
        Some("xs") => 3.0,
        Some("sm") => 6.0,
        Some("md") => 10.0,
        Some("lg") => 16.0,
        Some("xl") => 24.0,
        _ => 4.0,
    }
}

/// A node with nothing to show: hidden, or a container whose children are all blank.
pub fn blank(cx: &Cx, n: &SNode) -> bool {
    if n.hidden() {
        return true;
    }
    match n.kind.as_str() {
        "col" | "row" | "status" => {
            n.text().is_empty()
                && n.arr("spans").is_empty()
                && cx.snap.kids(n).all(|c| blank(cx, c))
        }
        "text" | "md" => n.text().is_empty() && n.arr("spans").is_empty(),
        _ => false,
    }
}

/// Renders `n` and everything below it.
pub fn node(cx: &Cx, ui: &mut Ui, n: &SNode) {
    if blank(cx, n) {
        return;
    }
    if transcript::role_node(cx, ui, n) || dock::role_node(cx, ui, n) {
        return;
    }
    with_click(cx, ui, n, |ui| kind(cx, ui, n));
}

/// Wraps `add` so a node with `actions.click` is a button: hover pop, hand cursor, tooltip, event.
fn with_click(cx: &Cx, ui: &mut Ui, n: &SNode, add: impl FnOnce(&mut Ui)) {
    let click = n.click();
    let own = matches!(
        n.kind.as_str(),
        "tool" | "agent" | "item" | "tabs" | "list" | "editor" | "picker" | "section" | "card"
    ) && n.click().is_none_or(|c| c == "toggle");
    let (Some(act), false) = (click, own) else {
        add(ui);
        return;
    };
    let inner = ui.scope(add);
    let rect = inner.response.rect;
    let id = ui.scope_id().with(("click", n.id.as_str()));
    let resp = ui.interact(rect, id, Sense::click());
    let t = ui
        .ctx()
        .animate_bool_with_time(id.with("hov"), resp.hovered(), 0.1);
    paint::pop(ui.painter(), rect, 4, cx.ink, t);
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let resp = match n.str("title") {
        Some(t) => resp.on_hover_text(text::clean(t).into_owned()),
        None => resp,
    };
    if resp.clicked() {
        cx.click(n, act);
    } else if resp.double_clicked()
        && let Some(d) = n.dblclick()
    {
        cx.click(n, d);
    }
}

/// Dispatch by kind.
fn kind(cx: &Cx, ui: &mut Ui, n: &SNode) {
    match n.kind.as_str() {
        "col" => col(cx, ui, n),
        "row" => row(cx, ui, n),
        "spacer" => {
            ui.add_space(step(n.str("size")).max(if n.num("size").is_some() {
                n.num("size").unwrap_or(0.0) as f32
            } else {
                0.0
            }));
        }
        "rule" => rule(cx, ui, n),
        "card" => card(cx, ui, n),
        "section" => section(cx, ui, n),
        "text" => text_node(cx, ui, n),
        "md" => md::show(cx, ui, n),
        "code" => code::show_code(cx, ui, n),
        "diff" => code::show_diff(cx, ui, n),
        "ansi" => ansi::show(cx, ui, n),
        "tool" => tools::tool(cx, ui, n),
        "agent" => tools::agent(cx, ui, n),
        "checklist" => tools::checklist(cx, ui, n),
        "badge" => {
            badge(cx, ui, &text::node_plain(n), n.tone());
        }
        "kbd" => kbd(cx, ui, n),
        "icon" => icon(cx, ui, n),
        "spinner" => spinner(cx, ui, n),
        "shimmer" => shimmer(cx, ui, n),
        "elapsed" => elapsed(cx, ui, n),
        "progress" => progress(cx, ui, n),
        "rate" => rate(cx, ui, n),
        "effort" => effort(cx, ui, n),
        "meter" => meter(cx, ui, n),
        "list" => list(cx, ui, n),
        "item" => item(cx, ui, n, None, false),
        "tabs" => tabs(cx, ui, n),
        "kv" => kv(cx, ui, n),
        "table" => table(cx, ui, n),
        "image" => image(cx, ui, n),
        "status" => status(cx, ui, n),
        "seg" => seg(cx, ui, n),
        "editor" | "input" => dock::editor_readonly(cx, ui, n),
        "overlay" | "picker" => layer::inline(cx, ui, n),
        // Toasts are recorded when the surface syncs and drawn over the chat (`layer::overlays`).
        "toast" => {}
        "rows" => ansi::show_rows(cx, ui, n),
        _ => unknown(cx, ui, n),
    }
}

/// Anything without a renderer: its text, else its children, else nothing.
fn unknown(cx: &Cx, ui: &mut Ui, n: &SNode) {
    if !n.text().is_empty() || !n.arr("spans").is_empty() {
        text_node(cx, ui, n);
    }
    if !n.children.is_empty() {
        col(cx, ui, n);
    }
}

// ---- layout ---------------------------------------------------------------------------------

pub fn children(cx: &Cx, ui: &mut Ui, n: &SNode) {
    for c in cx.snap.kids(n) {
        node(cx, ui, c);
    }
}

fn col(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let layout = match n.str("align") {
        Some("center") => Layout::top_down(Align::Center),
        Some("end") => Layout::top_down(Align::Max),
        _ => Layout::top_down(Align::Min),
    };
    let gap = step(n.str("gap"));
    ui.with_layout(layout, |ui| {
        ui.spacing_mut().item_spacing.y = gap;
        children(cx, ui, n);
    });
}

/// Natural width of what `f` draws on one row (nothing is painted or allocated).
pub fn measure(ui: &mut Ui, f: &dyn Fn(&mut Ui)) -> f32 {
    let rect = Rect::from_min_size(
        ui.cursor().min,
        vec2(4000.0, ui.available_height().max(40.0)),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt("measure")
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center))
            .invisible()
            .sizing_pass(),
    );
    f(&mut child);
    child.min_rect().width()
}

fn row(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let gap = step(n.str("gap"));
    let kids: Vec<&SNode> = cx.snap.kids(n).filter(|c| !blank(cx, c)).collect();
    let align = match n.str("align") {
        Some("start") => Align::Min,
        Some("end") => Align::Max,
        _ => Align::Center,
    };
    // The working line takes the room the HUD pill leaves instead of spanning the whole row.
    let hud = n.role() == "omp.hud.activity";
    let grow = kids.iter().position(|c| {
        c.num("grow").is_some_and(|g| g > 0.0)
            || (c.kind == "spacer" && c.flag("grow"))
            || (hud && c.role() == "omp.hud.status")
    });
    if n.flag("wrap") && grow.is_none() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(gap, gap);
            for c in &kids {
                node(cx, ui, c);
            }
        });
        return;
    }
    let justify_end = n.str("justify") == Some("end") && grow.is_none();
    let (left, mid, right): (&[&SNode], Option<&SNode>, &[&SNode]) = match grow {
        Some(i) if kids[i].kind == "spacer" => (&kids[..i], None, &kids[i + 1..]),
        Some(i) => (&kids[..i], Some(kids[i]), &kids[i + 1..]),
        None if justify_end => (&[][..], None, &kids[..]),
        None => (&kids[..], None, &[][..]),
    };
    let draw = |ui: &mut Ui, ks: &[&SNode]| {
        ui.spacing_mut().item_spacing.x = gap;
        for c in ks {
            node(cx, ui, c);
        }
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        if right.is_empty() {
            ui.with_layout(Layout::left_to_right(align), |ui| {
                draw(ui, left);
                if let Some(m) = mid {
                    node(cx, ui, m);
                }
            });
            return;
        }
        let rw = measure(ui, &|ui| draw(ui, right));
        let total = ui.available_width();
        let lw = (total - rw - gap).max(0.0);
        if !left.is_empty() || mid.is_some() {
            ui.allocate_ui_with_layout(
                vec2(lw, ui.available_height()),
                Layout::left_to_right(align),
                |ui| {
                    ui.set_max_width(lw);
                    draw(ui, left);
                    if let Some(m) = mid {
                        node(cx, ui, m);
                    }
                },
            );
        }
        let used = ui.min_rect().width();
        // A growing child was given the whole `lw`; the gap before the right group is already in
        // `used`'s spacing, so only the spacing after the padding remains to be reserved.
        let spacing = if mid.is_some() { gap } else { 0.0 };
        let pad = (total - used - rw - spacing).max(0.0);
        ui.add_space(pad);
        draw(ui, right);
    });
}

fn rule(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 14.0), Sense::hover());
    let y = rect.center().y;
    let line = cx.ink.line();
    let label = text::prop(n, "label");
    match label {
        Some(l) if !l.is_empty() => {
            let g = ui
                .painter()
                .layout_no_wrap(l, cx.ink.label(LABEL), cx.ink.dim());
            let x = rect.center().x - g.size().x / 2.0;
            ui.painter().line_segment(
                [pos2(rect.left(), y), pos2(x - 8.0, y)],
                Stroke::new(1.0, line),
            );
            ui.painter().line_segment(
                [pos2(x + g.size().x + 8.0, y), pos2(rect.right(), y)],
                Stroke::new(1.0, line),
            );
            ui.painter()
                .galley(pos2(x, y - g.size().y / 2.0), g, cx.ink.dim());
        }
        _ => {
            ui.painter().line_segment(
                [pos2(rect.left(), y), pos2(rect.right(), y)],
                Stroke::new(1.0, line),
            );
        }
    }
}

/// Head of a card/section: a string or spans.
pub fn head_job(cx: &Cx, n: &SNode, base: &Base) -> Option<LayoutJob> {
    let head = n.get("head")?;
    let mut job = LayoutJob::default();
    match head {
        Value::String(s) => job.append(
            &text::clean(s),
            0.0,
            text::Base::new(base.font.clone(), base.color, cx.ink).format_plain(),
        ),
        Value::Array(a) => text::push_spans(&mut job, cx.ink, base, a),
        _ => return None,
    }
    (!job.is_empty()).then_some(job)
}

fn card(cx: &Cx, ui: &mut Ui, n: &SNode) {
    if n.role_under("omp.welcome") {
        return transcript::welcome(cx, ui, n);
    }
    let ink = cx.ink;
    let tone = n.tone().map(|t| ink.tone(t));
    let bare = n.str("variant") == Some("bare");
    let collapsible = n.flag("collapsible");
    let collapsed = collapsible && cx.collapsed(n);
    let status = n.str("status");
    let edge = match status {
        Some("error") => ink.p.danger,
        Some("running" | "pending") => ink.b,
        _ => tone.map_or(ink.line(), |t| ink.edge(t)),
    };
    let inset = n.flag("inset");
    let margin = if bare {
        0
    } else if inset {
        4
    } else {
        8
    };
    let frame = egui::Frame::new()
        .fill(if bare {
            Color32::TRANSPARENT
        } else {
            ink.card()
        })
        .stroke(if bare {
            Stroke::NONE
        } else {
            Stroke::new(1.0, edge)
        })
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(margin, if bare { 0 } else { 6 }));
    let base = Base::new(ink.label(12.0), ink.dim(), ink);
    let head = head_job(cx, n, &base);
    let r = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        if let Some(job) = head {
            let resp = ui
                .horizontal(|ui| {
                    if collapsible {
                        chevron(ui, ink.dim(), !collapsed);
                    }
                    ui.add(
                        egui::Label::new(job)
                            .wrap_mode(TextWrapMode::Truncate)
                            .selectable(false),
                    );
                })
                .response;
            if collapsible
                && ui
                    .interact(
                        resp.rect,
                        ui.scope_id().with(("fold", &n.id)),
                        Sense::click(),
                    )
                    .clicked()
            {
                cx.toggle(n);
            }
        }
        if !collapsed {
            ui.spacing_mut().item_spacing.y = 4.0;
            children(cx, ui, n);
        }
    });
    let _ = r;
}

/// Small chevron: pointing down when `open`, else right.
pub fn chevron(ui: &mut Ui, color: Color32, open: bool) -> Response {
    glyphs::icon(
        ui,
        if open { "chev" } else { "chev-right" },
        11.0,
        color,
        Sense::hover(),
    )
}

fn section(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let collapsible = n.flag("collapsible") || n.role_under("omp.thinking");
    let collapsed = collapsible && cx.collapsed(n);
    let thinking = n.role_under("omp.thinking");
    let base = Base::new(ink.label(12.0), ink.dim(), ink);
    let kids: Vec<&SNode> = cx.snap.kids(n).collect();
    // A head is spans/text in the `head` prop, or a child (thinking: `head` names it, or it is
    // the first child).
    let head_ref = n
        .str("head")
        .and_then(|h| kids.iter().position(|k| k.id == h));
    let head_at = head_ref.or(if n.get("head").is_none() && thinking && !kids.is_empty() {
        Some(0)
    } else {
        None
    });
    let head_child = head_at.map(|i| kids[i]);
    let body: Vec<&SNode> = kids
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != head_at)
        .map(|(_, k)| *k)
        .collect();
    let head = if head_at.is_some() {
        None
    } else {
        head_job(cx, n, &base)
    };
    let took = n.num("took");
    let resp = ui
        .horizontal(|ui| {
            if collapsible {
                chevron(ui, ink.dim(), !collapsed);
            }
            match (&head, head_child) {
                (Some(job), _) => {
                    ui.add(
                        egui::Label::new(job.clone())
                            .wrap_mode(TextWrapMode::Truncate)
                            .selectable(false),
                    );
                }
                (None, Some(h)) => node(cx, ui, h),
                (None, None) => {}
            }
            if let Some(t) = took
                && head.is_some()
            {
                ui.label(
                    RichText::new(fmt_ms(t))
                        .font(ink.label(LABEL))
                        .color(ink.faint()),
                );
            }
        })
        .response;
    if collapsible {
        let r = ui.interact(
            resp.rect,
            ui.scope_id().with(("fold", &n.id)),
            Sense::click(),
        );
        if r.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if r.clicked() {
            cx.toggle(n);
        }
    }
    if !collapsed {
        let dim = thinking;
        let tx = if dim {
            Tx {
                font: ink.prose(14.0),
                color: ink.dim(),
                line: Some(14.0 * 1.55),
            }
        } else {
            cx.tx()
        };
        cx.with_tx(tx, || {
            ui.horizontal(|ui| {
                // A quiet rail along the body.
                let x = ui.cursor().left() + 3.0;
                ui.add_space(14.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    for c in &body {
                        node(cx, ui, c);
                    }
                });
                let r = ui.min_rect();
                ui.painter().line_segment(
                    [pos2(x, r.top()), pos2(x, r.bottom())],
                    Stroke::new(1.0, ink.line()),
                );
            });
        });
    }
}

// ---- text -----------------------------------------------------------------------------------

/// A link inside a paragraph: char range of the job's text and its target.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub from: usize,
    pub to: usize,
    pub url: String,
}

/// Lays out `job` at the available width as selectable text; returns the response and the link
/// clicked this frame.
pub fn para(
    ui: &mut Ui,
    mut job: LayoutJob,
    fallback: Color32,
    links: &[Link],
) -> (Response, Option<String>) {
    let w = ui.available_width();
    if w.is_finite() && job.wrap.max_width > w {
        job.wrap.max_width = w;
    }
    let galley = ui.painter().layout_job(job);
    para_galley(ui, galley, fallback, links)
}

/// [`para`] for a galley already laid out (cached by the caller).
pub fn para_galley(
    ui: &mut Ui,
    galley: Arc<egui::Galley>,
    fallback: Color32,
    links: &[Link],
) -> (Response, Option<String>) {
    let mut sense = Sense::click_and_drag();
    sense -= Sense::FOCUSABLE;
    let (rect, resp) = ui.allocate_exact_size(galley.size(), sense);
    let mut clicked = None;
    if ui.is_rect_visible(rect) {
        if !links.is_empty()
            && let Some(pos) = resp.hover_pos()
        {
            let idx = galley.cursor_from_pos(pos - rect.min).index.0;
            if let Some(l) = links.iter().find(|l| (l.from..l.to).contains(&idx)) {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                if resp.clicked() {
                    clicked = Some(l.url.clone());
                }
            }
        }
        egui::text_selection::LabelSelectionState::label_text_selection(
            ui,
            &resp,
            rect.min,
            Arc::clone(&galley),
            fallback,
            Stroke::NONE,
        );
    }
    (resp, clicked)
}

fn text_node(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let tx = cx.tx();
    let tone = n.tone().map(|t| cx.ink.tone(t)).unwrap_or(tx.color);
    let mut base = Base::new(tx.font.clone(), tone, cx.ink);
    base.line_height = tx.line;
    let job = text::node_job(cx.ink, n, &base, None);
    if job.is_empty() {
        return;
    }
    let truncate = n.str("wrap") == Some("none") || n.str("truncate").is_some();
    let resp = if truncate {
        ui.add(
            egui::Label::new(job)
                .wrap_mode(TextWrapMode::Truncate)
                .selectable(true),
        )
    } else {
        let (resp, url) = para(ui, job, tone, &[]);
        if let Some(u) = url {
            cx.open(u);
        }
        resp
    };
    if let Some(t) = n.str("title")
        && n.click().is_none()
    {
        resp.on_hover_text(text::clean(t).into_owned());
    }
}

// ---- chips ----------------------------------------------------------------------------------

/// A pill with `text` tinted by `tone`.
pub fn badge(cx: &Cx, ui: &mut Ui, text: &str, tone: Option<&str>) -> Response {
    let ink = cx.ink;
    let c = tone.map_or(ink.tone("accent"), |t| ink.tone(t));
    let g = ui
        .painter()
        .layout_no_wrap(text::clean(text).into_owned(), ink.label(LABEL), c);
    let size = vec2(g.size().x + 12.0, g.size().y + 5.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    paint::boxed(
        ui.painter(),
        rect,
        3,
        ink.wash(c),
        Stroke::new(1.0, ink.edge(c)),
    );
    ui.painter().galley(rect.center() - g.size() / 2.0, g, c);
    resp
}

/// Glyph of a key name.
pub fn key_glyph(key: &str) -> String {
    match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => "⏎".into(),
        "escape" | "esc" => "Esc".into(),
        "alt" | "option" => "Alt".into(),
        "ctrl" | "control" => "Ctrl".into(),
        "shift" => "⇧".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "tab" => "Tab".into(),
        "space" => "Space".into(),
        "backspace" => "⌫".into(),
        other => other.to_uppercase(),
    }
}

/// Keycaps: `keys` joined as one cap each.
pub fn keycaps(cx: &Cx, ui: &mut Ui, keys: &[String]) {
    let ink = cx.ink;
    ui.spacing_mut().item_spacing.x = 3.0;
    for k in keys {
        let g = ui
            .painter()
            .layout_no_wrap(key_glyph(k), ink.label(LABEL), ink.text());
        let size = vec2((g.size().x + 10.0).max(g.size().y + 6.0), g.size().y + 4.0);
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        let p = ui.painter();
        p.rect_filled(
            rect.translate(vec2(0.0, 1.5)),
            3.0,
            ink.p.stroke.gamma_multiply(0.9),
        );
        paint::boxed(p, rect, 3, ink.chip(), Stroke::new(1.0, ink.p.stroke));
        p.galley(rect.center() - g.size() / 2.0, g, ink.text());
    }
}

fn kbd(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let keys: Vec<String> = n
        .arr("keys")
        .iter()
        .filter_map(|k| k.as_str().map(str::to_owned))
        .collect();
    ui.horizontal(|ui| keycaps(cx, ui, &keys));
}

fn icon(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let name = n.str("name").unwrap_or_default();
    let color = n.tone().map_or(cx.ink.dim(), |t| cx.ink.tone(t));
    let size = ui
        .text_style_height(&egui::TextStyle::Body)
        .clamp(13.0, 16.0);
    let r = glyphs::icon(ui, name, size, color, Sense::hover());
    if let Some(t) = n.str("title") {
        r.on_hover_text(text::clean(t).into_owned());
    }
}

/// Seconds-and-minutes text of `ms`.
pub fn fmt_ms(ms: f64) -> String {
    let s = ms / 1000.0;
    if ms < 1000.0 {
        format!("{}ms", ms.round() as i64)
    } else if s < 60.0 {
        format!("{s:.1}s")
    } else if s < 3600.0 {
        format!("{}m {:02}s", (s / 60.0) as i64, (s % 60.0) as i64)
    } else {
        format!(
            "{}h {:02}m",
            (s / 3600.0) as i64,
            ((s % 3600.0) / 60.0) as i64
        )
    }
}

/// `mm:ss` of `ms`.
pub fn fmt_clock(ms: f64) -> String {
    let s = (ms / 1000.0).max(0.0) as u64;
    if s >= 3600 {
        format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// Milliseconds an `elapsed`/`age`-carrying node shows now: the age omp sent plus the time since
/// that frame arrived, unless it is stopped (`stopped`, or a `took`).
pub fn live_ms(cx: &Cx, n: &SNode, key: &str) -> Option<f64> {
    let age = n.num(key)?;
    if clock_stopped(cx, n) {
        return Some(age);
    }
    Some(age + cx.now.saturating_duration_since(n.changed).as_secs_f64() * 1000.0)
}

/// Whether the clock `n` no longer ticks. A clock inside a tool card stops with the tool even when
/// omp never marks it `stopped`.
fn clock_stopped(cx: &Cx, n: &SNode) -> bool {
    let finished = |s: &str| s != "running" && s != "pending";
    if n.flag("stopped") || n.str("status").is_some_and(finished) {
        return true;
    }
    let mut at = cx.snap.parent_of(&n.id);
    while let Some(p) = at {
        if p.kind == "tool" {
            return p.str("status").is_some_and(finished);
        }
        at = cx.snap.parent_of(&p.id);
    }
    false
}

fn elapsed(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let Some(ms) = live_ms(cx, n, "age") else {
        return;
    };
    if !clock_stopped(cx, n) {
        cx.animate_in(std::time::Duration::from_millis(200));
    }
    let s = if n.str("format") == Some("clock") {
        fmt_clock(ms)
    } else {
        fmt_ms(ms)
    };
    ui.label(
        RichText::new(s)
            .font(cx.ink.label(12.0))
            .color(cx.ink.dim()),
    );
}

fn rate(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let Some(v) = n.num("value") else { return };
    let unit = n.str("unit").unwrap_or("tok/s");
    ui.label(
        RichText::new(format!("{v:.0} {unit}"))
            .font(cx.ink.label(12.0))
            .color(cx.ink.dim()),
    );
}

fn spinner(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let color = n.tone().map_or(cx.ink.tone("accent"), |t| cx.ink.tone(t));
    ui.horizontal(|ui| {
        spin(cx, ui, 13.0, color);
        if let Some(l) = text::prop(n, "label") {
            ui.label(
                RichText::new(l)
                    .font(cx.ink.label(12.0))
                    .color(cx.ink.dim()),
            );
        }
    });
}

/// An animated ring spinner of diameter `d`.
pub fn spin(cx: &Cx, ui: &mut Ui, d: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(d + 2.0, d + 2.0), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let c = rect.center();
    let r = d / 2.0 - 1.0;
    let p = ui.painter();
    p.circle_stroke(c, r, Stroke::new(1.6, color.gamma_multiply(0.22)));
    if cx.ink.reduce_motion {
        p.circle_filled(c, 2.0, color);
        return resp;
    }
    cx.animate();
    let t = ui.input(|i| i.time) as f32;
    let a0 = t * 5.0;
    let pts: Vec<egui::Pos2> = (0..=14)
        .map(|k| {
            let a = a0 + k as f32 * 0.11 * std::f32::consts::PI * 0.7;
            c + vec2(a.cos(), a.sin()) * r
        })
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(1.8, color)));
    resp
}

fn shimmer(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let text = text::node_plain(n);
    shimmer_text(cx, ui, &text, cx.ink.label(13.0));
}

/// Text with a highlight sweeping through it (a steady accent when motion is reduced).
pub fn shimmer_text(cx: &Cx, ui: &mut Ui, text: &str, font: FontId) {
    let ink = cx.ink;
    let mut job = LayoutJob::default();
    let chars: Vec<char> = text.chars().collect();
    let t = if ink.reduce_motion {
        0.0
    } else {
        ui.input(|i| i.time) as f32
    };
    if !ink.reduce_motion {
        cx.animate();
    }
    let n = chars.len().max(1) as f32;
    for (i, ch) in chars.iter().enumerate() {
        let k = if ink.reduce_motion {
            0.6
        } else {
            let pos = (t * 9.0) % (n + 10.0) - 5.0;
            (1.0 - ((i as f32 - pos).abs() / 4.0)).clamp(0.0, 1.0)
        };
        let c = crate::theme::mix(ink.tone("accent"), ink.strong(), k);
        job.append(
            &ch.to_string(),
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color: c,
                ..Default::default()
            },
        );
    }
    ui.add(
        egui::Label::new(job)
            .wrap_mode(TextWrapMode::Extend)
            .selectable(false),
    );
}

fn progress(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let w = ui.available_width().clamp(60.0, 260.0);
    let (rect, _) = ui.allocate_exact_size(vec2(w, 6.0), Sense::hover());
    ui.painter().rect_filled(rect, 3.0, ink.chip());
    match n.num("value") {
        Some(v) => {
            let f = v.clamp(0.0, 1.0) as f32;
            let mut fill = rect;
            fill.set_width(rect.width() * f);
            paint::fill_h(ui.painter(), &paint::slant_band(fill, 0.0), ink.a, ink.b);
        }
        None => {
            cx.animate();
            let t = (ui.input(|i| i.time) as f32 * 0.8) % 1.0;
            let seg = rect.width() * 0.3;
            let x = rect.left() + (rect.width() + seg) * t - seg;
            let r = Rect::from_min_max(
                pos2(x.max(rect.left()), rect.top()),
                pos2((x + seg).min(rect.right()), rect.bottom()),
            );
            if r.width() > 0.0 {
                ui.painter().rect_filled(r, 3.0, ink.b);
            }
        }
    }
    if let Some(l) = text::prop(n, "label") {
        ui.label(RichText::new(l).font(ink.label(LABEL)).color(ink.dim()));
    }
}

const LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Fill (0..=1) of an effort ring; `None` for `auto`.
pub fn effort_fill(level: &str) -> Option<f32> {
    if level == "auto" {
        return None;
    }
    let i = LEVELS.iter().position(|l| *l == level).unwrap_or(0);
    Some(i as f32 / (LEVELS.len() - 1) as f32)
}

fn effort(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let level = n.str("level").unwrap_or("off");
    ring(
        cx,
        ui,
        effort_fill(level),
        13.0,
        cx.ink.tone(if level == "off" { "muted" } else { "accent" }),
    );
}

/// A ring glyph filled to `fill` (a dashed look for `None`).
pub fn ring(cx: &Cx, ui: &mut Ui, fill: Option<f32>, d: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(d + 2.0, d + 2.0), Sense::hover());
    let c = rect.center();
    let r = d / 2.0 - 0.5;
    let p = ui.painter();
    p.circle_stroke(c, r, Stroke::new(1.4, color.gamma_multiply(0.3)));
    let f = fill.unwrap_or(0.5);
    if f > 0.0 {
        let steps = ((f * 32.0) as usize).max(2);
        let pts: Vec<egui::Pos2> = (0..=steps)
            .map(|k| {
                let a = -std::f32::consts::FRAC_PI_2
                    + std::f32::consts::TAU * f * k as f32 / steps as f32;
                c + vec2(a.cos(), a.sin()) * r
            })
            .collect();
        p.add(egui::Shape::line(pts, Stroke::new(1.8, color)));
    }
    if fill.is_none() {
        p.circle_filled(c, 1.5, color);
    }
    let _ = cx;
    resp
}

fn meter(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let value = n.num("value").map(|v| v.clamp(0.0, 1.0) as f32);
    let warn = n
        .obj("thresholds")
        .and_then(|t| t.get("warn"))
        .and_then(Value::as_f64)
        .unwrap_or(0.6) as f32;
    let bad = n
        .obj("thresholds")
        .and_then(|t| t.get("bad"))
        .and_then(Value::as_f64)
        .unwrap_or(0.85) as f32;
    let color = match value {
        Some(v) if v >= bad => ink.p.danger,
        Some(v) if v >= warn => ink.p.warn,
        _ => ink.b,
    };
    let label = text::prop(n, "label").unwrap_or_default();
    match n.str("style") {
        Some("ring") => {
            ui.horizontal(|ui| {
                ring(cx, ui, value, 18.0, color);
                if !label.is_empty() {
                    ui.label(RichText::new(label).font(ink.label(12.0)).color(ink.dim()));
                }
            });
        }
        _ => {
            let w = ui.available_width().clamp(60.0, 320.0);
            let (rect, _) = ui.allocate_exact_size(vec2(w, 5.0), Sense::hover());
            meter_bar(cx, ui, rect, value, color, n);
            if !label.is_empty() {
                ui.label(RichText::new(label).font(ink.label(LABEL)).color(ink.dim()));
            }
        }
    }
}

/// A thin meter bar with its `marks`.
pub fn meter_bar(cx: &Cx, ui: &Ui, rect: Rect, value: Option<f32>, color: Color32, n: &SNode) {
    let ink = cx.ink;
    let p = ui.painter();
    p.rect_filled(rect, 2.5, ink.line().gamma_multiply(0.7));
    if let Some(v) = value {
        // An empty meter shows only its track.
        if v > 0.004 {
            let mut f = rect;
            f.set_width((rect.width() * v).max(3.0));
            p.rect_filled(f, 2.5, color);
        }
    }
    for m in n.arr("marks") {
        if let Some(at) = m.get("at").and_then(Value::as_f64) {
            let x = rect.left() + rect.width() * at as f32;
            p.line_segment(
                [pos2(x, rect.top() - 2.0), pos2(x, rect.bottom() + 2.0)],
                Stroke::new(1.0, ink.p.warn),
            );
        }
    }
}

// ---- lists ----------------------------------------------------------------------------------

fn list(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let selected = n.str("selected");
    let items: Vec<&SNode> = cx.snap.kids(n).filter(|c| !c.hidden()).collect();
    if items.is_empty() {
        if let Some(e) = n.str("empty") {
            ui.label(
                RichText::new(e)
                    .font(cx.ink.label(12.0))
                    .color(cx.ink.faint()),
            );
        }
        return;
    }
    let max = n.num("max").map(|m| m as usize);
    let body = |ui: &mut Ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for it in &items {
            item(cx, ui, it, Some(n), selected == Some(it.id.as_str()));
        }
    };
    match max {
        Some(m) if items.len() > m => {
            egui::ScrollArea::vertical()
                .id_salt(("list", &n.id))
                .max_height(m as f32 * 30.0)
                .show(ui, body);
        }
        _ => body(ui),
    }
}

/// One list row. `list` is the owner (for the select/activate target), `selected` its highlight.
pub fn item(cx: &Cx, ui: &mut Ui, n: &SNode, list: Option<&SNode>, selected: bool) {
    let ink = cx.ink;
    let tone = n.tone().map(|t| ink.tone(t));
    let disabled = n.flag("disabled");
    let w = ui.available_width();
    let id = ui.scope_id().with(("item", n.id.as_str()));
    let height = 28.0;
    let (rect, _) = ui.allocate_exact_size(vec2(w, height), Sense::hover());
    let resp = ui.interact(
        rect,
        id,
        if disabled {
            Sense::hover()
        } else {
            Sense::click()
        },
    );
    let hov = ui
        .ctx()
        .animate_bool_with_time(id.with("h"), resp.hovered() && !disabled, 0.08);
    // A row that becomes the highlighted one scrolls into view (long menus and pickers).
    let seen = ui.data_mut(|d| d.get_temp::<bool>(id.with("sel")).unwrap_or(false));
    if selected != seen {
        ui.data_mut(|d| d.insert_temp(id.with("sel"), selected));
        if selected {
            ui.scroll_to_rect(rect, None);
        }
    }
    let p = ui.painter();
    if selected {
        p.rect(
            rect,
            CornerRadius::same(4),
            ink.wash(tone.unwrap_or(ink.p.accent)),
            Stroke::new(1.0, ink.edge(tone.unwrap_or(ink.p.accent))),
            StrokeKind::Inside,
        );
    } else if hov > 0.0 {
        p.rect_filled(rect, 4.0, ink.p.hover_bg.gamma_multiply(hov));
        paint::pop(p, rect, 4, ink, hov);
    }
    let text_c = if disabled {
        ink.faint()
    } else if selected {
        ink.strong()
    } else {
        ink.text()
    };
    let mut x = rect.left() + 8.0;
    let cy = rect.center().y;
    if let Some(ic) = n.str("icon") {
        glyphs::paint(p, ic, pos2(x + 7.0, cy), 13.0, tone.unwrap_or(ink.dim()));
        x += 22.0;
    }
    let label = text::prop(n, "label").unwrap_or_default();
    let g = p.layout_no_wrap(label, ink.prose(13.5), text_c);
    let gw = g.size().x;
    p.galley(pos2(x, cy - g.size().y / 2.0), g, text_c);
    x += gw + 10.0;
    // Right side: hint keycaps and the value badge, from the edge in.
    let mut rx = rect.right() - 8.0;
    if let Some(v) = text::prop(n, "value") {
        let g = p.layout_no_wrap(v, ink.label(LABEL), tone.unwrap_or(ink.dim()));
        let r = Rect::from_center_size(
            pos2(rx - g.size().x / 2.0 - 5.0, cy),
            g.size() + vec2(10.0, 4.0),
        );
        paint::boxed(
            p,
            r,
            3,
            ink.wash(tone.unwrap_or(ink.dim())),
            Stroke::new(1.0, ink.edge(tone.unwrap_or(ink.dim()))),
        );
        p.galley(r.center() - g.size() / 2.0, g, tone.unwrap_or(ink.dim()));
        rx = r.left() - 6.0;
    }
    for h in n.arr("hint").iter().rev() {
        if let Some(s) = h.as_str() {
            let g = p.layout_no_wrap(key_glyph(s), ink.label(LABEL), ink.dim());
            let r = Rect::from_center_size(
                pos2(rx - g.size().x / 2.0 - 4.0, cy),
                g.size() + vec2(8.0, 4.0),
            );
            paint::boxed(p, r, 3, ink.chip(), Stroke::new(1.0, ink.p.stroke));
            p.galley(r.center() - g.size() / 2.0, g, ink.dim());
            rx = r.left() - 4.0;
        }
    }
    if let Some(d) = text::prop(n, "detail") {
        let avail = (rx - x).max(0.0);
        if avail > 30.0 {
            let mut job = LayoutJob::simple_singleline(d, ink.label(12.0), ink.dim());
            job.wrap.max_width = avail;
            job.wrap.max_rows = 1;
            let g = p.layout_job(job);
            p.galley(pos2(x, cy - g.size().y / 2.0), g, ink.dim());
        }
    }
    if !disabled {
        let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
        if resp.double_clicked() {
            cx.click_item(n, list, true);
        } else if resp.clicked() {
            cx.click_item(n, list, selected);
        }
    }
}

fn tabs(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let active = n.str("active").map(str::to_owned);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for t in n.arr("items") {
            let Some(id) = t.get("id").and_then(Value::as_str) else {
                continue;
            };
            let label = t.get("label").map(text::prop_value).unwrap_or_default();
            let is = active.as_deref() == Some(id);
            let g = ui.painter().layout_no_wrap(
                label,
                ink.label(12.0),
                if is { ink.strong() } else { ink.dim() },
            );
            let size = vec2(g.size().x + 26.0, 26.0);
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            let resp = ui.interact(rect, ui.scope_id().with(("tab", &n.id, id)), Sense::click());
            let hov = ui
                .ctx()
                .animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.08);
            let pts = paint::parallelogram(rect, 7.0);
            if is {
                paint::fill_h(ui.painter(), &pts, ink.head().0, ink.head().1);
                crate::deco::gradient_outline(
                    ui.painter(),
                    pts.clone(),
                    1.0,
                    &ink.rule_stops(),
                    vec2(1.0, 0.0),
                );
            } else {
                crate::deco::fill(
                    ui.painter(),
                    pts.clone(),
                    ink.chip().gamma_multiply(0.6 + 0.4 * hov),
                );
                crate::deco::outline(
                    ui.painter(),
                    pts,
                    Stroke::new(1.0, ink.p.stroke.gamma_multiply(0.8 + 0.2 * hov)),
                );
            }
            ui.painter().galley(
                rect.center() - g.size() / 2.0,
                g,
                if is { ink.strong() } else { ink.dim() },
            );
            if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                cx.send(super::events::select(cx.sf(), &n.id, id));
            }
        }
    });
}

fn cell_job(cx: &Cx, v: &Value, base: &Base) -> LayoutJob {
    let mut job = LayoutJob::default();
    match v {
        Value::String(s) => job.append(&text::clean(s), 0.0, base.format_plain()),
        Value::Array(a) => text::push_spans(&mut job, cx.ink, base, a),
        Value::Null => {}
        other => job.append(&other.to_string(), 0.0, base.format_plain()),
    }
    job
}

fn kv(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let tx = cx.tx();
    let key_base = Base::new(ink.label(12.0), ink.dim(), ink);
    let val_base = Base::new(tx.font.clone(), ink.strong(), ink);
    let items = n.arr("items");
    egui::Grid::new(ui.scope_id().with(("kv", &n.id)))
        .num_columns(2)
        .spacing(vec2(14.0, 4.0))
        .show(ui, |ui| {
            for it in items {
                let k = it
                    .get("k")
                    .map(|v| cell_job(cx, v, &key_base))
                    .unwrap_or_default();
                let v = it
                    .get("v")
                    .map(|v| cell_job(cx, v, &val_base))
                    .unwrap_or_default();
                ui.add(egui::Label::new(k).selectable(false));
                ui.add(
                    egui::Label::new(v)
                        .wrap_mode(TextWrapMode::Wrap)
                        .selectable(true),
                );
                ui.end_row();
            }
        });
}

fn table(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let head_base = Base::new(ink.label(LABEL), ink.dim(), ink);
    let cell_base = Base::new(ink.mono(0.95), ink.text(), ink);
    let cols: Vec<(&str, &str, bool)> = n
        .arr("cols")
        .iter()
        .filter_map(|c| {
            Some((
                c.get("id")?.as_str()?,
                c.get("head").and_then(Value::as_str).unwrap_or_default(),
                c.get("align").and_then(Value::as_str) == Some("end"),
            ))
        })
        .collect();
    egui::Grid::new(ui.scope_id().with(("table", &n.id)))
        .num_columns(cols.len().max(1))
        .spacing(vec2(20.0, 5.0))
        .striped(false)
        .show(ui, |ui| {
            for (_, head, _) in &cols {
                let mut job = LayoutJob::default();
                job.append(&head.to_uppercase(), 0.0, head_base.format_plain());
                ui.add(egui::Label::new(job).selectable(false));
            }
            ui.end_row();
            for row in n.arr("rows") {
                let cells = row.get("cells");
                for (id, _, end) in &cols {
                    let job = cells
                        .and_then(|c| c.get(*id))
                        .map(|v| cell_job(cx, v, &cell_base))
                        .unwrap_or_default();
                    ui.with_layout(
                        Layout::top_down(if *end { Align::Max } else { Align::Min }),
                        |ui| {
                            ui.add(egui::Label::new(job).selectable(true));
                        },
                    );
                }
                ui.end_row();
            }
        });
}

fn image(cx: &Cx, ui: &mut Ui, n: &SNode) {
    layer::image(cx, ui, n);
}

fn status(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let kids: Vec<&SNode> = cx.snap.kids(n).collect();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        for k in kids {
            node(cx, ui, k);
        }
    });
}

fn seg(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let base = Base::new(ink.label(12.0), ink.dim(), ink);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if let Some(ic) = n.str("icon") {
            glyphs::icon(ui, ic, 12.0, ink.dim(), Sense::hover());
        }
        let mut job = LayoutJob::default();
        text::push_spans(&mut job, ink, &base, n.arr("spans"));
        if job.is_empty() && !n.text().is_empty() {
            job.append(&text::clean(n.text()), 0.0, base.format_plain());
        }
        if !job.is_empty() {
            ui.add(
                egui::Label::new(job)
                    .wrap_mode(TextWrapMode::Extend)
                    .selectable(false),
            );
        }
    });
}
