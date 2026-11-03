//! The transcript's text selection, owned by OmpDeck instead of egui's label selection.
//!
//! egui drops a label selection as soon as one of its endpoint widgets is not painted in a frame,
//! and the transcript is virtualized: an item far out of view is never laid out. So the selection
//! is kept as two *positions* in the document, and the text between them is read from the items
//! themselves, laid out or not.
//!
//! **Position.** A selectable galley is a *run*. Its [`RunKey`] is `(ord, seq)`: `ord` is the
//! pre-order index of the node being rendered ([`SNode::ord`](super::snap::SNode::ord), valid for
//! one document revision) and `seq` the run's index within that node, counted by [`Cx::seqs`] over
//! everything the renderers visit, painted or culled, so it does not depend on what is on screen.
//! Runs order by `(ord, seq)`, which is document order. An endpoint is `(node id, seq, char)`: the
//! node id survives revisions that renumber `ord`, and a deleted node clamps the endpoint to its
//! successor in document order.
//!
//! **Painting and hit testing.** [`Cx::text_run`] paints a run (with the selected part
//! highlighted) and registers it for the frame ([`Tsel::runs`]); the pointer handler hits the
//! previous frame's registry, so what is under the pointer is what the reader saw.
//!
//! **Copy.** The text of the selection is not read from the registry (it only holds what is on
//! screen) but collected by rendering every item between the endpoints once more, invisibly, with
//! a clip rect that covers everything ([`copy`]). The renderers run the same code they paint with,
//! so their text and the selection cannot drift apart; folds, previews and clamps show the same
//! text the reader sees.

use std::sync::Arc;

use egui::text::CCursor;
use egui::text_selection::{CCursorRange, visuals::paint_text_selection};
use egui::{Color32, Context, CursorIcon, Galley, LayerId, Pos2, Rect, Ui, UiBuilder, pos2, vec2};

use super::ctx::Cx;
use super::select;
use super::snap::{SNode, Snap};
use super::transcript;

/// Pixels the pointer moves before a press becomes a drag.
const DRAG_SLOP: f32 = 3.0;
/// Seconds and pixels within which a second press counts as a double click.
const MULTI_CLICK: (f64, f32) = (0.5, 8.0);
/// Height of the band at the top and bottom of the transcript in which a drag scrolls it.
const EDGE: f32 = 28.0;
/// Scroll speed per pixel the pointer is past the edge band (px/s), and its limit.
const SCROLL_PER_PX: f32 = 14.0;
const SCROLL_MAX: f32 = 2600.0;
/// Width of the scrollbar strip at the right edge, which a press never starts a selection in.
const SCROLLBAR: f32 = 14.0;

/// Where a run sits in the document: the node's pre-order index, then the run within the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunKey {
    pub ord: u32,
    pub seq: u32,
}

/// What copying puts between a run and the one before it (within one node).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sep {
    Line,
    Block,
    Tab,
}

/// A list marker that precedes a run's text in a copy (it is painted apart from the run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lead {
    None,
    Bullet,
    Num(u64),
    Task(bool),
    /// A unified diff row's `+`, `-` or space.
    Diff(char),
}

impl Lead {
    fn push(self, out: &mut String) {
        use std::fmt::Write;
        match self {
            Lead::None => {}
            Lead::Bullet => out.push_str("• "),
            Lead::Num(n) => {
                let _ = write!(out, "{n}. ");
            }
            Lead::Task(done) => out.push_str(if done { "[x] " } else { "[ ] " }),
            Lead::Diff(c) => out.push(c),
        }
    }
}

/// How a run takes part in selection.
#[derive(Debug, Clone, Copy)]
pub struct RunOpts {
    /// Between this run and the previous run of the same node.
    pub sep: Sep,
    pub lead: Lead,
    /// Leading spaces of the plain text that the galley leaves out (a wrapped code row hangs its
    /// continuation by them): plain index = galley index + `skip`.
    pub skip: usize,
    /// Painted with the selection but never selected or copied (the streaming caret).
    pub inert: bool,
    /// The column of a multi-column block (split diff) the run sits in, 1-based (0: none): a
    /// drag that starts in a column and ends below its last row stays in that column.
    pub lane: u8,
}

