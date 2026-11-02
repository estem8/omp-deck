//! Data-first cards: `tool` (a tool call, F's slanted-header card or an inline head line), `agent`
//! (one subagent, live) and `checklist` (todo list: full / hud / reminder).
//!
//! Everything is drawn from the active [`Ink`]; nothing here owns state beyond egui's temp data
//! (the measured height of a clamped body, a checklist phase's local fold, the nesting depth of
//! tools). Layout jobs are rebuilt per frame from the node's props; egui's galley cache makes
//! that a hash lookup, and the node's own caches (`gen_`) keep the props themselves untouched.

use std::sync::Arc;

use egui::text::LayoutJob;
use egui::{
    Align, Color32, CornerRadius, CursorIcon, Id, Label, Layout, Margin, Pos2, Rect, Response,
    RichText, Sense, Shape, Stroke, StrokeKind, TextWrapMode, Ui, UiBuilder, pos2, vec2,
};
use serde_json::Value;

use super::ctx::{Cx, Tx};
use super::glyphs;
use super::paint;
use super::render;
use super::snap::{Props, SNode};
use super::style::{Ink, LABEL};
use super::text::{self, Base};
use crate::deco;
use crate::theme::mix;

/// How far the header band's right end leans (px).
const SLANT: f32 = 14.0;
/// Height of a card's header band / an inline head line (px).
const HEAD_H: f32 = 32.0;
const INLINE_H: f32 = 26.0;
/// Opposite corners of the tool card: `0 10 0 10`.
const CARD_CORNER: CornerRadius = CornerRadius {
    nw: 0,
    ne: 10,
    se: 0,
    sw: 10,
};
/// Temp-data key of the nesting depth of tools (a tool inside a tool body is flattened).
const DEPTH_KEY: &str = "chat-tool-depth";

// ---- status looks ---------------------------------------------------------------------------

/// Which colour a status takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hue {
    /// Something is happening now: the accent's `b` end.
    Live,
    Ok,
    Warn,
    Bad,
    Dim,
}

/// Mark in front of a status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// Animated ring (only while something runs).
    Spin,
    /// Static half-filled ring (an active item that is not animated).
    Half,
    /// A Lucide icon by Tern name.
    Icon(&'static str),
}

/// How one status of a tool, agent or checklist item looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Look {
    pub glyph: Glyph,
    pub hue: Hue,
    /// Upper-case word shown next to the elapsed time (`done` shows the time alone).
    pub label: &'static str,
}

const FALLBACK: Look = Look {
    glyph: Glyph::Icon("circle"),
    hue: Hue::Dim,
    label: "",
};

const fn look(glyph: Glyph, hue: Hue, label: &'static str) -> Look {
    Look { glyph, hue, label }
}

/// Look of a `tool` status (`TspCardStatus`).
pub fn tool_look(status: &str) -> Option<Look> {
    Some(match status {
        "pending" => look(Glyph::Icon("circle"), Hue::Warn, "PENDING"),
        "running" => look(Glyph::Spin, Hue::Live, "RUNNING"),
        "done" => look(Glyph::Icon("check"), Hue::Ok, "DONE"),
        "error" => look(Glyph::Icon("x"), Hue::Bad, "ERROR"),
        "cancelled" => look(Glyph::Icon("ban"), Hue::Dim, "CANCELLED"),
        _ => return None,
    })
}

/// Look of an `agent` status.
pub fn agent_look(status: &str) -> Option<Look> {
    Some(match status {
        "pending" => look(Glyph::Icon("circle"), Hue::Warn, "PENDING"),
        "running" => look(Glyph::Spin, Hue::Live, "RUNNING"),
        "done" => look(Glyph::Icon("check"), Hue::Ok, "DONE"),
        "failed" => look(Glyph::Icon("x"), Hue::Bad, "FAILED"),
        "aborted" => look(Glyph::Icon("ban"), Hue::Dim, "ABORTED"),
        "idle" => look(Glyph::Icon("circle"), Hue::Dim, "IDLE"),
        "parked" => look(Glyph::Icon("pause"), Hue::Dim, "PARKED"),
        _ => return None,
    })
}

/// Look of a checklist item status.
pub fn item_look(status: &str) -> Option<Look> {
    Some(match status {
        "pending" => look(Glyph::Icon("circle"), Hue::Dim, "PENDING"),
        "active" => look(Glyph::Half, Hue::Live, "ACTIVE"),
        "done" => look(Glyph::Icon("check"), Hue::Ok, "DONE"),
        "dropped" => look(Glyph::Icon("minus"), Hue::Dim, "DROPPED"),
        "blocked" => look(Glyph::Icon("warn"), Hue::Warn, "BLOCKED"),
        _ => return None,
    })
}

fn hue(ink: &Ink, h: Hue) -> Color32 {
    match h {
        Hue::Live => ink.accent_text(),
        Hue::Ok => ink.p.ok,
        Hue::Warn => ink.p.warn,
        Hue::Bad => ink.p.danger,
        Hue::Dim => ink.dim(),
    }
}

/// The mark of `look` at `size` px. `spin` turns a [`Glyph::Half`] into the animated ring.
fn mark(cx: &Cx, ui: &mut Ui, look: Look, size: f32, spin: bool) {
    let c = hue(cx.ink, look.hue);
    match look.glyph {
        Glyph::Spin => {
            render::spin(cx, ui, size, c);
        }
        Glyph::Half if spin => {
            render::spin(cx, ui, size, c);
        }
        Glyph::Half => {
            render::ring(cx, ui, Some(0.5), size, c);
        }
        Glyph::Icon(name) => {
            glyphs::icon(ui, name, size, c, Sense::hover());
        }
    }
}

/// Right-hand status text of a head: the elapsed time and, unless the status is a plain `done`,
/// its word (`8.2s · RUNNING`, `1.2s · ERROR`, `DONE` → `1.2s`).
pub fn head_status(look: Look, ms: Option<f64>) -> String {
    let word = (look.hue != Hue::Ok && !look.label.is_empty()).then_some(look.label);
    match (ms, word) {
        (Some(ms), Some(w)) => format!("{} · {w}", render::fmt_ms(ms)),
        (Some(ms), None) => render::fmt_ms(ms),
        (None, Some(w)) => w.to_owned(),
        (None, None) => String::new(),
    }
}

// ---- preview clamp --------------------------------------------------------------------------

/// What a collapsed tool still shows of its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preview {
    /// Nothing: the body is hidden.
    None,
    /// The first `n` rows.
    Lines(usize),
    /// The last `n` rows (bash output).
    Tail(usize),
    /// The whole body; the children clamp themselves.
    Children,
}

/// Parses the `preview` prop (`{lines}`, `{tail}`, `"none"`, `"children"`; anything else hides).
pub fn preview_of(v: Option<&Value>) -> Preview {
    let rows = |v: &Value| v.as_f64().map(|n| n.max(0.0) as usize);
    match v {
        Some(Value::String(s)) if s == "children" => Preview::Children,
        Some(Value::Object(o)) => {
            if let Some(n) = o.get("lines").and_then(rows) {
                Preview::Lines(n)
            } else if let Some(n) = o.get("tail").and_then(rows) {
                Preview::Tail(n)
            } else {
                Preview::None
            }
        }
        _ => Preview::None,
    }
}

/// Rows a collapsed body is clamped to and whether the end is kept, `None` when not clamped.
pub fn clamp_rows(p: Preview) -> Option<(usize, bool)> {
    match p {
        Preview::Lines(n) => Some((n, false)),
        Preview::Tail(n) => Some((n, true)),
        Preview::None | Preview::Children => None,
    }
}

// ---- text helpers ---------------------------------------------------------------------------

/// A `TspText` (string or spans) as one layout job.
fn tjob(ink: &Ink, base: &Base, v: &Value) -> LayoutJob {
    let mut job = LayoutJob::default();
    match v {
        Value::String(s) => job.append(&text::clean(s), 0.0, base.format_plain()),
        Value::Array(a) => text::push_spans(&mut job, ink, base, a),
        _ => {}
    }
    job
}

/// A `TspText` without styling.
pub fn tplain(v: &Value) -> String {
    match v {
        Value::String(s) => text::clean(s).into_owned(),
        Value::Array(a) => text::spans_plain(a),
        _ => String::new(),
    }
}

/// The texts of a head that derive from the node's props alone, rebuilt only when it changes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HeadText {
    /// The primary argument, unstyled.
    pub target: String,
    /// `meta[]`, joined.
    pub meta: String,
    /// The warning `note`.
    pub note: String,
    /// The agent stat chips.
    pub stats: String,
}

