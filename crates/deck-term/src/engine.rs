//! The output engine of one terminal: every byte from the child (local PTY loop or remote
//! feeder) is parsed into the `Term`, run through the output scanners and, for an owner, the
//! snapshot side scanner and published to output subscribers — all in one critical section
//! ([`Engine`]'s lock, taken before the `Term`'s). A subscriber joins under the same lock: its
//! snapshot covers exactly the bytes before the join and its queue exactly the ones after it,
//! so nothing is lost or applied twice.
//!
//! The snapshot leaves the replaying parser in the owner's parser state: a sequence still
//! unfinished at the join (an escape, string or UTF-8 character) is appended to it as received,
//! so its continuation completes it on both sides. Joins never alter the owner: during a
//! synchronized update (whose bytes vte keeps unapplied) a join waits for the update's end —
//! its `CSI ? 2026 l`, vte's timeout or size limit, or the end of output — and an unfinished
//! sequence longer than [`MAX_PREFIX`] makes it wait for that sequence's end.
//! A join also waits while the TSP mirror is inside a chunked message; a sequence the program
//! abandoned expires after `CHUNK_IDLE` on its own (a timer thread, started only while a join
//! waits for it), so a quiet program cannot hold a join up for ever.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use deck_host::snapshot::{self, SideScanner};

use crate::core::{Listener, SharedTerm};
use crate::frontend::FrontendState;
use crate::remote::ScrollKeep;
use crate::scan::{GroundTracker, OutputScan};

/// Called after an event was queued for a subscription. It runs on the output thread (possibly
/// under the engine lock): it must neither block nor touch the terminal.
pub type OutputNotify = Arc<dyn Fn() + Send + Sync>;

/// What a [`OutputSubscription`] delivers, in output order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputEvent {
    /// Authoritative geometry. Precedes the first snapshot; later ones fall between the
    /// output bytes exactly where the owner resized.
    Resize {
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    },
    /// Whole-terminal replay for a fresh renderer (retained kitty images, then grids,
    /// scrollback, cursor, modes, colors, hyperlinks, then any sequence still unfinished). The
    /// first one follows the first `Resize`; a later one replaces the terminal (a host-backed
    /// owner received a new host snapshot). Shared by every subscriber that joined together.
    Snapshot(Arc<[u8]>),
    /// Child output, contiguous with the snapshot.
    Output(Arc<[u8]>),
    /// Owner state for the frontend; the first one follows the first snapshot.
    State(FrontendState),
    /// The child exited (`None`: no exit code). Final.
    Exited(Option<i32>),
    /// The subscriber fell more than its budget behind: its queue was dropped. Final;
    /// subscribe again for a fresh snapshot.
    Overflow,
    /// The owner stopped feeding output without an exit (dropped, link to the host lost), or
    /// this terminal does not own its output (renderers, observers). Final.
    Closed,
}

impl OutputEvent {
    /// Budget charge: the payload plus a fixed amount per event, so payload-less events
    /// (state, geometry) cannot pile up without bound either.
    fn cost(&self) -> usize {
        EVENT_COST
            + match self {
                OutputEvent::Output(bytes) | OutputEvent::Snapshot(bytes) => bytes.len(),
                _ => 0,
            }
    }

    fn is_final(&self) -> bool {
        matches!(
            self,
            OutputEvent::Exited(_) | OutputEvent::Overflow | OutputEvent::Closed
        )
    }
}

/// Longest run of bytes parsed under one `Term` lock (alacritty's event loop uses the same
/// bound), so a large burst never stalls the UI thread for long.
const MAX_LOCKED: usize = 0x1_0000;
/// Budget a subscriber may have queued before it is cut off ([`OutputEvent::Overflow`]). The
/// replay a subscriber joins with is finite and exempt.
const SUBSCRIBER_BUDGET: usize = 4 << 20;
/// Per-event part of [`OutputEvent::cost`].
const EVENT_COST: usize = 64;
/// Longest unfinished sequence kept for replay to joiners.
const MAX_PREFIX: usize = 4 << 20;
/// End of a synchronized update (vte recognises exactly this form).
const ESU: &[u8] = b"\x1b[?2026l";

pub(crate) struct Engine {
    term: SharedTerm,
    /// Config of the fresh `Term` a remote snapshot starts from.
    config: Config,
    listener: Listener,
    /// Holds a scrolled-back view across the redraw an app answers a resize with.
    keep: Arc<ScrollKeep>,
    inner: Mutex<Inner>,
    /// Itself, for the expiry timer.
    me: Weak<Engine>,
}

