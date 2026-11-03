//! Terminal output as styled text: a pure SGR / OSC 8 / `\r` parser, its layout job, and the
//! `ansi` / `rows` node renderers. The 16 base colours come from the active theme (so they read
//! on light styles too); 256-colour and truecolour values are taken as the program sent them.
//!
//! Also home of the preview clamp ("N more lines") that `code` blocks share.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::str::Chars;
use std::sync::Arc;

use egui::text::LayoutJob;
use egui::{Color32, Galley, Id, Rect, Response, Sense, Stroke, Ui, pos2, vec2};
use serde_json::Value;

use super::ctx::{Cx, FoldOverride, RowEnds};
use super::glyphs;
use super::paint;
use super::render::{self, Link};
use super::snap::SNode;
use super::style::{Ink, LABEL};
use super::text::{self, Base};
use super::tsel::RunOpts;
use crate::theme::mix;

/// Most output kept per node (the tail wins): a runaway log must not stall every frame.
const MAX_BYTES: usize = 1 << 20;
/// Most lines drawn at once of an unclamped block (the tail is kept).
const MAX_SHOWN: usize = 400;
/// Lines of a `preview: "auto"` clamp.
pub const AUTO_LINES: usize = 12;

// ---- parse ----------------------------------------------------------------------------------