impl HeadText {
    fn of(n: &SNode) -> HeadText {
        HeadText {
            target: n.get("target").map(tplain).unwrap_or_default(),
            meta: meta_line(n.arr("meta")),
            note: n.get("note").map(tplain).unwrap_or_default(),
            stats: n.obj("stats").map(stats_line).unwrap_or_default(),
        }
    }
}

/// [`HeadText`] of `n`, cached by (node id, generation).
fn head_text(cx: &Cx, n: &SNode) -> Arc<HeadText> {
    cx.loc(|l| match l.heads.get(&n.id) {
        Some((g, h)) if *g == n.gen_ => Arc::clone(h),
        _ => {
            let h = Arc::new(HeadText::of(n));
            l.heads.insert(n.id.clone(), (n.gen_, Arc::clone(&h)));
            h
        }
    })
}

/// First line of `s`, with ` …` when more follow.
pub fn one_line(s: &str) -> String {
    let mut lines = s
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty());
    let first = lines.next().unwrap_or_default().trim_start();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first.to_owned()
    }
}

/// `dir/` and `name[:sel]` of a path target.
pub fn split_path(p: &str) -> (&str, &str) {
    match p.rfind('/') {
        Some(i) => p.split_at(i + 1),
        None => ("", p),
    }
}

/// `dir` cut from the start to its last `keep` chars (`…tail/`), `""` when nothing useful is left.
pub fn shorten_dir(dir: &str, keep: usize) -> String {
    let n = dir.chars().count();
    if n <= keep {
        return dir.to_owned();
    }
    if keep < 4 {
        return String::new();
    }
    let tail: String = dir.chars().skip(n - (keep - 1)).collect();
    format!("…{tail}")
}

/// `1234` → `1.2k`, `2_500_000` → `2.5M`.
pub fn fmt_tokens(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1000.0 {
        format!("{:.1}k", n / 1000.0)
    } else {
        format!("{}", n.round() as i64)
    }
}

/// `$0.03`, `$0.004`, `$12.40`.
pub fn fmt_cost(c: f64) -> String {
    if c > 0.0 && c < 0.01 {
        format!("${c:.3}")
    } else {
        format!("${c:.2}")
    }
}

/// The stat chips of an agent head: `3 tools · 12 req · 48.1k tok · 42% ctx · $0.03`.
pub fn stats_line(stats: &Props) -> String {
    let num = |k: &str| stats.get(k).and_then(Value::as_f64);
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = num("tools") {
        let t = t as i64;
        parts.push(format!("{t} {}", if t == 1 { "tool" } else { "tools" }));
    }
    if let Some(r) = num("requests") {
        parts.push(format!("{} req", r as i64));
    }
    if let Some(t) = num("tokens") {
        parts.push(format!("{} tok", fmt_tokens(t)));
    }
    if let Some(l) = stats.get("contextLabel").and_then(Value::as_str) {
        parts.push(format!("{} ctx", text::clean(l)));
    } else if let Some(c) = num("context") {
        parts.push(if c <= 1.0 {
            format!("{:.0}% ctx", c * 100.0)
        } else {
            format!("{} ctx", fmt_tokens(c))
        });
    }
    if let Some(c) = num("cost") {
        parts.push(fmt_cost(c));
    }
    parts.join(" · ")
}

/// `retry 2/3 in 3.4s` while the delay runs, `retry 2/3` after.
pub fn retry_line(attempt: f64, max: f64, remaining_ms: f64) -> String {
    let head = format!("retry {}/{}", attempt as i64, max as i64);
    if remaining_ms > 50.0 {
        format!("{head} in {}", render::fmt_ms(remaining_ms))
    } else {
        head
    }
}

