//! The `main` region: a virtualized, stick-to-bottom conversation column with A's structure
//! (right-aligned user bubbles, unboxed assistant prose under an `omp · model` byline, tools
//! inline in document order) in F's language (cut-corner bubbles, accent hairlines).

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Color32, CursorIcon, Id, Layout, Rect, RichText, ScrollArea, Sense, Stroke, Ui,
    UiBuilder, Vec2, pos2, vec2,
};

use super::ctx::{Cx, Tx};
use super::glyphs;
use super::paint;
use super::render::{self, blank};
use super::snap::SNode;
use super::style::{Ink, LABEL, MEASURE, PROSE, PROSE_LINE};
use super::text::{self, Base};
use super::tsel::RunOpts;

/// Horizontal padding of the column inside the pane.
const PAD: f32 = 36.0;
/// Space between top-level items.
const GAP: f32 = 14.0;
/// Breathing room above the first item.
const TOP: f32 = 32.0;
/// Rows beyond the viewport that are still laid out.
const OVERSCAN: f32 = 500.0;
/// Bubbles and tool cards leave this much room to the right of the user bubble for the avatar.
const AVATAR: f32 = 30.0;

/// Width of the conversation column in a pane `width` wide.
pub fn column_width(width: f32) -> f32 {
    (width - 2.0 * PAD).clamp(220.0, MEASURE)
}

/// Size `f` takes when laid out in at most `max_w` (nothing painted, nothing allocated).
pub fn measure_size(ui: &mut Ui, max_w: f32, f: &dyn Fn(&mut Ui)) -> Vec2 {
    let rect = Rect::from_min_size(ui.cursor().min, vec2(max_w, 10_000.0));
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt("measure-size")
            .max_rect(rect)
            .layout(Layout::top_down(Align::Min))
            .invisible()
            .sizing_pass(),
    );
    f(&mut child);
    child.min_rect().size()
}

/// The model's display name: omp's composer model chip, else `omp`.
pub fn model_name(cx: &Cx) -> Option<String> {
    let dock = cx.snap.region("dock")?;
    let chip = cx.snap.by_role(dock, "omp.composer.model")?;
    let name = cx.snap.find(chip, &|c| c.kind == "text")?;
    let s = text::node_plain(name);
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

/// Initials for the user's avatar.
pub fn initials() -> String {
    let name = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let s: String = name
        .chars()
        .filter(|c| c.is_alphanumeric())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    if s.is_empty() { "U".into() } else { s }
}

/// [`estimate`], computed once per (item, subtree, width): it walks the item's whole subtree.
fn estimated(cx: &Cx, n: &SNode, cw: f32, wbucket: i32) -> f32 {
    let hit = cx.loc(|l| {
        l.estimates
            .get(&n.id)
            .filter(|(sub, w, _)| *sub == n.sub && *w == wbucket)
            .map(|e| e.2)
    });
    hit.unwrap_or_else(|| {
        let h = estimate(cx, n, cw);
        cx.loc(|l| l.estimates.insert(n.id.clone(), (n.sub, wbucket, h)));
        h
    })
}

/// Estimated height of an item not measured yet.
fn estimate(cx: &Cx, n: &SNode, cw: f32) -> f32 {
    fn chars(cx: &Cx, n: &SNode) -> usize {
        n.text().len() + cx.snap.kids(n).map(|c| chars(cx, c)).sum::<usize>()
    }
    let per_line = (cw / 8.0).max(20.0);
    let lines = (chars(cx, n) as f32 / per_line).ceil().max(1.0);
    match (n.kind.as_str(), n.role()) {
        ("tool", _) => 34.0,
        (_, "omp.user") => 70.0 + lines * 22.0,
        _ => 30.0 + lines * PROSE * PROSE_LINE,
    }
}

/// The conversation's items: the kids of `main` that draw something.
pub(super) fn items<'a>(cx: &Cx<'a>) -> Vec<&'a SNode> {
    cx.snap
        .region("main")
        .map(|m| {
            cx.snap
                .kids(m)
                .filter(|c| !matches!(c.kind.as_str(), "spacer" | "toast") && !blank(cx, c))
                .collect()
        })
        .unwrap_or_default()
}