/// A colour as the program asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    /// 0–7 base, 8–15 bright, 16–231 cube, 232–255 greys.
    Idx(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Style {
    fg: Option<Col>,
    bg: Option<Col>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    reverse: bool,
    strike: bool,
}

#[derive(Debug, Clone, Copy)]
struct Cell {
    ch: char,
    style: Style,
    /// 0 = none, else index into `Doc::urls` plus one.
    link: u32,
}

/// A stretch of text in one style.
#[derive(Debug, Clone)]
struct Run {
    text: String,
    style: Style,
    link: u32,
}

/// Parsed output: lines of runs and the hyperlink targets they refer to.
#[derive(Debug, Clone, Default)]
pub struct Doc {
    lines: Vec<Vec<Run>>,
    urls: Vec<String>,
}

impl Doc {
    pub fn len(&self) -> usize {
        self.lines.len()
    }
}

#[derive(Default)]
struct Parser {
    cells: Vec<Cell>,
    col: usize,
    style: Style,
    link: u32,
    doc: Doc,
}

impl Parser {
    fn put(&mut self, ch: char) {
        while self.cells.len() < self.col {
            self.cells.push(Cell {
                ch: ' ',
                style: Style::default(),
                link: 0,
            });
        }
        let cell = Cell {
            ch,
            style: self.style,
            link: self.link,
        };
        if self.col < self.cells.len() {
            self.cells[self.col] = cell;
        } else {
            self.cells.push(cell);
        }
        self.col += 1;
    }

    /// Ends the current line: its cells become runs.
    fn flush(&mut self) {
        let mut runs: Vec<Run> = Vec::new();
        for c in self.cells.drain(..) {
            match runs.last_mut() {
                Some(r) if r.style == c.style && r.link == c.link => r.text.push(c.ch),
                _ => runs.push(Run {
                    text: c.ch.to_string(),
                    style: c.style,
                    link: c.link,
                }),
            }
        }
        self.doc.lines.push(runs);
        self.col = 0;
    }

    fn escape(&mut self, it: &mut Chars) {
        match it.clone().next() {
            Some('[') => {
                it.next();
                self.csi(it);
            }
            Some(']') => {
                it.next();
                self.osc(it);
            }
            // DCS, SOS, PM, APC: a string up to its terminator.
            Some('P' | 'X' | '^' | '_') => {
                it.next();
                skip_string(it);
            }
            // nF: intermediates, then one final byte (`ESC ( B`).
            Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
                while matches!(it.clone().next(), Some(c) if ('\u{20}'..='\u{2f}').contains(&c)) {
                    it.next();
                }
                if matches!(it.clone().next(), Some(c) if ('\u{30}'..='\u{7e}').contains(&c)) {
                    it.next();
                }
            }
            Some(c) if ('\u{30}'..='\u{7e}').contains(&c) => {
                it.next();
            }
            _ => {}
        }
    }

    fn csi(&mut self, it: &mut Chars) {
        let mut params = String::new();
        let fin = loop {
            match it.clone().next() {
                Some(c) if ('\u{20}'..='\u{3f}').contains(&c) => {
                    params.push(c);
                    it.next();
                }
                Some(c) if ('\u{40}'..='\u{7e}').contains(&c) => {
                    it.next();
                    break Some(c);
                }
                _ => break None,
            }
        };
        match fin {
            Some('m')
                if params
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == ';' || c == ':') =>
            {
                self.sgr(&params);
            }
            // Erase in line: what `\r` progress bars use to clear their tail.
            Some('K') if params.chars().all(|c| c.is_ascii_digit()) => match num(&params) {
                0 => self.cells.truncate(self.col),
                1 => {
                    for c in self.cells.iter_mut().take(self.col) {
                        *c = Cell {
                            ch: ' ',
                            style: Style::default(),
                            link: 0,
                        };
                    }
                }
                2 => self.cells.clear(),
                _ => {}
            },
            _ => {}
        }
    }

    fn osc(&mut self, it: &mut Chars) {
        let mut payload = String::new();
        loop {
            let mut look = it.clone();
            match look.next() {
                None => return,
                Some('\u{7}' | '\u{9c}') => {
                    *it = look;
                    break;
                }
                Some('\u{1b}') => {
                    if look.clone().next() == Some('\\') {
                        look.next();
                        *it = look;
                        break;
                    }
                    // A new sequence starts: this one was cut off.
                    return;
                }
                Some(c) => {
                    *it = look;
                    payload.push(c);
                    if payload.len() > 4096 {
                        return;
                    }
                }
            }
        }
        if let Some(rest) = payload.strip_prefix("8;")
            && let Some((_, url)) = rest.split_once(';')
        {
            self.link = if url.is_empty() {
                0
            } else {
                match self.doc.urls.iter().position(|u| u == url) {
                    Some(i) => i as u32 + 1,
                    None => {
                        self.doc.urls.push(url.to_owned());
                        self.doc.urls.len() as u32
                    }
                }
            };
        }
    }

    fn sgr(&mut self, params: &str) {
        if params.is_empty() {
            self.style = Style::default();
            return;
        }
        let groups: Vec<&str> = params.split(';').collect();
        let mut i = 0;
        while i < groups.len() {
            let g = groups[i];
            i += 1;
            let mut sub = g.split(':');
            let head = num(sub.next().unwrap_or_default());
            let s = &mut self.style;
            match head {
                0 => *s = Style::default(),
                1 => s.bold = true,
                2 => s.dim = true,
                3 => s.italic = true,
                4 => s.underline = sub.next().map(num) != Some(0),
                7 => s.reverse = true,
                9 => s.strike = true,
                21 => s.underline = true,
                22 => {
                    s.bold = false;
                    s.dim = false;
                }
                23 => s.italic = false,
                24 => s.underline = false,
                27 => s.reverse = false,
                29 => s.strike = false,
                30..=37 => s.fg = Some(Col::Idx((head - 30) as u8)),
                39 => s.fg = None,
                40..=47 => s.bg = Some(Col::Idx((head - 40) as u8)),
                49 => s.bg = None,
                90..=97 => s.fg = Some(Col::Idx((head - 90) as u8 + 8)),
                100..=107 => s.bg = Some(Col::Idx((head - 100) as u8 + 8)),
                38 | 48 => {
                    let (col, used) = if g.contains(':') {
                        let parts: Vec<u32> = g.split(':').map(num).collect();
                        (colon_colour(&parts), 0)
                    } else {
                        semicolon_colour(&groups[i..])
                    };
                    i += used;
                    if let Some(c) = col {
                        if head == 38 {
                            s.fg = Some(c);
                        } else {
                            s.bg = Some(c);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn num(s: &str) -> u32 {
    s.parse().unwrap_or(0)
}

/// `38;5;n` / `38;2;r;g;b` after the 38/48: the colour and how many groups it used.
fn semicolon_colour(rest: &[&str]) -> (Option<Col>, usize) {
    match rest.first().map(|s| num(s)) {
        Some(5) => (rest.get(1).map(|s| Col::Idx(num(s).min(255) as u8)), 2),
        Some(2) if rest.len() >= 4 => (
            Some(Col::Rgb(
                num(rest[1]).min(255) as u8,
                num(rest[2]).min(255) as u8,
                num(rest[3]).min(255) as u8,
            )),
            4,
        ),
        Some(2) => (None, rest.len()),
        _ => (None, 0),
    }
}

/// `38:5:n` / `38:2:r:g:b` / `38:2::r:g:b` (colour space slot) in one group.
fn colon_colour(parts: &[u32]) -> Option<Col> {
    match parts.get(1) {
        Some(5) => parts.get(2).map(|n| Col::Idx((*n).min(255) as u8)),
        Some(2) if parts.len() >= 5 => {
            let t = &parts[parts.len() - 3..];
            Some(Col::Rgb(
                t[0].min(255) as u8,
                t[1].min(255) as u8,
                t[2].min(255) as u8,
            ))
        }
        _ => None,
    }
}

fn skip_string(it: &mut Chars) {
    loop {
        let mut look = it.clone();
        match look.next() {
            None => return,
            Some('\u{7}' | '\u{9c}') => {
                *it = look;
                return;
            }
            Some('\u{1b}') => {
                if look.clone().next() == Some('\\') {
                    look.next();
                    *it = look;
                }
                return;
            }
            Some(_) => *it = look,
        }
    }
}

/// The last `max` bytes of `s`, cut at a char boundary.
fn tail_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut at = s.len() - max;
    while !s.is_char_boundary(at) {
        at += 1;
    }
    &s[at..]
}

/// Parses raw terminal output. Handles SGR, OSC 8, `\r` (overwrite from column 0), `\b`, `\n`,
/// tabs (4 spaces) and erase-in-line; every other escape sequence is dropped.
pub fn parse(text: &str) -> Doc {
    let cleaned = text::clean(text);
    let text = tail_bytes(&cleaned, MAX_BYTES);
    let mut p = Parser::default();
    let mut it = text.chars();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => p.escape(&mut it),
            '\n' => p.flush(),
            '\r' => p.col = 0,
            '\u{8}' => p.col = p.col.saturating_sub(1),
            '\t' => {
                for _ in 0..4 {
                    p.put(' ');
                }
            }
            c if c.is_control() => {}
            c => p.put(c),
        }
    }
    if !p.cells.is_empty() {
        p.flush();
    }
    p.doc
}

// ---- colours --------------------------------------------------------------------------------

/// The theme's 16 terminal colours plus what a run needs to resolve its own.
struct Pal {
    c: [Color32; 16],
    strong: Color32,
    default: Color32,
    card: Color32,
    link: Color32,
}

fn near(a: Color32, b: Color32) -> bool {
    let d = |x: u8, y: u8| (i32::from(x) - i32::from(y)).abs();
    d(a.r(), b.r()) + d(a.g(), b.g()) + d(a.b(), b.b()) < 60
}

/// The theme's 16 terminal colours (the chat's own ANSI palette; syntax highlighting maps its
/// scopes onto the same ones).
pub(super) fn palette16(ink: &Ink) -> [Color32; 16] {
    let p = ink.p;
    let strong = p.text_strong;
    let blue = p.accent;
    // Cyan is the gradient end; where a flat style makes it the accent too, lean it to green.
    let cyan = if near(ink.b, blue) {
        mix(ink.b, p.ok, 0.4)
    } else {
        ink.b
    };
    let magenta = if near(ink.a, blue) {
        mix(ink.a, p.danger, 0.45)
    } else {
        mix(ink.a, p.danger, 0.2)
    };
    let base = [
        mix(p.bg, p.text, 0.22),
        p.danger,
        p.ok,
        p.warn,
        blue,
        magenta,
        cyan,
        p.text,
    ];
    let mut c = [Color32::TRANSPARENT; 16];
    for (i, b) in base.iter().enumerate() {
        c[i] = *b;
        c[i + 8] = mix(*b, strong, 0.28);
    }
    c[8] = p.weak;
    c[15] = strong;
    c
}

impl Pal {
    fn new(ink: &Ink, default: Color32) -> Pal {
        Pal {
            c: palette16(ink),
            strong: ink.p.text_strong,
            default,
            card: ink.card(),
            link: ink.tone("accent"),
        }
    }

    fn xterm(&self, i: u8) -> Color32 {
        match i {
            0..=15 => self.c[usize::from(i)],
            16..=231 => {
                let n = i - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
                Color32::from_rgb(level(n / 36), level((n / 6) % 6), level(n % 6))
            }
            _ => {
                let g = 8 + 10 * (i - 232);
                Color32::from_rgb(g, g, g)
            }
        }
    }

    fn colour(&self, c: Col, bold: bool) -> Color32 {
        match c {
            Col::Idx(i) if bold && i < 8 => self.xterm(i + 8),
            Col::Idx(i) => self.xterm(i),
            Col::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        }
    }

    /// Foreground and (optional) background of a style.
    fn resolve(&self, st: &Style, linked: bool) -> (Color32, Option<Color32>) {
        let mut fg = match st.fg {
            Some(c) => self.colour(c, st.bold),
            None if linked => self.link,
            None if st.bold => self.strong,
            None => self.default,
        };
        let mut bg = st.bg.map(|c| self.colour(c, false));
        if st.reverse {
            let f = fg;
            fg = bg.unwrap_or(self.card);
            bg = Some(f);
        }
        if st.dim {
            fg = mix(fg, bg.unwrap_or(self.card), 0.5);
        }
        (fg, bg)
    }
}

// ---- layout ---------------------------------------------------------------------------------

/// The layout job of `doc`'s lines `from..to` and the links in it (char ranges of the job text).
fn build(ink: &Ink, base: &Base, doc: &Doc, from: usize, to: usize) -> (LayoutJob, Vec<Link>) {
    let pal = Pal::new(ink, base.color);
    let plain = base.format_plain();
    let mut job = LayoutJob::default();
    let mut links: Vec<Link> = Vec::new();
    let mut pos = 0usize;
    let to = to.min(doc.lines.len());
    for (li, line) in doc.lines[from.min(to)..to].iter().enumerate() {
        if li > 0 {
            job.append("\n", 0.0, plain.clone());
            pos += 1;
        }
        for run in line {
            let st = &run.style;
            let (fg, bg) = pal.resolve(st, run.link != 0);
            let stroke = Stroke::new(1.0, fg);
            let mut f = plain.clone();
            f.color = fg;
            f.background = bg.unwrap_or(Color32::TRANSPARENT);
            f.italics = st.italic;
            if st.underline || run.link != 0 {
                f.underline = stroke;
            }
            if st.strike {
                f.strikethrough = stroke;
            }
            job.append(&run.text, 0.0, f);
            let n = run.text.chars().count();
            if run.link != 0
                && let Some(url) = doc.urls.get(run.link as usize - 1)
            {
                match links.last_mut() {
                    Some(l) if l.to == pos && l.url == *url => l.to = pos + n,
                    _ => links.push(Link {
                        from: pos,
                        to: pos + n,
                        url: url.clone(),
                    }),
                }
            }
            pos += n;
        }
    }
    (job, links)
}

/// `text` as a layout job in the theme's terminal colours (SGR and `\r` aware, OSC 8 links styled
/// but not clickable here: [`show`] adds the click targets).
#[cfg(test)]
pub fn lines_job(ink: &Ink, base: &Base, text: &str) -> LayoutJob {
    let doc = parse(text);
    build(ink, base, &doc, 0, doc.len()).0
}

// ---- clamps ---------------------------------------------------------------------------------

/// Which lines of a block are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// The first `n`.
    Head(usize),
    /// The last `n`.
    Tail(usize),
    /// An opened block: at most [`MAX_SHOWN`] lines from the end the preview kept (`tail`), the
    /// rest stays reachable by "Show less" and is counted in a note.
    Window { tail: bool },
}

/// A node's preview clamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preview {
    pub lines: usize,
    /// Keep the end (output that follows), not the start.
    pub tail: bool,
}

fn pos_usize(v: &Value) -> Option<usize> {
    v.as_f64().filter(|x| *x >= 1.0).map(|x| x as usize)
}

/// `n`'s preview: its `preview` (`{lines}`, `{tail}` or `"auto"`; none inside an open
/// disclosure, which gives way) or, always, a plain `lines` / `tail` count prop.
pub fn preview(cx: &Cx, n: &SNode) -> Option<Preview> {
    if let Some(l) = n.get("tail").and_then(pos_usize) {
        return Some(Preview {
            lines: l,
            tail: true,
        });
    }
    if let Some(l) = n.get("lines").and_then(pos_usize) {
        return Some(Preview {
            lines: l,
            tail: false,
        });
    }
    let follow = n.flag("follow");
    let p = match n.get("preview")? {
        Value::String(s) if s == "auto" => Preview {
            lines: AUTO_LINES,
            tail: follow,
        },
        Value::Object(o) => {
            if let Some(l) = o.get("tail").and_then(pos_usize) {
                Preview {
                    lines: l,
                    tail: true,
                }
            } else {
                Preview {
                    lines: o.get("lines").and_then(pos_usize).unwrap_or(AUTO_LINES),
                    tail: follow,
                }
            }
        }
        _ => return None,
    };
    if open_disclosure(cx, n) {
        None
    } else {
        Some(p)
    }
}

/// Whether `n` sits in a collapsible card/tool/section that is currently open.
pub fn open_disclosure(cx: &Cx, n: &SNode) -> bool {
    let mut cur = n.parent.as_deref();
    while let Some(id) = cur {
        let Some(p) = cx.snap.get(id) else {
            break;
        };
        if matches!(p.kind.as_str(), "tool" | "card" | "section" | "agent") && p.flag("collapsible")
        {
            return !cx.collapsed(p);
        }
        cur = p.parent.as_deref();
    }
    false
}

/// Whether `n` sits below a node of `kind`.
pub fn inside(cx: &Cx, n: &SNode, kind: &str) -> bool {
    let mut cur = n.parent.as_deref();
    while let Some(id) = cur {
        let Some(p) = cx.snap.get(id) else {
            return false;
        };
        if p.kind == kind {
            return true;
        }
        cur = p.parent.as_deref();
    }
    false
}

fn more_key(n: &SNode) -> String {
    format!("{}\u{0}more", n.id)
}

/// The user opened `n`'s clamped lines ("N more lines").
pub fn more_open(cx: &Cx, n: &SNode) -> bool {
    let k = more_key(n);
    cx.loc(|l| l.folds.get(&k).is_some_and(|o| !o.collapsed))
}

/// Flips [`more_open`].
pub fn toggle_more(cx: &Cx, n: &SNode) {
    let open = more_open(cx, n);
    let k = more_key(n);
    cx.loc(|l| {
        l.folds.insert(
            k,
            FoldOverride {
                collapsed: open,
                gen_: 0,
            },
        );
    });
}

/// The `from..to` line range of `total` lines under `limit` (unclamped blocks keep their tail).
pub fn pick(total: usize, limit: Option<Limit>) -> (usize, usize) {
    match limit {
        Some(Limit::Head(n)) => (0, n.min(total)),
        Some(Limit::Tail(n)) => (total.saturating_sub(n), total),
        Some(Limit::Window { tail: false }) => (0, MAX_SHOWN.min(total)),
        Some(Limit::Window { tail: true }) | None => (total.saturating_sub(MAX_SHOWN), total),
    }
}

/// What sits next to a block: the chip that opens or closes its clamp and a note on lines that
/// stay out of view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Footer {
    /// "N more lines" / "N earlier lines": opens a clamped block.
    pub more: Option<String>,
    /// "Show less": closes an opened one.
    pub less: bool,
    /// Lines even an opened block does not draw.
    pub note: Option<String>,
}

