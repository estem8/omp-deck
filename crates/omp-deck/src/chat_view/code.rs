//! `code` and `diff` nodes: framed mono blocks with an optional line-number gutter, tone-washed
//! lines, a hover-revealed Copy button, the "N more lines" preview clamp and horizontal scrolling
//! only when a line outruns the block. Both kinds share [`block`]; they differ in the rows they
//! feed it.

use std::sync::Arc;
use std::time::Duration;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Color32, CornerRadius, FontId, Galley, Id, Pos2, Rect, Sense, Stroke, Ui, UiBuilder,
    pos2, vec2,
};
use serde_json::Value;

use super::ansi;
use super::ctx::Cx;
use super::glyphs;
use super::paint;
use super::snap::SNode;
use super::style::LABEL;
use super::text;
use crate::theme::mix;

const PAD_X: f32 = 10.0;
const PAD_Y: f32 = 8.0;
const GUT_PAD: f32 = 7.0;
const HEADER_H: f32 = 26.0;
const FOOT_H: f32 = 30.0;
const RADIUS: u8 = 6;
/// Most rows drawn of one block.
const MAX_ROWS: usize = 2000;
/// How long the Copy button shows its check.
const COPIED: Duration = Duration::from_millis(1200);

// ---- shared block ---------------------------------------------------------------------------

/// One drawn row.
struct Row<'a> {
    num: Option<u32>,
    marker: Option<(char, Color32)>,
    text: &'a str,
    color: Color32,
    /// Wash across the whole row.
    fill: Option<Color32>,
    /// Tint of the gutter cell.
    gutter_fill: Option<Color32>,
    /// 2 px bar on the left edge.
    bar: Option<Color32>,
    num_color: Color32,
}

/// What sits under the rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Foot {
    None,
    /// Clamped: this many lines hidden, a button opens them.
    More(usize),
    /// Opened: a button clamps again.
    Less,
    /// Too many rows to draw: this many are cut for good.
    Cut(usize),
}

struct Spec<'a> {
    key: &'a str,
    count: usize,
    row: &'a dyn Fn(usize) -> Row<'a>,
    /// Digits of the gutter's numbers (0 = no gutter).
    digits: usize,
    marker: bool,
    wrap: bool,
    /// Longest line in chars (monospace: its width).
    max_chars: usize,
    header: Option<&'a str>,
    foot: Foot,
    copy: &'a dyn Fn() -> String,
}

/// Row count and footer of `total` rows under a preview clamp.
fn clamp(total: usize, pv: Option<ansi::Preview>, expanded: bool) -> (usize, Foot) {
    match pv {
        Some(p) if total > p.lines && !expanded => (p.lines, Foot::More(total - p.lines)),
        Some(p) if total > p.lines => (total.min(MAX_ROWS), Foot::Less),
        _ if total > MAX_ROWS => (MAX_ROWS, Foot::Cut(total - MAX_ROWS)),
        _ => (total, Foot::None),
    }
}

/// Digits of `max` (at least one).
pub fn digits(max: u32) -> usize {
    max.max(1).ilog10() as usize + 1
}

/// Digits the gutter needs for `total` lines numbered from `start`, or up to the largest explicit
/// number `nums_max`.
pub fn gutter_digits(start: u32, total: usize, nums_max: Option<u32>) -> usize {
    let last = start.saturating_add(total.saturating_sub(1).min(u32::MAX as usize) as u32);
    digits(nums_max.unwrap_or(last))
}

fn row_job(text: &str, font: &FontId, color: Color32, line_h: f32, wrap: Option<f32>) -> LayoutJob {
    let mut job = LayoutJob::single_section(
        text.to_owned(),
        TextFormat {
            font_id: font.clone(),
            color,
            line_height: Some(line_h),
            valign: Align::Center,
            ..TextFormat::default()
        },
    );
    if let Some(w) = wrap {
        job.wrap.max_width = w;
        job.wrap.break_anywhere = true;
    }
    job
}