/// Shows the conversation in `rect`.
///
/// `end_pad` is the room kept below the last line: the part of `rect` that the floating dock and
/// the fade above it cover, so the end of the conversation can scroll fully into view.
///
/// Returns whether the reader is away from the end (scrolled up): the caller then draws the
/// [`jump_pill`], after everything that floats over the transcript's bottom edge.
pub fn show(cx: &Cx, ui: &mut Ui, rect: Rect, id: Id, end_pad: f32) -> bool {
    let items = items(cx);
    let cw = column_width(rect.width());
    let left = ((rect.width() - cw) / 2.0).max(0.0);
    // A new or replaced surface opens at its end, like a live session does.
    cx.loc(|l| {
        if l.resets_seen != cx.snap.resets {
            l.resets_seen = cx.snap.resets;
            l.follow = true;
            l.jump = true;
        }
    });
    // A drag that selects text scrolls the view at its edges: that is the reader's own scrolling.
    let auto = cx.loc(|l| std::mem::take(&mut l.tsel.scroll_dy));
    if auto < 0.0 {
        cx.loc(|l| l.follow = false);
    }
    let follow = cx.loc(|l| l.follow);
    // The reader is scrolling this frame (wheel, or a drag on the scrollbar strip).
    let scrolling = auto != 0.0
        || ui.input(|i| {
            i.smooth_scroll_delta.y != 0.0
                || (i.pointer.primary_down()
                    && i.pointer
                        .interact_pos()
                        .is_some_and(|p| rect.right() - 14.0 <= p.x && rect.contains(p)))
        });
    let mut child = ui.new_child(UiBuilder::new().id_salt(id.with("main")).max_rect(rect));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    // Text is selected by the chat view's own selection: egui's labels stay out of it.
    child.style_mut().interaction.selectable_labels = false;
    let mut moved = false;
    let wbucket = cw.round() as i32;
    let mut area = ScrollArea::vertical()
        .id_salt(id.with("scroll"))
        .auto_shrink([false, false])
        .stick_to_bottom(follow);
    if auto != 0.0 {
        let at = cx.loc(|l| l.tsel.scroll_y);
        area = area.vertical_scroll_offset((at + auto).max(0.0));
    }
    let out = area.show_viewport(&mut child, |ui, viewport| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let x0 = ui.max_rect().left() + left;
        // Prefix sums of the item heights (measured, else estimated).
        let mut offsets = cx.loc(|l| std::mem::take(&mut l.offsets));
        offsets.clear();
        offsets.push(0.0);
        let mut y = TOP;
        for it in &items {
            // The measured height; while an item changes (a streaming reply grows every
            // frame) its last measurement stands in until the new one is taken. An estimate
            // says nothing about an item that is already thousands of lines long, and one
            // that came out far shorter than the drawn item would leave the offset past the
            // content end (a blank transcript).
            let h = cx.loc(|l| l.heights.get(&it.id).map(|e| e.2));
            let h = h.unwrap_or_else(|| estimated(cx, it, cw, wbucket));
            y += h + GAP;
            offsets.push(y);
        }
        let total = y + end_pad;
        let n = items.len();
        let lo = viewport.min.y - OVERSCAN;
        let hi = viewport.max.y + OVERSCAN;
        let first = offsets[1..].partition_point(|&o| o <= lo).min(n);
        let end = offsets[..n].partition_point(|&o| o < hi).max(first);
        ui.add_space(if first == 0 { TOP } else { offsets[first] });
        let mut prev_user = first == 0;
        if first > 0
            && let Some(p) = items.get(first - 1)
        {
            prev_user = p.role() == "omp.user";
        }
        for it in items.iter().take(end).skip(first) {
            let top = ui.cursor().top();
            let r = Rect::from_min_size(pos2(x0, top), vec2(cw, 10_000.0));
            let byline = prev_user && it.role() == "omp.assistant";
            let inner = ui.scope_builder(UiBuilder::new().max_rect(r), |ui| {
                ui.set_width(cw);
                item(cx, ui, it, byline);
            });
            let h = inner.response.rect.height();
            prev_user = it.role() == "omp.user";
            cx.loc(|l| {
                let e = l.heights.get(&it.id).copied();
                if e.is_none_or(|(sub, w, old)| {
                    sub != it.sub || w != wbucket || (old - h).abs() > 0.5
                }) {
                    l.heights.insert(it.id.clone(), (it.sub, wbucket, h));
                    moved |= e.is_some_and(|(_, w, old)| w == wbucket && (old - h).abs() > 0.5);
                }
            });
            ui.add_space(GAP);
        }
        ui.add_space((total - offsets[end]).max(0.0));
        cx.loc(|l| l.offsets = offsets);
        let (marker, _) = ui.allocate_exact_size(vec2(1.0, 1.0), Sense::hover());
        // A chase of the end gives way to the reader's own scrolling.
        let chase = cx.loc(|l| std::mem::take(&mut l.chase)) && !scrolling;
        if cx.loc(|l| std::mem::take(&mut l.jump)) || chase {
            ui.scroll_to_rect(marker, Some(Align::BOTTOM));
            moved = true;
        }
    });
    cx.loc(|l| l.tsel.scroll_y = out.state.clamped_offset().y);
    if moved {
        ui.ctx().request_repaint();
    }
    // Wheel or scrollbar drag away from the end releases "follow"; reaching the end re-engages it.
    let at_end = out.state.clamped_offset().y + out.inner_rect.height() >= out.content_size.y - 8.0;
    let user = scrolling;
    if user || cx.loc(|l| l.frame) == 0 {
        cx.loc(|l| l.follow = at_end || l.frame == 0);
    }
    cx.loc(|l| l.frame += 1);
    // egui keeps a scroll area stuck to the end only while the content does not grow by more than
    // a pixel within a frame; a streamed reply arrives in chunks of many lines, which unsticks
    // it. Following means being at the end, so go back there until the content settles.
    if chases(cx.loc(|l| l.follow), at_end, user) {
        cx.loc(|l| l.chase = true);
        ui.ctx().request_repaint();
    }
    !cx.loc(|l| l.follow) && !at_end
}