/// The [`Footer`] of a block of `total` lines of which `before`/`after` are not drawn. `preview`
/// is the node's clamp, `expanded` whether the user opened it.
pub fn footer_plan(
    preview: Option<Preview>,
    expanded: bool,
    before: usize,
    after: usize,
    total: usize,
) -> Footer {
    let hidden = before + after;
    let mut f = Footer::default();
    match preview {
        Some(p) if !expanded => {
            if hidden > 0 {
                let word = if p.tail { "earlier" } else { "more" };
                let s = if hidden == 1 { "" } else { "s" };
                f.more = Some(format!("{hidden} {word} line{s}"));
            }
        }
        Some(p) => {
            f.less = total > p.lines;
            if hidden > 0 {
                let side = if before > 0 { "earlier" } else { "later" };
                f.note = Some(format!("… {hidden} {side} lines not shown"));
            }
        }
        None if hidden > 0 => f.note = Some(format!("… {hidden} earlier lines not shown")),
        None => {}
    }
    f
}

/// The chip under/over a clamped block: "⌄ 12 more lines" / "⌃ Show less". Allocates its own row.
pub fn more_button(cx: &Cx, ui: &mut Ui, id: Id, label: &str, up: bool) -> Response {
    let ink = cx.ink;
    let g = ui
        .painter()
        .layout_no_wrap(label.to_owned(), ink.label(LABEL), ink.dim());
    let size = vec2(g.size().x + 30.0, 22.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    more_button_at(cx, ui, rect, id, label, up)
}

/// [`more_button`] drawn into `rect`.
pub fn more_button_at(cx: &Cx, ui: &mut Ui, rect: Rect, id: Id, label: &str, up: bool) -> Response {
    let ink = cx.ink;
    let resp = ui
        .interact(rect, id, Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let t = ui
        .ctx()
        .animate_bool_with_time(id.with("hov"), resp.hovered(), 0.1);
    let color = mix(ink.dim(), ink.strong(), t);
    let p = ui.painter();
    paint::boxed(
        p,
        rect,
        4,
        mix(ink.chip(), ink.p.hover_bg, t),
        Stroke::new(1.0, mix(ink.line(), ink.p.hover_line, t)),
    );
    paint::pop(p, rect, 4, ink, t);
    glyphs::paint(
        p,
        if up { "chev-up" } else { "chev" },
        pos2(rect.left() + 11.0, rect.center().y),
        11.0,
        color,
    );
    let g = p.layout_no_wrap(label.to_owned(), ink.label(LABEL), color);
    p.galley(
        pos2(rect.left() + 22.0, rect.center().y - g.size().y / 2.0),
        g,
        color,
    );
    resp
}

// ---- cache ----------------------------------------------------------------------------------

struct Entry {
    gen_: u64,
    sig: u64,
    range: (usize, usize),
    rows: usize,
    doc: Doc,
    job: LayoutJob,
    links: Arc<[Link]>,
    /// The laid-out job at one width and scale, until the range, theme or fonts change.
    galley: Option<Slot>,
    used: u64,
}

/// A laid-out [`Entry`].
struct Slot {
    width: u32,
    ppp: u32,
    galley: Arc<Galley>,
}

/// Parsed output per node, keyed by (node id, generation); the job per (theme, range).
#[derive(Default)]
pub struct Cache {
    map: HashMap<String, Entry>,
}

/// What [`Cache::view`] reports of a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct View {
    /// Lines hidden before / after the drawn ones.
    pub before: usize,
    pub after: usize,
    pub total: usize,
}

/// Fingerprint of everything in the theme and font that colours or sizes a job.
fn signature(ink: &Ink, base: &Base) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let p = ink.p;
    for c in [
        p.bg,
        p.text,
        p.text_strong,
        p.weak,
        p.danger,
        p.ok,
        p.warn,
        p.accent,
        p.surface,
        ink.a,
        ink.b,
        base.color,
    ] {
        c.hash(&mut h);
    }
    ink.dark.hash(&mut h);
    base.font.size.to_bits().hash(&mut h);
    base.font.family.hash(&mut h);
    base.line_height.map(f32::to_bits).hash(&mut h);
    h.finish()
}