impl RunOpts {
    pub const LINE: RunOpts = RunOpts::sep(Sep::Line);
    pub const BLOCK: RunOpts = RunOpts::sep(Sep::Block);
    pub const INERT: RunOpts = RunOpts {
        inert: true,
        ..RunOpts::LINE
    };

    pub const fn sep(sep: Sep) -> RunOpts {
        RunOpts {
            sep,
            lead: Lead::None,
            skip: 0,
            inert: false,
            lane: 0,
        }
    }

    pub const fn lead(mut self, lead: Lead) -> RunOpts {
        self.lead = lead;
        self
    }

    pub const fn skip(mut self, skip: usize) -> RunOpts {
        self.skip = skip;
        self
    }

    pub const fn lane(mut self, lane: u8) -> RunOpts {
        self.lane = lane;
        self
    }
}

/// A position in the plain text of a run.
type At = (RunKey, usize);

/// The selection, resolved for this frame: `from <= to`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    from: At,
    to: At,
}

/// One end of the selection as the user made it.
#[derive(Debug, Clone, PartialEq)]
struct Point {
    node: String,
    /// `ord` of `node` when last seen.
    ord: u32,
    /// The node that follows `node` and its subtree in document order when last seen (empty: the
    /// end): where the position goes when `node` is deleted.
    after: String,
    seq: u32,
    ch: usize,
}

/// The first node after `n` and everything under it, in document order.
fn after<'a>(snap: &'a Snap, n: &'a SNode) -> Option<&'a str> {
    let mut n = n;
    loop {
        let parent = snap.parent_of(&n.id)?;
        let at = parent.children.iter().position(|c| *c == n.id)?;
        if let Some(next) = parent.children.get(at + 1) {
            return Some(next);
        }
        n = parent;
    }
}

/// The last node of `n`'s subtree in document order.
fn last_in<'a>(snap: &'a Snap, n: &'a SNode) -> &'a SNode {
    let mut last = n;
    while let Some(c) = last.children.last().and_then(|id| snap.get(id)) {
        last = c;
    }
    last
}

/// A galley painted this frame.
struct Reg {
    key: RunKey,
    galley: Arc<Galley>,
    pos: Pos2,
    skip: usize,
    lane: u8,
}

impl Reg {
    fn rect(&self) -> Rect {
        Rect::from_min_size(self.pos, self.galley.size())
    }

    fn text(&self) -> String {
        let mut s = " ".repeat(self.skip);
        s.push_str(self.galley.text());
        s
    }
}

/// A run's text collected for a copy.
struct Got {
    key: RunKey,
    /// Index of the transcript item it belongs to.
    item: u32,
    sep: Sep,
    lead: Lead,
    text: String,
}

/// A press in progress.
struct Down {
    origin: Pos2,
    hit: Option<At>,
    locked: bool,
    /// The column the press landed in ([`RunOpts::lane`]).
    lane: u8,
}

/// What the pointer handler needs to know about the frame's layout.
pub struct Area {
    /// The whole chat view: a press anywhere in it ends a selection.
    pub chat: Rect,
    /// The part of it the transcript is visible in (above the dock).
    pub view: Rect,
    pub layer: LayerId,
}