/// Whether the view must scroll back to the end: it follows, is not there, and the reader is not
/// the one moving it.
fn chases(follow: bool, at_end: bool, scrolling: bool) -> bool {
    follow && !at_end && !scrolling
}

/// Where the "Jump to latest" pill's bottom edge goes: just above the dock (the transcript runs
/// under it and its fade, which would cover the pill), else near the bottom of `rect_bottom`.
pub fn pill_bottom(dock_top: Option<f32>, rect_bottom: f32) -> f32 {
    dock_top.map_or(rect_bottom - 14.0, |t| t - 10.0)
}

/// "Jump to latest" pill centred on `x`, its bottom edge at `bottom` (above the dock).
pub fn jump_pill(cx: &Cx, ui: &mut Ui, x: f32, bottom: f32) {
    let ink = cx.ink;
    let label = "Jump to latest";
    let g = ui
        .painter()
        .layout_no_wrap(label.to_owned(), ink.label(12.0), ink.strong());
    let size = vec2(g.size().x + 44.0, 30.0);
    let r = Rect::from_min_size(pos2(x - size.x / 2.0, bottom - size.y), size);
    let resp = ui.interact(r, ui.scope_id().with("jump-latest"), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
    let p = ui.painter();
    crate::deco::drop_shadow(p, r, 15, 6, 14, ink.dark, 150);
    let pts = paint::parallelogram(r, 8.0);
    let (a, b) = ink.head();
    paint::fill_h(p, &pts, a, b);
    crate::deco::gradient_outline(p, pts, 1.0, &ink.rule_stops(), vec2(1.0, 0.0));
    paint::pop(p, r, 6, ink, hov);
    glyphs::paint(
        p,
        "arrow-down",
        pos2(r.left() + 20.0, r.center().y),
        13.0,
        ink.b,
    );
    p.galley(
        pos2(r.left() + 34.0, r.center().y - g.size().y / 2.0),
        g,
        ink.strong(),
    );
    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
        cx.loc(|l| {
            l.follow = true;
            l.jump = true;
        });
        ui.ctx().request_repaint();
    }
}

/// One top-level transcript item.
pub(super) fn item(cx: &Cx, ui: &mut Ui, n: &SNode, byline: bool) {
    cx.in_node(n.ord, || match (n.kind.as_str(), n.role()) {
        ("col", "omp.assistant") => assistant(cx, ui, n, byline),
        _ => render::node(cx, ui, n),
    });
}