impl Cache {
    #[allow(clippy::too_many_arguments)]
    pub fn view(
        &mut self,
        ink: &Ink,
        base: &Base,
        key: &str,
        gen_: u64,
        frame: u64,
        source: &dyn Fn() -> String,
        limit: Option<Limit>,
    ) -> Option<View> {
        if self.map.get(key).is_none_or(|e| e.gen_ != gen_) {
            if self.map.len() > 256 {
                self.map.retain(|_, e| e.used + 900 >= frame);
            }
            self.map.insert(
                key.to_owned(),
                Entry {
                    gen_,
                    sig: 0,
                    range: (usize::MAX, 0),
                    rows: 0,
                    doc: parse(&source()),
                    job: LayoutJob::default(),
                    links: Arc::from(Vec::new()),
                    galley: None,
                    used: frame,
                },
            );
        }
        let e = self.map.get_mut(key)?;
        e.used = frame;
        let total = e.doc.len();
        let range = pick(total, limit);
        let rows = match limit {
            Some(Limit::Head(n)) => n,
            _ => usize::MAX,
        };
        let sig = signature(ink, base);
        if e.sig != sig || e.range != range || e.rows != rows {
            let (mut job, links) = build(ink, base, &e.doc, range.0, range.1);
            job.wrap.break_anywhere = true;
            job.wrap.max_rows = rows;
            e.job = job;
            e.links = links.into();
            e.galley = None;
            e.sig = sig;
            e.range = range;
            e.rows = rows;
        }
        Some(View {
            before: range.0,
            after: total - range.1,
            total,
        })
    }

