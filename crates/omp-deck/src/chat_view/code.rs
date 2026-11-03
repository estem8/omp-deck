//! `code` and `diff` nodes: framed mono blocks with an optional line-number gutter, tone-washed
//! lines, syntax colours, a hover-revealed Copy button, the "N more lines" preview clamp and
//! horizontal scrolling only when a line outruns the block. Both kinds share [`block`]; they differ
//! in the rows they feed it. A diff draws unified (one pane) or split (old and new side by side, a
//! row is two panes) and wraps instead of scrolling when split.

use std::sync::Arc;
use std::time::Duration;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Color32, CornerRadius, FontId, Id, Painter, Rect, Sense, Stroke, Ui, UiBuilder, pos2,
    vec2,
};
use serde_json::Value;

use super::ansi;
use super::ctx::{Cx, RowEnds};
use super::glyphs;
use super::highlight::{self, Hl, Source, Span, TokColors};
use super::paint;
use super::snap::SNode;
use super::style::{Ink, LABEL};
use super::text;
use super::tsel::{Lead, RunOpts};
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
/// Gap between the panes of a split diff.
const DIVIDER: f32 = 1.0;
/// Columns each pane of a split diff needs before `auto` picks split.
const SPLIT_COLS: f32 = 60.0;

// ---- shared block ---------------------------------------------------------------------------

/// One drawn pane of a row: a unified row is one, a split row is two.
struct Pane<'a> {
    num: Option<u32>,
    marker: Option<(char, Color32)>,
    /// The `+`/`-`/space a unified diff row starts with in a copy (it is painted apart).
    lead: Option<char>,
    text: &'a str,
    /// Syntax tokens of `text` (`None`: plain).
    spans: Option<&'a [Span]>,
    color: Color32,
    /// Wash across the whole pane.
    fill: Option<Color32>,
    /// Tint of the gutter cell.
    gutter_fill: Option<Color32>,
    /// 2 px bar on the left edge.
    bar: Option<Color32>,
    num_color: Color32,
}

impl<'a> Pane<'a> {
    fn plain(text: &'a str, color: Color32, num_color: Color32) -> Pane<'a> {
        Pane {
            num: None,
            marker: None,
            lead: None,
            text,
            spans: None,
            color,
            fill: None,
            gutter_fill: None,
            bar: None,
            num_color,
        }
    }
}

/// One drawn row. In a split block `b` is the right pane; a split row without one is a banner (hunk
/// header, file header) across the full width.
struct Row<'a> {
    a: Pane<'a>,
    b: Option<Pane<'a>>,
}

impl<'a> Row<'a> {
    fn one(a: Pane<'a>) -> Row<'a> {
        Row { a, b: None }
    }

    fn panes(&self) -> impl Iterator<Item = (usize, &Pane<'a>)> {
        std::iter::once(&self.a).chain(self.b.as_ref()).enumerate()
    }
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

/// The band above the rows.
struct Header<'a> {
    label: &'a str,
    /// Lines added and removed.
    stats: Option<(usize, usize)>,
    /// A unified ⇄ split switch showing this state (`true`: split).
    split: Option<bool>,
}

struct Spec<'a> {
    key: &'a str,
    /// Generation of the rows' text (caches of laid-out heights are valid for it).
    gen_: u64,
    count: usize,
    row: &'a dyn Fn(usize) -> Row<'a>,
    /// Digits of the gutter's numbers (0 = no gutter).
    digits: usize,
    marker: bool,
    wrap: bool,
    /// Two panes per row; rows wrap (a split block never scrolls sideways).
    split: bool,
    /// Longest line in chars (monospace: its width).
    max_chars: usize,
    header: Option<Header<'a>>,
    foot: Foot,
    copy: &'a dyn Fn() -> String,
    colors: &'a TokColors,
}