/// Role-specific nodes of the transcript; `true` when handled.
pub fn role_node(cx: &Cx, ui: &mut Ui, n: &SNode) -> bool {
    match n.role() {
        "omp.user" if n.kind == "card" => user(cx, ui, n),
        "omp.assistant" if n.kind == "col" => assistant(cx, ui, n, false),
        "omp.turn.usage" => usage(cx, ui, n),
        "omp.compaction" | "omp.handoff" | "omp.branch" if n.kind == "section" => {
            divider_section(cx, ui, n)
        }
        "omp.recap" => recap(cx, ui, n),
        _ => return false,
    }
    true
}

// ---- assistant ------------------------------------------------------------------------------

fn assistant(cx: &Cx, ui: &mut Ui, n: &SNode, byline: bool) {
    let ink = cx.ink;
    if byline {
        let model = model_name(cx)
            .map(|now| cx.loc(|l| l.bylines.entry(n.id.clone()).or_insert(now).clone()));
        let label = match model {
            Some(m) => format!("omp · {m}"),
            None => "omp".to_owned(),
        };
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(18.0, 14.0), Sense::hover());
            paint::omp_mark(ui.painter(), rect, ink);
            ui.add_space(2.0);
            ui.label(
                RichText::new(label)
                    .font(ink.label(LABEL + 0.5))
                    .color(ink.dim()),
            );
        });
        // Accent hairline under the byline, fading out to the right.
        let w = ui.available_width();
        let (rule, _) = ui.allocate_exact_size(vec2(w, 7.0), Sense::hover());
        crate::deco::gradient_rule(
            ui.painter(),
            pos2(rule.left(), rule.center().y),
            pos2(rule.right(), rule.center().y),
            1.0,
            &[
                (0.0, ink.a.gamma_multiply(0.75)),
                (0.45, ink.b.gamma_multiply(0.35)),
                (1.0, Color32::TRANSPARENT),
            ],
        );
        ui.add_space(4.0);
    }
    ui.spacing_mut().item_spacing.y = 8.0;
    let tx = Tx {
        font: ink.prose(PROSE),
        color: ink.text(),
        line: Some(PROSE * PROSE_LINE),
    };
    cx.with_tx(tx, || render::children(cx, ui, n));
}

/// `This turn: …` footer under an answer: the copy icon, then values (numbers) lit and their
/// units dim, followed by a faint rule that separates the turns.
fn usage(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let tx = Tx {
        font: ink.label(12.0),
        color: ink.dim(),
        line: None,
    };
    cx.with_tx(tx, || {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for c in cx.snap.kids(n) {
                if c.kind == "text" && text::node_plain(c).chars().any(|ch| ch.is_ascii_digit()) {
                    ui.add(
                        egui::Label::new(usage_job(ink, &text::node_plain(c))).selectable(false),
                    );
                } else {
                    render::node(cx, ui, c);
                }
            }
        });
    });
    let (rule, _) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::hover());
    crate::deco::gradient_rule(
        ui.painter(),
        pos2(rule.left(), rule.bottom() - 2.0),
        pos2(rule.right(), rule.bottom() - 2.0),
        1.0,
        &[
            (0.0, ink.line()),
            (0.5, ink.line().gamma_multiply(0.5)),
            (1.0, Color32::TRANSPARENT),
        ],
    );
}

/// `7.3s · 15 tok this turn` with the numbers in the text colour and the words dim.
fn usage_job(ink: &Ink, s: &str) -> LayoutJob {
    let font = ink.label(12.0);
    let mut job = LayoutJob::default();
    for (i, w) in s.split_whitespace().enumerate() {
        let value = w
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == '$');
        let color = if value {
            ink.text()
        } else if w == "·" {
            ink.faint()
        } else {
            ink.dim()
        };
        if i > 0 {
            job.append(" ", 0.0, TextFormat::simple(font.clone(), color));
        }
        job.append(w, 0.0, TextFormat::simple(font.clone(), color));
    }
    job
}

fn recap(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let s = text::node_plain(&flatten_text(cx, n));
    let mut format = TextFormat::simple(ink.prose(13.5), ink.dim());
    format.italics = true;
    let mut job = LayoutJob::default();
    job.append(&format!("※ {}", s.trim()), 0.0, format);
    render::para(cx, ui, job, ink.dim(), &[], RunOpts::LINE);
}