    /// The galley (and links) of `key`'s current view at wrap `width`, laid out by `layout` only
    /// when nothing cached fits. Call after [`Cache::view`].
    pub fn galley(
        &mut self,
        key: &str,
        width: f32,
        ppp: f32,
        layout: impl FnOnce(LayoutJob) -> Arc<Galley>,
    ) -> Option<(Arc<Galley>, Arc<[Link]>)> {
        let e = self.map.get_mut(key)?;
        let (w, p) = (width.to_bits(), ppp.to_bits());
        if e.galley.as_ref().is_none_or(|s| s.width != w || s.ppp != p) {
            let mut job = e.job.clone();
            if width.is_finite() && job.wrap.max_width > width {
                job.wrap.max_width = width;
            }
            e.galley = Some(Slot {
                width: w,
                ppp: p,
                galley: layout(job),
            });
        }
        let slot = e.galley.as_ref()?;
        Some((Arc::clone(&slot.galley), Arc::clone(&e.links)))
    }

    /// Drops every laid-out galley (the fonts or their atlas changed under them).
    pub fn forget_galleys(&mut self) {
        for e in self.map.values_mut() {
            e.galley = None;
        }
    }
}

// ---- nodes ----------------------------------------------------------------------------------

/// The ANSI text of a node: its `lines` array joined, else its `text`.
fn source(n: &SNode) -> String {
    let lines = n.arr("lines");
    if lines.is_empty() {
        return n.text().to_owned();
    }
    let mut s = String::new();
    for (i, l) in lines.iter().filter_map(Value::as_str).enumerate() {
        if i > 0 {
            s.push('\n');
        }
        s.push_str(l);
    }
    s
}

/// The text of an ANSI node without its escapes (what "Copy output" copies).
pub fn plain(cx: &Cx, n: &SNode) -> String {
    let ink = cx.ink;
    let base = Base::new(ink.mono(0.95), ink.text(), ink);
    let doc = parse(&source(n));
    build(ink, &base, &doc, 0, doc.len()).0.text
}

/// Kind `ansi`: tool output in the terminal font, selectable, clamped by its preview.
pub fn show(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let pv = preview(cx, n);
    draw(cx, ui, n, pv);
}

/// Kind `rows`: pre-rendered ANSI lines (migration fallback).
pub fn show_rows(cx: &Cx, ui: &mut Ui, n: &SNode) {
    draw(cx, ui, n, None);
}