/// Facts after a tool's target (`meta[]`), joined.
pub fn meta_line(meta: &[Value]) -> String {
    meta.iter()
        .map(tplain)
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// `(closed, total)` items of a checklist; dropped items count as closed.
pub fn counts(phases: &[Value]) -> (usize, usize) {
    let mut closed = 0;
    let mut total = 0;
    for p in phases {
        for it in items_of(p) {
            total += 1;
            if matches!(status_of(it), "done" | "dropped") {
                closed += 1;
            }
        }
    }
    (closed, total)
}

fn items_of(phase: &Value) -> &[Value] {
    phase
        .get("items")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn status_of(item: &Value) -> &str {
    item.get("status")
        .and_then(Value::as_str)
        .unwrap_or("pending")
}

/// Text to copy for a tool head: the raw command, else a command/`copy`-tool target.
pub fn copy_text(n: &SNode) -> Option<String> {
    if let Some(c) = n.str("command").filter(|c| !c.trim().is_empty()) {
        return Some(text::clean(c).into_owned());
    }
    let wants = n.str("targetKind") == Some("command")
        || n.arr("tools")
            .iter()
            .any(|t| t.get("id").and_then(Value::as_str) == Some("copy"));
    let t = n
        .get("target")
        .map(tplain)
        .filter(|t| !t.trim().is_empty())?;
    wants.then_some(t)
}

// ---- shapes ---------------------------------------------------------------------------------

/// Header band polygon: the right end leans by `slant`, the top-right corner follows the card's
/// `r_tr` fillet, the bottom-left one `r_bl` (a card without a body rounds it). Convex, clockwise.
pub fn head_band(rect: Rect, slant: f32, r_tr: f32, r_bl: f32) -> Vec<Pos2> {
    let h = rect.height().max(1.0);
    let s = slant.min(rect.width() / 2.0);
    let r_tr = r_tr.min(h).min(rect.width() / 2.0);
    let r_bl = r_bl.min(h).min(rect.width() / 2.0);
    let mut pts = vec![rect.left_top()];
    if r_tr > 0.5 {
        let c = pos2(rect.right() - r_tr, rect.top() + r_tr);
        const STEPS: usize = 6;
        for k in 0..=STEPS {
            let a = -std::f32::consts::FRAC_PI_2
                + std::f32::consts::FRAC_PI_2 * k as f32 / STEPS as f32;
            let p = c + vec2(a.cos(), a.sin()) * r_tr;
            let lean = rect.right() - s * (p.y - rect.top()) / h;
            if p.x <= lean + 0.01 {
                pts.push(p);
            }
        }
    } else {
        pts.push(rect.right_top());
    }
    pts.push(pos2(rect.right() - s, rect.bottom()));
    if r_bl > 0.5 {
        let c = pos2(rect.left() + r_bl, rect.bottom() - r_bl);
        const STEPS: usize = 6;
        for k in 0..=STEPS {
            let a =
                std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_2 * k as f32 / STEPS as f32;
            pts.push(c + vec2(a.cos(), a.sin()) * r_bl);
        }
    } else {
        pts.push(rect.left_bottom());
    }
    pts
}

/// The hover pop of a card with per-corner radii: a second outline 2 px outside.
fn pop_shaped(painter: &egui::Painter, rect: Rect, corner: CornerRadius, ink: &Ink, t: f32) {
    if t <= 0.01 {
        return;
    }
    let grow = |r: u8| if r > 0 { r + 2 } else { 0 };
    painter.rect_stroke(
        rect.expand(2.0),
        CornerRadius {
            nw: grow(corner.nw),
            ne: grow(corner.ne),
            se: grow(corner.se),
            sw: grow(corner.sw),
        },
        Stroke::new(1.0, ink.p.hover_line.gamma_multiply(0.75 * t)),
        StrokeKind::Outside,
    );
}

fn finite_width(ui: &Ui) -> f32 {
    let w = ui.available_width();
    if w.is_finite() { w.max(80.0) } else { 480.0 }
}

fn depth_id() -> Id {
    Id::unique(DEPTH_KEY)
}

fn tool_depth(ui: &Ui) -> u8 {
    ui.data(|d| d.get_temp::<u8>(depth_id())).unwrap_or(0)
}

/// A quiet one-line label.
fn lab(ui: &mut Ui, ink: &Ink, text: &str, size: f32, color: Color32) -> Response {
    ui.add(
        Label::new(RichText::new(text).font(ink.label(size)).color(color))
            .selectable(false)
            .wrap_mode(TextWrapMode::Extend),
    )
}

/// A small boxed chip with `job` inside (painted manually so truncation stays exact).
fn chip(
    ui: &mut Ui,
    ink: &Ink,
    job: LayoutJob,
    fill: Color32,
    edge: Color32,
    sense: Sense,
) -> Response {
    let g = ui.painter().layout_job(job);
    let pad = vec2(7.0, 2.0);
    let (rect, resp) = ui.allocate_exact_size(g.size() + pad * 2.0, sense);
    let line = if resp.hovered() && sense != Sense::hover() {
        ink.p.hover_line
    } else {
        edge
    };
    paint::boxed(ui.painter(), rect, 4, fill, Stroke::new(1.0, line));
    ui.painter().galley(rect.min + pad, g, ink.text());
    resp
}

/// Lays out a right-aligned cluster, then a left one in the space it leaves, inside `rect`.
fn clusters(
    ui: &mut Ui,
    salt: &str,
    rect: Rect,
    pad: (f32, f32),
    right: impl FnOnce(&mut Ui),
    left: impl FnOnce(&mut Ui),
) {
    let clip = ui.clip_rect();
    let r_rect = Rect::from_min_max(
        pos2(rect.left() + rect.width() * 0.38, rect.top()),
        pos2(rect.right() - pad.1, rect.bottom()),
    );
    let mut r = ui.new_child(
        UiBuilder::new()
            .id_salt((salt, "r"))
            .max_rect(r_rect)
            .layout(Layout::right_to_left(Align::Center)),
    );
    r.set_clip_rect(r_rect.intersect(clip));
    right(&mut r);
    let used = r.min_rect();
    let stop = if used.width() > 0.5 && used.left() >= r_rect.left() - 1.0 {
        used.left() - 10.0
    } else {
        r_rect.right()
    };
    let l_rect = Rect::from_min_max(
        pos2(rect.left() + pad.0, rect.top()),
        pos2(stop.max(rect.left() + pad.0 + 24.0), rect.bottom()),
    );
    let mut l = ui.new_child(
        UiBuilder::new()
            .id_salt((salt, "l"))
            .max_rect(l_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    l.set_clip_rect(l_rect.intersect(clip));
    left(&mut l);
}

// ---- tool -----------------------------------------------------------------------------------

/// `tool` node: a tool call as a card (or an inline head line) with its children as the body.
pub fn tool(cx: &Cx, ui: &mut Ui, n: &SNode) {
    ui.scope(|ui| tool_inner(cx, ui, n));
}

fn tool_inner(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    ui.spacing_mut().item_spacing.y = 0.0;
    let depth = tool_depth(ui);
    let status = n.str("status").unwrap_or("done");
    let lk = tool_look(status).unwrap_or(FALLBACK);
    let running = status == "running";
    let errored = status == "error";
    let inline = depth > 0 || n.str("frame") == Some("inline");
    let collapsible = n.flag("collapsible");
    let collapsed = collapsible && cx.collapsed(n);
    let pv = preview_of(n.get("preview"));
    let has_body = cx.snap.kids(n).any(|c| !render::blank(cx, c));
    let show_body = has_body && (!collapsed || pv != Preview::None);
    let clamp = if collapsed { pv } else { Preview::Children };
    if running {
        cx.animate();
    }

    let w = finite_width(ui);
    let head_h = if inline { INLINE_H } else { HEAD_H };
    let bg = (!inline).then(|| paint::reserve(ui.painter()));
    let (head_rect, _) = ui.allocate_exact_size(vec2(w, head_h), Sense::hover());
    let hid = ui.scope_id().with(("tool-head", n.id.as_str()));
    let action = n.click().filter(|a| *a != "toggle");
    let head = ui.interact(head_rect, hid, Sense::click());
    let hot = ui
        .ctx()
        .animate_bool_with_time(hid.with("hot"), head.contains_pointer(), 0.1);

    // Colours of this state.
    let edge = if running {
        ink.b
    } else if errored {
        mix(ink.p.stroke, ink.p.danger, 0.85)
    } else {
        ink.line()
    };
    let (mut ha, mut hb) = ink.head();
    let mut fill = ink.card();
    if running {
        ha = mix(ha, ink.p.ok, 0.16);
        hb = mix(hb, ink.p.ok, 0.10);
        fill = mix(fill, ink.b, 0.04);
    } else if errored {
        ha = mix(ha, ink.p.danger, 0.22);
        hb = mix(hb, ink.p.danger, 0.16);
        fill = mix(fill, ink.p.danger, 0.06);
    }

    // Header background.
    if inline {
        let band = head_band(head_rect, SLANT, 0.0, 0.0);
        let k = 0.55 + 0.45 * hot;
        paint::fill_h(
            ui.painter(),
            &band,
            ink.chip().gamma_multiply(k),
            ink.chip().gamma_multiply(k * 0.4),
        );
        let tick = Rect::from_min_size(head_rect.left_top(), vec2(2.0, head_h));
        ui.painter().rect_filled(tick, 0.0, hue(ink, lk.hue));
    } else {
        let band = head_band(
            head_rect.shrink(1.0),
            SLANT,
            9.0,
            if show_body { 0.0 } else { 9.0 },
        );
        paint::fill_h(ui.painter(), &band, ha, hb);
        if hot > 0.01 {
            let c = ink.p.hover_bg.gamma_multiply(0.6 * hot);
            paint::fill_h(ui.painter(), &band, c, c);
        }
        let p = ui.painter();
        let y = head_rect.bottom() - 0.5;
        if show_body {
            p.line_segment(
                [
                    pos2(head_rect.left() + 1.0, y),
                    pos2(head_rect.right() - SLANT, y),
                ],
                Stroke::new(1.0, mix(edge, ink.a, 0.35)),
            );
        }
        p.line_segment(
            [
                pos2(head_rect.right() - 1.0, head_rect.top() + 4.0),
                pos2(head_rect.right() - SLANT, head_rect.bottom() - 0.5),
            ],
            Stroke::new(1.0, edge),
        );
    }

    // Header content.
    let ms = if running {
        render::live_ms(cx, n, "age")
    } else {
        n.num("took").or_else(|| n.num("age"))
    };
    let pad = if inline {
        (10.0, 10.0)
    } else {
        (14.0, SLANT + 6.0)
    };
    clusters(
        ui,
        &n.id,
        head_rect,
        pad,
        |ui| head_right(cx, ui, n, lk, ms, hot, collapsible.then_some(collapsed)),
        |ui| head_left(cx, ui, n, lk),
    );
    let mut head = head;
    if let Some(i) = n.str("intent").filter(|i| !i.trim().is_empty()) {
        head = head.on_hover_text(text::clean(i).into_owned());
    }
    if collapsible || action.is_some() {
        head = head.on_hover_cursor(CursorIcon::PointingHand);
    }
    if head.clicked() {
        if collapsible {
            cx.toggle(n);
        } else if let Some(a) = action {
            cx.click(n, a);
        }
    }

    // Body.
    if show_body {
        if inline {
            let top = ui.cursor().top();
            ui.add_space(5.0);
            body_scope(
                cx,
                ui,
                n,
                head_rect.left() + 18.0,
                head_rect.right(),
                clamp,
                ink.p.bg,
            );
            ui.add_space(3.0);
            let bottom = ui.cursor().top() - 3.0;
            let x = head_rect.left() + 6.5;
            ui.painter().line_segment(
                [pos2(x, top + 3.0), pos2(x, bottom)],
                Stroke::new(1.0, ink.line()),
            );
        } else {
            let ml = if running { 16.0 } else { 12.0 };
            ui.add_space(8.0);
            body_scope(
                cx,
                ui,
                n,
                head_rect.left() + ml,
                head_rect.right() - 12.0,
                clamp,
                fill,
            );
            ui.add_space(8.0);
        }
    }
    if inline {
        let t = ui
            .ctx()
            .animate_bool_with_time(hid.with("card"), head.contains_pointer(), 0.12);
        paint::pop(ui.painter(), head_rect, 3, ink, t);
        return;
    }

    // The card around everything.
    let bottom = ui.min_rect().bottom().max(head_rect.bottom());
    let rect = Rect::from_min_max(head_rect.min, pos2(head_rect.right(), bottom));
    let t = ui
        .ctx()
        .animate_bool_with_time(hid.with("card"), ui.rect_contains_pointer(rect), 0.12);
    let mut under = vec![deco::drop_shadow_shape(rect, 10, 3, 12, ink.dark, 80)];
    if running {
        let k = 0.45 + 0.55 * paint::breath(ink, ui.input(|i| i.time));
        under.push(deco::glow_shape(
            rect,
            10,
            ink.b.gamma_multiply(0.42 * k),
            30,
        ));
    }
    under.push(Shape::rect_filled(rect, CARD_CORNER, fill));
    if let Some(bg) = bg {
        ui.painter().set(bg, Shape::Vec(under));
    }
    let p = ui.painter();
    p.add(Shape::rect_stroke(
        rect,
        CARD_CORNER,
        Stroke::new(1.0, edge),
        StrokeKind::Inside,
    ));
    paint::hairline(
        p,
        ink,
        rect.left() + 1.0,
        rect.right() - SLANT,
        rect.top(),
        if running { 1.0 } else { 0.85 },
    );
    if running {
        let stripe_bottom = (rect.bottom() - 8.0).max(rect.top() + head_h);
        p.rect_filled(
            Rect::from_min_max(
                pos2(rect.left() + 1.0, rect.top() + 1.0),
                pos2(rect.left() + 4.0, stripe_bottom),
            ),
            0.0,
            ink.b,
        );
    }
    pop_shaped(p, rect, CARD_CORNER, ink, t);
}

/// Right cluster of a tool head, right to left: fold chevron, hover buttons, time/status, exit
/// chip, note, meta, badges. Optional parts that do not fit are dropped.
fn head_right(
    cx: &Cx,
    ui: &mut Ui,
    n: &SNode,
    lk: Look,
    ms: Option<f64>,
    hot: f32,
    fold: Option<bool>,
) {
    let ink = cx.ink;
    ui.spacing_mut().item_spacing.x = 7.0;
    fn fits(ui: &Ui, need: f32) -> bool {
        ui.available_width() >= need + 8.0
    }
    if let Some(collapsed) = fold {
        render::chevron(ui, ink.dim(), !collapsed);
    }
    hover_buttons(cx, ui, n, hot);
    let status = head_status(lk, ms);
    if !status.is_empty() {
        let c = if lk.hue == Hue::Ok {
            ink.dim()
        } else {
            hue(ink, lk.hue)
        };
        lab(ui, ink, &status, 11.5, c);
    }
    if let Some(code) = n.num("exit").filter(|c| *c != 0.0) {
        let t = format!("exit {}", code as i64);
        if fits(ui, t.len() as f32 * 7.0 + 12.0) {
            render::badge(cx, ui, &t, Some("error"));
        }
    }
    let ht = head_text(cx, n);
    if !ht.note.trim().is_empty() && fits(ui, ht.note.chars().count() as f32 * 7.0 + 12.0) {
        render::badge(cx, ui, &ht.note, Some("warning"));
    }
    if !ht.meta.is_empty() && fits(ui, ht.meta.chars().count() as f32 * 6.5) {
        lab(ui, ink, &ht.meta, 11.5, ink.dim());
    }
    for b in n.arr("badges").iter().rev() {
        let Some(t) = b.get("text").and_then(Value::as_str).map(text::clean) else {
            continue;
        };
        if !fits(ui, t.chars().count() as f32 * 7.0 + 12.0) {
            break;
        }
        let r = render::badge(cx, ui, &t, b.get("tone").and_then(Value::as_str));
        if let Some(tip) = b.get("title").and_then(Value::as_str) {
            r.on_hover_text(text::clean(tip).into_owned());
        }
    }
}

/// Copy / open buttons of a head: always laid out (so the target does not jump), drawn and
/// clickable only while the pointer is over the head.
fn hover_buttons(cx: &Cx, ui: &mut Ui, n: &SNode, hot: f32) {
    let ink = cx.ink;
    let live = hot > 0.35;
    let a = hot.clamp(0.0, 1.0);
    if let Some(href) = n
        .str("href")
        .filter(|_| n.str("targetKind") == Some("path"))
    {
        let sense = if live { Sense::click() } else { Sense::hover() };
        let r = glyphs::icon(
            ui,
            "external-link",
            13.0,
            ink.dim().gamma_multiply(a),
            sense,
        );
        if live {
            let r = r
                .on_hover_cursor(CursorIcon::PointingHand)
                .on_hover_text("Open");
            if r.clicked() {
                cx.open(href.to_owned());
            }
        }
    }
    if let Some(text) = copy_text(n) {
        let copied = cx.loc(|l| l.copied.get(&n.id).copied());
        let flash = copied.is_some_and(|t| cx.now.saturating_duration_since(t).as_millis() < 1200);
        if flash {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(1300));
        }
        let sense = if live { Sense::click() } else { Sense::hover() };
        let (name, color) = if flash {
            ("check", ink.p.ok)
        } else {
            ("copy", ink.dim().gamma_multiply(a))
        };
        let r = glyphs::icon(ui, name, 13.0, color, sense);
        if live {
            let r = r
                .on_hover_cursor(CursorIcon::PointingHand)
                .on_hover_text("Copy");
            if r.clicked() {
                cx.copy(text);
                cx.loc(|l| {
                    l.copied.insert(n.id.clone(), cx.now);
                });
            }
        }
    }
}

/// Left cluster of a tool head: status mark, title, target.
fn head_left(cx: &Cx, ui: &mut Ui, n: &SNode, lk: Look) {
    let ink = cx.ink;
    ui.spacing_mut().item_spacing.x = 7.0;
    mark(cx, ui, lk, 13.0, false);
    let base = Base::new(ink.label(12.0), ink.strong(), ink);
    let mut job = n
        .get("title")
        .map(|v| tjob(ink, &base, v))
        .filter(|j| !j.is_empty())
        .unwrap_or_default();
    if job.is_empty() {
        job.append(n.str("name").unwrap_or_default(), 0.0, base.format_plain());
    }
    ui.add(
        Label::new(job)
            .selectable(false)
            .wrap_mode(TextWrapMode::Extend),
    );
    target(cx, ui, n);
}

/// The primary argument, styled by `targetKind`.
fn target(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let Some(tv) = n.get("target") else { return };
    let ht = head_text(cx, n);
    let plain = ht.target.as_str();
    if plain.trim().is_empty() {
        return;
    }
    let kind = n.str("targetKind").unwrap_or("text");
    let avail = ui.available_width() - 4.0;
    if avail < 30.0 {
        return;
    }
    let font = ink.mono(0.92);
    let href = n.str("href").filter(|_| kind == "path");
    let sense = if href.is_some() {
        Sense::click()
    } else {
        Sense::hover()
    };
    let one_row = |mut job: LayoutJob, width: f32| {
        job.wrap.max_width = width;
        job.wrap.max_rows = 1;
        job
    };
    let resp = match kind {
        "command" => {
            let base = Base::new(font.clone(), ink.text(), ink);
            let mut job = LayoutJob::default();
            job.append(
                "$ ",
                0.0,
                Base::new(font.clone(), ink.faint(), ink).format_plain(),
            );
            match tv {
                Value::String(s) => {
                    job.append(&one_line(&text::clean(s)), 0.0, base.format_plain())
                }
                Value::Array(a) => text::push_spans(&mut job, ink, &base, a),
                _ => {}
            }
            ui.add(Label::new(one_row(job, avail)).selectable(false).truncate())
        }
        "path" => {
            let dim = Base::new(font.clone(), ink.dim(), ink);
            let strong = Base::new(font.clone(), ink.strong(), ink);
            let inner = avail - 14.0;
            let build = |dir: &str, name: &str| {
                let mut job = LayoutJob::default();
                if !dir.is_empty() {
                    job.append(dir, 0.0, dim.format_plain());
                }
                job.append(name, 0.0, strong.format_plain());
                one_row(job, inner)
            };
            let job = match tv {
                Value::String(_) => {
                    let (dir, name) = split_path(plain);
                    // Whether (and how far) the directory is cut depends on the width only:
                    // measured once per node, generation and width, not every frame.
                    let bucket = inner.round() as i32;
                    let shown = cx.loc(|l| match l.path_dirs.get(&n.id) {
                        Some((g, b, d)) if *g == n.gen_ && *b == bucket => Some(d.clone()),
                        _ => None,
                    });
                    let shown = shown.unwrap_or_else(|| {
                        let width = ui.painter().layout_job(build(dir, name)).size().x;
                        let d = if width > inner && !dir.is_empty() {
                            let name_w = ui
                                .painter()
                                .layout_no_wrap(name.to_owned(), font.clone(), ink.strong())
                                .size()
                                .x;
                            let char_w = ui.ctx().fonts_mut(|f| f.glyph_width(&font, 'm')).max(1.0);
                            let keep = ((inner - name_w) / char_w).floor().max(0.0) as usize;
                            shorten_dir(dir, keep)
                        } else {
                            dir.to_owned()
                        };
                        cx.loc(|l| {
                            l.path_dirs
                                .insert(n.id.clone(), (n.gen_, bucket, d.clone()));
                        });
                        d
                    });
                    build(&shown, name)
                }
                _ => one_row(tjob(ink, &strong, tv), inner),
            };
            chip(ui, ink, job, ink.chip(), ink.line(), sense)
        }
        "pattern" | "query" => {
            let base = Base::new(font.clone(), ink.tone("accent"), ink);
            let mut job = LayoutJob::default();
            job.append("\"", 0.0, base.format_plain());
            job.append(&one_line(plain), 0.0, base.format_plain());
            job.append("\"", 0.0, base.format_plain());
            let c = ink.tone("accent");
            chip(
                ui,
                ink,
                one_row(job, avail - 14.0),
                ink.wash(c),
                ink.edge(c),
                sense,
            )
        }
        _ => {
            let base = Base::new(ink.label(12.0), ink.text(), ink);
            ui.add(
                Label::new(one_row(tjob(ink, &base, tv), avail))
                    .selectable(false)
                    .truncate(),
            )
        }
    };
    let resp = if let Some(h) = href {
        let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
        if resp.clicked() {
            cx.open(h.to_owned());
        }
        resp
    } else {
        resp
    };
    if plain.chars().count() > 24 {
        resp.on_hover_text(plain);
    }
}

/// Body of a tool: its children in a compact mono look, clamped per `clamp` (collapsed previews).
fn body_scope(
    cx: &Cx,
    ui: &mut Ui,
    n: &SNode,
    left: f32,
    right: f32,
    clamp: Preview,
    fill: Color32,
) {
    let ink = cx.ink;
    let w = (right - left).max(40.0);
    let top = ui.cursor().top();
    let region = Rect::from_min_size(pos2(left, top), vec2(w, 10_000.0));
    let tx = Tx {
        font: ink.mono(0.95),
        color: ink.text(),
        line: None,
    };
    let Some((rows, tail)) = clamp_rows(clamp) else {
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(region)
                .layout(Layout::top_down(Align::Min)),
            |ui| {
                ui.set_width(w);
                body_children(cx, ui, n, &tx);
            },
        );
        return;
    };
    let row_h = ui.ctx().fonts_mut(|f| f.row_height(&tx.font)).max(10.0);
    let max_h = rows.max(1) as f32 * row_h;
    let origin = pos2(left, top);
    let area = Rect::from_min_size(origin, vec2(w, max_h));
    let full = if tail {
        cached_height(ui, n, w, |ui| body_children(cx, ui, n, &tx))
    } else {
        f32::INFINITY
    };
    let shift = if tail && full.is_finite() {
        (full - max_h).max(0.0)
    } else {
        0.0
    };
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(("tool-preview", n.id.as_str()))
            .max_rect(Rect::from_min_size(
                pos2(left, top - shift),
                vec2(w, 10_000.0),
            ))
            .layout(Layout::top_down(Align::Min)),
    );
    child.set_clip_rect(area.intersect(ui.clip_rect()));
    child.set_width(w);
    body_children(cx, &mut child, n, &tx);
    let used = child.min_rect().height();
    let h = (if tail { used - shift } else { used }).min(max_h).max(0.0);
    ui.allocate_rect(Rect::from_min_size(origin, vec2(w, h)), Sense::hover());
    // Soft edge where rows were cut off.
    let cut = used > max_h + 0.5;
    if cut && h > 6.0 {
        let fade = h.min(14.0);
        let clear = fill.gamma_multiply(0.0);
        let (r, a, b) = if tail {
            (Rect::from_min_size(origin, vec2(w, fade)), fill, clear)
        } else {
            (
                Rect::from_min_max(
                    pos2(origin.x, origin.y + h - fade),
                    pos2(origin.x + w, origin.y + h),
                ),
                clear,
                fill,
            )
        };
        deco::fill_vertical(
            ui.painter(),
            &[
                r.left_top(),
                r.right_top(),
                r.right_bottom(),
                r.left_bottom(),
            ],
            a,
            b,
        );
    }
}

/// Renders the children of `n` as a tool body (one level deeper than the tool itself).
fn body_children(cx: &Cx, ui: &mut Ui, n: &SNode, tx: &Tx) {
    let depth = tool_depth(ui);
    ui.data_mut(|d| d.insert_temp(depth_id(), depth.saturating_add(1)));
    ui.spacing_mut().item_spacing.y = 6.0;
    cx.with_tx(tx.clone(), || render::children(cx, ui, n));
    ui.data_mut(|d| d.insert_temp(depth_id(), depth));
}

/// Height `f` lays out to at width `w`, remembered per node until its subtree or width changes.
fn cached_height(ui: &mut Ui, n: &SNode, w: f32, f: impl Fn(&mut Ui)) -> f32 {
    let key = ui.scope_id().with(("tool-body-h", n.id.as_str()));
    let bucket = w.round() as i32;
    if let Some((sub, b, h)) = ui.data(|d| d.get_temp::<(u64, i32, f32)>(key))
        && sub == n.sub
        && b == bucket
    {
        return h;
    }
    let rect = Rect::from_min_size(ui.cursor().min, vec2(w, 10_000.0));
    let mut probe = ui.new_child(
        UiBuilder::new()
            .id_salt("tool-measure")
            .max_rect(rect)
            .layout(Layout::top_down(Align::Min))
            .invisible()
            .sizing_pass(),
    );
    probe.set_width(w);
    f(&mut probe);
    let h = probe.min_rect().height();
    ui.data_mut(|d| d.insert_temp(key, (n.sub, bucket, h)));
    h
}

// ---- agent ----------------------------------------------------------------------------------

/// `agent` node: one subagent as a nested card (name, type, model, progress), children expanded.
pub fn agent(cx: &Cx, ui: &mut Ui, n: &SNode) {
    ui.scope(|ui| agent_inner(cx, ui, n));
}

fn agent_inner(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    ui.spacing_mut().item_spacing.y = 0.0;
    let status = n.str("status").unwrap_or("pending");
    let lk = agent_look(status).unwrap_or(FALLBACK);
    let running = status == "running";
    let collapsible = n.flag("collapsible");
    let collapsed = collapsible && cx.collapsed(n);
    let has_body = !collapsed && cx.snap.kids(n).any(|c| !render::blank(cx, c));
    if running {
        cx.animate();
    }
    let indent = (n.num("depth").unwrap_or(0.0).clamp(0.0, 4.0) as f32) * 14.0;
    let full_w = finite_width(ui);
    let w = (full_w - indent).max(120.0);
    let left = ui.cursor().left() + indent;
    let accent = hue(ink, lk.hue);
    let bg = paint::reserve(ui.painter());
    let top = ui.cursor().top();
    let head_rect = Rect::from_min_size(pos2(left, top), vec2(w, 28.0));
    ui.allocate_rect(
        Rect::from_min_size(pos2(left, top), vec2(w, 28.0)),
        Sense::hover(),
    );
    let hid = ui.scope_id().with(("agent-head", n.id.as_str()));
    let head = ui.interact(head_rect, hid, Sense::click());
    let hot = ui
        .ctx()
        .animate_bool_with_time(hid.with("hot"), head.contains_pointer(), 0.1);

    let ms = agent_clock(cx, n, running);
    clusters(
        ui,
        &n.id,
        head_rect,
        (14.0, 10.0),
        |ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            if collapsible {
                render::chevron(ui, ink.dim(), !collapsed);
            }
            let status = head_status(lk, ms);
            if !status.is_empty() {
                let c = if lk.hue == Hue::Ok { ink.dim() } else { accent };
                lab(ui, ink, &status, 11.5, c);
            }
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            mark(cx, ui, lk, 13.0, false);
            let name = n.str("name").map(text::clean).unwrap_or_default();
            lab(ui, ink, &name, 12.5, ink.strong());
            if let Some(kind) = n.str("agent").filter(|k| !k.is_empty()) {
                render::badge(cx, ui, kind, Some("muted"));
            }
            if let Some(model) = n.str("model").filter(|m| !m.is_empty()) {
                let mut job = LayoutJob::default();
                job.append(
                    &text::clean(model),
                    0.0,
                    Base::new(ink.label(LABEL), ink.dim(), ink).format_plain(),
                );
                job.wrap.max_rows = 1;
                job.wrap.max_width = (ui.available_width() - 16.0).max(20.0);
                chip(ui, ink, job, ink.chip(), ink.line(), Sense::hover());
                if let Some(level) = n.str("thinking") {
                    let level = level.trim_start_matches("thinking").to_ascii_lowercase();
                    render::ring(cx, ui, render::effort_fill(&level), 11.0, ink.b);
                }
            }
        },
    );
    if let Some(task) = n.get("task").map(tplain).filter(|t| !t.trim().is_empty()) {
        head.clone().on_hover_text(task);
    }
    if collapsible {
        let head = head.clone().on_hover_cursor(CursorIcon::PointingHand);
        if head.clicked() {
            cx.toggle(n);
        }
    }
    if hot > 0.01 {
        paint::pop(ui.painter(), head_rect, 4, ink, hot);
    }

    // Detail rows, under the status mark.
    let inner = Rect::from_min_max(
        pos2(left + 14.0 + 20.0, top),
        pos2(left + w - 12.0, top + 10_000.0),
    );
    let detail_w = (inner.width()).max(60.0);
    let mut rows = false;
    let base = Base::new(ink.label(12.0), ink.dim(), ink);
    let mut detail = |ui: &mut Ui, f: &mut dyn FnMut(&mut Ui)| {
        if !rows {
            rows = true;
        }
        let r = Rect::from_min_size(
            pos2(inner.left(), ui.cursor().top()),
            vec2(detail_w, 10_000.0),
        );
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(r)
                .layout(Layout::top_down(Align::Min)),
            |ui| {
                ui.set_width(detail_w);
                f(ui);
            },
        );
        ui.add_space(3.0);
    };
    if let Some(task) = n.get("task").filter(|t| !tplain(t).trim().is_empty()) {
        let job = tjob(ink, &base, task);
        detail(ui, &mut |ui: &mut Ui| {
            render::para(ui, job.clone(), ink.dim(), &[]);
        });
    }
    if running
        && let Some(t) = n.obj("tool")
        && let Some(name) = t.get("name").and_then(Value::as_str)
    {
        let mono = ink.mono(0.9);
        let mut job = LayoutJob::default();
        job.append(
            "▸ ",
            0.0,
            Base::new(mono.clone(), ink.b, ink).format_plain(),
        );
        job.append(
            name,
            0.0,
            Base::new(mono.clone(), ink.strong(), ink).format_plain(),
        );
        if let Some(i) = t.get("intent").map(tplain).filter(|i| !i.trim().is_empty()) {
            job.append(
                &format!("  {}", one_line(&i)),
                0.0,
                Base::new(mono.clone(), ink.dim(), ink).format_plain(),
            );
        }
        if let Some(age) = t.get("age").and_then(Value::as_f64) {
            let live = age + cx.now.saturating_duration_since(n.changed).as_secs_f64() * 1000.0;
            job.append(
                &format!("  {}", render::fmt_ms(live)),
                0.0,
                Base::new(mono, ink.faint(), ink).format_plain(),
            );
        }
        job.wrap.max_rows = 1;
        job.wrap.max_width = detail_w;
        detail(ui, &mut |ui: &mut Ui| {
            ui.add(Label::new(job.clone()).selectable(false).truncate());
        });
    }
    if let Some(stats) = n.obj("stats") {
        let ht = head_text(cx, n);
        let line = ht.stats.as_str();
        if !line.is_empty() {
            detail(ui, &mut |ui: &mut Ui| {
                lab(ui, ink, line, 11.5, ink.dim());
            });
        }
        if running && let Some(done) = stats.get("done").and_then(Value::as_f64) {
            detail(ui, &mut |ui: &mut Ui| {
                progress_bar(ui, ink, done as f32, detail_w.min(240.0));
            });
        }
    }
    if let Some(r) = n.obj("retry") {
        let num = |k: &str| r.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        let remaining = num("delay")
            - num("age")
            - cx.now.saturating_duration_since(n.changed).as_secs_f64() * 1000.0;
        if remaining > 50.0 {
            cx.animate();
        }
        let mut text = retry_line(num("attempt"), num("max"), remaining);
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            text.push_str(" · ");
            text.push_str(&one_line(&text::clean(e)));
        }
        detail(ui, &mut |ui: &mut Ui| {
            ui.add(
                Label::new(RichText::new(&text).font(ink.label(11.5)).color(ink.p.warn))
                    .selectable(false)
                    .truncate(),
            );
        });
    }
    if rows {
        ui.add_space(4.0);
    }

    // Expanded children.
    if has_body {
        let r = Rect::from_min_size(
            pos2(left + 14.0, ui.cursor().top() + 2.0),
            vec2((w - 26.0).max(40.0), 10_000.0),
        );
        ui.add_space(2.0);
        let y0 = ui.cursor().top();
        let tx = Tx {
            font: ink.mono(0.95),
            color: ink.text(),
            line: None,
        };
        let inner = ui.scope_builder(
            UiBuilder::new()
                .max_rect(Rect::from_min_max(
                    pos2(r.left() + 8.0, y0),
                    pos2(r.right(), y0 + 10_000.0),
                ))
                .layout(Layout::top_down(Align::Min)),
            |ui| {
                ui.set_width(r.width() - 8.0);
                body_children(cx, ui, n, &tx);
            },
        );
        let rail = left + 14.0 + 2.5;
        ui.painter().line_segment(
            [pos2(rail, y0), pos2(rail, inner.response.rect.bottom())],
            Stroke::new(1.0, ink.line()),
        );
        ui.add_space(8.0);
    }
    ui.add_space(4.0);

    // Frame behind it all.
    let bottom = ui.min_rect().bottom().max(head_rect.bottom());
    let rect = Rect::from_min_max(pos2(left, top), pos2(left + w, bottom));
    let corner = CornerRadius {
        nw: 0,
        ne: 6,
        se: 0,
        sw: 6,
    };
    let fill = mix(ink.card(), ink.p.text, if ink.dark { 0.03 } else { 0.02 });
    ui.painter().set(bg, Shape::rect_filled(rect, corner, fill));
    let p = ui.painter();
    let edge = if running {
        ink.edge(ink.b)
    } else if lk.hue == Hue::Bad {
        ink.edge(ink.p.danger)
    } else {
        ink.line()
    };
    p.add(Shape::rect_stroke(
        rect,
        corner,
        Stroke::new(1.0, edge),
        StrokeKind::Inside,
    ));
    p.rect_filled(
        Rect::from_min_max(
            pos2(rect.left() + 1.0, rect.top() + 1.0),
            pos2(rect.left() + 3.0, rect.bottom() - 6.0),
        ),
        0.0,
        accent,
    );
    if running {
        paint::hairline(
            p,
            ink,
            rect.left() + 3.0,
            rect.right() - 10.0,
            rect.top(),
            0.9,
        );
    }
}