/// A synthetic text node standing for the first text descendant of `n`.
fn flatten_text(cx: &Cx, n: &SNode) -> SNode {
    let found = cx.snap.find(n, &|c| c.kind == "text" || c.kind == "md");
    found.cloned().unwrap_or_else(|| n.clone())
}

/// Compaction/handoff/branch: a labelled divider that unfolds to its summary.
fn divider_section(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let collapsed = cx.collapsed(n);
    let base = Base::new(ink.label(12.0), ink.dim(), ink);
    let head = render::head_job(cx, n, &base);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 26.0), Sense::hover());
    let resp = ui.interact(rect, ui.scope_id().with(("fold", &n.id)), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
    let y = rect.center().y;
    let galley = head.map(|j| ui.painter().layout_job(j));
    let gw = galley.as_ref().map_or(0.0, |g| g.size().x) + 34.0;
    let x0 = rect.center().x - gw / 2.0;
    let p = ui.painter();
    let line = |a: f32, b: f32| {
        crate::deco::gradient_rule(
            p,
            pos2(a, y),
            pos2(b, y),
            1.0,
            &[(0.0, Color32::TRANSPARENT), (1.0, ink.line())],
        );
    };
    if x0 - 12.0 > rect.left() {
        line(rect.left(), x0 - 12.0);
        crate::deco::gradient_rule(
            p,
            pos2(x0 + gw + 12.0, y),
            pos2(rect.right(), y),
            1.0,
            &[(0.0, ink.line()), (1.0, Color32::TRANSPARENT)],
        );
    }
    glyphs::paint(
        p,
        if collapsed { "chev-right" } else { "chev" },
        pos2(x0 + 6.0, y),
        11.0,
        ink.dim(),
    );
    if let Some(g) = galley {
        p.galley(pos2(x0 + 22.0, y - g.size().y / 2.0), g, ink.dim());
    }
    paint::pop(
        p,
        Rect::from_center_size(rect.center(), vec2(gw + 12.0, 22.0)),
        6,
        ink,
        hov,
    );
    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
        cx.toggle(n);
    }
    if !collapsed {
        let tx = Tx {
            font: ink.prose(14.0),
            color: ink.dim(),
            line: Some(21.0),
        };
        cx.with_tx(tx, || {
            ui.spacing_mut().item_spacing.y = 6.0;
            render::children(cx, ui, n);
        });
    }
}

// ---- user -----------------------------------------------------------------------------------