fn draw(cx: &Cx, ui: &mut Ui, n: &SNode, pv: Option<Preview>) {
    let ink = cx.ink;
    let font = ink.mono(0.95);
    let color = n.tone().map_or_else(|| ink.text(), |t| ink.tone(t));
    let base = Base::new(font.clone(), color, ink).lined((font.size * 1.45).round());
    let expanded = pv.is_some() && more_open(cx, n);
    // The footer sits where the hidden lines are: above for a kept tail (and unclamped blocks,
    // which keep theirs), below for a kept head.
    let footer_top = pv.is_none_or(|p| p.tail);
    let limit = pv.map(|p| match (expanded, p.tail) {
        (false, true) => Limit::Tail(p.lines),
        (false, false) => Limit::Head(p.lines),
        (true, tail) => Limit::Window { tail },
    });
    let frame = cx.loc(|l| l.frame);
    let Some(view) = cx.loc(|l| {
        l.ansi
            .view(ink, &base, &n.id, n.gen_, frame, &|| source(n), limit)
    }) else {
        return;
    };
    if view.total == 0 {
        return;
    }
    let id = ui.scope_id().with(("ansi-more", n.id.as_str()));
    let plan = footer_plan(pv, expanded, view.before, view.after, view.total);
    let footer = |ui: &mut Ui| {
        if let Some(note) = &plan.note {
            ui.label(
                egui::RichText::new(note)
                    .font(ink.label(LABEL))
                    .color(ink.faint()),
            );
        }
        let label = match (&plan.more, plan.less) {
            (Some(m), _) => Some((m.as_str(), false)),
            (None, true) => Some(("Show less", true)),
            (None, false) => None,
        };
        if let Some((label, up)) = label
            && more_button(cx, ui, id, label, up).clicked()
        {
            toggle_more(cx, n);
        }
    };
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        if footer_top {
            footer(ui);
        }
        let ppp = ui.ctx().pixels_per_point();
        let width = ui.available_width();
        let laid = cx.loc(|l| {
            l.ansi
                .galley(&n.id, width, ppp, |job| ui.painter().layout_job(job))
        });
        if let Some((galley, links)) = laid {
            if !galley.rows.is_empty() {
                cx.note_rows(
                    ui.cursor().top(),
                    RowEnds::Even {
                        pitch: galley.size().y / galley.rows.len() as f32,
                        count: galley.rows.len(),
                    },
                );
            }
            let (_, url) = render::para_galley(cx, ui, galley, color, &links, RunOpts::LINE);
            if let Some(u) = url {
                cx.open(u);
            }
        }
        if !footer_top {
            footer(ui);
        }
    });
}

#[cfg(test)]
mod tests {
    use egui::{Context, FontId};

    use super::*;

    fn ink() -> Ink {
        crate::chat_view::style::test_ink()
    }

    fn base(ink: &Ink) -> Base {
        Base::new(FontId::monospace(13.0), ink.text(), ink)
    }

    struct R {
        text: String,
        fg: Color32,
        bg: Color32,
        italic: bool,
        underline: bool,
        strike: bool,
    }

    fn runs(job: &LayoutJob) -> Vec<R> {
        job.sections
            .iter()
            .map(|s| R {
                text: job.text[s.byte_range.start.0..s.byte_range.end.0].to_owned(),
                fg: s.format.color,
                bg: s.format.background,
                italic: s.format.italics,
                underline: s.format.underline != Stroke::NONE,
                strike: s.format.strikethrough != Stroke::NONE,
            })
            .collect()
    }

    #[test]
    fn sgr_colours_bold_dim_and_reset() {
        let ink = ink();
        let b = base(&ink);
        let pal = Pal::new(&ink, b.color);
        let job = lines_job(
            &ink,
            &b,
            "\x1b[31mred\x1b[0m plain \x1b[1;32mbold green\x1b[22m ok\x1b[0m \x1b[92mbright\x1b[m",
        );
        let r = runs(&job);
        assert_eq!(job.text, "red plain bold green ok bright");
        assert_eq!((r[0].text.as_str(), r[0].fg), ("red", ink.p.danger));
        assert_eq!((r[1].text.as_str(), r[1].fg), (" plain ", ink.text()));
        // Bold on a base colour reads as its bright variant.
        assert_eq!(r[2].text, "bold green");
        assert_eq!(r[2].fg, pal.c[10]);
        assert_ne!(r[2].fg, ink.p.ok);
        // Bold off restores the plain colour.
        assert_eq!((r[3].text.as_str(), r[3].fg), (" ok", ink.p.ok));
        assert_eq!(r[5].text, "bright");
        assert_eq!(r[5].fg, pal.c[10]);
    }

    #[test]
    fn sgr_attributes_and_their_off_codes() {
        let ink = ink();
        let b = base(&ink);
        let job = lines_job(
            &ink,
            &b,
            "\x1b[3;4;9mx\x1b[23;24;29my\x1b[2mdim\x1b[22mz\x1b[1mB\x1b[0m",
        );
        let r = runs(&job);
        assert!(r[0].italic && r[0].underline && r[0].strike);
        assert!(!r[1].italic && !r[1].underline && !r[1].strike);
        let card = ink.card();
        assert_eq!(r[2].fg, mix(ink.text(), card, 0.5));
        assert_eq!(r[3].fg, ink.text());
        // Bold with the default colour is the strong text colour.
        assert_eq!(r[4].fg, ink.strong());
    }

    #[test]
    fn reverse_swaps_foreground_and_background() {
        let ink = ink();
        let b = base(&ink);
        let job = lines_job(&ink, &b, "\x1b[7mrev\x1b[27m\x1b[31;42mpair\x1b[7mswap");
        let r = runs(&job);
        assert_eq!((r[0].fg, r[0].bg), (ink.card(), ink.text()));
        let pal = Pal::new(&ink, b.color);
        assert_eq!((r[1].fg, r[1].bg), (pal.c[1], pal.c[2]));
        assert_eq!((r[2].fg, r[2].bg), (pal.c[2], pal.c[1]));
    }