/// Milliseconds an agent has run: its `stats.age` ticking while running, else `stats.took`.
fn agent_clock(cx: &Cx, n: &SNode, running: bool) -> Option<f64> {
    let st = n.obj("stats")?;
    let num = |k: &str| st.get(k).and_then(Value::as_f64);
    if running {
        let age = num("age")?;
        Some(age + cx.now.saturating_duration_since(n.changed).as_secs_f64() * 1000.0)
    } else {
        num("took").or_else(|| num("age"))
    }
}

/// A thin slanted progress bar (`value` 0..=1).
fn progress_bar(ui: &mut Ui, ink: &Ink, value: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 5.0), Sense::hover());
    ui.painter().rect_filled(rect, 2.0, ink.chip());
    let f = value.clamp(0.0, 1.0);
    if f > 0.0 {
        let mut fill = rect;
        fill.set_width((rect.width() * f).max(3.0));
        let lean = (fill.width() / 2.0).min(3.0);
        paint::fill_h(ui.painter(), &paint::slant_band(fill, lean), ink.a, ink.b);
    }
}

// ---- checklist ------------------------------------------------------------------------------

/// `checklist` node: `full` (a todo tool's body), `hud` (pill + popover) or `reminder`.
pub fn checklist(cx: &Cx, ui: &mut Ui, n: &SNode) {
    ui.scope(|ui| match n.str("mode") {
        Some("hud") => hud(cx, ui, n),
        Some("reminder") => reminder(cx, ui, n),
        _ => {
            let live = cx
                .snap
                .parent_of(&n.id)
                .is_some_and(|p| p.kind == "tool" && p.str("status") == Some("running"));
            full(cx, ui, n, live);
        }
    });
}