#[derive(Default)]
pub struct Tsel {
    anchor: Option<Point>,
    focus: Option<Point>,
    /// The selection resolved against this frame's document.
    span: Option<Span>,
    /// Runs painted this frame and last frame (the pointer hits the latter).
    runs: Vec<Reg>,
    prev: Vec<Reg>,
    /// The runs being collected for a copy while one is made.
    got: Option<Vec<Got>>,
    item: u32,
    /// `(ord, next seq)` of the node being rendered.
    scope: (u32, u32),
    epoch: u64,
    down: Option<Down>,
    dragging: bool,
    /// Pre-order index of the transcript's last node: what comes after (the composer, layers)
    /// is chrome, never selected.
    end: u32,
    clicks: Option<(f64, Pos2, u8)>,
    /// Pixels the drag scrolls the transcript by this frame (negative: up), and the offset the
    /// transcript was last drawn at.
    pub scroll_dy: f32,
    pub scroll_y: f32,
    /// A copy was asked for: [`copy`] runs once the frame's context exists.
    pub want_copy: bool,
    /// The transcript's visible rect last frame (where a right click opens the menu).
    pub view: Option<Rect>,
}

impl Tsel {
    pub fn has_selection(&self) -> bool {
        self.span.is_some()
    }

    pub fn clear(&mut self) {
        self.anchor = None;
        self.focus = None;
        self.span = None;
        self.down = None;
        self.dragging = false;
        self.scroll_dy = 0.0;
    }

    /// Starts a frame: the registry of the frame before is what the pointer hits, and the
    /// selection is resolved against the document as it is now.
    pub fn begin(&mut self, snap: &Snap) {
        std::mem::swap(&mut self.runs, &mut self.prev);
        self.runs.clear();
        self.end = snap.region("main").map_or(0, |m| last_in(snap, m).ord);
        if self.epoch != snap.resets {
            self.epoch = snap.resets;
            self.clear();
        }
        self.span = self.resolve(snap);
    }

    /// The selection's endpoints as document positions. An endpoint whose node is gone moves to
    /// the node that took its place in document order.
    fn resolve(&mut self, snap: &Snap) -> Option<Span> {
        let (a, f) = (self.anchor.as_mut()?, self.focus.as_mut()?);
        let (a, f) = (Self::at(a, snap)?, Self::at(f, snap)?);
        if a == f {
            return None;
        }
        Some(if a < f {
            Span { from: a, to: f }
        } else {
            Span { from: f, to: a }
        })
    }

    fn at(p: &mut Point, snap: &Snap) -> Option<At> {
        if let Some(n) = snap.get(&p.node) {
            p.ord = n.ord;
            let next = after(snap, n).unwrap_or("");
            if p.after != next {
                p.after.clear();
                p.after.push_str(next);
            }
        } else if let Some(a) = snap.get(&p.after) {
            // The node is gone: the position moves to the start of what followed it.
            p.node.clone_from(&a.id);
            p.ord = a.ord;
            p.after.clear();
            p.seq = 0;
            p.ch = 0;
        } else {
            // Nothing followed it (or that is gone too): the last node's end, or the same slot.
            let len = snap.node_count();
            if len == 0 {
                return None;
            }
            if p.after.is_empty() || (p.ord as usize) >= len {
                p.ord = (len - 1) as u32;
                p.seq = u32::MAX;
                p.ch = usize::MAX;
            } else {
                p.seq = 0;
                p.ch = 0;
            }
            p.node.clear();
            p.node.push_str(snap.id_at(p.ord)?);
            p.after.clear();
        }
        Some((
            RunKey {
                ord: p.ord,
                seq: p.seq,
            },
            p.ch,
        ))
    }

    /// Selects everything the transcript shows.
    pub fn select_all(&mut self, snap: &Snap) {
        let Some(main) = snap.region("main") else {
            return;
        };
        // The conversation, not the welcome card (logo, version, tip) above it.
        let Some(first) = snap.kids(main).find(|c| {
            c.kind != "spacer"
                && !c.role_under("omp.welcome")
                && snap.by_role(c, "omp.welcome").is_none()
        }) else {
            return;
        };
        let last = last_in(snap, main);
        self.anchor = Some(Point {
            node: first.id.clone(),
            ord: first.ord,
            after: String::new(),
            seq: 0,
            ch: 0,
        });
        self.focus = Some(Point {
            node: last.id.clone(),
            ord: last.ord,
            after: String::new(),
            seq: u32::MAX,
            ch: usize::MAX,
        });
        self.span = self.resolve(snap);
    }