/// Draws `s` and returns whether its footer button was clicked.
fn block(cx: &Cx, ui: &mut Ui, s: &Spec) -> bool {
    let ink = cx.ink;
    let font = {
        let f = ink.mono(0.95);
        if f.size < LABEL {
            ink.mono_px(LABEL)
        } else {
            f
        }
    };
    let num_font = ink.mono_px((font.size * 0.88).max(LABEL));
    let line_h = (font.size * 1.5).round();
    let painter = ui.painter().clone();
    let cw = painter
        .layout_no_wrap("0".to_owned(), font.clone(), Color32::WHITE)
        .size()
        .x;
    let ncw = painter
        .layout_no_wrap("0".to_owned(), num_font.clone(), Color32::WHITE)
        .size()
        .x;
    let avail = ui.available_width();
    let w = if avail.is_finite() {
        avail.max(120.0)
    } else {
        520.0
    };

    let gutter_w = if s.digits > 0 {
        s.digits as f32 * ncw + 2.0 * GUT_PAD
    } else {
        0.0
    };
    let marker_w = if s.marker { cw + 8.0 } else { 0.0 };
    let lead = gutter_w + marker_w;
    let text_pad = if lead > 0.0 { 8.0 } else { PAD_X };
    let text_w = (w - 2.0 - lead - text_pad - PAD_X).max(40.0);

    let galleys: Vec<Arc<Galley>> = if s.wrap {
        (0..s.count)
            .map(|i| {
                let r = (s.row)(i);
                painter.layout_job(row_job(r.text, &font, r.color, line_h, Some(text_w)))
            })
            .collect()
    } else {
        Vec::new()
    };
    let height_of = |i: usize| -> f32 {
        if s.wrap {
            galleys.get(i).map_or(line_h, |g| g.size().y.max(line_h))
        } else {
            line_h
        }
    };
    let rows_h: f32 = if s.wrap {
        (0..s.count).map(height_of).sum()
    } else {
        s.count as f32 * line_h
    };
    let header_h = if s.header.is_some() { HEADER_H } else { 0.0 };
    let foot_h = if s.foot == Foot::None { 0.0 } else { FOOT_H };
    let content_w = s.max_chars as f32 * cw + 2.0;
    let scroll = !s.wrap && content_w > text_w + 0.5;
    let total_h = 2.0 + header_h + 2.0 * PAD_Y + rows_h + foot_h;

    let (rect, _) = ui.allocate_exact_size(vec2(w, total_h), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return false;
    }
    let id = ui.scope_id().with(("chat-block", s.key));
    let fill = mix(ink.card(), ink.p.bg, 0.3);
    let r_in = RADIUS.saturating_sub(1);

    // Frame, hover pop, accent hairline.
    let hovered = ui.rect_contains_pointer(rect);
    let hot = ui
        .ctx()
        .animate_bool_with_time(id.with("hov"), hovered, 0.12);
    paint::boxed(&painter, rect, RADIUS, fill, Stroke::new(1.0, ink.line()));
    paint::pop(&painter, rect, RADIUS, ink, hot);
    paint::hairline(
        &painter,
        ink,
        rect.left() + 10.0,
        rect.right() - 10.0,
        rect.top(),
        0.5,
    );

    // Header: the file the block comes from.
    if let Some(h) = s.header {
        let band = Rect::from_min_size(rect.min + vec2(1.0, 1.0), vec2(w - 2.0, header_h - 1.0));
        painter.rect_filled(
            band,
            CornerRadius {
                nw: r_in,
                ne: r_in,
                sw: 0,
                se: 0,
            },
            mix(fill, ink.a, 0.07),
        );
        painter.hline(
            band.x_range(),
            rect.top() + header_h,
            Stroke::new(1.0, ink.line()),
        );
        let g = painter.layout_no_wrap(h.to_owned(), ink.label(12.0), ink.dim());
        painter.galley(
            pos2(rect.left() + PAD_X, band.center().y - g.size().y / 2.0),
            g,
            ink.dim(),
        );
    }

    let gx0 = rect.left() + 1.0;
    let body_top = rect.top() + 1.0 + header_h;
    let body_bot = rect.bottom() - 1.0 - foot_h;
    let y0 = body_top + PAD_Y;
    let clip = painter.clip_rect();

    // Gutter column.
    if gutter_w > 0.0 {
        let g = Rect::from_min_max(pos2(gx0, body_top), pos2(gx0 + gutter_w, body_bot));
        painter.rect_filled(
            g,
            CornerRadius {
                nw: if header_h == 0.0 { r_in } else { 0 },
                sw: if foot_h == 0.0 { r_in } else { 0 },
                ne: 0,
                se: 0,
            },
            mix(fill, ink.p.bg, 0.35),
        );
        painter.vline(
            g.right(),
            g.y_range(),
            Stroke::new(1.0, ink.line().gamma_multiply(0.7)),
        );
    }

    // Row backdrops, numbers and markers.
    let (first, last) = visible(s, clip, y0, line_h);
    let mut y = if s.wrap {
        y0
    } else {
        y0 + first as f32 * line_h
    };
    for i in first..last {
        let rh = height_of(i);
        if s.wrap && (y + rh < clip.top() || y > clip.bottom()) {
            y += rh;
            continue;
        }
        let r = (s.row)(i);
        if let Some(f) = r.fill {
            painter.rect_filled(Rect::from_min_size(pos2(gx0, y), vec2(w - 2.0, rh)), 0.0, f);
        }
        if let (Some(f), true) = (r.gutter_fill, gutter_w > 0.0) {
            painter.rect_filled(
                Rect::from_min_size(pos2(gx0, y), vec2(gutter_w, rh)),
                0.0,
                f,
            );
        }
        if let Some(b) = r.bar {
            painter.rect_filled(Rect::from_min_size(pos2(gx0, y), vec2(2.0, rh)), 0.0, b);
        }
        if let (Some(num), true) = (r.num, gutter_w > 0.0) {
            let g = painter.layout_no_wrap(num.to_string(), num_font.clone(), r.num_color);
            let at = pos2(
                gx0 + gutter_w - GUT_PAD - g.size().x,
                y + (line_h - g.size().y) / 2.0,
            );
            painter.galley(at, g, r.num_color);
        }
        if let Some((ch, c)) = r.marker {
            let g = painter.layout_no_wrap(ch.to_string(), font.clone(), c);
            let at = pos2(gx0 + gutter_w + 4.0, y + (line_h - g.size().y) / 2.0);
            painter.galley(at, g, c);
        }
        y += rh;
    }

    // The text itself: selectable per row, in a horizontal scroller when a line is too long.
    let text_x = gx0 + lead + text_pad;
    let right = rect.right() - 1.0 - if scroll { 2.0 } else { PAD_X };
    let text_rect = Rect::from_min_max(pos2(text_x, y0), pos2(right, y0 + rows_h));
    let draw_text = |ui: &mut Ui, origin: Pos2| {
        let clip = ui.clip_rect();
        let (first, last) = visible(s, clip, origin.y, line_h);
        let mut y = if s.wrap {
            origin.y
        } else {
            origin.y + first as f32 * line_h
        };
        let mut sense = Sense::click_and_drag();
        sense -= Sense::FOCUSABLE;
        for i in first..last {
            let rh = height_of(i);
            if s.wrap && (y + rh < clip.top() || y > clip.bottom()) {
                y += rh;
                continue;
            }
            let r = (s.row)(i);
            let galley = if s.wrap {
                match galleys.get(i) {
                    Some(g) => Arc::clone(g),
                    None => break,
                }
            } else {
                ui.painter()
                    .layout_job(row_job(r.text, &font, r.color, line_h, None))
            };
            let pos = pos2(origin.x, y + (rh - galley.size().y) / 2.0);
            let hit = Rect::from_min_size(pos, vec2(galley.size().x.max(1.0), galley.size().y));
            let resp = ui.interact(hit, id.with(("t", i)), sense);
            egui::text_selection::LabelSelectionState::label_text_selection(
                ui,
                &resp,
                pos,
                galley,
                r.color,
                Stroke::NONE,
            );
            y += rh;
        }
    };
    let mut child = ui.new_child(UiBuilder::new().max_rect(text_rect));
    if scroll {
        egui::ScrollArea::horizontal()
            .id_salt(id.with("scroll"))
            .auto_shrink([false, false])
            .show(&mut child, |ui| {
                let (c, _) = ui.allocate_exact_size(vec2(content_w, rows_h), Sense::hover());
                draw_text(ui, c.min);
            });
    } else {
        draw_text(&mut child, text_rect.min);
    }

    // Copy: revealed by hovering the block, a check for a moment after the click.
    let copied = cx.loc(|l| {
        l.copied
            .get(s.key)
            .map(|t| cx.now.duration_since(*t))
            .filter(|d| *d < COPIED)
    });
    if let Some(d) = copied {
        ui.ctx()
            .request_repaint_after(COPIED - d + Duration::from_millis(20));
    }
    let vis = ui
        .ctx()
        .animate_bool_with_time(id.with("cp-vis"), hovered || copied.is_some(), 0.12);
    if vis > 0.02 {
        let size = vec2(26.0, 22.0);
        let top = if header_h > 0.0 {
            rect.top() + (header_h - size.y) / 2.0 + 0.5
        } else {
            rect.top() + 5.0
        };
        let btn = Rect::from_min_size(pos2(rect.right() - 6.0 - size.x, top), size);
        let resp = ui
            .interact(btn, id.with("cp"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let ht = ui
            .ctx()
            .animate_bool_with_time(id.with("cp-hov"), resp.hovered(), 0.1);
        paint::boxed(
            &painter,
            btn,
            4,
            mix(fill, ink.p.hover_bg, 0.6 + 0.4 * ht).gamma_multiply(vis),
            Stroke::new(
                1.0,
                mix(ink.line(), ink.p.hover_line, ht).gamma_multiply(vis),
            ),
        );
        let (name, c) = if copied.is_some() {
            ("check", ink.p.ok)
        } else {
            ("copy", mix(ink.dim(), ink.strong(), ht))
        };
        glyphs::paint(&painter, name, btn.center(), 13.0, c.gamma_multiply(vis));
        if resp.clicked() {
            cx.copy((s.copy)());
            cx.loc(|l| l.copied.insert(s.key.to_owned(), cx.now));
        }
        resp.on_hover_text(if copied.is_some() { "Copied" } else { "Copy" });
    }

    // Footer: the clamp's button.
    let mut clicked = false;
    if foot_h > 0.0 {
        let fr = Rect::from_min_max(
            pos2(rect.left() + 1.0, rect.bottom() - 1.0 - foot_h),
            pos2(rect.right() - 1.0, rect.bottom() - 1.0),
        );
        painter.hline(
            fr.x_range(),
            fr.top(),
            Stroke::new(1.0, ink.line().gamma_multiply(0.7)),
        );
        let (label, up, button) = match s.foot {
            Foot::More(n) => (
                format!("{n} more line{}", if n == 1 { "" } else { "s" }),
                false,
                true,
            ),
            Foot::Less => ("Show less".to_owned(), true, true),
            Foot::Cut(n) => (format!("… {n} more lines not shown"), false, false),
            Foot::None => (String::new(), false, false),
        };
        let g = painter.layout_no_wrap(label.clone(), ink.label(LABEL), ink.dim());
        if button {
            let b = Rect::from_min_size(
                pos2(fr.left() + PAD_X - 2.0, fr.center().y - 11.0),
                vec2(g.size().x + 30.0, 22.0),
            );
            clicked = ansi::more_button_at(cx, ui, b, id.with("more"), &label, up).clicked();
        } else {
            painter.galley(
                pos2(fr.left() + PAD_X, fr.center().y - g.size().y / 2.0),
                g,
                ink.faint(),
            );
        }
    }
    clicked
}

/// Rows of `s` whose rect can touch `clip` for a body starting at `top` (all of them when wrapped:
/// their heights vary, the caller skips by position).
fn visible(s: &Spec, clip: Rect, top: f32, line_h: f32) -> (usize, usize) {
    if s.wrap {
        return (0, s.count);
    }
    let first = ((clip.top() - top) / line_h).floor().max(0.0) as usize;
    let last = ((clip.bottom() - top) / line_h).ceil().max(0.0) as usize;
    (first.min(s.count), last.min(s.count))
}

/// A parse cached in egui's temp memory per (node, generation).
fn cached<T: Send + Sync + 'static>(
    ctx: &egui::Context,
    key: Id,
    gen_: u64,
    make: impl FnOnce() -> T,
) -> Arc<T> {
    if let Some((g, d)) = ctx.data(|m| m.get_temp::<(u64, Arc<T>)>(key))
        && g == gen_
    {
        return d;
    }
    let d = Arc::new(make());
    ctx.data_mut(|m| m.insert_temp(key, (gen_, Arc::clone(&d))));
    d
}

// ---- code -----------------------------------------------------------------------------------

/// A code text split into lines (ranges into its own cleaned, tab-expanded copy).
struct CodeDoc {
    text: String,
    lines: Vec<(u32, u32)>,
    max_chars: usize,
}

impl CodeDoc {
    fn new(src: &str) -> CodeDoc {
        let text = text::clean(src).replace('\t', "    ");
        let mut lines = Vec::new();
        let mut max_chars = 0;
        if !text.is_empty() {
            let mut pos = 0usize;
            for raw in text.split('\n') {
                let line = raw.strip_suffix('\r').unwrap_or(raw);
                lines.push((pos as u32, (pos + line.len()) as u32));
                max_chars = max_chars.max(line.chars().count());
                pos += raw.len() + 1;
            }
            if text.ends_with('\n') {
                lines.pop();
            }
        }
        CodeDoc {
            text,
            lines,
            max_chars,
        }
    }

    fn line(&self, i: usize) -> &str {
        self.lines
            .get(i)
            .and_then(|(a, b)| self.text.get(*a as usize..*b as usize))
            .unwrap_or_default()
    }
}

struct CodeRows<'a> {
    doc: &'a CodeDoc,
    n: &'a SNode,
    ink: &'a super::style::Ink,
    start: u32,
    numbers: bool,
}

