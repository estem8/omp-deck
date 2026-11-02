//! The per-frame render context: the snapshot, the ink, the view's persistent local state and the
//! outbox (events for omp, URLs to open, text to copy). Render functions take `&Cx`; the local
//! state and the outbox sit behind `RefCell`s so closures that render twice (measure, then draw)
//! stay `Fn`. Borrows are always short: never hold one across a nested render call.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use deck_tsp::wire::Event;
use egui::{Color32, FontId, Rect, TextureHandle};

use super::events::{self, Click};
use super::snap::{SNode, Snap};
use super::style::Ink;

/// Everything the frame hands back to the app.
#[derive(Default)]
pub struct Out {
    pub events: Vec<Event>,
    pub open_url: Option<String>,
    pub copy: Option<String>,
    /// Blob id of an image to enlarge.
    pub zoom: Option<String>,
}

/// A fold flipped locally, until omp's echo (`set collapsed`) arrives (the node's generation moves).
#[derive(Debug, Clone, Copy)]
pub struct FoldOverride {
    pub collapsed: bool,
    pub gen_: u64,
}

/// Look of plain text where the nodes do not say otherwise: the reading face in the transcript,
/// compact mono in tool bodies, the label face in the dock.
#[derive(Clone)]
pub struct Tx {
    pub font: FontId,
    pub color: Color32,
    pub line: Option<f32>,
}

pub struct Cx<'a> {
    pub snap: &'a Snap,
    pub ink: &'a Ink,
    pub now: Instant,
    loc: RefCell<&'a mut Local>,
    out: RefCell<Out>,
    animating: Cell<Option<Duration>>,
    tx: RefCell<Tx>,
    /// The chat has keyboard focus (the caret is live).
    pub focused: bool,
    /// What omp's hello advertised for input: native caret placement.
    pub can_edit: bool,
}

/// A toast shown for its `ttl`.
#[derive(Debug, Clone)]
pub struct Toast {
    pub id: String,
    pub text: String,
    pub sub: Option<String>,
    pub tone: Option<String>,
    pub until: Instant,
}

/// State that lives as long as the tab's chat view (the snapshot sits beside it in `ChatState`
/// so a frame can borrow the snapshot immutably and this mutably).
#[derive(Default)]
pub struct Local {
    pub folds: HashMap<String, FoldOverride>,
    pub heights: HashMap<String, (u64, i32, f32)>,
    /// Estimated heights of items not measured yet, by the same key.
    pub estimates: HashMap<String, (u64, i32, f32)>,
    pub md: super::md::Cache,
    pub ansi: super::ansi::Cache,
    pub textures: HashMap<String, TextureHandle>,
    pub follow: bool,
    pub jump: bool,
    /// Following, but not at the end (the content grew): scroll back there next frame unless
    /// the reader is scrolling.
    pub chase: bool,
    /// [`Snap::resets`](super::snap::Snap::resets) the transcript last followed the end for.
    pub resets_seen: u64,
    /// A tiny galley laid out every frame: when egui hands out another one, its galley cache or
    /// font atlas was reset and the galleys kept here point into the old atlas.
    pub heads: HashMap<String, (u64, std::sync::Arc<super::tools::HeadText>)>,
    /// Directory shown of a path target at one width: `(generation, width bucket, dir)`.
    pub path_dirs: HashMap<String, (u64, i32, String)>,
    pub probe: Option<std::sync::Arc<egui::Galley>>,
    pub copied: HashMap<String, Instant>,
    pub zoomed: Option<String>,
    pub blink_epoch: Option<Instant>,
    /// The input method's composition in progress (`Terminal::preedit`), shown at the caret.
    pub preedit: String,
    pub frame: u64,
    /// Screen rect of the composer's caret last frame (anchors the slash menu).
    pub caret: Option<Rect>,
    /// Transient toasts omp already deleted (it removes a toast the frame after adding it).
    pub toasts: Vec<Toast>,
    /// Items of each picker, kept across `itemsAdd`/`itemsDel` patches.
    pub pickers: HashMap<String, super::layer::PickerItems>,
    /// Scratch of the transcript's prefix sums (reused between frames).
    pub offsets: Vec<f32>,
}

/// How long a "Copied" feedback is kept; entries older than this are dead weight.
const COPIED_KEEP: Duration = Duration::from_secs(30);

impl Local {
    /// Forgets what belonged to nodes (and blobs) that are gone from `snap`, so a session that
    /// streams for days keeps these maps bounded by what is on screen's document.
    pub fn prune(&mut self, snap: &Snap, now: Instant) {
        let live = |id: &str| snap.get(id).is_some();
        self.heights.retain(|id, _| live(id));
        self.estimates.retain(|id, _| live(id));
        self.heads.retain(|id, _| live(id));
        self.path_dirs.retain(|id, _| live(id));
        self.pickers.retain(|id, _| live(id));
        // Keys of folds are a node id, optionally followed by `\0` and a suffix.
        self.folds
            .retain(|k, _| live(k.split('\0').next().unwrap_or(k)));
        self.textures.retain(|id, _| snap.blob(id).is_some());
        self.copied
            .retain(|_, t| now.saturating_duration_since(*t) < COPIED_KEEP);
    }
}