    fn put(slot: &mut Option<Point>, snap: &Snap, (key, ch): At) {
        let Some(id) = snap.id_at(key.ord) else {
            return;
        };
        match slot {
            Some(p) => {
                if p.node != id {
                    p.node.clear();
                    p.node.push_str(id);
                }
                p.ord = key.ord;
                p.seq = key.seq;
                p.ch = ch;
            }
            None => {
                *slot = Some(Point {
                    node: id.to_owned(),
                    ord: key.ord,
                    after: String::new(),
                    seq: key.seq,
                    ch,
                });
            }
        }
    }

    /// The part of run `key` (of `len` plain chars) the selection covers.
    fn selected(&self, key: RunKey, len: usize) -> Option<(usize, usize)> {
        let s = self.span?;
        if key < s.from.0 || key > s.to.0 {
            return None;
        }
        let a = if key == s.from.0 { s.from.1 } else { 0 };
        let b = if key == s.to.0 { s.to.1 } else { len };
        let b = b.min(len);
        (a < b).then_some((a, b))
    }

    /// The registered run nearest to `p` among those `keep` accepts, and how far (vertically)
    /// `p` is from it.
    fn nearest(&self, p: Pos2, keep: impl Fn(&Reg) -> bool) -> Option<(usize, f32)> {
        let mut best: Option<((f32, f32), usize)> = None;
        for (i, r) in self.prev.iter().enumerate() {
            if !keep(r) {
                continue;
            }
            let rect = r.rect();
            let dy = (rect.top() - p.y).max(p.y - rect.bottom()).max(0.0);
            let dx = (rect.left() - p.x).max(p.x - rect.right()).max(0.0);
            if best.is_none_or(|(d, _)| (dy, dx) < d) {
                best = Some(((dy, dx), i));
                if dy == 0.0 && dx == 0.0 {
                    break;
                }
            }
        }
        best.map(|((dy, _), i)| (i, dy))
    }

    /// The plain char of run `i` under (or nearest to) `p`.
    fn char_at(&self, i: usize, p: Pos2) -> usize {
        let r = &self.prev[i];
        r.galley.cursor_from_pos(p - r.pos).index.0 + r.skip
    }

    /// The registered run nearest to `p` and the plain char under (or nearest to) it.
    fn hit(&self, p: Pos2) -> Option<(usize, usize)> {
        let (i, _) = self.nearest(p, |_| true)?;
        Some((i, self.char_at(i, p)))
    }

    fn hit_at(&self, p: Pos2) -> Option<At> {
        let (i, ch) = self.hit(p)?;
        Some((self.prev[i].key, ch))
    }

    /// [`Self::hit_at`] for a drag that started in column `lane` (0: none): past the last row
    /// of a column the pointer stays in the column it started in rather than jumping to the
    /// run beside it.
    fn hit_in(&self, p: Pos2, lane: u8) -> Option<At> {
        let (i, dy) = self.nearest(p, |_| true)?;
        let (i, ch) = if lane != 0 && dy > 0.0 && self.prev[i].lane != lane {
            let ord = self.prev[i].key.ord;
            match self.nearest(p, |r| r.lane == lane && r.key.ord == ord) {
                Some((j, _)) => (j, self.char_at(j, p)),
                None => (i, self.char_at(i, p)),
            }
        } else {
            (i, self.char_at(i, p))
        };
        Some((self.prev[i].key, ch))
    }