fn user(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let avail = ui.available_width();
    let max_w = (avail * 0.7 + 40.0).min(avail - AVATAR - 10.0).max(160.0);
    let kids: Vec<&SNode> = cx.snap.kids(n).collect();
    let tools = kids.iter().copied().find(|c| c.role() == "omp.user.tools");
    let body: Vec<&SNode> = kids
        .iter()
        .copied()
        .filter(|c| c.role() != "omp.user.tools" && !blank(cx, c))
        .collect();
    let time = tools.and_then(|t| {
        cx.snap
            .kids(t)
            .find(|c| c.role() == "omp.user.time")
            .map(text::node_plain)
    });
    let pad_x = 18.0;
    let head_h = 26.0;
    let inner_w = max_w - 2.0 * pad_x;
    let tx = Tx {
        font: ink.prose(PROSE),
        color: ink.strong(),
        line: Some(PROSE * PROSE_LINE),
    };
    let content = |ui: &mut Ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        cx.with_tx(tx.clone(), || {
            for c in &body {
                render::node(cx, ui, c);
            }
        });
    };
    // Natural size of the body decides the bubble's width (wide text fills the measure).
    let size = measure_size(ui, inner_w, &content);
    let w = (size.x + 2.0 * pad_x).clamp(150.0, max_w);
    let h = head_h + size.y + 14.0;
    let x1 = ui.max_rect().right() - AVATAR - 10.0;
    let origin = pos2(ui.max_rect().left(), ui.cursor().top());
    let rect = Rect::from_min_size(pos2(x1 - w, origin.y + 4.0), vec2(w, h));
    let (_, resp) = ui.allocate_exact_size(vec2(avail, h + 8.0), Sense::hover());
    let hov_t = ui.ctx().animate_bool_with_time(
        ui.scope_id().with(("user-hov", &n.id)),
        ui.rect_contains_pointer(rect.expand2(vec2(40.0, 4.0))),
        0.12,
    );
    let p = ui.painter();
    let pts = paint::cut_bubble(rect, 14.0);
    let (c0, c1) = ink.bubble();
    crate::deco::drop_shadow(p, rect, 8, 5, 14, ink.dark, 70);
    crate::deco::fill_linear(p, &pts, c0, c1, vec2(1.0, 0.6));
    // Header band with the slanted end.
    let band = Rect::from_min_size(rect.min, vec2(w, head_h));
    let (b0, b1) = ink.head();
    let bpts = vec![
        band.left_top(),
        band.right_top(),
        pos2(band.right() - 12.0, band.bottom()),
        band.left_bottom(),
    ];
    crate::deco::fill_linear(p, &bpts, b0, b1, vec2(1.0, 0.0));
    crate::deco::outline(
        p,
        pts.clone(),
        Stroke::new(1.0, ink.edge(ink.a).gamma_multiply(0.8 + 0.2 * hov_t)),
    );
    // Accent bar on the left edge.
    p.line_segment(
        [
            pos2(rect.left() + 1.0, rect.top() + 14.0),
            pos2(rect.left() + 1.0, rect.bottom() - 14.0),
        ],
        Stroke::new(2.0, ink.b),
    );
    paint::hairline(p, ink, rect.left(), rect.right() - 14.0, rect.top(), 0.9);
    paint::pop(p, rect, 4, ink, hov_t * 0.6);
    // Header: label left, tools + time right.
    let label = ui
        .painter()
        .layout_no_wrap("YOU".to_owned(), ink.label(LABEL), ink.accent_text());
    ui.painter().galley(
        pos2(rect.left() + pad_x, band.center().y - label.size().y / 2.0),
        label,
        ink.accent_text(),
    );
    let mut rx = rect.right() - pad_x - 6.0;
    if let Some(t) = &time {
        let g = ui
            .painter()
            .layout_no_wrap(t.clone(), ink.label(LABEL), ink.dim());
        rx -= g.size().x;
        ui.painter()
            .galley(pos2(rx, band.center().y - g.size().y / 2.0), g, ink.dim());
        rx -= 8.0;
    }
    if let Some(tools) = tools {
        for ic in cx
            .snap
            .kids(tools)
            .filter(|c| c.kind == "icon")
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let name = ic.str("name").unwrap_or_default();
            let r = Rect::from_center_size(pos2(rx - 8.0, band.center().y), vec2(20.0, 20.0));
            rx -= 22.0;
            let id = ui.scope_id().with(("user-tool", &ic.id));
            let resp = ui.interact(
                r,
                id,
                if hov_t > 0.05 {
                    Sense::click()
                } else {
                    Sense::hover()
                },
            );
            let hv = ui
                .ctx()
                .animate_bool_with_time(id.with("h"), resp.hovered(), 0.08);
            if hov_t > 0.02 {
                if hv > 0.0 {
                    ui.painter()
                        .rect_filled(r, 4.0, ink.p.hover_bg.gamma_multiply(hv));
                    paint::pop(ui.painter(), r, 4, ink, hv);
                }
                glyphs::paint(
                    ui.painter(),
                    name,
                    r.center(),
                    13.0,
                    ink.dim()
                        .gamma_multiply(0.5 + 0.5 * hov_t)
                        .lerp_to_gamma(ink.strong(), hv),
                );
            }
            let resp = match ic.str("title") {
                Some(t) if hov_t > 0.05 => resp.on_hover_text(text::clean(t).into_owned()),
                _ => resp,
            };
            if hov_t > 0.05
                && resp.on_hover_cursor(CursorIcon::PointingHand).clicked()
                && let Some(a) = ic.click()
            {
                cx.click(ic, a);
            }
        }
    }
    // Body.
    let body_rect = Rect::from_min_size(
        pos2(rect.left() + pad_x, rect.top() + head_h + 6.0),
        vec2(w - 2.0 * pad_x, size.y + 4.0),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(("user-body", &n.id))
            .max_rect(body_rect)
            .layout(Layout::top_down(Align::Min)),
    );
    child.set_clip_rect(ui.clip_rect());
    content(&mut child);
    // Avatar.
    let av = Rect::from_min_size(pos2(x1 + 10.0, rect.top() + 2.0), vec2(AVATAR, AVATAR));
    let p = ui.painter();
    p.circle_filled(av.center(), AVATAR / 2.0, ink.a);
    crate::deco::fill_linear(
        p,
        &circle_pts(av.center(), AVATAR / 2.0),
        ink.a,
        ink.b,
        vec2(1.0, 1.0),
    );
    p.circle_stroke(
        av.center(),
        AVATAR / 2.0,
        Stroke::new(1.0, Color32::from_white_alpha(36)),
    );
    let ini = p.layout_no_wrap(
        initials(),
        ink.label(12.0),
        crate::deco::ink_on_pair(ink.a, ink.b),
    );
    p.galley(
        av.center() - ini.size() / 2.0,
        ini,
        crate::deco::ink_on_pair(ink.a, ink.b),
    );
    let _ = resp;
}

