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
use super::tsel::{RunOpts, Sep};
use super::{ansi, code, dock, layer, md, page, tools, transcript};

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
    // The footer's grow spacer: with every fact hidden as a duplicate it still pushes the
    // send/stop button to the right edge, so it is not "nothing to show".
    if cx.hide_duplicates
        && n.role() == "omp.composer.extras"
        && n.num("grow").is_some_and(|g| g > 0.0)
    {
        return false;
    }
    if cx.hide_duplicates && (dock::duplicate(n) || dock::dangling_separator(cx, n)) {
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
    cx.in_node(n.ord, || {
        if transcript::role_node(cx, ui, n)
            || dock::role_node(cx, ui, n)
            || page::role_node(cx, ui, n)
        {
            return;
        }
        with_click(cx, ui, n, |ui| kind(cx, ui, n));
    });
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
        // A toast's node is gone a frame after it arrives; the document's toast ring is what is
        // drawn over the chat (`layer::overlays`), so the node itself shows nothing.
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
    measure_salted(ui, "measure", f)
}

/// [`measure`] under its own id salt, for several measurements in one `ui`.
fn measure_salted(
    ui: &mut Ui,
    salt: impl std::hash::Hash + std::fmt::Debug,
    f: &dyn Fn(&mut Ui),
) -> f32 {
    let rect = Rect::from_min_size(
        ui.cursor().min,
        vec2(4000.0, ui.available_height().max(40.0)),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(salt)
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
    if n.flag("wrap") {
        wrapped(cx, ui, n, &kids, gap, align);
        return;
    }
    // Idle, the activity row holds the todo pill alone: it sits at the right, where the pill of a
    // run sits next to the working line.
    let justify_end = (n.str("justify") == Some("end") || hud) && grow.is_none();
    // `between`: the first child at the left edge, the rest as one group at the right.
    let justify_between = n.str("justify") == Some("between") && grow.is_none() && kids.len() > 1;
    let (left, mid, right): (&[&SNode], Option<&SNode>, &[&SNode]) = match grow {
        Some(i) if kids[i].kind == "spacer" => (&kids[..i], None, &kids[i + 1..]),
        Some(i) => (&kids[..i], Some(kids[i]), &kids[i + 1..]),
        None if justify_between => (&kids[..1], None, &kids[1..]),
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

/// Role whose children omp's TUI draws one per line at full width (the jobs' tiles of `wait`).
const STACKED: &str = "omp.job.tiles";

/// Greedy line breaking as an iterator of `kids` index ranges, one per line of `avail` px with
/// children `gap` apart. A width beyond `avail` is bounded to it, so it fills a line alone; every
/// line holds at least one child.
pub fn line_breaks(widths: &[f32], gap: f32, avail: f32) -> Lines<'_> {
    Lines {
        widths,
        gap,
        avail,
        at: 0,
    }
}

/// See [`line_breaks`].
pub struct Lines<'a> {
    widths: &'a [f32],
    gap: f32,
    avail: f32,
    at: usize,
}

impl Iterator for Lines<'_> {
    type Item = std::ops::Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.at;
        let mut end = start;
        let mut x = 0.0f32;
        while let Some(&w) = self.widths.get(end) {
            let w = w.min(self.avail);
            let lead = if end > start { self.gap } else { 0.0 };
            // Half a pixel of slack: measured widths are rounded sums of the same galleys.
            if end > start && x + lead + w > self.avail + 0.5 {
                break;
            }
            x += lead + w;
            end += 1;
        }
        self.at = end;
        (end > start).then_some(start..end)
    }
}

/// One child of a wrapped row, held to `w` px: a container does not wrap by itself, so a child
/// handed the rest of a line would otherwise lay out in a sliver (or past the card's edge).
fn bounded(cx: &Cx, ui: &mut Ui, c: &SNode, w: f32, fill: bool) {
    let h = ui.available_height().clamp(20.0, 4000.0);
    ui.allocate_ui_with_layout(vec2(w, h), Layout::top_down(Align::Min), |ui| {
        if fill {
            ui.set_width(w);
        } else {
            ui.set_max_width(w);
        }
        node(cx, ui, c);
    });
}