impl<'a> CodeRows<'a> {
    /// The number shown for row `i`: an explicit `nums[i]`, else `start + i`.
    fn label(&self, i: usize) -> u32 {
        self.n
            .arr("nums")
            .get(i)
            .and_then(Value::as_u64)
            .map_or_else(|| self.start.saturating_add(i as u32), |v| v as u32)
    }

    fn row(&self, i: usize) -> Row<'a> {
        let ink = self.ink;
        let label = self.label(i);
        let mark = self
            .n
            .arr("marks")
            .iter()
            .find(|m| m.get("line").and_then(Value::as_u64) == Some(u64::from(label)))
            .map(|m| ink.tone(m.get("tone").and_then(Value::as_str).unwrap_or("accent")));
        Row {
            num: self.numbers.then_some(label),
            marker: None,
            text: self.doc.line(i),
            color: ink.text(),
            fill: mark.map(|c| ink.wash(c)),
            gutter_fill: mark.map(|c| mix(ink.wash(c), c, 0.2)),
            bar: mark,
            num_color: mark.map_or_else(|| ink.faint(), |c| mix(ink.dim(), c, 0.6)),
        }
    }
}

/// Kind `code`.
pub fn show_code(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let doc = cached(
        ui.ctx(),
        Id::unique(("chat-code", n.id.as_str())),
        n.gen_,
        || CodeDoc::new(n.text()),
    );
    let total = doc.lines.len();
    if total == 0 {
        return;
    }
    let start = n.num("start").map_or(1, |v| v.max(0.0) as u32);
    let numbers = n
        .get("numbers")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| !n.arr("nums").is_empty() || n.get("start").is_some());
    let nums_max = n
        .arr("nums")
        .iter()
        .filter_map(Value::as_u64)
        .max()
        .map(|v| v as u32);
    let pv = ansi::preview(cx, n).map(|p| ansi::Preview { tail: false, ..p });
    let expanded = pv.is_some() && ansi::more_open(cx, n);
    let (count, foot) = clamp(total, pv, expanded);
    let header = n
        .str("path")
        .filter(|p| !p.is_empty())
        .filter(|_| !ansi::inside(cx, n, "tool"));
    let rows = CodeRows {
        doc: &doc,
        n,
        ink,
        start,
        numbers,
    };
    let row = |i: usize| rows.row(i);
    let copy = || text::clean(n.text()).into_owned();
    let spec = Spec {
        key: &n.id,
        count,
        row: &row,
        digits: if numbers {
            gutter_digits(start, total, nums_max)
        } else {
            0
        },
        marker: false,
        wrap: n.flag("wrap"),
        max_chars: doc.max_chars,
        header,
        foot,
        copy: &copy,
    };
    if block(cx, ui, &spec) {
        ansi::toggle_more(cx, n);
    }
}