    /// Handles this frame's primary-button input: a press (click count, shift-extend, double and
    /// triple click), the drag and its auto-scroll, and the text cursor.
    pub fn pointer(&mut self, ctx: &Context, snap: &Snap, area: &Area) {
        self.view = Some(area.view);
        self.scroll_dy = 0.0;
        let (press, latest, down, released, shift, time, dt) = ctx.input(|i| {
            (
                i.pointer
                    .primary_pressed()
                    .then(|| i.pointer.interact_pos())
                    .flatten(),
                i.pointer.latest_pos(),
                i.pointer.primary_down(),
                i.pointer.primary_released(),
                i.modifiers.shift,
                i.time,
                i.stable_dt.min(0.05),
            )
        });
        if let Some(p) = press
            && area.chat.contains(p)
            && ctx.layer_id_at(p).is_none_or(|l| l == area.layer)
        {
            self.press(snap, area, p, shift, time);
        }
        if self.down.is_some()
            && (down || released)
            && let Some(p) = latest
        {
            self.drag(snap, area, p, dt);
        }
        if !down {
            self.down = None;
            self.dragging = false;
            self.scroll_dy = 0.0;
        }
        if self.dragging {
            ctx.set_cursor_icon(CursorIcon::Text);
            if self.scroll_dy != 0.0 {
                ctx.request_repaint();
            }
        } else if let Some(p) = latest
            && area.view.contains(p)
            && !down
            && ctx.layer_id_at(p).is_none_or(|l| l == area.layer)
            && self.prev.iter().any(|r| r.rect().contains(p))
        {
            ctx.set_cursor_icon(CursorIcon::Text);
        }
        self.span = self.resolve(snap);
    }

    fn press(&mut self, snap: &Snap, area: &Area, p: Pos2, shift: bool, time: f64) {
        let in_view = area.view.contains(p) && p.x < area.view.right() - SCROLLBAR;
        if !in_view {
            // The composer, the scrollbar or another control: the selection ends.
            self.clear();
            self.clicks = None;
            return;
        }
        let count = match self.clicks {
            Some((t, at, n)) if time - t < MULTI_CLICK.0 && at.distance(p) < MULTI_CLICK.1 => {
                (n % 3) + 1
            }
            _ => 1,
        };
        self.clicks = Some((time, p, count));
        let hit = self.hit_at(p);
        if shift
            && self.anchor.is_some()
            && self.focus.is_some()
            && let Some(h) = hit
        {
            Self::put(&mut self.focus, snap, h);
            self.down = Some(Down {
                origin: p,
                hit: None,
                locked: false,
                lane: 0,
            });
            self.dragging = true;
            return;
        }
        self.clear();
        let lane = self.hit(p).map_or(0, |(i, _)| self.prev[i].lane);
        self.down = Some(Down {
            origin: p,
            hit,
            locked: count > 1,
            lane,
        });
        if count == 1
            && let Some(h) = hit
        {
            // A plain click puts the caret: a later shift+click selects from here. (Both ends
            // alike select nothing.)
            Self::put(&mut self.anchor, snap, h);
            Self::put(&mut self.focus, snap, h);
        }
        if count > 1
            && let Some((i, ch)) = self.hit(p)
        {
            let text = self.prev[i].text();
            let (a, b) = if count == 2 {
                select::word_at(&text, ch)
            } else {
                select::line_at(&text, ch)
            };
            let key = self.prev[i].key;
            Self::put(&mut self.anchor, snap, (key, a));
            Self::put(&mut self.focus, snap, (key, b));
        }
    }

    fn drag(&mut self, snap: &Snap, area: &Area, p: Pos2, dt: f32) {
        let Some(d) = &self.down else {
            return;
        };
        if d.locked {
            return;
        }
        let lane = d.lane;
        if !self.dragging && p.distance(d.origin) > DRAG_SLOP {
            self.dragging = true;
            if let Some(h) = d.hit {
                Self::put(&mut self.anchor, snap, h);
            }
        }
        if !self.dragging {
            return;
        }
        if let Some(h) = self.hit_in(p, lane) {
            Self::put(&mut self.focus, snap, h);
        }
        // Scroll while the pointer is in (or past) the band at an edge of the transcript.
        let (top, bottom) = (area.view.top() + EDGE, area.view.bottom() - EDGE);
        let past = if p.y < top {
            p.y - top
        } else if p.y > bottom {
            p.y - bottom
        } else {
            0.0
        };
        self.scroll_dy = (past * SCROLL_PER_PX).clamp(-SCROLL_MAX, SCROLL_MAX) * dt;
    }