struct Inner {
    parser: Processor,
    scan: OutputScan,
    /// Owner terminals only.
    serve: Option<Serve>,
}

/// Publication state of an owner.
struct Serve {
    side: SideScanner,
    ground: GroundTracker,
    /// Bytes since the stream was last between sequences (see [`MAX_PREFIX`]).
    prefix: Vec<u8>,
    /// The unfinished sequence outgrew [`MAX_PREFIX`].
    prefix_lost: bool,
    size: WindowSize,
    state: FrontendState,
    subs: Vec<Arc<Queue>>,
    /// Waiting for the end of a synchronized update or of an over-long sequence.
    pending: Vec<Arc<Queue>>,
    end: Option<OutputEvent>,
    /// The expiry timer runs (see [`Engine::arm_expiry`]).
    expiry_armed: bool,
}

impl Serve {
    fn track_prefix(&mut self, bytes: &[u8]) {
        let tail = match self.ground.feed_last_ground(bytes) {
            Some(at) => {
                self.prefix.clear();
                self.prefix.shrink_to(MAX_LOCKED);
                self.prefix_lost = false;
                &bytes[at..]
            }
            None => bytes,
        };
        if self.prefix_lost || tail.is_empty() {
            return;
        }
        if self.prefix.len() + tail.len() > MAX_PREFIX {
            self.prefix_lost = true;
            self.prefix = Vec::new();
        } else {
            self.prefix.extend_from_slice(tail);
        }
    }
}

/// vte holds bytes back for a synchronized update.
fn syncing(parser: &Processor) -> bool {
    parser.sync_timeout().sync_timeout().is_some()
}