// ---- diff -----------------------------------------------------------------------------------

/// What a line of a unified diff is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Add,
    Del,
    Ctx,
    /// `@@ -a,b +c,d @@ …`
    Hunk,
    /// File headers, `index …`, `\ No newline …`.
    Meta,
}

/// Classifies one line of unified diff text; `in_hunk` tells whether the line can be hunk content
/// (inside a hunk `--- x` is a deleted line, outside it is a file header).
pub fn classify(line: &str, in_hunk: bool) -> Kind {
    const META: [&str; 13] = [
        "diff ",
        "index ",
        "new file",
        "deleted file",
        "old mode",
        "new mode",
        "similarity index",
        "dissimilarity index",
        "rename from",
        "rename to",
        "copy from",
        "copy to",
        "Binary files",
    ];
    if line.starts_with("@@") {
        return Kind::Hunk;
    }
    if line.starts_with('\\') || META.iter().any(|m| line.starts_with(m)) {
        return Kind::Meta;
    }
    if !in_hunk && (line.starts_with("--- ") || line.starts_with("+++ ")) {
        return Kind::Meta;
    }
    match line.as_bytes().first() {
        Some(b'+') => Kind::Add,
        Some(b'-') => Kind::Del,
        Some(b' ') | None => Kind::Ctx,
        Some(_) => Kind::Meta,
    }
}