/// What a frame of a block reports.
#[derive(Default)]
struct Out {
    /// The footer button was clicked.
    foot: bool,
    /// The mode switch was clicked: the mode it asks for (`true`: split).
    split: Option<bool>,
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

/// Fonts and cell sizes of a block.
struct Metrics {
    font: FontId,
    num_font: FontId,
    cw: f32,
    ncw: f32,
    line_h: f32,
}

fn metrics(ink: &Ink, painter: &Painter) -> Metrics {
    let font = {
        let f = ink.mono(0.95);
        if f.size < LABEL {
            ink.mono_px(LABEL)
        } else {
            f
        }
    };
    let num_font = ink.mono_px((font.size * 0.88).max(LABEL));
    let cell = |f: &FontId| {
        painter
            .layout_no_wrap("0".to_owned(), f.clone(), Color32::WHITE)
            .size()
            .x
    };
    Metrics {
        cw: cell(&font),
        ncw: cell(&num_font),
        line_h: (font.size * 1.5).round(),
        font,
        num_font,
    }
}

/// Width a block takes in `ui`.
fn block_width(ui: &Ui) -> f32 {
    let avail = ui.available_width();
    if avail.is_finite() {
        avail.max(120.0)
    } else {
        520.0
    }
}

/// Width of the gutter column for `digits` digits (0: none).
fn gutter_w(m: &Metrics, digits: usize) -> f32 {
    if digits > 0 {
        digits as f32 * m.ncw + 2.0 * GUT_PAD
    } else {
        0.0
    }
}

/// Space between a pane's gutter/marker column(s) (`lead` wide) and its text.
fn text_pad(lead: f32) -> f32 {
    if lead > 0.0 { 8.0 } else { PAD_X }
}

/// Width of one pane of a block `w` wide.
fn pane_width(w: f32, split: bool) -> f32 {
    if split {
        ((w - 2.0 - DIVIDER) / 2.0).floor()
    } else {
        w - 2.0
    }
}

/// Width available to the text of a pane `pane_w` wide with `lead` px of gutter and marker.
fn text_width(pane_w: f32, lead: f32) -> f32 {
    (pane_w - lead - text_pad(lead) - PAD_X).max(40.0)
}

/// Whether the panes of a split diff `w` wide (gutter `lead` px, cell `cw` px) hold at least
/// [`SPLIT_COLS`] columns each.
fn fits_split(w: f32, lead: f32, cw: f32) -> bool {
    (pane_width(w, true) - lead - text_pad(lead) - PAD_X) / cw >= SPLIT_COLS
}

/// Leading spaces a wrapped line hangs its continuation by (none when they would take more than
/// a third of the `width`).
fn hang_of(text: &str, width: f32, cw: f32) -> usize {
    let n = text.len() - text.trim_start_matches(' ').len();
    if (n as f32) * cw * 3.0 > width { 0 } else { n }
}

/// The job of one pane and the indent in px it is drawn at. A wrapped row's leading spaces are
/// that indent rather than text, so its continuation lines hang at the same column.
fn row_job(
    p: &Pane,
    font: &FontId,
    cw: f32,
    colors: &TokColors,
    line_h: f32,
    wrap: Option<f32>,
) -> (LayoutJob, f32) {
    let fmt = TextFormat {
        font_id: font.clone(),
        color: p.color,
        line_height: Some(line_h),
        valign: Align::Center,
        ..TextFormat::default()
    };
    let mut job = LayoutJob::default();
    let hang = wrap.map_or(0, |w| hang_of(p.text, w, cw));
    highlight::push_line_after(&mut job, p.text, p.spans, hang, colors, &fmt);
    let indent = hang as f32 * cw;
    if let Some(w) = wrap {
        job.wrap.max_width = w - indent;
        job.wrap.break_anywhere = true;
    }
    (job, indent)
}

/// Wrapping rows laid out exactly per height pass; past them a row's height is its column count's.
const LAYOUT_ROWS: usize = 300;

/// The chars of `text` when it may not fit one line of `cols` monospace cells (`None`: it surely
/// does). A non-ASCII char may be two cells wide.
fn may_wrap(text: &str, cols: usize) -> Option<usize> {
    let (chars, cells) = if text.is_ascii() {
        (text.len(), text.len())
    } else {
        let n = text.chars().count();
        (n, n * 2)
    };
    (cells > cols).then_some(chars)
}

/// Heights of the wrapped rows of a block, as prefix sums (`prefix[i]` is the top of row `i`).
struct Heights {
    prefix: Arc<[f32]>,
}

/// What a [`Heights`] was laid out for: generation, rows, text width, font size, split.
type HeightsKey = (u64, usize, u32, u32, bool);

/// The heights cached under `id` in egui's memory, laid out by `build` when `key` changed.
fn heights(ui: &Ui, id: Id, key: HeightsKey, build: impl FnOnce() -> Vec<f32>) -> Arc<Heights> {
    if let Some((k, h)) = ui.data(|d| d.get_temp::<(HeightsKey, Arc<Heights>)>(id))
        && k == key
    {
        return h;
    }
    let h = Arc::new(Heights {
        prefix: build().into(),
    });
    ui.data_mut(|d| d.insert_temp(id, (key, Arc::clone(&h))));
    h
}

/// The mode switch of a diff header: `Unified | Split`, the active one raised. Returns the mode
/// clicked (`true`: split).
fn mode_switch(ui: &mut Ui, ink: &Ink, id: Id, right: f32, cy: f32, split: bool) -> Option<bool> {
    const H: f32 = 20.0;
    const PAD: f32 = 8.0;
    let font = ink.label(LABEL);
    let painter = ui.painter().clone();
    let cells: [(bool, Arc<egui::Galley>); 2] =
        [(false, "Unified"), (true, "Split")].map(|(m, t)| {
            (
                m,
                painter.layout_no_wrap(t.to_owned(), font.clone(), ink.dim()),
            )
        });
    let total: f32 = cells.iter().map(|(_, g)| g.size().x + 2.0 * PAD).sum();
    let rect = Rect::from_min_size(pos2(right - total, cy - H / 2.0), vec2(total, H));
    paint::boxed(
        &painter,
        rect,
        5,
        mix(ink.card(), ink.p.bg, 0.45),
        Stroke::new(1.0, ink.line()),
    );
    let mut x = rect.left();
    let mut clicked = None;
    for (mode, g) in cells {
        let cell = Rect::from_min_size(pos2(x, rect.top()), vec2(g.size().x + 2.0 * PAD, H));
        x = cell.right();
        let resp = ui
            .interact(cell, id.with(mode), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let active = mode == split;
        let color = if active {
            ink.strong()
        } else if resp.hovered() {
            ink.text()
        } else {
            ink.dim()
        };
        if active {
            paint::boxed(
                &painter,
                cell.shrink(2.0),
                3,
                ink.wash(ink.a),
                Stroke::new(1.0, ink.edge(ink.a)),
            );
        }
        painter.galley(
            pos2(
                cell.center().x - g.size().x / 2.0,
                cell.center().y - g.size().y / 2.0,
            ),
            g,
            color,
        );
        if resp.clicked() && !active {
            clicked = Some(mode);
        }
    }
    clicked
}

/// Rows of a block with `count` rows that can touch `clip` for a body starting at `top`; wrapped
/// blocks carry the tops of their rows in `prefix` (`count + 1` entries).
fn visible(
    count: usize,
    prefix: Option<&[f32]>,
    clip: Rect,
    top: f32,
    line_h: f32,
) -> (usize, usize) {
    if let Some(p) = prefix {
        let (lo, hi) = (clip.top() - top, clip.bottom() - top);
        let first = p.partition_point(|&y| y <= lo).saturating_sub(1).min(count);
        let last = p.partition_point(|&y| y < hi).min(count);
        return (first, last.max(first));
    }
    let first = ((clip.top() - top) / line_h).floor().max(0.0) as usize;
    let last = ((clip.bottom() - top) / line_h).ceil().max(0.0) as usize;
    (first.min(count), last.min(count))
}

/// Fill of a block's body.
fn block_fill(ink: &Ink) -> Color32 {
    mix(ink.card(), ink.p.bg, 0.3)
}

/// Draws `s` and reports what was clicked.
fn block(cx: &Cx, ui: &mut Ui, s: &Spec) -> Out {
    let ink = cx.ink;
    let painter = ui.painter().clone();
    let m = metrics(ink, &painter);
    let line_h = m.line_h;
    let w = block_width(ui);

    let gutter_w = gutter_w(&m, s.digits);
    let marker_w = if s.marker { m.cw + 8.0 } else { 0.0 };
    let lead = gutter_w + marker_w;
    let text_pad = text_pad(lead);
    let pane_w = pane_width(w, s.split);
    let text_w = text_width(pane_w, lead);
    let banner_w = (w - 2.0 - 2.0 * PAD_X).max(40.0);
    let wrap = s.wrap || s.split;
    let id = ui.scope_id().with(("chat-block", s.key));
    // Text width of pane `k` of `r`.
    let width_of = |r: &Row, k: usize| {
        if s.split && k == 0 && r.b.is_none() {
            banner_w
        } else {
            text_w
        }
    };

    let heights = wrap.then(|| {
        let key = (
            s.gen_,
            s.count,
            (text_w + w).to_bits(),
            m.font.size.to_bits(),
            s.split,
        );
        heights(ui, id.with("heights"), key, || {
            let mut prefix = Vec::with_capacity(s.count + 1);
            let mut y = 0.0;
            prefix.push(y);
            let mut laid = 0usize;
            for i in 0..s.count {
                let r = (s.row)(i);
                let mut h = line_h;
                for (k, p) in r.panes() {
                    let width = width_of(&r, k);
                    let hang = hang_of(p.text, width, m.cw);
                    let cols = (((width / m.cw).floor() as usize).saturating_sub(hang)).max(1);
                    // Most rows are one line: they are not laid out at all.
                    let Some(chars) = may_wrap(&p.text[hang..], cols) else {
                        continue;
                    };
                    if laid < LAYOUT_ROWS {
                        laid += 1;
                        let (job, _) = row_job(p, &m.font, m.cw, s.colors, line_h, Some(width));
                        h = h.max(painter.layout_job(job).size().y);
                    } else {
                        h = h.max(chars.div_ceil(cols) as f32 * line_h);
                    }
                }
                y += h;
                prefix.push(y);
            }
            prefix
        })
    });
    let prefix = heights.as_ref().map(|h| &h.prefix[..]);
    let row_top = |i: usize| prefix.map_or(i as f32 * line_h, |p| p[i]);
    let row_h = |i: usize| row_top(i + 1) - row_top(i);
    let rows_h = row_top(s.count);

    let header_h = if s.header.is_some() { HEADER_H } else { 0.0 };
    let foot_h = if s.foot == Foot::None { 0.0 } else { FOOT_H };
    let content_w = s.max_chars as f32 * m.cw + 2.0;
    let scroll = !wrap && content_w > text_w + 0.5;
    let total_h = 2.0 + header_h + 2.0 * PAD_Y + rows_h + foot_h;

    let (rect, _) = ui.allocate_exact_size(vec2(w, total_h), Sense::hover());
    // Where the rows sit, for a tool body that clamps between them.
    let ends = match &heights {
        Some(h) => RowEnds::Prefix(Arc::clone(&h.prefix)),
        None => RowEnds::Even {
            pitch: line_h,
            count: s.count,
        },
    };
    cx.note_rows(rect.top() + 1.0 + header_h + PAD_Y, ends);
    // Every row (pane) is a run, painted or not.
    let seq0 = cx.seqs(
        ui,
        u32::try_from(s.count * if s.split { 2 } else { 1 }).unwrap_or(u32::MAX),
    );
    let mut out = Out::default();
    if !cx.visible(ui, rect) {
        return out;
    }
    let fill = block_fill(ink);
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

    // Header: what the block comes from, how much changed, the mode switch.
    if let Some(h) = &s.header {
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
        let mut x = rect.left() + PAD_X;
        let mut put = |text: String, color: Color32, gap: f32| {
            let g = painter.layout_no_wrap(text, ink.label(12.0), color);
            painter.galley(
                pos2(x, band.center().y - g.size().y / 2.0),
                g.clone(),
                color,
            );
            x += g.size().x + gap;
        };
        if let Some((added, removed)) = h.stats {
            put(format!("+{added}"), ink.p.ok, 7.0);
            put(format!("\u{2212}{removed}"), ink.p.danger, 12.0);
        }
        if !h.label.is_empty() {
            put(h.label.to_owned(), ink.dim(), 0.0);
        }
        if let Some(split) = h.split {
            // Left of the Copy button.
            let right = rect.right() - 6.0 - 26.0 - 8.0;
            out.split = mode_switch(ui, ink, id.with("mode"), right, band.center().y, split);
        }
    }

    let gx0 = rect.left() + 1.0;
    let body_top = rect.top() + 1.0 + header_h;
    let body_bot = rect.bottom() - 1.0 - foot_h;
    let y0 = body_top + PAD_Y;
    let clip = painter.clip_rect();
    let panes = if s.split { 2 } else { 1 };
    // Left edge of pane `k`.
    let pane_x = |k: usize| gx0 + k as f32 * (pane_w + DIVIDER);

    // Gutter columns (and the divider of a split block).
    if gutter_w > 0.0 {
        for k in 0..panes {
            let g = Rect::from_min_max(
                pos2(pane_x(k), body_top),
                pos2(pane_x(k) + gutter_w, body_bot),
            );
            painter.rect_filled(
                g,
                CornerRadius {
                    nw: if header_h == 0.0 && k == 0 { r_in } else { 0 },
                    sw: if foot_h == 0.0 && k == 0 { r_in } else { 0 },
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
    }
    if s.split {
        painter.vline(
            pane_x(1) - DIVIDER / 2.0,
            body_top..=body_bot,
            Stroke::new(1.0, ink.line()),
        );
    }

    // Row backdrops, numbers and markers.
    let (first, last) = visible(s.count, prefix, clip, y0, line_h);
    for i in first..last {
        let (y, rh) = (y0 + row_top(i), row_h(i));
        let r = (s.row)(i);
        for (k, p) in r.panes() {
            let banner = s.split && r.b.is_none();
            let (x, pw) = if banner {
                (gx0, w - 2.0)
            } else {
                (pane_x(k), pane_w)
            };
            if let Some(f) = p.fill {
                painter.rect_filled(Rect::from_min_size(pos2(x, y), vec2(pw, rh)), 0.0, f);
            }
            if banner {
                continue;
            }
            if let (Some(f), true) = (p.gutter_fill, gutter_w > 0.0) {
                painter.rect_filled(Rect::from_min_size(pos2(x, y), vec2(gutter_w, rh)), 0.0, f);
            }
            if let Some(b) = p.bar {
                painter.rect_filled(Rect::from_min_size(pos2(x, y), vec2(2.0, rh)), 0.0, b);
            }
            if let (Some(num), true) = (p.num, gutter_w > 0.0) {
                let g = painter.layout_no_wrap(num.to_string(), m.num_font.clone(), p.num_color);
                let at = pos2(
                    x + gutter_w - GUT_PAD - g.size().x,
                    y + (line_h - g.size().y) / 2.0,
                );
                painter.galley(at, g, p.num_color);
            }
            if let Some((ch, c)) = p.marker {
                let g = painter.layout_no_wrap(ch.to_string(), m.font.clone(), c);
                let at = pos2(x + gutter_w + 4.0, y + (line_h - g.size().y) / 2.0);
                painter.galley(at, g, c);
            }
        }
    }

    // The text itself: selectable per row and pane; one pane scrolls sideways when a line is too
    // long. `base` is the left edge of the first pane.
    let right = rect.right() - 1.0 - if scroll { 2.0 } else { PAD_X };
    let text_rect = Rect::from_min_max(pos2(gx0 + lead + text_pad, y0), pos2(right, y0 + rows_h));
    let draw_text = |ui: &mut Ui, base: f32, top: f32| {
        let clip = ui.clip_rect();
        let (first, last) = visible(s.count, prefix, clip, top, line_h);
        for i in first..last {
            let y = top + row_top(i);
            let r = (s.row)(i);
            for (k, p) in r.panes() {
                // Column-major: a selection inside one side of a split diff stays on that side.
                let seq = seq0 + (k * s.count + i) as u32;
                let lane = if s.split && r.b.is_some() {
                    k as u8 + 1
                } else {
                    0
                };
                let opts = RunOpts::LINE.lane(lane);
                // A copy reads the row's text; it needs no layout.
                if cx.collecting() {
                    cx.text_plain(
                        ui,
                        seq,
                        p.text,
                        opts.lead(p.lead.map_or(Lead::None, Lead::Diff)),
                    );
                    continue;
                }
                let banner = s.split && r.b.is_none();
                let x = if banner {
                    base + PAD_X
                } else {
                    base + k as f32 * (pane_w + DIVIDER) + lead + text_pad
                };
                let wrap_at = wrap.then(|| width_of(&r, k));
                let (job, indent) = row_job(p, &m.font, m.cw, s.colors, line_h, wrap_at);
                let galley = ui.painter().layout_job(job);
                // Top-aligned: the other pane of a split row may be taller.
                let pos = pos2(x + indent, y);
                let hang = wrap_at.map_or(0, |w| hang_of(p.text, w, m.cw));
                cx.text_run(ui, seq, &galley, pos, p.color, opts.skip(hang));
            }
        }
    };
    let mut child = ui.new_child(UiBuilder::new().max_rect(text_rect));
    if scroll {
        egui::ScrollArea::horizontal()
            .id_salt(id.with("scroll"))
            .auto_shrink([false, false])
            .show(&mut child, |ui| {
                let (c, _) = ui.allocate_exact_size(vec2(content_w, rows_h), Sense::hover());
                draw_text(ui, c.min.x - lead - text_pad, c.min.y);
            });
    } else {
        draw_text(&mut child, gx0, y0);
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
            out.foot = ansi::more_button_at(cx, ui, b, id.with("more"), &label, up).clicked();
        } else {
            painter.galley(
                pos2(fr.left() + PAD_X, fr.center().y - g.size().y / 2.0),
                g,
                ink.faint(),
            );
        }
    }
    out
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

/// The highlight of `key` (a result of an earlier generation while the worker catches up; `None`
/// before the first, and always for a text with no language hint).
fn highlighted(
    cx: &Cx,
    ui: &Ui,
    key: &str,
    gen_: u64,
    hint: (&str, &str),
    source: impl FnOnce() -> Source,
) -> Option<Hl> {
    if hint.0.is_empty() && hint.1.is_empty() {
        return None;
    }
    cx.loc(|l| l.hl.get(ui.ctx(), key, gen_, source))
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

    fn source(&self, lang: &str, path: &str) -> Source {
        Source {
            lang: lang.to_owned(),
            path: path.to_owned(),
            lines: (0..self.lines.len())
                .map(|i| self.line(i).to_owned())
                .collect(),
            resets: Vec::new(),
        }
    }
}

struct CodeRows<'a> {
    doc: &'a CodeDoc,
    n: &'a SNode,
    ink: &'a Ink,
    hl: Option<&'a Hl>,
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
        let line = self.doc.line(i);
        Row::one(Pane {
            num: self.numbers.then_some(label),
            spans: self.hl.and_then(|h| h.line(i, line)),
            fill: mark.map(|c| ink.wash(c)),
            gutter_fill: mark.map(|c| mix(ink.wash(c), c, 0.2)),
            bar: mark,
            ..Pane::plain(
                line,
                ink.text(),
                mark.map_or_else(|| ink.faint(), |c| mix(ink.dim(), c, 0.6)),
            )
        })
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
    let path = n.str("path").filter(|p| !p.is_empty());
    let header = path
        .filter(|_| !ansi::inside(cx, n, "tool"))
        .map(|label| Header {
            label,
            stats: None,
            split: None,
        });
    let lang = n.str("lang").unwrap_or("");
    let hl = highlighted(cx, ui, &n.id, n.gen_, (lang, path.unwrap_or("")), || {
        doc.source(lang, path.unwrap_or(""))
    });
    let colors = highlight::colors(ui.ctx(), ink, block_fill(ink));
    let rows = CodeRows {
        doc: &doc,
        n,
        ink,
        hl: hl.as_ref(),
        start,
        numbers,
    };
    let row = |i: usize| rows.row(i);
    let copy = || text::clean(n.text()).into_owned();
    let spec = Spec {
        key: &n.id,
        gen_: n.gen_,
        count,
        row: &row,
        digits: if numbers {
            gutter_digits(start, total, nums_max)
        } else {
            0
        },
        marker: false,
        wrap: n.flag("wrap"),
        split: false,
        max_chars: doc.max_chars,
        header,
        foot,
        copy: &copy,
        colors: &colors,
    };
    if block(cx, ui, &spec).foot {
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
    /// File headers, `index …`.
    Meta,
    /// `\ No newline at end of file`: a note on the line before it.
    NoEol,
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
    if line.starts_with('\\') {
        return Kind::NoEol;
    }
    if META.iter().any(|m| line.starts_with(m)) {
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
    /// Number shown in a unified diff (the new line's, the old one's for a deletion).
    num: Option<u32>,
    /// Number of the line on the old side (deletions and context).
    old: Option<u32>,
    a: u32,
    b: u32,
    /// Index of the line in the old text (deletions, context) and in the new text (additions,
    /// context): what the highlighter parses.
    o: u32,
    n: u32,
}

/// One row of a split diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitRow {
    /// Hunk and file headers across both panes (index of the unified row).
    Banner(usize),
    /// Old and new line side by side; a side without a line is a blank pad.
    Pair(Option<usize>, Option<usize>),
}

/// Aligns the rows of a unified diff into side-by-side rows: context lines sit on both sides, and
/// a run of deletions and additions pairs up in order with blanks padding the shorter side.
fn split_rows(rows: &[DRow]) -> Vec<SplitRow> {
    let mut out = Vec::with_capacity(rows.len());
    let mut i = 0;
    while i < rows.len() {
        match rows[i].kind {
            Kind::Hunk | Kind::Meta => {
                out.push(SplitRow::Banner(i));
                i += 1;
            }
            // `\ No newline at end of file` annotates the row before it; a side-by-side row has
            // no room for it and it must not split a run of changes.
            Kind::NoEol => i += 1,
            Kind::Ctx => {
                out.push(SplitRow::Pair(Some(i), Some(i)));
                i += 1;
            }
            Kind::Add | Kind::Del => {
                let (mut dels, mut adds) = (Vec::new(), Vec::new());
                while i < rows.len() && matches!(rows[i].kind, Kind::Add | Kind::Del | Kind::NoEol)
                {
                    match rows[i].kind {
                        Kind::Del => dels.push(i),
                        Kind::Add => adds.push(i),
                        _ => {}
                    }
                    i += 1;
                }
                for k in 0..dels.len().max(adds.len()) {
                    out.push(SplitRow::Pair(dels.get(k).copied(), adds.get(k).copied()));
                }
            }
        }
    }
    out
}

/// A diff as rows over one owned text.
struct DiffDoc {
    text: String,
    rows: Vec<DRow>,
    split: Vec<SplitRow>,
    max_chars: usize,
    /// Digits of the gutter numbers (0 when no row is numbered).
    digits: usize,
    added: usize,
    removed: usize,
    /// Lines of the old / new text where the parse restarts (each hunk is a fragment).
    old_resets: Vec<u32>,
    new_resets: Vec<u32>,
    /// The highlight cache keys of the old and new text (built once per diff, not per frame).
    keys: [String; 2],
}

#[derive(Default)]
struct DiffBuilder {
    text: String,
    rows: Vec<DRow>,
    max_chars: usize,
    top: u32,
    old_len: u32,
    new_len: u32,
    old_resets: Vec<u32>,
    new_resets: Vec<u32>,
    /// A hunk or file header came since the last line of each text.
    reset_old: bool,
    reset_new: bool,
}

impl DiffBuilder {
    fn push(&mut self, kind: Kind, num: Option<u32>, old: Option<u32>, s: &str) {
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
        self.top = self.top.max(num.unwrap_or(0)).max(old.unwrap_or(0));
        let (mut o, mut n) = (0, 0);
        match kind {
            Kind::Hunk | Kind::Meta => {
                self.reset_old = true;
                self.reset_new = true;
            }
            // Belongs to the line before it: no new line, and the parse carries on.
            Kind::NoEol => {}
            _ => {
                if kind != Kind::Add {
                    o = self.old_len;
                    if std::mem::take(&mut self.reset_old) && o > 0 {
                        self.old_resets.push(o);
                    }
                    self.old_len += 1;
                }
                if kind != Kind::Del {
                    n = self.new_len;
                    if std::mem::take(&mut self.reset_new) && n > 0 {
                        self.new_resets.push(n);
                    }
                    self.new_len += 1;
                }
            }
        }
        self.rows.push(DRow {
            kind,
            num,
            old,
            a,
            b: self.text.len() as u32,
            o,
            n,
        });
    }

    /// An add/delete/context line: numbers it (new numbering, old for deletions) and moves the
    /// counters.
    fn line(&mut self, kind: Kind, content: &str, numbered: bool, old: &mut u32, new: &mut u32) {
        let num = |n: u32| numbered.then_some(n);
        match kind {
            Kind::Add => {
                self.push(kind, num(*new), None, content);
                *new += 1;
            }
            Kind::Del => {
                self.push(kind, num(*old), num(*old), content);
                *old += 1;
            }
            _ => {
                self.push(Kind::Ctx, num(*new), num(*old), content);
                *old += 1;
                *new += 1;
            }
        }
    }

    fn finish(self) -> DiffDoc {
        let count = |k: Kind| self.rows.iter().filter(|r| r.kind == k).count();
        DiffDoc {
            digits: if self.top == 0 { 0 } else { digits(self.top) },
            split: split_rows(&self.rows),
            added: count(Kind::Add),
            removed: count(Kind::Del),
            text: self.text,
            rows: self.rows,
            max_chars: self.max_chars,
            old_resets: self.old_resets,
            new_resets: self.new_resets,
            keys: [String::new(), String::new()],
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
                b.push(Kind::Hunk, None, None, line);
            }
            kind @ (Kind::Meta | Kind::NoEol) => b.push(kind, None, None, line),
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
            b.push(Kind::Hunk, None, None, &format!("@@ -{old} +{new} @@"));
        }
        let lines = h.get("lines").and_then(Value::as_array);
        for l in lines.into_iter().flatten().filter_map(Value::as_str) {
            if l.starts_with('\\') {
                b.push(Kind::NoEol, None, None, l);
                continue;
            }
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

/// Which side of the diff a pane shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    /// The one pane of a unified diff.
    Unified,
    Old,
    New,
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
        let mut doc = b.finish();
        doc.keys = ["old", "new"].map(|side| format!("{}\0{side}", n.id));
        doc
    }

    fn text_of(&self, r: &DRow) -> &str {
        self.text
            .get(r.a as usize..r.b as usize)
            .unwrap_or_default()
    }

    /// The old (`false`) or new text for the highlighter: syntax state runs through the lines of
    /// one side only, so a deleted line never breaks the state of the added ones.
    fn source(&self, lang: &str, path: &str, new: bool) -> Source {
        let on_side = |k: Kind| k == Kind::Ctx || k == if new { Kind::Add } else { Kind::Del };
        Source {
            lang: lang.to_owned(),
            path: path.to_owned(),
            lines: self
                .rows
                .iter()
                .filter(|r| on_side(r.kind))
                .map(|r| self.text_of(r).to_owned())
                .collect(),
            resets: if new {
                self.new_resets.clone()
            } else {
                self.old_resets.clone()
            },
        }
    }
}

struct DiffRows<'a> {
    doc: &'a DiffDoc,
    ink: &'a Ink,
    old: Option<&'a Hl>,
    new: Option<&'a Hl>,
}

impl<'a> DiffRows<'a> {
    /// The pane of content row `r` on `side`.
    fn content(&self, r: &DRow, side: Side) -> Pane<'a> {
        let ink = self.ink;
        let text = self.doc.text_of(r);
        let (hl, idx) = match (side, r.kind) {
            (Side::Old, _) | (Side::Unified, Kind::Del) => (self.old, r.o),
            _ => (self.new, r.n),
        };
        let spans = hl.and_then(|h| h.line(idx as usize, text));
        let num = if side == Side::Old { r.old } else { r.num };
        let marker = |c: Color32, ch: char| (side == Side::Unified).then_some((ch, c));
        let tinted = |c: Color32, ch: char| Pane {
            num,
            marker: marker(c, ch),
            lead: (side == Side::Unified).then_some(ch),
            spans,
            fill: Some(ink.wash(c)),
            gutter_fill: Some(mix(ink.wash(c), c, 0.22)),
            bar: Some(c),
            ..Pane::plain(text, ink.strong(), mix(ink.dim(), c, 0.65))
        };
        match r.kind {
            Kind::Add => tinted(ink.p.ok, '+'),
            Kind::Del => tinted(ink.p.danger, '-'),
            _ => Pane {
                num,
                spans,
                lead: (side == Side::Unified).then_some(' '),
                ..Pane::plain(text, mix(ink.text(), ink.dim(), 0.25), ink.faint())
            },
        }
    }

    /// A hunk or file header.
    fn banner(&self, r: &DRow) -> Pane<'a> {
        let ink = self.ink;
        Pane {
            fill: (r.kind == Kind::Hunk).then(|| mix(ink.card(), ink.p.accent, 0.08)),
            ..Pane::plain(self.doc.text_of(r), ink.dim(), ink.faint())
        }
    }

    /// The side of a split row that has no line.
    fn pad(&self) -> Pane<'a> {
        let ink = self.ink;
        Pane {
            fill: Some(mix(ink.card(), ink.p.bg, 0.55)),
            ..Pane::plain("", ink.text(), ink.faint())
        }
    }

    fn row(&self, i: usize) -> Row<'a> {
        match self.doc.rows.get(i) {
            Some(r) if matches!(r.kind, Kind::Hunk | Kind::Meta | Kind::NoEol) => {
                Row::one(self.banner(r))
            }
            Some(r) => Row::one(self.content(r, Side::Unified)),
            None => Row::one(Pane::plain("", self.ink.text(), self.ink.faint())),
        }
    }