/// A `wrap: true` row. Children are measured at their natural width, packed greedily into lines
/// no wider than the row, and each is allocated at most the row's width, so containers (cols,
/// cards, tiles) stay inside it and text inside them wraps at that width. The jobs' tiles take a
/// line each at full width.
fn wrapped(cx: &Cx, ui: &mut Ui, n: &SNode, kids: &[&SNode], gap: f32, align: Align) {
    let avail = ui.available_width();
    let stack = n.role() == STACKED && avail.is_finite();
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = vec2(gap, gap);
        if stack {
            for (i, c) in kids.iter().enumerate() {
                if i > 0 {
                    let (r, _) = ui.allocate_exact_size(vec2(avail, 7.0), Sense::hover());
                    let y = r.center().y.round() - 0.5;
                    ui.painter().line_segment(
                        [pos2(r.left(), y), pos2(r.right(), y)],
                        Stroke::new(1.0, cx.ink.line()),
                    );
                }
                bounded(cx, ui, c, avail, true);
            }
            return;
        }
        // The widths live in a buffer kept between frames: taken out of the context's memory so
        // drawing (which may reach this node's subtree again) never aliases it, put back after.
        let key = ui.scope_id().with(("wrap-widths", n.id.as_str()));
        let mut widths = ui
            .data_mut(|d| d.remove_temp::<Vec<f32>>(key))
            .unwrap_or_default();
        widths.clear();
        // Each child's basis: its natural width (one pixel over, so a label given exactly its
        // measured width does not wrap), raised to its `min.w`; a growing child with a `min.w`
        // starts from that minimum and takes its share of the line's spare room.
        let mut ch = None;
        widths.extend(kids.iter().map(|c| {
            let min = c
                .obj("min")
                .and_then(|m| m.get("w"))
                .and_then(|v| len_px(v, || *ch.get_or_insert_with(|| ch_width(cx, ui))));
            match min {
                Some(m) if grow_of(c) > 0.0 => m,
                _ => {
                    let natural =
                        measure_salted(ui, ("wrap-measure", c.id.as_str()), &|ui| node(cx, ui, c))
                            + 1.0;
                    min.map_or(natural, |m| natural.max(m))
                }
            }
        }));
        for line in line_breaks(&widths, gap, avail) {
            let ks = &kids[line.clone()];
            let ws = &widths[line];
            let used = ws.iter().map(|w| w.min(avail)).sum::<f32>()
                + gap * (ws.len().saturating_sub(1)) as f32;
            let growth: f32 = ks.iter().map(|c| grow_of(c)).sum();
            let spare = if avail.is_finite() {
                (avail - used).max(0.0)
            } else {
                0.0
            };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                ui.with_layout(Layout::left_to_right(align), |ui| {
                    for (c, w) in ks.iter().zip(ws) {
                        let g = grow_of(c);
                        let w = share(w.min(avail), g, growth, spare).min(avail);
                        bounded(cx, ui, c, w, g > 0.0);
                    }
                });
            });
        }
        ui.data_mut(|d| d.insert_temp(key, widths));
    });
}

/// The flex-grow factor of a child of a wrapped row (0: it keeps its basis).
fn grow_of(c: &SNode) -> f32 {
    if c.kind == "spacer" && c.flag("grow") {
        return 1.0;
    }
    c.num("grow").filter(|g| *g > 0.0).unwrap_or(0.0) as f32
}

/// A child's width on a line: its `basis` plus its `grow / growth` part of the line's `spare` px.
fn share(basis: f32, grow: f32, growth: f32, spare: f32) -> f32 {
    if growth > 0.0 && grow > 0.0 {
        basis + spare * grow / growth
    } else {
        basis
    }
}

/// A `min.w` length in px: a number, `"12px"`, or `"48ch"` (`ch`: the width of a `0` in the
/// current text font, asked for only when needed).
fn len_px(v: &serde_json::Value, ch: impl FnOnce() -> f32) -> Option<f32> {
    match v {
        Value::Number(n) => n.as_f64().map(|n| n as f32),
        Value::String(s) => {
            let s = s.trim();
            if let Some(n) = s.strip_suffix("ch") {
                n.trim().parse::<f32>().ok().map(|n| n * ch())
            } else {
                s.strip_suffix("px").unwrap_or(s).trim().parse().ok()
            }
        }
        _ => None,
    }
}