impl<'a> Cx<'a> {
    pub fn new(
        snap: &'a Snap,
        ink: &'a Ink,
        loc: &'a mut Local,
        now: Instant,
        focused: bool,
        can_edit: bool,
    ) -> Cx<'a> {
        Cx {
            snap,
            ink,
            now,
            loc: RefCell::new(loc),
            out: RefCell::new(Out::default()),
            animating: Cell::new(None),
            tx: RefCell::new(Tx {
                font: ink.prose(super::style::PROSE),
                color: ink.text(),
                line: Some(super::style::PROSE * super::style::PROSE_LINE),
            }),
            focused,
            can_edit,
        }
    }

    /// The outbox and when the frame wants to run again (`None`: only on new input/frames).
    pub fn into_out(self) -> (Out, Option<Duration>) {
        (self.out.into_inner(), self.animating.get())
    }

    /// Runs `f` on the local state (keep it short, never render inside).
    pub fn loc<R>(&self, f: impl FnOnce(&mut Local) -> R) -> R {
        f(&mut self.loc.borrow_mut())
    }

    /// Something on screen moves continuously: the frame repaints at once.
    pub fn animate(&self) {
        self.animate_in(Duration::from_millis(16));
    }

    /// Something changes by itself after `after` (a blink, a clock tick).
    pub fn animate_in(&self, after: Duration) {
        self.animating
            .set(Some(self.animating.get().map_or(after, |d| d.min(after))));
    }

    /// The current plain-text look.
    pub fn tx(&self) -> Tx {
        self.tx.borrow().clone()
    }

    /// Runs `f` with `tx` as the plain-text look.
    pub fn with_tx<R>(&self, tx: Tx, f: impl FnOnce() -> R) -> R {
        let old = self.tx.replace(tx);
        let r = f();
        *self.tx.borrow_mut() = old;
        r
    }

    pub fn send(&self, e: Event) {
        self.out.borrow_mut().events.push(e);
    }

    pub fn open(&self, url: String) {
        self.out.borrow_mut().open_url = Some(url);
    }

    pub fn copy(&self, text: String) {
        self.out.borrow_mut().copy = Some(text);
    }

    pub fn zoom(&self, blob: String) {
        self.out.borrow_mut().zoom = Some(blob);
    }

    pub fn sf(&self) -> &'a str {
        self.snap.sf.as_str()
    }

    /// `n`'s fold state: a local flip not yet echoed by omp, else its `collapsed` prop.
    pub fn collapsed(&self, n: &SNode) -> bool {
        let o = self.loc(|l| l.folds.get(&n.id).copied());
        match o {
            Some(o) if o.gen_ == n.gen_ => o.collapsed,
            _ => n.flag("collapsed"),
        }
    }

    /// Folds/unfolds `n`: flips it locally and tells omp.
    pub fn toggle(&self, n: &SNode) {
        let collapsed = !self.collapsed(n);
        self.loc(|l| {
            l.folds.insert(
                n.id.clone(),
                FoldOverride {
                    collapsed,
                    gen_: n.gen_,
                },
            );
        });
        self.send(events::toggle(self.sf(), &n.id, n.str("key"), collapsed));
    }

    /// Applies the click action `name` of `n` (what its `actions` declare).
    pub fn click(&self, n: &SNode, name: &str) {
        match events::resolve_click(self.snap, n, name, self.collapsed(n)) {
            Click::Toggle(_) => self.toggle(n),
            Click::Copy => self.copy(super::text::node_plain(n)),
            Click::Open => {
                if let Some(u) = n.str("href").or_else(|| n.str("text")) {
                    self.open(u.to_owned());
                }
            }
            Click::Zoom => {
                if let Some(b) = n.str("blob") {
                    self.zoom(b.to_owned());
                }
            }
            Click::Send(e) => self.send(e),
        }
    }

    /// A click on a list/tab row: its own `actions.click` if it has one, else `select` (or
    /// `activate` for a double click or a click on the selected row) addressed to its list.
    pub fn click_item(&self, n: &SNode, list: Option<&SNode>, activate: bool) {
        if let Some(act) = n.click() {
            return self.click(n, act);
        }
        let list = list.or_else(|| events::list_of(self.snap, n));
        let target = list.map_or(n.id.as_str(), |l| l.id.as_str());
        self.send(if activate {
            events::activate(self.sf(), target, &n.id)
        } else {
            events::select(self.sf(), target, &n.id)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_view::snap::tests::{fixture_mirror, sync_doc};

    #[test]
    fn pruning_forgets_what_belongs_to_nodes_that_are_gone() {
        let m = fixture_mirror(usize::MAX);
        let mut snap = Snap::default();
        sync_doc(&mut snap, &m);
        let live = "main".to_owned();
        assert!(snap.get(&live).is_some());
        let now = Instant::now();
        let mut l = Local::default();
        for id in [live.as_str(), "gone"] {
            l.heights.insert(id.to_owned(), (1, 600, 10.0));
            l.estimates.insert(id.to_owned(), (1, 600, 10.0));
            l.path_dirs.insert(id.to_owned(), (1, 600, String::new()));
            l.copied.insert(id.to_owned(), now);
        }
        let fold = FoldOverride {
            collapsed: true,
            gen_: 1,
        };
        l.folds.insert(format!("{live}\0more"), fold);
        l.folds.insert("gone\0more".to_owned(), fold);
        l.folds.insert("gone".to_owned(), fold);
        l.copied.insert(
            "stale".to_owned(),
            now - COPIED_KEEP - Duration::from_secs(1),
        );
        l.prune(&snap, now);
        assert_eq!(l.heights.keys().collect::<Vec<_>>(), [&live]);
        assert_eq!(l.estimates.keys().collect::<Vec<_>>(), [&live]);
        assert_eq!(l.path_dirs.keys().collect::<Vec<_>>(), [&live]);
        assert_eq!(l.folds.len(), 1, "{:?}", l.folds.keys());
        assert!(l.folds.contains_key(&format!("{live}\0more")));
        assert!(l.copied.contains_key(&live) && !l.copied.contains_key("stale"));
    }
}