    /// Records a run for a copy. The run is collected when its plain text is `text`.
    fn push(&mut self, key: RunKey, o: RunOpts, text: String) {
        let item = self.item;
        if let Some(got) = &mut self.got {
            got.push(Got {
                key,
                item,
                sep: o.sep,
                lead: o.lead,
                text,
            });
        }
    }
}

// ---- what the renderers call ----------------------------------------------------------------

impl Cx<'_> {
    /// Renders `f` as the node with pre-order index `ord`: runs drawn inside belong to it and
    /// number from 0.
    pub fn in_node<R>(&self, ord: u32, f: impl FnOnce() -> R) -> R {
        let old = self.loc(|l| std::mem::replace(&mut l.tsel.scope, (ord, 0)));
        let r = f();
        self.loc(|l| l.tsel.scope = old);
        r
    }

    /// Reserves `n` run numbers of the current node and returns the first. Every renderer that
    /// has runs calls this for each one it *visits*, whether or not it paints it, so the numbers
    /// do not depend on what is on screen. (A sizing pass reserves nothing: it paints nothing.)
    pub fn seqs(&self, ui: &Ui, n: u32) -> u32 {
        if ui.is_sizing_pass() {
            return 0;
        }
        self.loc(|l| {
            let at = l.tsel.scope.1;
            l.tsel.scope.1 = at.saturating_add(n);
            at
        })
    }

    /// Whether a copy is being collected (nothing is painted; every row is laid out).
    pub fn collecting(&self) -> bool {
        self.loc(|l| l.tsel.got.is_some())
    }

    /// Runs `f`; while a copy is collected, what it collected and the run numbers it used are
    /// dropped (a draw that only exists to give egui sizes to the real one).
    pub fn discarding(&self, f: impl FnOnce()) {
        if !self.collecting() {
            return;
        }
        let (len, scope) = self.loc(|l| (l.tsel.got.as_ref().map_or(0, Vec::len), l.tsel.scope));
        f();
        self.loc(|l| {
            if let Some(g) = &mut l.tsel.got {
                g.truncate(len);
            }
            l.tsel.scope = scope;
        });
    }

    /// [`Ui::is_rect_visible`], except that a copy being collected sees everything inside the
    /// clip rect (its ui is invisible).
    pub fn visible(&self, ui: &Ui, rect: Rect) -> bool {
        if self.collecting() {
            !ui.is_sizing_pass() && rect.intersects(ui.clip_rect())
        } else {
            ui.is_rect_visible(rect)
        }
    }

    /// Paints `galley` at `pos` as the selectable run `seq` of the current node, with the selected
    /// part highlighted, and registers it for hit testing; while a copy is collected it records
    /// the run's text instead.
    pub fn text_run(
        &self,
        ui: &Ui,
        seq: u32,
        galley: &Arc<Galley>,
        pos: Pos2,
        color: Color32,
        o: RunOpts,
    ) {
        if ui.is_sizing_pass() {
            return;
        }
        let rect = Rect::from_min_size(pos, galley.size());
        if self.collecting() {
            if !o.inert && rect.intersects(ui.clip_rect()) {
                let mut text = " ".repeat(o.skip);
                text.push_str(galley.text());
                self.loc(|l| {
                    let key = RunKey {
                        ord: l.tsel.scope.0,
                        seq,
                    };
                    l.tsel.push(key, o, text);
                });
            }
            return;
        }
        if !ui.is_rect_visible(rect) {
            return;
        }
        let range = self.loc(|l| {
            let t = &mut l.tsel;
            let key = RunKey {
                ord: t.scope.0,
                seq,
            };
            if o.inert || key.ord > t.end {
                // Inert, or the composer / a layer: painted, never selected.
                return None;
            }
            t.runs.push(Reg {
                key,
                galley: Arc::clone(galley),
                pos,
                skip: o.skip,
                lane: o.lane,
            });
            t.span?;
            t.selected(key, galley.end().index.0 + o.skip)
        });
        match range {
            Some((a, b)) => {
                let mut g = Arc::clone(galley);
                let at = |i: usize| CCursor::new(i.saturating_sub(o.skip));
                paint_text_selection(&mut g, ui.visuals(), &CCursorRange::two(at(a), at(b)), None);
                ui.painter().galley(pos, g, color);
            }
            None => {
                ui.painter().galley(pos, Arc::clone(galley), color);
            }
        }
    }

    /// While a copy is collected: the run `seq` whose plain text is `text`, without laying it out
    /// (a code row). Outside a copy it does nothing.
    pub fn text_plain(&self, ui: &Ui, seq: u32, text: &str, o: RunOpts) {
        if ui.is_sizing_pass() || o.inert {
            return;
        }
        self.loc(|l| {
            if l.tsel.got.is_some() {
                let key = RunKey {
                    ord: l.tsel.scope.0,
                    seq,
                };
                l.tsel.push(key, o, text.to_owned());
            }
        });
    }
}