    #[test]
    fn carriage_return_overwrites_from_column_zero_and_keeps_the_tail() {
        let ink = ink();
        let b = base(&ink);
        assert_eq!(lines_job(&ink, &b, "abcdef\rXY").text, "XYcdef");
        assert_eq!(
            lines_job(&ink, &b, " 10%\r 50%\r100% done").text,
            "100% done"
        );
        assert_eq!(lines_job(&ink, &b, "keep\rno").text, "noep");
        // Erase-in-line after `\r` drops the stale tail.
        assert_eq!(lines_job(&ink, &b, "downloading\r\x1b[Kdone").text, "done");
        // CRLF is a plain line break.
        assert_eq!(lines_job(&ink, &b, "a\r\nb\r\n").text, "a\nb");
        // The overwriting text takes its own style, the tail keeps the old one.
        let r = runs(&lines_job(&ink, &b, "\x1b[31mabcd\x1b[0m\rXY"));
        assert_eq!((r[0].text.as_str(), r[0].fg), ("XY", ink.text()));
        assert_eq!((r[1].text.as_str(), r[1].fg), ("cd", ink.p.danger));
    }

    #[test]
    fn lines_tabs_and_private_use_glyphs() {
        let ink = ink();
        let b = base(&ink);
        assert_eq!(lines_job(&ink, &b, "a\n\nb").text, "a\n\nb");
        assert_eq!(lines_job(&ink, &b, "a\n").text, "a");
        assert_eq!(lines_job(&ink, &b, "a\n\n").text, "a\n");
        assert_eq!(lines_job(&ink, &b, "").text, "");
        assert_eq!(lines_job(&ink, &b, "a\tb").text, "a    b");
        assert_eq!(lines_job(&ink, &b, "x\u{f121}y").text, "xy");
        assert_eq!(lines_job(&ink, &b, "ab\x08c").text, "ac");
    }

    #[test]
    fn other_sequences_are_dropped() {
        let ink = ink();
        let b = base(&ink);
        let t = "\x1b[2J\x1b[?25l\x1b[1;1Hhi\x1b]0;title\x07!\x1b(B\x1bP1$r\x1b\\?\x1b[";
        assert_eq!(lines_job(&ink, &b, t).text, "hi!?");
    }

    #[test]
    fn colours_256_and_truecolour() {
        let ink = ink();
        let b = base(&ink);
        let job = lines_job(
            &ink,
            &b,
            "\x1b[38;5;196ma\x1b[38;5;21;48;5;232mb\x1b[38;2;10;20;30mc\x1b[48;2;1;2;3md\x1b[38:2::7:8:9me\x1b[38:5:46mf\x1b[38;5;16mg",
        );
        let r = runs(&job);
        assert_eq!(r[0].fg, Color32::from_rgb(255, 0, 0));
        assert_eq!(r[1].fg, Color32::from_rgb(0, 0, 255));
        assert_eq!(r[1].bg, Color32::from_rgb(8, 8, 8));
        assert_eq!(r[2].fg, Color32::from_rgb(10, 20, 30));
        // Background set later keeps the foreground set before.
        assert_eq!(r[3].fg, Color32::from_rgb(10, 20, 30));
        assert_eq!(r[3].bg, Color32::from_rgb(1, 2, 3));
        assert_eq!(r[4].fg, Color32::from_rgb(7, 8, 9));
        assert_eq!(r[5].fg, Color32::from_rgb(0, 255, 0));
        assert_eq!(r[6].fg, Color32::from_rgb(0, 0, 0));
        // Greys: 232 → 8, 255 → 238.
        let pal = Pal::new(&ink, b.color);
        assert_eq!(pal.xterm(255), Color32::from_rgb(238, 238, 238));
        assert_eq!(pal.xterm(59), Color32::from_rgb(95, 95, 95));
    }

    #[test]
    fn theme_colours_for_the_sixteen() {
        let ink = ink();
        let pal = Pal::new(&ink, ink.text());
        assert_eq!(pal.c[1], ink.p.danger);
        assert_eq!(pal.c[2], ink.p.ok);
        assert_eq!(pal.c[3], ink.p.warn);
        assert_eq!(pal.c[4], ink.p.accent);
        assert_eq!(pal.c[7], ink.p.text);
        assert_eq!(pal.c[15], ink.p.text_strong);
        assert_ne!(pal.c[5], pal.c[1]);
        assert_ne!(pal.c[6], pal.c[4]);
    }

    #[test]
    fn osc8_hyperlinks_become_link_ranges() {
        let ink = ink();
        let b = base(&ink);
        let doc = parse(
            "see \x1b]8;;https://x.dev/a\x1b\\docs\x1b]8;;\x1b\\ and \x1b]8;id=1;https://y.dev\x07more\x1b]8;;\x07!",
        );
        let (job, links) = build(&ink, &b, &doc, 0, doc.len());
        assert_eq!(job.text, "see docs and more!");
        assert_eq!(
            links,
            vec![
                Link {
                    from: 4,
                    to: 8,
                    url: "https://x.dev/a".into()
                },
                Link {
                    from: 13,
                    to: 17,
                    url: "https://y.dev".into()
                },
            ]
        );
        let r = runs(&job);
        assert!(r[1].underline);
        assert!(!r[0].underline);
    }

    #[test]
    fn link_ranges_count_chars_across_lines() {
        let ink = ink();
        let b = base(&ink);
        let doc = parse("é\n\x1b]8;;u\x07ñ\x1b]8;;\x07");
        let (job, links) = build(&ink, &b, &doc, 0, doc.len());
        assert_eq!(job.text, "é\nñ");
        assert_eq!((links[0].from, links[0].to), (2, 3));
    }