/// `@@ -a[,b] +c[,d] @@` → (a, b, c, d); a missing length is 1.
fn parse_hunk(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let new = rest.split_once(" @@").map_or(rest, |(n, _)| n);
    let pair = |s: &str| -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    };
    let (os, ol) = pair(old)?;
    let (ns, nl) = pair(new)?;
    Some((os, ol, ns, nl))
}

struct DRow {
    kind: Kind,
    num: Option<u32>,
    a: u32,
    b: u32,
}

/// A diff as rows over one owned text.
struct DiffDoc {
    text: String,
    rows: Vec<DRow>,
    max_chars: usize,
    /// Digits of the gutter numbers (0 when no row is numbered).
    digits: usize,
}

#[derive(Default)]
struct DiffBuilder {
    text: String,
    rows: Vec<DRow>,
    max_chars: usize,
    top: u32,
}

impl DiffBuilder {
    fn push(&mut self, kind: Kind, num: Option<u32>, s: &str) {
        let a = self.text.len() as u32;
        let mut chars = 0;
        for c in text::clean(s).chars() {
            if c == '\t' {
                self.text.push_str("    ");
                chars += 4;
            } else {
                self.text.push(c);
                chars += 1;
            }
        }
        self.max_chars = self.max_chars.max(chars);
        self.top = self.top.max(num.unwrap_or(0));
        self.rows.push(DRow {
            kind,
            num,
            a,
            b: self.text.len() as u32,
        });
    }