    fn split_row(&self, i: usize) -> Row<'a> {
        let at = |k: Option<usize>, side: Side| match k.and_then(|k| self.doc.rows.get(k)) {
            Some(r) => self.content(r, side),
            None => self.pad(),
        };
        match self.doc.split.get(i) {
            Some(SplitRow::Banner(k)) => match self.doc.rows.get(*k) {
                Some(r) => Row::one(self.banner(r)),
                None => Row::one(self.pad()),
            },
            Some(SplitRow::Pair(l, r)) => Row {
                a: at(*l, Side::Old),
                b: Some(at(*r, Side::New)),
            },
            None => Row::one(self.pad()),
        }
    }
}

/// How a diff node asks to be drawn (`diff.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Unified,
    Split,
    Auto,
}

impl Mode {
    /// An absent or unknown mode is unified (what omp's edit tool sends).
    fn of(s: Option<&str>) -> Mode {
        match s {
            Some("split") => Mode::Split,
            Some("auto") => Mode::Auto,
            _ => Mode::Unified,
        }
    }
}

/// Whether to draw split: the card's own local choice wins, else the node's `mode`, `auto` being
/// split when the panes have room (`fits`).
fn use_split(mode: Mode, local: Option<bool>, fits: bool) -> bool {
    local.unwrap_or(match mode {
        Mode::Split => true,
        Mode::Unified => false,
        Mode::Auto => fits,
    })
}