fn circle_pts(c: egui::Pos2, r: f32) -> Vec<egui::Pos2> {
    (0..28)
        .map(|k| {
            let a = k as f32 / 28.0 * std::f32::consts::TAU;
            c + vec2(a.cos(), a.sin()) * r
        })
        .collect()
}

// ---- welcome --------------------------------------------------------------------------------

/// The `omp.welcome` card: the mark, wordmark, version and tip.
pub fn welcome(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let kids: Vec<&SNode> = cx.snap.kids(n).collect();
    let version = kids
        .iter()
        .find_map(|c| cx.snap.by_role(c, "omp.welcome.version"))
        .map(text::node_plain);
    let tip = kids
        .iter()
        .find_map(|c| cx.snap.by_role(c, "omp.welcome.tip-text"))
        .map(text::node_plain);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(46.0, 40.0), Sense::hover());
        paint::omp_mark(ui.painter(), rect, ink);
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(
                RichText::new("omp")
                    .font(ink.prose(20.0))
                    .color(ink.strong()),
            );
            if let Some(v) = version {
                ui.label(RichText::new(v).font(ink.label(12.0)).color(ink.dim()));
            }
        });
    });
    if let Some(tip) = tip {
        ui.add_space(8.0);
        ui.horizontal_top(|ui| {
            glyphs::icon(ui, "lightbulb", 14.0, ink.tone("warning"), Sense::hover());
            let tx = Tx {
                font: ink.prose(13.5),
                color: ink.dim(),
                line: Some(20.0),
            };
            let mut base = Base::new(tx.font.clone(), tx.color, ink);
            base.line_height = tx.line;
            let mut job = egui::text::LayoutJob::default();
            md_inline(&mut job, ink, &base, &tip);
            let _ = render::para(cx, ui, job, tx.color, &[], RunOpts::LINE);
        });
    }
}

/// `text` with `inline code` runs set in the code style.
fn md_inline(job: &mut egui::text::LayoutJob, ink: &super::style::Ink, base: &Base, s: &str) {
    for (i, part) in s.split('`').enumerate() {
        if part.is_empty() {
            continue;
        }
        let st = if i % 2 == 1 {
            text::span_style(ink, "code")
        } else {
            text::SpanStyle::default()
        };
        let spans = [serde_json::json!({"t": part, "s": if i % 2 == 1 { "code" } else { "" }})];
        let _ = st;
        text::push_spans(job, ink, base, &spans);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_following_view_that_is_off_the_end_goes_back_unless_the_reader_scrolls() {
        // Streamed chunks grow the content past what egui keeps stuck to.
        assert!(chases(true, false, false));
        assert!(!chases(true, true, false), "already at the end");
        assert!(!chases(true, false, true), "the reader's wheel wins");
        assert!(!chases(false, false, false), "scrolled away: stay put");
    }

    #[test]
    fn the_jump_pill_sits_above_the_dock_not_under_it() {
        let dock_top = 700.0;
        let bottom = pill_bottom(Some(dock_top), 850.0);
        assert!(bottom < dock_top, "the dock would paint over it otherwise");
        assert!(pill_bottom(None, 850.0) < 850.0);
    }
}