    #[test]
    fn clamps_pick_head_tail_and_cap_the_rest() {
        assert_eq!(pick(30, Some(Limit::Head(12))), (0, 12));
        assert_eq!(pick(5, Some(Limit::Head(12))), (0, 5));
        assert_eq!(pick(30, Some(Limit::Tail(12))), (18, 30));
        assert_eq!(pick(5, Some(Limit::Tail(12))), (0, 5));
        assert_eq!(pick(100, None), (0, 100));
        assert_eq!(pick(1000, None), (600, 1000));
    }

    #[test]
    fn an_opened_block_keeps_the_end_its_preview_kept() {
        // A head preview opens onto the first MAX_SHOWN lines, a tail preview onto the last.
        assert_eq!(
            pick(1000, Some(Limit::Window { tail: false })),
            (0, MAX_SHOWN)
        );
        assert_eq!(
            pick(1000, Some(Limit::Window { tail: true })),
            (1000 - MAX_SHOWN, 1000)
        );
        assert_eq!(pick(30, Some(Limit::Window { tail: false })), (0, 30));
        assert_eq!(pick(30, Some(Limit::Window { tail: true })), (0, 30));
    }

    fn pv(lines: usize, tail: bool) -> Option<Preview> {
        Some(Preview { lines, tail })
    }

    #[test]
    fn the_footer_opens_a_clamp_then_offers_show_less_and_counts_what_stays_out() {
        // Clamped: one chip naming the hidden side.
        let f = footer_plan(pv(12, true), false, 18, 0, 30);
        assert_eq!(f.more.as_deref(), Some("18 earlier lines"));
        assert!(!f.less && f.note.is_none());
        let f = footer_plan(pv(12, false), false, 0, 1, 13);
        assert_eq!(f.more.as_deref(), Some("1 more line"));
        // Opened and fully shown: only "Show less".
        let f = footer_plan(pv(12, false), true, 0, 0, 30);
        assert_eq!(
            f,
            Footer {
                more: None,
                less: true,
                note: None
            }
        );
        // Opened but capped: "Show less" stays reachable and the rest is counted, on the side
        // it is on.
        let f = footer_plan(pv(12, false), true, 0, 600, 1000);
        assert!(f.less && f.more.is_none());
        assert_eq!(f.note.as_deref(), Some("… 600 later lines not shown"));
        let f = footer_plan(pv(12, true), true, 600, 0, 1000);
        assert!(f.less);
        assert_eq!(f.note.as_deref(), Some("… 600 earlier lines not shown"));
        // A short block opened beyond its preview has nothing to close.
        assert!(!footer_plan(pv(12, false), true, 0, 0, 5).less);
        // Unclamped blocks only note the lines past the cap.
        assert_eq!(footer_plan(None, false, 0, 0, 5), Footer::default());
        assert_eq!(
            footer_plan(None, false, 600, 0, 1000).note.as_deref(),
            Some("… 600 earlier lines not shown")
        );
    }

    #[test]
    fn the_cache_reports_hidden_lines_and_rebuilds_on_a_new_generation() {
        let ink = ink();
        let b = base(&ink);
        let mut c = Cache::default();
        let text = (1..=10)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let v = c
            .view(&ink, &b, "n", 1, 0, &|| text.clone(), Some(Limit::Tail(3)))
            .expect("view");
        assert_eq!(c.map["n"].job.text, "8\n9\n10");
        assert_eq!((v.before, v.after, v.total), (7, 0, 10));
        let v = c
            .view(
                &ink,
                &b,
                "n",
                1,
                0,
                &|| unreachable!(),
                Some(Limit::Head(4)),
            )
            .expect("view");
        assert_eq!(c.map["n"].job.text, "1\n2\n3\n4");
        assert_eq!((v.before, v.after), (0, 6));
        let v = c
            .view(&ink, &b, "n", 2, 0, &|| "only".to_owned(), None)
            .expect("view");
        assert_eq!((c.map["n"].job.text.as_str(), v.total), ("only", 1));
    }

    #[test]
    fn a_laid_out_galley_is_reused_until_width_scale_or_range_change() {
        let ink = ink();
        let b = base(&ink);
        let ctx = Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut c = Cache::default();
            let text = "one\ntwo\nthree";
            let lay = |job: LayoutJob| ui.painter().layout_job(job);
            c.view(&ink, &b, "n", 1, 0, &|| text.to_owned(), None);
            let (g1, _) = c.galley("n", 400.0, 1.0, lay).expect("galley");
            let (g2, _) = c
                .galley("n", 400.0, 1.0, |_| unreachable!("cached"))
                .expect("galley");
            assert!(Arc::ptr_eq(&g1, &g2));
            let (g3, _) = c.galley("n", 300.0, 1.0, lay).expect("galley");
            assert!(!Arc::ptr_eq(&g1, &g3), "another width lays out again");
            // A new range (and an explicit forget) drop it as well.
            c.view(
                &ink,
                &b,
                "n",
                1,
                1,
                &|| unreachable!(),
                Some(Limit::Head(1)),
            );
            let (g4, _) = c.galley("n", 300.0, 1.0, lay).expect("galley");
            assert!(!Arc::ptr_eq(&g3, &g4));
            // After a forget the job is laid out again (egui's own cache may hand back the same
            // galley, so count the calls instead).
            c.forget_galleys();
            let mut laid = 0;
            c.galley("n", 300.0, 1.0, |job| {
                laid += 1;
                lay(job)
            })
            .expect("galley");
            assert_eq!(laid, 1);
        })
        .textures_delta
        .clear();
    }
}