// ---- copying --------------------------------------------------------------------------------

/// A clip rect nothing falls outside of.
fn everything() -> Rect {
    Rect::from_min_max(pos2(-1.0e6, -1.0e6), pos2(1.0e6, 1.0e6))
}

/// The text of the selection: every item from the one holding its start to the one holding its
/// end is rendered once more, invisibly and unclipped, and the runs inside the selection are
/// joined in document order. `width` is the conversation column's width.
pub fn copy(cx: &Cx, ui: &mut Ui, width: f32) -> Option<String> {
    let span = cx.loc(|l| l.tsel.span)?;
    let items = transcript::items(cx);
    if items.is_empty() {
        return None;
    }
    let holding = |ord: u32| items.partition_point(|it| it.ord <= ord).saturating_sub(1);
    let (first, last) = (holding(span.from.0.ord), holding(span.to.0.ord));
    cx.loc(|l| l.tsel.got = Some(Vec::new()));
    let origin = ui.max_rect().min;
    for (idx, it) in items.iter().enumerate().take(last + 1).skip(first) {
        cx.loc(|l| l.tsel.item = idx as u32);
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt(("tsel-collect", idx))
                .max_rect(Rect::from_min_size(origin, vec2(width, 10_000.0)))
                .invisible(),
        );
        child.set_clip_rect(everything());
        child.set_width(width);
        child.style_mut().interaction.selectable_labels = false;
        transcript::item(cx, &mut child, it, false);
    }
    let got = cx.loc(|l| l.tsel.got.take()).unwrap_or_default();
    let text = join(got, &span);
    (!text.is_empty()).then_some(text)
}