fn full(cx: &Cx, ui: &mut Ui, n: &SNode, live: bool) {
    let phases = n.arr("phases");
    let titled = phases.len() > 1 || phases.first().is_some_and(|p| phase_title(p).is_some());
    ui.spacing_mut().item_spacing.y = 4.0;
    for (i, p) in phases.iter().enumerate() {
        phase(cx, ui, n, p, i, titled, live);
    }
}

fn phase_title(p: &Value) -> Option<&Value> {
    p.get("title").filter(|t| !tplain(t).trim().is_empty())
}

fn phase(cx: &Cx, ui: &mut Ui, n: &SNode, p: &Value, index: usize, titled: bool, live: bool) {
    let ink = cx.ink;
    let items = items_of(p);
    let key = ui.scope_id().with(("ck-phase", n.id.as_str(), index));
    let default_open = !p.get("collapsed").and_then(Value::as_bool).unwrap_or(false);
    let open = ui.data(|d| d.get_temp::<bool>(key)).unwrap_or(default_open);
    if titled {
        let (closed, total) = counts(std::slice::from_ref(p));
        let resp = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                render::chevron(ui, ink.dim(), open);
                let base = Base::new(ink.label(12.0), ink.strong(), ink);
                if let Some(t) = phase_title(p) {
                    ui.add(
                        Label::new(tjob(ink, &base, t))
                            .selectable(false)
                            .wrap_mode(TextWrapMode::Extend),
                    );
                }
                let tone = if total > 0 && closed == total {
                    "success"
                } else {
                    "accent"
                };
                render::badge(cx, ui, &format!("{closed}/{total}"), Some(tone));
                if total > 0 {
                    progress_bar(ui, ink, closed as f32 / total as f32, 46.0);
                }
            })
            .response;
        let r = ui.interact(resp.rect, key.with("hit"), Sense::click());
        if r.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if r.clicked() {
            ui.data_mut(|d| d.insert_temp(key, !open));
        }
    }
    if titled && !open {
        return;
    }
    for it in items {
        item_row(cx, ui, it, live, titled);
    }
}