    /// An add/delete/context line: numbers it (new numbering, old for deletions) and moves the
    /// counters.
    fn line(&mut self, kind: Kind, content: &str, numbered: bool, old: &mut u32, new: &mut u32) {
        let num = |n: u32| numbered.then_some(n);
        match kind {
            Kind::Add => {
                self.push(kind, num(*new), content);
                *new += 1;
            }
            Kind::Del => {
                self.push(kind, num(*old), content);
                *old += 1;
            }
            _ => {
                self.push(Kind::Ctx, num(*new), content);
                *old += 1;
                *new += 1;
            }
        }
    }

    fn finish(self) -> DiffDoc {
        DiffDoc {
            digits: if self.top == 0 { 0 } else { digits(self.top) },
            text: self.text,
            rows: self.rows,
            max_chars: self.max_chars,
        }
    }
}

fn from_unified(b: &mut DiffBuilder, src: &str) {
    let body = src.strip_suffix('\n').unwrap_or(src);
    if body.is_empty() {
        return;
    }
    let (mut old, mut new) = (0u32, 0u32);
    let (mut seen, mut in_hunk) = (false, false);
    let (mut rem_old, mut rem_new) = (0u32, 0u32);
    for raw in body.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        match classify(line, in_hunk) {
            Kind::Hunk => {
                match parse_hunk(line) {
                    Some((os, ol, ns, nl)) => {
                        (old, new, rem_old, rem_new) = (os, ns, ol, nl);
                        seen = true;
                    }
                    None => (rem_old, rem_new) = (u32::MAX, u32::MAX),
                }
                in_hunk = true;
                b.push(Kind::Hunk, None, line);
            }
            Kind::Meta => b.push(Kind::Meta, None, line),
            kind => {
                let content = match kind {
                    Kind::Ctx => line.strip_prefix(' ').unwrap_or(line),
                    _ => &line[1..],
                };
                b.line(kind, content, seen, &mut old, &mut new);
                if kind != Kind::Add {
                    rem_old = rem_old.saturating_sub(1);
                }
                if kind != Kind::Del {
                    rem_new = rem_new.saturating_sub(1);
                }
            }
        }
        if in_hunk && rem_old == 0 && rem_new == 0 {
            in_hunk = false;
        }
    }
}

fn from_hunks(b: &mut DiffBuilder, hunks: &[Value]) {
    for (hi, h) in hunks.iter().enumerate() {
        let start = |k: &str| h.get(k).and_then(Value::as_u64).unwrap_or(1) as u32;
        let (mut old, mut new) = (start("oldStart"), start("newStart"));
        if hi > 0 {
            b.push(Kind::Hunk, None, &format!("@@ -{old} +{new} @@"));
        }
        let lines = h.get("lines").and_then(Value::as_array);
        for l in lines.into_iter().flatten().filter_map(Value::as_str) {
            let (kind, content) = match l.as_bytes().first() {
                Some(b'+') => (Kind::Add, &l[1..]),
                Some(b'-') => (Kind::Del, &l[1..]),
                Some(b' ') => (Kind::Ctx, &l[1..]),
                _ => (Kind::Ctx, l),
            };
            b.line(kind, content, true, &mut old, &mut new);
        }
    }
}

impl DiffDoc {
    /// Prefers the pre-split `hunks`, else parses the unified `text`.
    fn of(n: &SNode) -> DiffDoc {
        let mut b = DiffBuilder::default();
        let hunks = n.arr("hunks");
        if hunks.is_empty() {
            from_unified(&mut b, n.text());
        } else {
            from_hunks(&mut b, hunks);
        }
        b.finish()
    }

    fn text_of(&self, r: &DRow) -> &str {
        self.text
            .get(r.a as usize..r.b as usize)
            .unwrap_or_default()
    }
}

struct DiffRows<'a> {
    doc: &'a DiffDoc,
    ink: &'a super::style::Ink,
}