/// Width of a `0` in the current plain-text font.
fn ch_width(cx: &Cx, ui: &Ui) -> f32 {
    let font = cx.tx().font;
    ui.ctx().fonts_mut(|f| f.glyph_width(&font, '0')).max(1.0)
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
    // IRC identity keeps its warning outline even when delivery reports an error; the
    // message's status text and content still convey the error independently.
    let irc = !bare && n.role_under("omp.irc");
    let edge = match status {
        _ if irc => tools::irc_edge(ink),
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
    let outer_clip = ui.clip_rect();
    let r = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        // Reserved before the content (and after the frame's own fill) so the tint is behind it.
        let band = (irc && head.is_some()).then(|| paint::reserve(ui.painter()));
        let inner = ui.max_rect();
        let head_rect = head.map(|job| {
            let resp = ui
                .horizontal(|ui| {
                    if collapsible {
                        chevron(ui, ink.dim(), !collapsed);
                    }
                    if irc {
                        glyphs::icon(ui, "message", 13.0, tools::irc_icon(ink), Sense::hover());
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
            resp.rect
        });
        if let (Some(idx), Some(head)) = (band, head_rect) {
            let m = f32::from(margin);
            let rect = Rect::from_min_max(
                pos2(inner.left() - m + 1.0, inner.top() - 5.0),
                pos2(inner.right() + m - 1.0, head.bottom() + 5.0),
            );
            let (a, b) = tools::irc_head(ink);
            let low = if collapsed { 5.0 } else { 0.0 };
            let tint = gradient_h(&tools::head_band(rect, 0.0, 5.0, low), a, b);
            ui.painter().with_clip_rect(outer_clip).set(idx, tint);
        }
        if !collapsed {
            if irc && head_rect.is_some() {
                // The tinted header extends 5px below its row. Start the body after that band
                // plus modest padding, rather than letting its first baseline touch the tint.
                ui.add_space(12.0);
            }
            ui.spacing_mut().item_spacing.y = 4.0;
            // The card's width is the body's width: whatever a child cannot fit is cut at the
            // card's side (a margin's worth stays so hover outlines are not shaved), and the
            // height stays the content's.
            let side = (f32::from(margin) - 1.0).max(2.0);
            let clip = ui.clip_rect();
            ui.set_clip_rect(
                Rect::from_x_y_ranges(
                    (inner.left() - side)..=(inner.right() + side),
                    clip.y_range(),
                )
                .intersect(clip),
            );
            children(cx, ui, n);
        }
    });
    let _ = r;
}

/// The convex outline `pts` filled with a left→right gradient `a → b`, as one shape (a painter
/// call would draw immediately; this one can fill a slot reserved behind content).
fn gradient_h(pts: &[egui::Pos2], a: Color32, b: Color32) -> egui::Shape {
    let Some(first) = pts.first() else {
        return egui::Shape::Noop;
    };
    let (mut lo, mut hi) = (first.x, first.x);
    for p in pts {
        lo = lo.min(p.x);
        hi = hi.max(p.x);
    }
    let span = (hi - lo).max(1.0);
    let col = |x: f32| a.lerp_to_gamma(b, ((x - lo) / span).clamp(0.0, 1.0));
    let mut m = egui::epaint::Mesh::default();
    let c = pts.iter().fold(egui::Vec2::ZERO, |s, p| s + p.to_vec2()) / pts.len() as f32;
    m.colored_vertex(c.to_pos2(), col(c.x));
    for p in pts {
        m.colored_vertex(*p, col(p.x));
    }
    let n = pts.len() as u32;
    for i in 1..=n {
        m.add_triangle(0, i, if i == n { 1 } else { i + 1 });
    }
    egui::Shape::mesh(m)
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
                font: ink.prose(THINK_PX),
                color: ink.dim(),
                line: Some(THINK_LINE),
            }
        } else {
            cx.tx()
        };
        let live = n.role() == "omp.thinking.live";
        cx.with_tx(tx, || {
            ui.horizontal(|ui| {
                // A quiet rail along the body.
                let x = ui.cursor().left() + 3.0;
                ui.add_space(14.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    let draw = |ui: &mut Ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        for c in &body {
                            node(cx, ui, c);
                        }
                    };
                    if live {
                        live_body(cx, ui, n, &draw);
                    } else {
                        draw(ui);
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

/// Font size and line height of a thinking body.
const THINK_PX: f32 = 14.0;
const THINK_LINE: f32 = THINK_PX * 1.55;
/// Lines of a streaming thinking body that stay visible (its tail).
const THINK_ROWS: f32 = 6.0;
/// Height of the soft fade at the top of a clamped thinking body.
const THINK_FADE: f32 = 22.0;

/// How far a body of `content` px is shifted up so that its last `max` px show, `None` when it
/// fits (a clamp that hides less than half a line is not worth a control).
pub fn tail_clamp(content: f32, max: f32) -> Option<f32> {
    (content > max + THINK_LINE * 0.5).then_some(content - max)
}

/// A live thinking body: only its tail (the last few lines, fading in from above) while it
/// streams; the reader can open all of it, and close it again.
fn live_body(cx: &Cx, ui: &mut Ui, n: &SNode, draw: &dyn Fn(&mut Ui)) {
    let ink = cx.ink;
    let w = ui.available_width();
    let max_h = THINK_ROWS * THINK_LINE;
    let full = transcript::measure_size(ui, w, draw).y;
    let Some(shift) = tail_clamp(full, max_h) else {
        draw(ui);
        return;
    };
    let lines = (full / THINK_LINE).round() as usize;
    let open = cx.loc(|l| l.thinking_full.contains(&n.id));
    let id = ui.scope_id().with(("thinking-more", &n.id));
    if open {
        draw(ui);
        if pill(cx, ui, id, "Show less").clicked() {
            cx.loc(|l| l.thinking_full.remove(&n.id));
        }
        return;
    }
    let (rect, _) = ui.allocate_exact_size(vec2(w, max_h), Sense::hover());
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(("thinking-tail", &n.id))
            .max_rect(Rect::from_min_size(
                rect.min - vec2(0.0, shift),
                vec2(w, full + 8.0),
            ))
            .layout(Layout::top_down(Align::Min)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    child.set_width(w);
    draw(&mut child);
    let fade = Rect::from_min_size(rect.min, vec2(w, THINK_FADE.min(rect.height())));
    crate::deco::fill_vertical(
        ui.painter(),
        &[
            fade.left_top(),
            fade.right_top(),
            fade.right_bottom(),
            fade.left_bottom(),
        ],
        ink.p.bg,
        ink.p.bg.gamma_multiply(0.0),
    );
    if pill(cx, ui, id, &format!("Show all · {lines} lines")).clicked() {
        cx.loc(|l| l.thinking_full.insert(n.id.clone()));
    }
}

/// A small outlined control `label` on its own row, right-aligned.
fn pill(cx: &Cx, ui: &mut Ui, id: egui::Id, label: &str) -> Response {
    let g = ui
        .painter()
        .layout_no_wrap(label.to_owned(), cx.ink.label(LABEL), cx.ink.dim());
    let size = g.size() + vec2(18.0, 6.0);
    let row = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(row, size.y), Sense::hover());
    let at = Rect::from_min_size(pos2(rect.right() - size.x, rect.top()), size);
    pill_at(cx, ui, id, at, g)
}

fn pill_at(cx: &Cx, ui: &mut Ui, id: egui::Id, at: Rect, g: Arc<egui::Galley>) -> Response {
    let ink = cx.ink;
    let resp = ui
        .interact(at, id, Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    let hot = ui
        .ctx()
        .animate_bool_with_time(id.with("h"), resp.hovered(), 0.1);
    let p = ui.painter();
    paint::boxed(
        p,
        at,
        8,
        ink.chip().gamma_multiply(0.85 + 0.15 * hot),
        Stroke::new(1.0, crate::theme::mix(ink.p.stroke, ink.b, 0.55 * hot)),
    );
    p.galley(
        at.center() - g.size() / 2.0,
        g,
        crate::theme::mix(ink.dim(), ink.strong(), hot),
    );
    resp
}

// ---- text -----------------------------------------------------------------------------------

/// A link inside a paragraph: char range of the job's text and its target.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub from: usize,
    pub to: usize,
    pub url: String,
}

/// Lays out `job` at the available width as selectable text (a run of the current node, see
/// [`tsel`]); returns the response and the link clicked this frame.
pub fn para(
    cx: &Cx,
    ui: &mut Ui,
    mut job: LayoutJob,
    fallback: Color32,
    links: &[Link],
    opts: RunOpts,
) -> (Response, Option<String>) {
    let w = ui.available_width();
    if w.is_finite() && job.wrap.max_width > w {
        job.wrap.max_width = w;
    }
    let galley = ui.painter().layout_job(job);
    para_galley(cx, ui, galley, fallback, links, opts)
}

/// [`para`] for a galley already laid out (cached by the caller).
pub fn para_galley(
    cx: &Cx,
    ui: &mut Ui,
    galley: Arc<egui::Galley>,
    fallback: Color32,
    links: &[Link],
    opts: RunOpts,
) -> (Response, Option<String>) {
    // Text takes no pointer input of its own (the chat view's selection reads the raw pointer);
    // only a paragraph with links is a click target.
    let sense = if links.is_empty() {
        Sense::hover()
    } else {
        Sense::click()
    };
    let (rect, resp) = ui.allocate_exact_size(galley.size(), sense);
    let seq = cx.seqs(ui, 1);
    let mut clicked = None;
    if ui.is_rect_visible(rect)
        && !links.is_empty()
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
    cx.text_run(ui, seq, &galley, rect.min, fallback, opts);
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
    let (resp, url) = if truncate {
        let mut job = job;
        let w = ui.available_width();
        if w.is_finite() {
            job.wrap = egui::text::TextWrapping::truncate_at_width(w);
        }
        let galley = ui.painter().layout_job(job);
        para_galley(cx, ui, galley, tone, &[], RunOpts::LINE)
    } else {
        para(cx, ui, job, tone, &[], RunOpts::LINE)
    };
    if let Some(u) = url {
        cx.open(u);
    }
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
    let color = n.tone().map_or(cx.ink.dim(), |t| cx.ink.glyph_tone(t));
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
    let color = cx.ink.glyph_tone(n.tone().unwrap_or("accent"));
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
    let body = |ui: &mut Ui| {
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
    };
    // A segmented control is one unbroken strip; plain tabs wrap.
    if n.str("variant") == Some("segmented") {
        ui.horizontal(body);
    } else {
        ui.horizontal_wrapped(body);
    }
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
    let grid = |ui: &mut Ui| {
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
                    para(cx, ui, v, ink.strong(), &[], RunOpts::LINE);
                    ui.end_row();
                }
            });
    };
    sized_grid(cx, ui, grid);
}

/// Draws a grid. One that egui has not laid out before first takes a sizing pass (nothing painted,
/// no runs), so a copy being collected draws it once more, the first draw's runs dropped.
fn sized_grid(cx: &Cx, ui: &mut Ui, grid: impl Fn(&mut Ui)) {
    cx.discarding(|| grid(ui));
    grid(ui);
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
    let grid = |ui: &mut Ui| {
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
                    for (c, (id, _, end)) in cols.iter().enumerate() {
                        let job = cells
                            .and_then(|c| c.get(*id))
                            .map(|v| cell_job(cx, v, &cell_base))
                            .unwrap_or_default();
                        let sep = if c == 0 { Sep::Line } else { Sep::Tab };
                        ui.with_layout(
                            Layout::top_down(if *end { Align::Max } else { Align::Min }),
                            |ui| {
                                if job.is_empty() {
                                    ui.add(egui::Label::new(job).selectable(false));
                                } else {
                                    para(cx, ui, job, ink.text(), &[], RunOpts::sep(sep));
                                }
                            },
                        );
                    }
                    ui.end_row();
                }
            });
    };
    sized_grid(cx, ui, grid);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streaming_thought_keeps_its_last_six_lines_and_a_short_one_stays_whole() {
        let max = THINK_ROWS * THINK_LINE;
        assert_eq!(tail_clamp(max, max), None, "exactly six lines");
        assert_eq!(
            tail_clamp(max + THINK_LINE * 0.4, max),
            None,
            "hiding less than half a line is not worth a control"
        );
        let shift = tail_clamp(20.0 * THINK_LINE, max).expect("twenty lines are clamped");
        assert!(
            (shift - 14.0 * THINK_LINE).abs() < 1e-3,
            "the first fourteen lines move out above: {shift}"
        );
    }

    #[test]
    fn wrapped_children_pack_greedily_and_never_exceed_the_line() {
        // 100 px of room, 10 px gaps: 40 + 10 + 40 fits, a third 40 does not.
        let breaks = |w: &[f32], gap, avail| {
            line_breaks(w, gap, avail)
                .map(|r| r.len())
                .collect::<Vec<_>>()
        };
        assert_eq!(breaks(&[40.0, 40.0, 40.0], 10.0, 100.0), vec![2, 1]);
        // Exactly full lines stay whole (the half pixel of slack).
        assert_eq!(breaks(&[45.0, 45.0], 10.0, 100.0), vec![2]);
        // A child wider than the line is bounded to it and fills a line alone.
        assert_eq!(
            breaks(&[20.0, 4000.0, 20.0, 20.0], 6.0, 100.0),
            vec![1, 1, 2]
        );
        assert_eq!(breaks(&[], 6.0, 100.0), Vec::<usize>::new());
        // Whatever the widths, every line holds at least one child and all children are placed.
        assert_eq!(breaks(&[500.0; 5], 0.0, 1.0), vec![1; 5]);
        // Ranges are contiguous and cover the children.
        let ranges: Vec<_> = line_breaks(&[40.0, 40.0, 40.0], 10.0, 100.0).collect();
        assert_eq!(ranges, vec![0..2, 2..3]);
    }

    #[test]
    fn min_widths_parse_in_px_and_ch() {
        let ch = || 8.0;
        assert_eq!(len_px(&serde_json::json!("48ch"), ch), Some(384.0));
        assert_eq!(len_px(&serde_json::json!("120px"), ch), Some(120.0));
        assert_eq!(len_px(&serde_json::json!(90), ch), Some(90.0));
        assert_eq!(len_px(&serde_json::json!("wide"), ch), None);
        assert_eq!(len_px(&serde_json::json!(true), ch), None);
    }

    /// omp's agent-hub body: roster `grow 3, min 48ch` beside detail `grow 2, min 34ch`.
    #[test]
    fn growing_children_share_the_spare_room_or_stack_when_their_minima_do_not_fit() {
        let (roster, detail) = (48.0 * 8.0, 34.0 * 8.0);
        let gap = 10.0;
        // Wide: both minima fit on one line; the spare room splits 3 : 2 and fills the line.
        let avail = 1000.0;
        let breaks: Vec<_> = line_breaks(&[roster, detail], gap, avail).collect();
        assert_eq!(breaks, vec![0..2]);
        let spare = avail - roster - detail - gap;
        let (a, b) = (
            share(roster, 3.0, 5.0, spare),
            share(detail, 2.0, 5.0, spare),
        );
        assert!((a + b + gap - avail).abs() < 1e-3, "{a} + {b}");
        assert!(
            ((a - roster) / (b - detail) - 1.5).abs() < 1e-3,
            "3 : 2 shares"
        );
        // Narrow: the minima do not fit side by side, so each takes a line and fills it.
        let avail = 500.0;
        let breaks: Vec<_> = line_breaks(&[roster, detail], gap, avail).collect();
        assert_eq!(breaks, vec![0..1, 1..2]);
        assert!((share(roster, 3.0, 3.0, avail - roster) - avail).abs() < 1e-3);
        // A child that does not grow keeps its basis.
        assert_eq!(share(40.0, 0.0, 5.0, 100.0), 40.0);
    }
}