/// One checklist item: mark, text (dim and struck through when closed, accent when active) and
/// its note underneath.
fn item_row(cx: &Cx, ui: &mut Ui, it: &Value, live: bool, indent: bool) {
    let ink = cx.ink;
    let st = status_of(it);
    let lk = item_look(st).unwrap_or(FALLBACK);
    let tx = cx.tx();
    let color = match st {
        "done" => ink.dim(),
        "dropped" => ink.faint(),
        "active" => ink.tone("accent"),
        "blocked" => ink.p.warn,
        _ => ink.text(),
    };
    let strike = matches!(st, "done" | "dropped");
    let base = Base::new(tx.font.clone(), color, ink);
    let mut job = it
        .get("text")
        .map(|v| tjob(ink, &base, v))
        .unwrap_or_default();
    for s in &mut job.sections {
        if strike {
            s.format.color = color;
            s.format.strikethrough = Stroke::new(1.0, color);
        }
    }
    let note = it.get("note").map(tplain).filter(|s| !s.trim().is_empty());
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        if indent {
            ui.add_space(16.0);
        }
        // The mark sits on the first text row.
        let row_h = ui.ctx().fonts_mut(|f| f.row_height(&tx.font)).max(14.0);
        let (slot, _) = ui.allocate_exact_size(vec2(15.0, row_h), Sense::hover());
        let mut m = ui.new_child(
            UiBuilder::new()
                .max_rect(slot)
                .layout(Layout::left_to_right(Align::Center)),
        );
        mark(cx, &mut m, lk, 13.0, live && st == "active");
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            render::para(ui, job, color, &[]);
            if let Some(note) = note {
                let c = if st == "blocked" {
                    ink.p.warn
                } else {
                    ink.faint()
                };
                ui.add(
                    Label::new(RichText::new(note).font(ink.label(11.5)).color(c))
                        .selectable(false)
                        .wrap_mode(TextWrapMode::Wrap),
                );
            }
        });
    });
}