impl<'a> DiffRows<'a> {
    fn row(&self, i: usize) -> Row<'a> {
        let ink = self.ink;
        let Some(r) = self.doc.rows.get(i) else {
            return Row {
                num: None,
                marker: None,
                text: "",
                color: ink.text(),
                fill: None,
                gutter_fill: None,
                bar: None,
                num_color: ink.faint(),
            };
        };
        let text = self.doc.text_of(r);
        let tinted = |c: Color32, marker: char| Row {
            num: r.num,
            marker: Some((marker, c)),
            text,
            color: ink.strong(),
            fill: Some(ink.wash(c)),
            gutter_fill: Some(mix(ink.wash(c), c, 0.22)),
            bar: Some(c),
            num_color: mix(ink.dim(), c, 0.65),
        };
        match r.kind {
            Kind::Add => tinted(ink.p.ok, '+'),
            Kind::Del => tinted(ink.p.danger, '-'),
            Kind::Ctx => Row {
                num: r.num,
                marker: None,
                text,
                color: mix(ink.text(), ink.dim(), 0.25),
                fill: None,
                gutter_fill: None,
                bar: None,
                num_color: ink.faint(),
            },
            Kind::Hunk => Row {
                num: None,
                marker: None,
                text,
                color: ink.dim(),
                fill: Some(mix(ink.card(), ink.p.accent, 0.08)),
                gutter_fill: None,
                bar: None,
                num_color: ink.faint(),
            },
            Kind::Meta => Row {
                num: None,
                marker: None,
                text,
                color: ink.dim(),
                fill: None,
                gutter_fill: None,
                bar: None,
                num_color: ink.faint(),
            },
        }
    }
}