/// The plain text of `got` inside `span`, in document order.
fn join(mut got: Vec<Got>, span: &Span) -> String {
    got.sort_by_key(|g| g.key);
    let mut out = String::new();
    let mut prev: Option<&Got> = None;
    for g in &got {
        if g.key < span.from.0 || g.key > span.to.0 {
            continue;
        }
        let n = g.text.chars().count();
        let from = if g.key == span.from.0 {
            span.from.1.min(n)
        } else {
            0
        };
        let to = if g.key == span.to.0 {
            span.to.1.min(n)
        } else {
            n
        };
        if from >= to {
            continue;
        }
        if let Some(p) = prev {
            out.push_str(if g.item != p.item {
                "\n\n"
            } else if g.key.ord != p.key.ord {
                "\n"
            } else {
                match g.sep {
                    Sep::Line => "\n",
                    Sep::Block => "\n\n",
                    Sep::Tab => "\t",
                }
            });
        }
        if from == 0 {
            g.lead.push(&mut out);
        }
        let byte = |c: usize| {
            g.text
                .char_indices()
                .nth(c)
                .map_or(g.text.len(), |(b, _)| b)
        };
        out.push_str(&g.text[byte(from)..byte(to)]);
        prev = Some(g);
    }
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn got(ord: u32, seq: u32, item: u32, sep: Sep, lead: Lead, text: &str) -> Got {
        Got {
            key: RunKey { ord, seq },
            item,
            sep,
            lead,
            text: text.to_owned(),
        }
    }

    fn span(from: (u32, u32, usize), to: (u32, u32, usize)) -> Span {
        Span {
            from: (
                RunKey {
                    ord: from.0,
                    seq: from.1,
                },
                from.2,
            ),
            to: (
                RunKey {
                    ord: to.0,
                    seq: to.1,
                },
                to.2,
            ),
        }
    }

    #[test]
    fn positions_order_by_node_then_run_then_char() {
        let k = |ord, seq| RunKey { ord, seq };
        assert!(
            k(1, 9) < k(2, 0),
            "a later node comes after every run of an earlier one"
        );
        assert!(k(2, 0) < k(2, 1));
        assert!((k(2, 1), 5) < (k(2, 1), 6));
        assert!((k(2, 1), usize::MAX) < (k(2, 2), 0));
    }

    #[test]
    fn a_copy_joins_runs_by_what_separates_them_and_cuts_the_ends() {
        let all = vec![
            got(3, 0, 0, Sep::Line, Lead::None, "first paragraph"),
            got(3, 1, 0, Sep::Block, Lead::Bullet, "second"),
            got(3, 2, 0, Sep::Line, Lead::Num(2), "third"),
            got(4, 0, 0, Sep::Line, Lead::None, "other node"),
            got(9, 0, 1, Sep::Line, Lead::None, "next item"),
            got(9, 1, 1, Sep::Tab, Lead::None, "cell"),
        ];
        let text = join(all, &span((3, 0, 6), (9, 1, 2)));
        assert_eq!(
            text, "paragraph\n\n• second\n2. third\nother node\n\nnext item\tce",
            "blocks are blank-line separated, nodes by a line, items by a blank line; the ends are cut at their chars"
        );
    }

    #[test]
    fn a_copy_lists_a_marker_only_when_its_run_starts_inside_the_selection() {
        let all = vec![got(1, 0, 0, Sep::Line, Lead::Bullet, "item text")];
        assert_eq!(join(all, &span((1, 0, 0), (1, 0, 4))), "• item");
        let all = vec![got(1, 0, 0, Sep::Line, Lead::Bullet, "item text")];
        assert_eq!(join(all, &span((1, 0, 2), (1, 0, 9))), "em text");
    }

    #[test]
    fn runs_collected_out_of_order_are_copied_in_document_order() {
        let all = vec![
            got(5, 1, 0, Sep::Line, Lead::None, "b"),
            got(5, 0, 0, Sep::Line, Lead::None, "a"),
            got(2, 0, 0, Sep::Line, Lead::None, "before"),
        ];
        assert_eq!(join(all, &span((2, 0, 0), (5, 1, 1))), "before\na\nb");
    }

    #[test]
    fn a_selected_run_is_the_part_between_the_endpoints() {
        let mut t = Tsel {
            span: Some(span((2, 1, 3), (4, 0, 2))),
            ..Tsel::default()
        };
        let k = |ord, seq| RunKey { ord, seq };
        assert_eq!(
            t.selected(k(2, 1), 10),
            Some((3, 10)),
            "the first run from its char on"
        );
        assert_eq!(
            t.selected(k(3, 0), 10),
            Some((0, 10)),
            "a run between is whole"
        );
        assert_eq!(
            t.selected(k(4, 0), 10),
            Some((0, 2)),
            "the last run up to its char"
        );
        assert_eq!(t.selected(k(2, 0), 10), None);
        assert_eq!(t.selected(k(4, 1), 10), None);
        t.span = Some(span((3, 0, 4), (3, 0, 4)));
        assert_eq!(
            t.selected(k(3, 0), 10),
            None,
            "an empty selection selects nothing"
        );
    }
}