impl Engine {
    pub(crate) fn new(
        term: SharedTerm,
        config: Config,
        listener: Listener,
        scan: OutputScan,
        serve: bool,
        size: WindowSize,
        keep: Arc<ScrollKeep>,
    ) -> Arc<Engine> {
        let serve = serve.then(|| Serve {
            side: SideScanner::new(size.num_lines),
            ground: GroundTracker::default(),
            prefix: Vec::new(),
            prefix_lost: false,
            size,
            state: FrontendState::default(),
            subs: Vec::new(),
            pending: Vec::new(),
            end: None,
            expiry_armed: false,
        });
        Arc::new_cyclic(|me| Engine {
            term,
            config,
            listener,
            keep,
            inner: Mutex::new(Inner {
                parser: Processor::new(),
                scan,
                serve,
            }),
            me: me.clone(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Child output.
    pub(crate) fn feed(&self, bytes: &[u8]) {
        let mut inner = self.lock();
        for chunk in bytes.chunks(MAX_LOCKED) {
            let mut rest = chunk;
            while !rest.is_empty() {
                let (now, later) = rest.split_at(Self::join_split(&inner, rest));
                self.parse(&mut inner, now, true);
                self.join_if_ready(&mut inner);
                rest = later;
            }
        }
    }

    /// How much of `rest` to parse before waiting joins are checked again: through the end
    /// of the synchronized update or sequence they wait for; all of it when none waits.
    fn join_split(inner: &Inner, rest: &[u8]) -> usize {
        let Some(s) = &inner.serve else {
            return rest.len();
        };
        if s.pending.is_empty() {
            return rest.len();
        }
        if syncing(&inner.parser) {
            return rest
                .windows(ESU.len())
                .position(|w| w == ESU)
                .map_or(rest.len(), |at| at + ESU.len());
        }
        if s.ground.is_ground() && !inner.scan.tsp_idle() {
            // Waiting for the end of a chunked TSP message: only an escape can complete one,
            // so plain bytes before the next ESC need no checks in between.
            if let Some(at) = rest.iter().position(|&b| b == 0x1b).filter(|&at| at > 0) {
                return at;
            }
        }
        let mut probe = s.ground;
        probe.feed_to_ground(rest).unwrap_or(rest.len())
    }

    /// Parses into the `Term`, scans, and (owner) tracks side state and publishes.
    fn parse(&self, inner: &mut Inner, bytes: &[u8], publish: bool) {
        {
            let mut term = self.term.lock();
            inner.parser.advance(&mut *term, bytes);
            self.keep.pin(&mut *term);
        }
        inner.scan.feed(bytes);
        if let Some(s) = &mut inner.serve {
            s.side.advance(bytes);
            s.track_prefix(bytes);
            if publish && !s.subs.is_empty() {
                // One copy shared by every subscriber.
                let output: Arc<[u8]> = Arc::from(bytes);
                broadcast(&mut s.subs, &OutputEvent::Output(output));
            }
        }
        self.listener.send_event(Event::Wakeup);
    }

    /// Whether a subscriber can join now (`None`: not an owner). An abandoned chunk sequence
    /// of the TSP mirror expires here, so a joiner is not held up by output that never comes.
    fn ready(inner: &mut Inner) -> Option<bool> {
        let s = inner.serve.as_ref()?;
        if s.end.is_some() {
            return Some(true);
        }
        if syncing(&inner.parser) || !(s.ground.is_ground() || !s.prefix_lost) {
            return Some(false);
        }
        Some(inner.scan.expire_tsp(Instant::now()))
    }

    fn join_if_ready(&self, inner: &mut Inner) {
        let waiting = inner.serve.as_ref().is_some_and(|s| !s.pending.is_empty());
        if !waiting {
            return;
        }
        if Self::ready(inner) == Some(true) {
            self.join_pending(inner);
        } else {
            self.arm_expiry(inner);
        }
    }

    /// Starts the timer that lets waiting joins re-check once the TSP chunk sequence they wait
    /// for has expired: expiry otherwise only happens when more output arrives, and an idle
    /// program sends none. One timer at a time, ended by [`Self::expiry_step`].
    fn arm_expiry(&self, inner: &mut Inner) {
        let Some(s) = &mut inner.serve else { return };
        if s.expiry_armed || inner.scan.tsp_idle() {
            return;
        }
        s.expiry_armed = true;
        let me = self.me.clone();
        let spawned = thread::Builder::new()
            .name("tsp-expiry".into())
            .spawn(move || {
                while let Some(engine) = me.upgrade() {
                    let Some(wait) = engine.expiry_step() else {
                        return;
                    };
                    drop(engine);
                    thread::sleep(wait);
                }
            });
        if let Err(e) = spawned {
            log::error!("tsp expiry timer: {e}");
            s.expiry_armed = false;
        }
    }

    /// One round of the expiry timer: how long to sleep before the next, or `None` once the
    /// joins no longer wait for a chunk sequence.
    fn expiry_step(&self) -> Option<Duration> {
        let mut inner = self.lock();
        let waiting = inner.serve.as_ref().is_some_and(|s| !s.pending.is_empty());
        if waiting && let Some(at) = inner.scan.tsp_deadline() {
            let now = Instant::now();
            if now <= at {
                return Some(at - now + Duration::from_millis(1));
            }
            self.join_if_ready(&mut inner);
        }
        if let Some(s) = &mut inner.serve {
            s.expiry_armed = false;
        }
        None
    }

    /// Deadline of a synchronized update in progress.
    pub(crate) fn sync_deadline(&self) -> Option<Instant> {
        self.lock().parser.sync_timeout().sync_timeout()
    }

    /// Ends a synchronized update whose deadline passed (the app never ended it).
    pub(crate) fn flush_sync_due(&self) {
        let mut inner = self.lock();
        if inner
            .parser
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|at| at <= Instant::now())
        {
            {
                let mut term = self.term.lock();
                inner.parser.stop_sync(&mut *term);
                self.keep.pin(&mut *term);
            }
            self.listener.send_event(Event::Wakeup);
            self.join_if_ready(&mut inner);
        }
    }

    /// A remote snapshot: fresh `Term` at the current size (keeping focus state), fresh
    /// parser and scanners, then the snapshot. Owners pass it on as is.
    pub(crate) fn reset(&self, bytes: &[u8]) {
        let mut inner = self.lock();
        inner.parser = Processor::new();
        // Ahead of the snapshot's own title events: the old title does not survive it.
        self.listener.send_event(Event::ResetTitle);
        let rows = {
            let mut term = self.term.lock();
            let size = TermSize::new(term.columns(), term.screen_lines());
            let focused = term.is_focused;
            *term = Term::new(self.config.clone(), &size, self.listener.clone());
            term.is_focused = focused;
            term.screen_lines()
        };
        inner.scan.reset();
        if let Some(s) = &mut inner.serve {
            s.side = SideScanner::new(u16::try_from(rows).unwrap_or(u16::MAX));
            s.ground = GroundTracker::default();
            s.prefix = Vec::new();
            s.prefix_lost = false;
        }
        for chunk in bytes.chunks(MAX_LOCKED) {
            self.parse(&mut inner, chunk, false);
        }
        inner.scan.end_reset();
        let Some(s) = &mut inner.serve else { return };
        broadcast(&mut s.subs, &OutputEvent::Snapshot(Arc::from(bytes)));
        self.join_if_ready(&mut inner);
    }

    /// New geometry: the `Term` (when the grid changed) and, for an owner, the side scanner
    /// and every subscriber, between the same bytes.
    pub(crate) fn resize(&self, size: WindowSize) {
        let mut inner = self.lock();
        let (cols, rows) = (usize::from(size.num_cols), usize::from(size.num_lines));
        let grid_changed = {
            let mut term = self.term.lock();
            let changed = term.columns() != cols || term.screen_lines() != rows;
            if changed {
                self.keep.arm(term.grid().display_offset());
                term.resize(TermSize::new(cols, rows));
            }
            changed
        };
        let Some(s) = &mut inner.serve else { return };
        if grid_changed {
            s.side.resize(size.num_lines);
        }
        let old = s.size;
        if (old.num_cols, old.num_lines, old.cell_width, old.cell_height)
            != (
                size.num_cols,
                size.num_lines,
                size.cell_width,
                size.cell_height,
            )
        {
            s.size = size;
            broadcast(&mut s.subs, &resize_event(size));
        }
    }

    /// Owner state (the clipboard flag is per subscriber).
    pub(crate) fn set_state(&self, state: FrontendState) {
        let mut inner = self.lock();
        let Some(s) = &mut inner.serve else { return };
        if s.state == state {
            return;
        }
        s.state = state;
        s.subs.retain(|q| {
            let owner = q.lock().clipboard_owner;
            q.push(OutputEvent::State(FrontendState {
                clipboard_owner: owner,
                ..state
            }))
        });
    }

    fn set_clipboard_owner(&self, queue: &Arc<Queue>, owner: bool) {
        let mut inner = self.lock();
        {
            let mut st = queue.lock();
            if st.clipboard_owner == owner {
                return;
            }
            st.clipboard_owner = owner;
        }
        let Some(s) = &mut inner.serve else { return };
        if s.subs.iter().any(|q| Arc::ptr_eq(q, queue)) {
            let state = FrontendState {
                clipboard_owner: owner,
                ..s.state
            };
            if !queue.push(OutputEvent::State(state)) {
                s.subs.retain(|q| !Arc::ptr_eq(q, queue));
            }
        }
    }

    /// The owner's output ended (`Exited` or `Closed`); later subscribers still get the
    /// final snapshot first. A synchronized update still open can no longer end: what it
    /// received is shown, as its timeout would.
    pub(crate) fn finish(&self, end: OutputEvent) {
        let mut inner = self.lock();
        inner.scan.finish();
        let Some(s) = &mut inner.serve else { return };
        if s.end.is_some() {
            return;
        }
        s.end = Some(end.clone());
        if syncing(&inner.parser) {
            inner.parser.stop_sync(&mut *self.term.lock());
            self.listener.send_event(Event::Wakeup);
        }
        self.join_if_ready(&mut inner);
        let Some(s) = &mut inner.serve else { return };
        for q in s.subs.drain(..) {
            q.push(end.clone());
        }
    }

    pub(crate) fn subscribe(self: &Arc<Self>, notify: OutputNotify) -> OutputSubscription {
        let queue = Arc::new(Queue::new(notify));
        let mut inner = self.lock();
        match Self::ready(&mut inner) {
            None => {
                queue.push(OutputEvent::Closed);
            }
            Some(true) => {
                let snapshot = self.snapshot(&inner);
                self.join(&mut inner, queue.clone(), snapshot);
            }
            Some(false) => {
                if let Some(s) = &mut inner.serve {
                    s.pending.push(queue.clone());
                }
                self.arm_expiry(&mut inner);
            }
        }
        OutputSubscription {
            queue,
            engine: Arc::downgrade(self),
        }
    }

    fn join_pending(&self, inner: &mut Inner) {
        let snapshot = self.snapshot(inner);
        let pending = inner
            .serve
            .as_mut()
            .map(|s| std::mem::take(&mut s.pending))
            .unwrap_or_default();
        for queue in pending {
            self.join(inner, queue, snapshot.clone());
        }
    }

    /// Delivers the initial events and registers the subscriber (unless output already ended).
    fn join(&self, inner: &mut Inner, queue: Arc<Queue>, snapshot: Arc<[u8]>) {
        let Some(s) = &mut inner.serve else { return };
        let state = FrontendState {
            clipboard_owner: queue.lock().clipboard_owner,
            ..s.state
        };
        let live = queue.push_replay(resize_event(s.size))
            && queue.push_replay(OutputEvent::Snapshot(snapshot))
            && queue.push_replay(OutputEvent::State(state));
        if !live {
            return;
        }
        match &s.end {
            Some(end) => {
                queue.push(end.clone());
            }
            None => s.subs.push(queue),
        }
    }

    /// Replay of the owner's current state: retained images, the VT snapshot, the TSP
    /// surfaces, then the unfinished sequence. Only taken outside synchronized updates and
    /// between TSP messages (see [`Self::ready`]), so the replaying parser must not start a
    /// synchronized update either.
    fn snapshot(&self, inner: &Inner) -> Arc<[u8]> {
        let mut out = Vec::new();
        inner.scan.image_replay(&mut out);
        if let Some(s) = &inner.serve {
            let mut side = s.side.state().clone();
            side.sync_update = false;
            out.extend_from_slice(&snapshot::encode(&mut *self.term.lock(), &side, usize::MAX));
            inner.scan.tsp_replay(Instant::now(), &mut out);
            if !s.ground.is_ground() && !s.prefix_lost {
                out.extend_from_slice(&s.prefix);
            }
        }
        Arc::from(out)
    }
}

fn resize_event(size: WindowSize) -> OutputEvent {
    OutputEvent::Resize {
        cols: size.num_cols,
        rows: size.num_lines,
        cell_width: size.cell_width,
        cell_height: size.cell_height,
    }
}

/// Queues `event` for every subscriber, forgetting those that ended or fell behind.
fn broadcast(subs: &mut Vec<Arc<Queue>>, event: &OutputEvent) {
    subs.retain(|q| q.push(event.clone()));
}

struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
    notify: OutputNotify,
}

#[derive(Default)]
struct QueueState {
    /// Events with their budget charge.
    events: VecDeque<(OutputEvent, usize)>,
    /// Budget charged by the queued events.
    bytes: usize,
    /// A final event was queued.
    ended: bool,
    /// The subscription was dropped.
    dropped: bool,
    clipboard_owner: bool,
}

impl Queue {
    fn new(notify: OutputNotify) -> Queue {
        Queue {
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
            notify,
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `event` against the budget; false once the subscription is over (final event,
    /// overflow, dropped). Never blocks: over the budget the queue is replaced by
    /// [`OutputEvent::Overflow`]. Final events are never refused.
    fn push(&self, event: OutputEvent) -> bool {
        let cost = if event.is_final() { 0 } else { event.cost() };
        self.enqueue(event, cost)
    }

    /// Queues part of the join replay: finite and delivered whatever its size.
    fn push_replay(&self, event: OutputEvent) -> bool {
        self.enqueue(event, 0)
    }

    fn enqueue(&self, event: OutputEvent, cost: usize) -> bool {
        let mut st = self.lock();
        if st.dropped || st.ended {
            return false;
        }
        let (event, cost) = if st.bytes + cost > SUBSCRIBER_BUDGET {
            st.events.clear();
            st.bytes = 0;
            (OutputEvent::Overflow, 0)
        } else {
            (event, cost)
        };
        let live = !event.is_final();
        st.ended = !live;
        st.bytes += cost;
        st.events.push_back((event, cost));
        drop(st);
        self.ready.notify_all();
        (self.notify)();
        live
    }

    fn pop(st: &mut QueueState) -> Option<OutputEvent> {
        let (event, cost) = st.events.pop_front()?;
        st.bytes -= cost;
        Some(event)
    }
}

/// A subscription to an owner terminal's output, see [`crate::Terminal::subscribe_output`].
/// Dropping it unsubscribes.
pub struct OutputSubscription {
    queue: Arc<Queue>,
    engine: Weak<Engine>,
}

impl OutputSubscription {
    /// The next event, if one is queued. `None` also once a final event was returned.
    pub fn try_recv(&self) -> Option<OutputEvent> {
        Queue::pop(&mut self.queue.lock())
    }

    /// Waits up to `timeout` for the next event. Returns `None` on timeout, or at once after
    /// a final event was returned.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<OutputEvent> {
        let deadline = Instant::now() + timeout;
        let mut st = self.queue.lock();
        loop {
            if let Some(event) = Queue::pop(&mut st) {
                return Some(event);
            }
            if st.ended {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            st = self
                .queue
                .ready
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Designates this subscriber as the one serving OSC 52 clipboard requests (or not);
    /// it gets the updated [`OutputEvent::State`] in order.
    pub fn set_clipboard_owner(&self, owner: bool) {
        match self.engine.upgrade() {
            Some(engine) => engine.set_clipboard_owner(&self.queue, owner),
            None => self.queue.lock().clipboard_owner = owner,
        }
    }
}

impl Drop for OutputSubscription {
    fn drop(&mut self) {
        self.queue.lock().dropped = true;
    }
}

impl std::fmt::Debug for OutputSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.queue.lock();
        f.debug_struct("OutputSubscription")
            .field("queued", &st.events.len())
            .field("ended", &st.ended)
            .finish()
    }
}