/// Kind `diff`: a unified diff with add/delete washes.
pub fn show_diff(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let doc = cached(
        ui.ctx(),
        Id::unique(("chat-diff", n.id.as_str())),
        n.gen_,
        || DiffDoc::of(n),
    );
    if doc.rows.is_empty() {
        return;
    }
    let (count, foot) = clamp(doc.rows.len(), None, false);
    let rows = DiffRows {
        doc: &doc,
        ink: cx.ink,
    };
    let row = |i: usize| rows.row(i);
    let copy = || {
        let mut s = String::new();
        for r in &doc.rows {
            match r.kind {
                Kind::Add => s.push('+'),
                Kind::Del => s.push('-'),
                Kind::Ctx => s.push(' '),
                _ => {}
            }
            s.push_str(doc.text_of(r));
            s.push('\n');
        }
        s
    };
    let spec = Spec {
        key: &n.id,
        count,
        row: &row,
        digits: doc.digits,
        marker: true,
        wrap: false,
        max_chars: doc.max_chars,
        header: None,
        foot,
        copy: &copy,
    };
    block(cx, ui, &spec);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn doc(src: &str) -> DiffDoc {
        let mut b = DiffBuilder::default();
        from_unified(&mut b, src);
        b.finish()
    }

    fn kinds(d: &DiffDoc) -> Vec<Kind> {
        d.rows.iter().map(|r| r.kind).collect()
    }

    fn nums(d: &DiffDoc) -> Vec<Option<u32>> {
        d.rows.iter().map(|r| r.num).collect()
    }

    #[test]
    fn digit_counts() {
        assert_eq!(digits(0), 1);
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(99), 2);
        assert_eq!(digits(100), 3);
        assert_eq!(digits(u32::MAX), 10);
    }

    #[test]
    fn the_gutter_is_as_wide_as_its_largest_line_number() {
        assert_eq!(gutter_digits(1, 9, None), 1);
        assert_eq!(gutter_digits(1, 10, None), 2);
        assert_eq!(gutter_digits(98, 5, None), 3);
        assert_eq!(gutter_digits(1, 3, Some(1200)), 4);
        assert_eq!(gutter_digits(1, 1, None), 1);
        assert_eq!(gutter_digits(1, 0, None), 1);
    }

    #[test]
    fn code_is_split_into_lines_tabs_expanded_and_glyphs_dropped() {
        let d = CodeDoc::new("fn a() {\n\tlet x\u{f121} = 1;\r\n}\n");
        assert_eq!(d.lines.len(), 3);
        assert_eq!(d.line(0), "fn a() {");
        assert_eq!(d.line(1), "    let x = 1;");
        assert_eq!(d.line(2), "}");
        assert_eq!(d.max_chars, 14);
        assert_eq!(CodeDoc::new("").lines.len(), 0);
        assert_eq!(CodeDoc::new("\n").lines.len(), 1);
        assert_eq!(CodeDoc::new("a\n\n").lines.len(), 2);
        assert_eq!(CodeDoc::new("a\n\n").line(1), "");
    }

    #[test]
    fn preview_clamp_counts_hidden_lines() {
        let pv = |lines| Some(ansi::Preview { lines, tail: false });
        assert_eq!(clamp(30, pv(12), false), (12, Foot::More(18)));
        assert_eq!(clamp(30, pv(12), true), (30, Foot::Less));
        assert_eq!(clamp(5, pv(12), false), (5, Foot::None));
        assert_eq!(clamp(30, None, false), (30, Foot::None));
        assert_eq!(clamp(MAX_ROWS + 5, None, false), (MAX_ROWS, Foot::Cut(5)));
    }

    #[test]
    fn diff_lines_are_classified() {
        assert_eq!(classify("@@ -1,2 +1,3 @@ fn x", false), Kind::Hunk);
        assert_eq!(classify("diff --git a/x b/x", true), Kind::Meta);
        assert_eq!(classify("index 12ab..34cd 100644", false), Kind::Meta);
        assert_eq!(classify("--- a/x", false), Kind::Meta);
        assert_eq!(classify("+++ b/x", false), Kind::Meta);
        assert_eq!(classify("\\ No newline at end of file", true), Kind::Meta);
        assert_eq!(classify("+added", true), Kind::Add);
        assert_eq!(classify("-removed", true), Kind::Del);
        assert_eq!(classify(" context", true), Kind::Ctx);
        assert_eq!(classify("", true), Kind::Ctx);
        // Inside a hunk a line that looks like a header is content.
        assert_eq!(classify("--- a rule", true), Kind::Del);
        assert_eq!(classify("+++ b", true), Kind::Add);
    }

    #[test]
    fn hunk_headers_parse() {
        assert_eq!(parse_hunk("@@ -10,3 +12,4 @@ fn x"), Some((10, 3, 12, 4)));
        assert_eq!(parse_hunk("@@ -1 +1 @@"), Some((1, 1, 1, 1)));
        assert_eq!(parse_hunk("@@ nonsense @@"), None);
    }

    #[test]
    fn unified_text_is_numbered_by_its_hunk_headers() {
        let d = doc(
            "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -10,3 +10,4 @@ fn x\n keep\n-old\n+new\n+extra\n tail\n\\ No newline at end of file\n",
        );
        use Kind::*;
        assert_eq!(
            kinds(&d),
            [Meta, Meta, Meta, Meta, Hunk, Ctx, Del, Add, Add, Ctx, Meta]
        );
        assert_eq!(
            nums(&d),
            [
                None,
                None,
                None,
                None,
                None,
                Some(10),
                Some(11),
                Some(11),
                Some(12),
                Some(13),
                None
            ]
        );
        assert_eq!(d.text_of(&d.rows[5]), "keep");
        assert_eq!(d.text_of(&d.rows[6]), "old");
        assert_eq!(d.digits, 2);
    }

    #[test]
    fn a_second_file_after_a_finished_hunk_has_its_own_headers() {
        let d =
            doc("--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n--- a/b\n+++ b/b\n@@ -5 +5 @@\n-c\n+d\n");
        use Kind::*;
        assert_eq!(
            kinds(&d),
            [Meta, Meta, Hunk, Del, Add, Meta, Meta, Hunk, Del, Add]
        );
        assert_eq!(d.rows[8].num, Some(5));
    }

    #[test]
    fn a_diff_without_hunk_headers_has_no_numbers() {
        let d = doc("-a\n+b\n c\n");
        assert_eq!(kinds(&d), [Kind::Del, Kind::Add, Kind::Ctx]);
        assert_eq!(nums(&d), [None, None, None]);
        assert_eq!(d.digits, 0);
    }

    #[test]
    fn hunks_prop_numbers_both_sides_and_separates_hunks() {
        let hunks = json!([
            {"oldStart": 5, "newStart": 5, "lines": [" a", "-b", "+c"]},
            {"oldStart": 20, "newStart": 21, "lines": ["+z", "tab\tend"]},
        ]);
        let mut b = DiffBuilder::default();
        from_hunks(&mut b, hunks.as_array().map_or(&[][..], Vec::as_slice));
        let d = b.finish();
        use Kind::*;
        assert_eq!(kinds(&d), [Ctx, Del, Add, Hunk, Add, Ctx]);
        assert_eq!(
            nums(&d),
            [Some(5), Some(6), Some(6), None, Some(21), Some(22)]
        );
        assert_eq!(d.text_of(&d.rows[3]), "@@ -20 +21 @@");
        assert_eq!(d.text_of(&d.rows[5]), "tab\tend".replace('\t', "    "));
        assert_eq!(d.digits, 2);
    }
}