/// Kind `diff`: a unified or side-by-side diff with add/delete washes and syntax colours.
pub fn show_diff(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let doc = cached(
        ui.ctx(),
        Id::unique(("chat-diff", n.id.as_str())),
        n.gen_,
        || DiffDoc::of(n),
    );
    if doc.rows.is_empty() {
        return;
    }
    let m = metrics(ink, ui.painter());
    let local = cx.loc(|l| l.diff_modes.get(&n.id).copied());
    let split = use_split(
        Mode::of(n.str("mode")),
        local,
        fits_split(block_width(ui), gutter_w(&m, doc.digits), m.cw),
    );

    let path = n.str("path").unwrap_or("");
    let lang = n.str("lang").unwrap_or("");
    let side = |new: bool| {
        highlighted(
            cx,
            ui,
            &doc.keys[usize::from(new)],
            n.gen_,
            (lang, path),
            || doc.source(lang, path, new),
        )
    };
    let (old, new) = (side(false), side(true));
    let colors = highlight::colors(ui.ctx(), ink, block_fill(ink));
    let rows = DiffRows {
        doc: &doc,
        ink,
        old: old.as_ref(),
        new: new.as_ref(),
    };
    let unified = |i: usize| rows.row(i);
    let sided = |i: usize| rows.split_row(i);
    let total = if split {
        doc.split.len()
    } else {
        doc.rows.len()
    };
    let (count, foot) = clamp(total, None, false);
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
    let label = if ansi::inside(cx, n, "tool") {
        ""
    } else {
        path
    };
    let spec = Spec {
        key: &n.id,
        gen_: n.gen_,
        count,
        row: if split { &sided } else { &unified },
        digits: doc.digits,
        marker: !split,
        wrap: true,
        split,
        max_chars: doc.max_chars,
        header: Some(Header {
            label,
            stats: Some((doc.added, doc.removed)),
            split: Some(split),
        }),
        foot,
        copy: &copy,
        colors: &colors,
    };
    if let Some(want) = block(cx, ui, &spec).split {
        cx.loc(|l| l.diff_modes.insert(n.id.clone(), want));
        ui.ctx().request_repaint();
    }
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
        assert_eq!(classify("\\ No newline at end of file", true), Kind::NoEol);
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
            [Meta, Meta, Meta, Meta, Hunk, Ctx, Del, Add, Add, Ctx, NoEol]
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
        assert_eq!((d.added, d.removed), (2, 1));
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

    #[test]
    fn a_no_newline_marker_in_a_hunk_is_a_note_not_a_context_line() {
        let hunks = json!([{"oldStart": 1, "newStart": 1, "lines": ["-a", "\\ No newline at end of file", "+b", "\\ No newline at end of file"]}]);
        let mut b = DiffBuilder::default();
        from_hunks(&mut b, hunks.as_array().map_or(&[][..], Vec::as_slice));
        let d = b.finish();
        assert_eq!(kinds(&d), [Kind::Del, Kind::NoEol, Kind::Add, Kind::NoEol]);
        assert_eq!(d.split, [SplitRow::Pair(Some(0), Some(2))]);
    }

    #[test]
    fn context_lines_carry_their_old_number_for_the_old_pane() {
        let d = doc("@@ -10,3 +12,3 @@\n keep\n-old\n+new\n tail\n");
        // Context: new number 12 on the right, old number 10 on the left.
        assert_eq!((d.rows[1].num, d.rows[1].old), (Some(12), Some(10)));
        assert_eq!((d.rows[2].num, d.rows[2].old), (Some(11), Some(11)));
        assert_eq!((d.rows[3].num, d.rows[3].old), (Some(13), None));
        assert_eq!((d.rows[4].num, d.rows[4].old), (Some(14), Some(12)));
    }

    #[test]
    fn a_split_hunk_with_uneven_runs_aligns_with_blanks() {
        use SplitRow::*;
        // rows: 0 hunk, 1 ctx, 2-4 del x3, 5 add x1, 6 ctx, 7-8 add x2, 9 ctx
        let d = doc("@@ -1,6 +1,6 @@\n a\n-b1\n-b2\n-b3\n+c1\n d\n+e1\n+e2\n f\n");
        assert_eq!(
            d.split,
            [
                Banner(0),
                Pair(Some(1), Some(1)),
                Pair(Some(2), Some(5)),
                Pair(Some(3), None),
                Pair(Some(4), None),
                Pair(Some(6), Some(6)),
                Pair(None, Some(7)),
                Pair(None, Some(8)),
                Pair(Some(9), Some(9)),
            ]
        );
        // Every content row appears exactly once per side it belongs to.
        let mut left: Vec<usize> = d
            .split
            .iter()
            .filter_map(|r| match r {
                Pair(Some(l), _) => Some(*l),
                _ => None,
            })
            .collect();
        left.sort_unstable();
        assert_eq!(left, [1, 2, 3, 4, 6, 9]);
        let right = d
            .split
            .iter()
            .filter(|r| matches!(r, Pair(_, Some(_))))
            .count();
        assert_eq!(right, 6);
    }

    #[test]
    fn additions_before_deletions_still_pair_up_in_order() {
        use SplitRow::*;
        let d = doc("+n1\n+n2\n-o1\n");
        assert_eq!(d.split, [Pair(Some(2), Some(0)), Pair(None, Some(1))]);
    }

    #[test]
    fn no_newline_notes_neither_split_nor_banner_a_side_by_side_diff() {
        use SplitRow::*;
        // rows: 0 hunk, 1 del, 2 note, 3 add, 4 note
        let d = doc(
            "@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n",
        );
        assert_eq!(kinds(&d)[2], Kind::NoEol);
        assert_eq!(d.split, [Banner(0), Pair(Some(1), Some(3))]);
        // After context it is skipped too, and the parse of either side carries on over it.
        let d = doc("@@ -1,3 +1,3 @@\n a\n\\ No newline at end of file\n b\n-c\n+d\n");
        assert_eq!(
            d.split,
            [
                Banner(0),
                Pair(Some(1), Some(1)),
                Pair(Some(3), Some(3)),
                Pair(Some(4), Some(5))
            ]
        );
        assert!(d.old_resets.is_empty() && d.new_resets.is_empty());
        assert_eq!(d.source("rust", "", true).lines, ["a", "b", "d"]);
        // A real file header still resets.
        let d = doc("@@ -1 +1 @@\n-a\n+b\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-c\n+d\n");
        assert_eq!(d.new_resets, [1]);
    }

    #[test]
    fn only_rows_that_may_overflow_their_pane_are_laid_out() {
        assert_eq!(may_wrap("fn main() {}", 40), None);
        assert_eq!(may_wrap(&"x".repeat(40), 40), None);
        assert_eq!(may_wrap(&"x".repeat(41), 40), Some(41));
        // A wide char can take two cells.
        assert_eq!(may_wrap(&"表".repeat(21), 40), Some(21));
        assert_eq!(may_wrap(&"表".repeat(20), 40), None);
    }

    #[test]
    fn a_wrapped_line_hangs_its_continuation_at_its_indentation() {
        assert_eq!(hang_of("    let x", 400.0, 8.0), 4);
        assert_eq!(hang_of("let x", 400.0, 8.0), 0);
        // An indentation past a third of the width is not worth hanging by.
        assert_eq!(hang_of(&format!("{}x", " ".repeat(30)), 400.0, 8.0), 0);

        let ink = crate::chat_view::style::test_ink();
        let colors = TokColors::new(&ink, block_fill(&ink));
        let pane = Pane::plain("    let wrapped = 1;", ink.text(), ink.faint());
        let (job, indent) = row_job(
            &pane,
            &FontId::monospace(12.0),
            8.0,
            &colors,
            18.0,
            Some(300.0),
        );
        assert_eq!(indent, 32.0);
        assert_eq!(job.text, "let wrapped = 1;");
        assert_eq!(job.wrap.max_width, 268.0);
        // Unwrapped rows keep every char where it is.
        let (job, indent) = row_job(&pane, &FontId::monospace(12.0), 8.0, &colors, 18.0, None);
        assert_eq!((indent, job.text.as_str()), (0.0, "    let wrapped = 1;"));
    }

    #[test]
    fn the_highlighter_sees_each_side_as_its_own_text() {
        let d = doc("@@ -1,2 +1,2 @@\n a\n-old\n+new\n@@ -9 +9 @@\n-z\n+y\n");
        let old = d.source("rust", "", false);
        let new = d.source("rust", "", true);
        assert_eq!(old.lines, ["a", "old", "z"]);
        assert_eq!(new.lines, ["a", "new", "y"]);
        // Each hunk is a fragment: the parse restarts at its first line.
        assert_eq!(old.resets, [2]);
        assert_eq!(new.resets, [2]);
        // Rows point at their line of the text they are on.
        assert_eq!((d.rows[2].o, d.rows[3].n), (1, 1));
        assert_eq!((d.rows[5].o, d.rows[6].n), (2, 2));
    }

    #[test]
    fn auto_goes_split_only_when_both_panes_hold_sixty_columns() {
        let (lead, cw) = (40.0, 8.0);
        // Pane text width = (w - 3) / 2 - lead - 8 - 10; sixty columns is 480 px.
        let edge = 2.0 * (60.0 * cw + lead + 8.0 + PAD_X) + 3.0;
        assert!(fits_split(edge, lead, cw));
        assert!(!fits_split(edge - 4.0, lead, cw));
        assert!(!fits_split(880.0, lead, cw), "880 px is 47 columns a pane");
        assert!(fits_split(1100.0, lead, cw));
        // A wider cell needs a wider card.
        assert!(!fits_split(1100.0, lead, 10.0));
    }

    #[test]
    fn the_mode_prop_and_the_local_switch_decide_split() {
        assert_eq!(Mode::of(None), Mode::Unified);
        assert_eq!(Mode::of(Some("split")), Mode::Split);
        assert_eq!(Mode::of(Some("auto")), Mode::Auto);
        assert_eq!(Mode::of(Some("bogus")), Mode::Unified);
        for fits in [false, true] {
            assert!(use_split(Mode::Split, None, fits));
            assert!(!use_split(Mode::Unified, None, fits));
            assert_eq!(use_split(Mode::Auto, None, fits), fits);
            // The card's own switch overrides every mode, `auto` included.
            for mode in [Mode::Unified, Mode::Split, Mode::Auto] {
                assert!(use_split(mode, Some(true), fits));
                assert!(!use_split(mode, Some(false), fits));
            }
        }
    }

    #[test]
    fn visible_rows_follow_the_clip_for_uniform_and_wrapped_heights() {
        let clip = Rect::from_min_max(pos2(0.0, 100.0), pos2(100.0, 160.0));
        // Uniform 20 px rows starting at y = 40: rows 3, 4 and 5 touch 100..160.
        assert_eq!(visible(50, None, clip, 40.0, 20.0), (3, 6));
        // Wrapped: row tops 0, 20, 80, 90, 150, 170 from y = 40; rows 1, 2 and 3 touch the clip.
        let p = [0.0, 20.0, 80.0, 90.0, 150.0, 170.0];
        assert_eq!(visible(5, Some(&p), clip, 40.0, 20.0), (1, 4));
        assert_eq!(visible(5, Some(&p), clip, 400.0, 20.0), (0, 0));
    }
}