fn reminder(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let phases = n.arr("phases");
    let open: Vec<&Value> = phases
        .iter()
        .flat_map(items_of)
        .filter(|it| !matches!(status_of(it), "done" | "dropped"))
        .collect();
    if open.is_empty() {
        return;
    }
    let warn = ink.p.warn;
    egui::Frame::new()
        .fill(ink.wash(warn))
        .stroke(Stroke::new(1.0, ink.edge(warn)))
        .corner_radius(CornerRadius {
            nw: 0,
            ne: 8,
            se: 0,
            sw: 8,
        })
        .inner_margin(Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                glyphs::icon(ui, "warn", 13.0, warn, Sense::hover());
                let head = format!(
                    "{} incomplete {}",
                    open.len(),
                    if open.len() == 1 { "todo" } else { "todos" }
                );
                lab(ui, ink, &head, 12.0, ink.strong());
                if let Some(note) = n.get("note").map(tplain).filter(|s| !s.trim().is_empty()) {
                    render::badge(cx, ui, &note, Some("warning"));
                }
            });
            for it in open {
                item_row(cx, ui, it, false, false);
            }
        });
}

fn hud(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let phases = n.arr("phases");
    let (closed, total) = counts(phases);
    if total == 0 {
        return;
    }
    let current = phases
        .iter()
        .flat_map(items_of)
        .find(|it| status_of(it) == "active")
        .or_else(|| {
            phases
                .iter()
                .flat_map(items_of)
                .find(|it| status_of(it) == "pending")
        })
        .and_then(|it| it.get("text").map(tplain))
        .map(|t| one_line(&t))
        .unwrap_or_default();
    let strong = Base::new(ink.label(12.0), ink.strong(), ink);
    let dim = Base::new(ink.label(12.0), ink.dim(), ink);
    let mut job = LayoutJob::default();
    job.append(
        &format!("Todo {closed}/{total}"),
        0.0,
        strong.format_plain(),
    );
    if !current.is_empty() {
        job.append(&format!("  ·  {current}"), 0.0, dim.format_plain());
    }
    job.wrap.max_rows = 1;
    job.wrap.max_width = 360.0;
    let g = ui.painter().layout_job(job);
    let size = vec2(g.size().x + 44.0, 26.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let id = ui.scope_id().with(("hud", n.id.as_str()));
    let resp = ui.interact(rect, id, Sense::click());
    let t = ui
        .ctx()
        .animate_bool_with_time(id.with("hov"), resp.hovered(), 0.1);
    let pts = paint::parallelogram(rect, 7.0);
    let (a, b) = ink.head();
    paint::fill_h(ui.painter(), &pts, a, b);
    deco::gradient_outline(ui.painter(), pts, 1.0, &ink.rule_stops(), vec2(1.0, 0.0));
    paint::pop(ui.painter(), rect, 6, ink, t);
    let cy = rect.center().y;
    render_half(cx, ui, pos2(rect.left() + 20.0, cy));
    ui.painter().galley(
        pos2(rect.left() + 34.0, cy - g.size().y / 2.0),
        g,
        ink.strong(),
    );
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    if resp.clicked() {
        cx.send(super::events::activate(cx.sf(), &n.id, "hud"));
    }
    let frame = egui::Frame::new()
        .fill(ink.card())
        .stroke(Stroke::new(1.0, ink.line()))
        .corner_radius(CARD_CORNER)
        .inner_margin(Margin::same(10))
        .shadow(egui::epaint::Shadow {
            offset: [0, 6],
            blur: 18,
            spread: 0,
            color: Color32::from_black_alpha(if ink.dark { 140 } else { 40 }),
        });
    egui::Popup::from_toggle_button_response(&resp)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .align(egui::RectAlign::TOP_START)
        .gap(6.0)
        .frame(frame)
        .width(380.0)
        .show(|ui| {
            let tx = Tx {
                font: ink.label(12.5),
                color: ink.text(),
                line: None,
            };
            cx.with_tx(tx, || full(cx, ui, n, false));
        });
}

/// The pill's ring at `at`.
fn render_half(cx: &Cx, ui: &mut Ui, at: Pos2) {
    let ink = cx.ink;
    let r = glyphs::square(at, 13.0);
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(r.expand(2.0))
            .layout(Layout::left_to_right(Align::Center)),
    );
    render::ring(cx, &mut child, Some(0.5), 13.0, ink.b);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const TOOL_STATUSES: [&str; 5] = ["pending", "running", "done", "error", "cancelled"];
    const AGENT_STATUSES: [&str; 7] = [
        "pending", "running", "done", "failed", "aborted", "idle", "parked",
    ];
    const ITEM_STATUSES: [&str; 5] = ["pending", "active", "done", "dropped", "blocked"];

    #[test]
    fn head_texts_come_from_the_props_of_a_recorded_tool() {
        use crate::chat_view::snap::{
            Snap,
            tests::{fixture_mirror, sync_doc},
        };
        let m = fixture_mirror(usize::MAX);
        let mut snap = Snap::default();
        sync_doc(&mut snap, &m);
        let main = snap.region("main").expect("main region");
        let tool = snap
            .find(main, &|c| c.kind == "tool" && c.get("target").is_some())
            .expect("a tool with a target");
        let h = HeadText::of(tool);
        assert_eq!(h.target, tplain(tool.get("target").expect("target")));
        assert_eq!(h.meta, meta_line(tool.arr("meta")));
        assert_eq!(h, HeadText::of(tool), "a pure function of the props");
    }

    #[test]
    fn every_status_of_the_catalog_has_a_style() {
        for s in TOOL_STATUSES {
            let l = tool_look(s).unwrap_or_else(|| panic!("tool status {s}"));
            assert!(!l.label.is_empty(), "{s} has a word");
        }
        for s in AGENT_STATUSES {
            let l = agent_look(s).unwrap_or_else(|| panic!("agent status {s}"));
            assert!(!l.label.is_empty(), "{s} has a word");
        }
        for s in ITEM_STATUSES {
            assert!(item_look(s).is_some(), "item status {s}");
        }
        assert!(tool_look("bogus").is_none());
        assert!(agent_look("").is_none());
        assert!(item_look("running").is_none());
    }

    #[test]
    fn only_running_things_spin_and_failures_are_red() {
        assert_eq!(tool_look("running").map(|l| l.glyph), Some(Glyph::Spin));
        assert_eq!(agent_look("running").map(|l| l.glyph), Some(Glyph::Spin));
        for s in TOOL_STATUSES.iter().filter(|s| **s != "running") {
            assert_ne!(tool_look(s).map(|l| l.glyph), Some(Glyph::Spin), "{s}");
        }
        assert_eq!(tool_look("error").map(|l| l.hue), Some(Hue::Bad));
        assert_eq!(agent_look("failed").map(|l| l.hue), Some(Hue::Bad));
        assert_eq!(tool_look("cancelled").map(|l| l.hue), Some(Hue::Dim));
        assert_eq!(item_look("blocked").map(|l| l.hue), Some(Hue::Warn));
        // The active item is a static ring unless the list is live.
        assert_eq!(item_look("active").map(|l| l.glyph), Some(Glyph::Half));
    }

    #[test]
    fn every_icon_a_look_names_is_a_known_glyph() {
        let looks = TOOL_STATUSES
            .iter()
            .filter_map(|s| tool_look(s))
            .chain(AGENT_STATUSES.iter().filter_map(|s| agent_look(s)))
            .chain(ITEM_STATUSES.iter().filter_map(|s| item_look(s)));
        for l in looks {
            if let Glyph::Icon(name) = l.glyph {
                // `circle` maps to the same neutral dot unknown names fall back to.
                assert!(
                    glyphs::known(name) || name == "circle",
                    "{name} is not a Lucide glyph we map"
                );
            }
        }
    }

    #[test]
    fn head_status_composes_time_and_word() {
        let running = tool_look("running").unwrap_or(FALLBACK);
        let done = tool_look("done").unwrap_or(FALLBACK);
        let err = tool_look("error").unwrap_or(FALLBACK);
        assert_eq!(head_status(running, Some(8200.0)), "8.2s · RUNNING");
        assert_eq!(head_status(done, Some(1200.0)), "1.2s");
        assert_eq!(head_status(done, None), "");
        assert_eq!(head_status(err, Some(300.0)), "300ms · ERROR");
        assert_eq!(head_status(err, None), "ERROR");
        let parked = agent_look("parked").unwrap_or(FALLBACK);
        assert_eq!(head_status(parked, None), "PARKED");
    }

    #[test]
    fn previews_parse_and_clamp() {
        assert_eq!(preview_of(None), Preview::None);
        assert_eq!(preview_of(Some(&json!("none"))), Preview::None);
        assert_eq!(preview_of(Some(&json!("children"))), Preview::Children);
        assert_eq!(preview_of(Some(&json!({"lines": 4}))), Preview::Lines(4));
        assert_eq!(preview_of(Some(&json!({"tail": 12}))), Preview::Tail(12));
        assert_eq!(preview_of(Some(&json!({"other": 1}))), Preview::None);
        assert_eq!(clamp_rows(Preview::Lines(4)), Some((4, false)));
        assert_eq!(clamp_rows(Preview::Tail(12)), Some((12, true)));
        assert_eq!(clamp_rows(Preview::Children), None);
        assert_eq!(clamp_rows(Preview::None), None);
    }

    #[test]
    fn paths_split_and_shorten_from_the_front() {
        assert_eq!(split_path("~/proj/notes.txt"), ("~/proj/", "notes.txt"));
        assert_eq!(split_path("a.rs:12"), ("", "a.rs:12"));
        assert_eq!(split_path("src/a.rs:5-9"), ("src/", "a.rs:5-9"));
        assert_eq!(shorten_dir("~/proj/deep/dir/", 40), "~/proj/deep/dir/");
        assert_eq!(shorten_dir("~/proj/deep/dir/", 6), "…/dir/");
        assert_eq!(shorten_dir("~/proj/deep/dir/", 3), "");
        assert!(shorten_dir("~/proj/deep/dir/", 9).starts_with('…'));
        assert_eq!(shorten_dir("~/proj/deep/dir/", 9).chars().count(), 9);
    }

    #[test]
    fn commands_collapse_to_their_first_line() {
        assert_eq!(one_line("cargo test"), "cargo test");
        assert_eq!(one_line("  echo a\n echo b"), "echo a …");
        assert_eq!(one_line(""), "");
    }

    #[test]
    fn numbers_read_short() {
        assert_eq!(fmt_tokens(950.0), "950");
        assert_eq!(fmt_tokens(48_100.0), "48.1k");
        assert_eq!(fmt_tokens(2_500_000.0), "2.5M");
        assert_eq!(fmt_cost(0.03), "$0.03");
        assert_eq!(fmt_cost(0.004), "$0.004");
        assert_eq!(fmt_cost(12.4), "$12.40");
        assert_eq!(fmt_cost(0.0), "$0.00");
    }

    #[test]
    fn agent_stats_compose_in_order() {
        let stats =
            json!({"tools": 3, "requests": 12, "tokens": 48100, "context": 0.42, "cost": 0.03});
        let m = stats.as_object().cloned().unwrap_or_default();
        assert_eq!(
            stats_line(&m),
            "3 tools · 12 req · 48.1k tok · 42% ctx · $0.03"
        );
        let one = json!({"tools": 1, "contextLabel": "81K/200K"});
        let m = one.as_object().cloned().unwrap_or_default();
        assert_eq!(stats_line(&m), "1 tool · 81K/200K ctx");
        assert_eq!(stats_line(&Props::new()), "");
    }

    #[test]
    fn retry_counts_down() {
        assert_eq!(retry_line(2.0, 3.0, 3400.0), "retry 2/3 in 3.4s");
        assert_eq!(retry_line(2.0, 3.0, 0.0), "retry 2/3");
    }

    #[test]
    fn meta_and_counts() {
        let meta = [
            json!("+8 −1"),
            json!(""),
            json!([{"t": "→ new/path", "s": "dim"}]),
        ];
        assert_eq!(meta_line(&meta), "+8 −1 · → new/path");
        let phases = [
            json!({"id": "p0", "title": "A", "items": [
                {"id": "a", "text": "x", "status": "done"},
                {"id": "b", "text": "y", "status": "dropped"},
                {"id": "c", "text": "z", "status": "active"},
            ]}),
            json!({"id": "p1", "title": "B", "items": [
                {"id": "d", "text": "w", "status": "blocked"},
                {"id": "e", "text": "v"},
            ]}),
        ];
        assert_eq!(counts(&phases), (2, 5));
        assert_eq!(counts(&[]), (0, 0));
        assert_eq!(counts(&phases[1..]), (0, 2));
    }

    #[test]
    fn head_band_stays_inside_its_rect_and_leans() {
        let rect = Rect::from_min_size(pos2(10.0, 20.0), vec2(300.0, 32.0));
        for (r_tr, r_bl) in [(0.0, 0.0), (9.0, 0.0), (9.0, 9.0)] {
            let pts = head_band(rect, 14.0, r_tr, r_bl);
            assert!(pts.len() >= 4);
            assert_eq!(pts[0], rect.left_top());
            for p in &pts {
                assert!(rect.expand(0.01).contains(*p), "{p:?} outside {rect:?}");
            }
            // The bottom edge ends 14 px short of the right edge: the slant.
            let right_bottom = pts
                .iter()
                .filter(|p| (p.y - rect.bottom()).abs() < 0.01)
                .map(|p| p.x)
                .fold(f32::MIN, f32::max);
            assert!((right_bottom - (rect.right() - 14.0)).abs() < 0.01);
            // Nothing past the slanted line.
            for p in &pts {
                let lean = rect.right() - 14.0 * (p.y - rect.top()) / rect.height();
                assert!(p.x <= lean + 0.02, "{p:?} spills past the slant");
            }
        }
        let sharp = head_band(rect, 14.0, 0.0, 0.0);
        assert_eq!(sharp.len(), 4);
        assert_eq!(sharp[1], rect.right_top());
        let rounded = head_band(rect, 14.0, 9.0, 9.0);
        assert!(!rounded.contains(&rect.left_bottom()));
    }

    #[test]
    fn text_values_flatten_to_plain() {
        assert_eq!(tplain(&json!("a\u{f121}b")), "ab");
        assert_eq!(tplain(&json!([{"t": "x", "s": "dim"}, {"t": "y"}])), "xy");
        assert_eq!(tplain(&json!(5)), "");
    }
}
