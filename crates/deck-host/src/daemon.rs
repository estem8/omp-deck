//! `omp-deck-host serve`: owns one session's PTY, mirrors its output into a headless alacritty
//! `Term` (answering terminal queries while no client is attached), and serves clients.
//!
//! Threads: accept loop (main), one per connection, PTY reader, PTY writer, child waiter,
//! sync-update timer, one writer per attached client and one per observer.
//!
//! Roles: at most one *controller* (`Attach`; a new attach displaces it) owns input, resize
//! and palette. Any number of *observers* (`Observe`) receive the same stream read-only:
//! - Joining is atomic: the observer's queue is registered and its info, grid and snapshot
//!   are encoded in one `Core` critical section, and `read_loop` feeds each PTY chunk and
//!   queues it to everyone under that same lock, so each byte is either inside the snapshot
//!   or queued after it.
//! - Observing changes nothing: no resize, palette, focus, displacement, `HostInfo::attached`
//!   or exit-linger effect. An observer connection may send only `Ping` and `Detach`; any
//!   other frame (`Input`, `Resize`, `Palette`, `Attach`, `Kill`, …) aborts the subscription
//!   and closes the connection at once (no reply). Killing stays available through a separate
//!   connection (`Namespace::kill`).
//! - Each observer has its own byte-bounded queue (live output, control frames and the
//!   snapshot all count until written); one that falls behind is disconnected alone. The PTY
//!   reader and the controller never wait for an observer. Output chunks are shared via `Arc`.
//! - Session end queues `Exited` and then closes every observer; shutdown waits at most
//!   `SHUTDOWN_GRACE` for their writers.
//!
//! TSP: a session spawned with `SessionSpec::tsp` also runs a `deck_tsp` terminal beside the
//! alacritty parser (which swallows APC). Under the core lock, `Core::feed` hands every output
//! chunk to it and queues its reply bytes (hello reply, acks, errors) ahead of the alacritty
//! query replies of the same chunk, so the `hello` answer precedes the DA1 answer. Acks are
//! produced here, attached or not, so a detached omp never stalls on the frame window.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::{Processor, Rgb};

use crate::error::{Error, Result};
use crate::namespace::Namespace;
use crate::protocol::{
    self, AttachParams, ExitedPayload, Grid, Hello, HostInfo, Kind, ObserveParams, PROTOCOL,
    Palette, PeekParams, ResizeParams, SNAPSHOT_CHUNK, SpawnRequest,
};
use crate::pty::{self, PtyChild, PtySize};
use crate::snapshot::{self, SideScanner};
use crate::transport::{Listener, Stream};

/// Scrollback kept by the daemon's terminal.
const SCROLLBACK: usize = 50_000;
/// Queued frames per attached client before it is considered stuck and dropped (it can
/// re-attach and gets a fresh snapshot).
const CLIENT_QUEUE: usize = 4096;
/// After `SIGHUP`, the process group gets this long before `SIGKILL`.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// After the child exited, remaining PTY output is drained for at most this long.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Shutdown waits this long for clients to receive `Exited`.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// A session whose child exited before any client attached keeps serving this long, so the
/// first attach (or `dump`) still sees the final screen and the exit code.
const EXIT_LINGER: Duration = Duration::from_secs(10);
/// Bytes queued for the PTY (client input plus query replies) before the attached client's
/// input stops being read (backpressure) and query replies are dropped.
const INPUT_BUDGET: usize = 8 << 20;
/// Bytes an observer may have queued (live output and control frames, counted until written)
/// beyond its initial snapshot before it is considered stuck and disconnected (it can observe
/// again and gets a fresh snapshot).
const OBSERVER_BUDGET: usize = 4 << 20;
/// Accounted per queued observer item on top of its payload, so floods of tiny frames are
/// bounded too.
const OBSERVER_ITEM_COST: usize = 64;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Options of `omp-deck-host serve`.
pub struct ServeOptions<'a> {
    pub ns: &'a str,
    pub name: &'a str,
    pub spec_file: &'a Path,
    /// Unix: stay in the foreground instead of forking into a new session.
    pub foreground: bool,
}

/// Runs the daemon. Returns only on startup failure (logged); the process exits when the
/// session ends.
pub fn serve(opts: &ServeOptions<'_>) -> Result<()> {
    let ns = Namespace::new(opts.ns)?;
    let endpoint = ns.endpoint(opts.name)?;
    let spec_bytes = std::fs::read(opts.spec_file);
    let _ = std::fs::remove_file(opts.spec_file);
    let request: SpawnRequest = serde_json::from_slice(&spec_bytes?)?;

    #[cfg(unix)]
    let ready = if opts.foreground {
        None
    } else {
        // SAFETY: this daemon entrypoint has not started any threads on the detach path.
        Some(unsafe { daemonize()? })
    };
    #[cfg(windows)]
    let _ = opts.foreground;

    crate::logfile::init(ns.log_path(opts.name).as_deref());
    let result = start(&endpoint, opts.name, request);
    let (shared, listener) = match result {
        Ok(started) => started,
        Err(e) => {
            log::error!("startup failed: {e}");
            log::logger().flush();
            return Err(e);
        }
    };
    log::info!("serving {} (child pid {})", opts.name, shared.child.pid());
    #[cfg(unix)]
    if let Some(ready) = ready {
        signal_ready(ready);
    }
    accept_loop(&shared, &listener);
}

fn start(
    endpoint: &crate::transport::Endpoint,
    name: &str,
    request: SpawnRequest,
) -> Result<(Arc<Shared>, Arc<Listener>)> {
    if request.spec.name != name {
        return Err(Error::Protocol(format!(
            "spec is for session {:?}, not {name:?}",
            request.spec.name
        )));
    }
    let listener = Arc::new(Listener::bind(endpoint)?);
    let size = PtySize {
        cols: request.cols.max(2),
        rows: request.rows.max(1),
        cell_width: request.cell_width,
        cell_height: request.cell_height,
    };
    let spawned = match pty::spawn(&request.spec, size) {
        Ok(s) => s,
        Err(e) => {
            listener.remove();
            return Err(e);
        }
    };
    let started_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);

    let (event_tx, events) = mpsc::channel();
    let config = Config {
        scrolling_history: SCROLLBACK,
        // ConPTY does not implement kitty keyboard input; omp then uses win32-input-mode.
        kitty_keyboard: cfg!(not(windows)),
        ..Config::default()
    };
    let term = Term::new(config, &term_size(size), EventProxy(event_tx));
    let input = InputQueue::new(INPUT_BUDGET);
    let spec = &request.spec;
    let mut argv = vec![spec.program.to_string_lossy().into_owned()];
    argv.extend(spec.args.iter().cloned());
    let shared = Arc::new(Shared {
        core: Mutex::new(Core {
            term,
            processor: Processor::new(),
            scanner: SideScanner::new(size.rows),
            events,
            palette: request.palette,
            size,
            client: None,
            observers: Vec::new(),
            next_client: 1,
            ever_attached: false,
            replies: Vec::new(),
            tsp: request.spec.tsp.then(|| {
                deck_tsp::terminal::Responder::new(deck_tsp::terminal::TermInfo {
                    cols: u32::from(size.cols),
                    dark: palette_is_dark(&request.palette),
                    reduce_motion: false,
                    hour12: false,
                    input: deck_tsp::terminal::InputEncoding::native(),
                })
            }),
        }),
        sync_cv: Condvar::new(),
        status: Mutex::new(Status {
            exited: None,
            reader_done: false,
            child_exited: false,
            busy: 0,
            first_attach_done: false,
            shutting_down: false,
        }),
        status_cv: Condvar::new(),
        input,
        child: spawned.child,
        listener: Arc::clone(&listener),
        name: name.to_owned(),
        argv,
        cwd: spec.cwd.clone(),
        term_session_id: spec
            .env_set
            .iter()
            .find(|(k, _)| k == "TERM_SESSION_ID")
            .map(|(_, v)| v.clone()),
        started_ms,
    });

    let mut writer = spawned.writer;
    let s = Arc::clone(&shared);
    spawn_thread("pty-writer", move || {
        while let Some(bytes) = s.input.pop() {
            let written = writer.write_all(&bytes);
            s.input.written(bytes.len());
            if let Err(e) = written {
                log::warn!("pty write: {e}");
                break;
            }
        }
        s.input.close();
    })?;
    let s = Arc::clone(&shared);
    let reader = spawned.reader;
    spawn_thread("pty-reader", move || read_loop(&s, reader))?;
    let s = Arc::clone(&shared);
    spawn_thread("sync-timer", move || sync_timer(&s))?;
    let s = Arc::clone(&shared);
    spawn_thread("child-waiter", move || {
        let code = s.child.wait();
        log::info!("child exited: {code:?}");
        s.update_status(|st| st.child_exited = true);
        // Windows: closing the pseudoconsole ends its output pipe; the reader keeps draining.
        s.child.close_console();
        s.wait_status(DRAIN_TIMEOUT, |st| st.reader_done);
        s.input.close();
        s.update_status(|st| st.exited = Some(code));
        // Nobody has seen this session yet (e.g. the program failed at launch): keep serving
        // until the first attach received the final screen and `Exited`, or a kill.
        if !s.core().ever_attached {
            s.wait_status(EXIT_LINGER, |st| st.first_attach_done);
        }
        finish(&s, code);
    })?;
    Ok((shared, listener))
}

/// Byte-budgeted queue of writes to the PTY, drained by the PTY writer thread. Bytes count
/// against the budget until written, so a child that stops reading stalls producers instead
/// of growing memory.
struct InputQueue {
    state: Mutex<InputState>,
    /// Signals both "chunk available" (writer) and "room freed / closed" (producers).
    cv: Condvar,
    budget: usize,
}

struct InputState {
    chunks: VecDeque<Vec<u8>>,
    /// Queued plus in-flight bytes.
    bytes: usize,
    closed: bool,
}

impl InputQueue {
    fn new(budget: usize) -> InputQueue {
        InputQueue {
            state: Mutex::new(InputState {
                chunks: VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            cv: Condvar::new(),
            budget,
        }
    }

    fn state(&self) -> MutexGuard<'_, InputState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn fits(&self, st: &InputState, len: usize) -> bool {
        // An oversized chunk is admitted into an empty queue rather than never.
        st.closed || st.bytes == 0 || st.bytes + len <= self.budget
    }

    /// Blocks until `len` more bytes fit (or the queue closed).
    fn wait_room(&self, len: usize) {
        let st = self.state();
        let _st = self
            .cv
            .wait_while(st, |st| !self.fits(st, len))
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// Queues `bytes` regardless of the budget (after [`InputQueue::wait_room`]).
    fn push(&self, bytes: Vec<u8>) {
        let mut st = self.state();
        if st.closed {
            return;
        }
        st.bytes += bytes.len();
        st.chunks.push_back(bytes);
        self.cv.notify_all();
    }

    /// Queues `bytes` if they fit; `false` if over budget. Never blocks on the child.
    fn try_push(&self, bytes: Vec<u8>) -> bool {
        let mut st = self.state();
        if st.closed {
            return true;
        }
        if st.bytes + bytes.len() > self.budget {
            return false;
        }
        st.bytes += bytes.len();
        st.chunks.push_back(bytes);
        self.cv.notify_all();
        true
    }

    /// Next chunk for the writer; `None` once closed.
    fn pop(&self) -> Option<Vec<u8>> {
        let st = self.state();
        let mut st = self
            .cv
            .wait_while(st, |st| st.chunks.is_empty() && !st.closed)
            .unwrap_or_else(PoisonError::into_inner);
        if st.closed {
            return None;
        }
        st.chunks.pop_front()
    }

    /// The writer finished `len` bytes.
    fn written(&self, len: usize) {
        let mut st = self.state();
        st.bytes = st.bytes.saturating_sub(len);
        self.cv.notify_all();
    }

    /// Drops everything queued and wakes all waiters (the PTY is gone).
    fn close(&self) {
        let mut st = self.state();
        st.closed = true;
        st.chunks.clear();
        st.bytes = 0;
        self.cv.notify_all();
    }
}

fn spawn_thread(name: &str, f: impl FnOnce() + Send + 'static) -> Result<()> {
    std::thread::Builder::new().name(name.to_owned()).spawn(f)?;
    Ok(())
}

fn term_size(size: PtySize) -> TermSize {
    TermSize::new(usize::from(size.cols), usize::from(size.rows))
}

/// Terminal events are queued and handled right after each `advance`, outside the handler.
struct EventProxy(Sender<Event>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event);
    }
}

enum Out {
    Frame(Kind, Vec<u8>),
    /// Live PTY output, shared with the observers.
    Output(Arc<[u8]>),
    Snapshot(Vec<u8>),
}

struct ClientHandle {
    id: u64,
    queue: SyncSender<Out>,
}

/// What an observer's writer sends.
enum ObsItem {
    Frame(Kind, Arc<[u8]>),
    Snapshot(Vec<u8>),
}

impl ObsItem {
    fn cost(&self) -> usize {
        OBSERVER_ITEM_COST
            + match self {
                ObsItem::Frame(_, payload) => payload.len(),
                ObsItem::Snapshot(bytes) => bytes.len(),
            }
    }
}

/// A registered observer (see the module docs).
struct ObserverHandle {
    id: u64,
    queue: Arc<ObserverQueue>,
}

/// Byte-bounded queue towards one observer, drained by its writer thread. Producers never
/// block: a push that does not fit fails and the caller disconnects the observer.
struct ObserverQueue {
    state: Mutex<ObsState>,
    cv: Condvar,
    budget: usize,
    /// The observer's connection, shut down on abort so a writer blocked on a peer that
    /// stopped reading (and the connection's reader) return at once.
    stream: Stream,
}

struct ObsState {
    items: VecDeque<ObsItem>,
    /// Queued plus in-flight cost.
    bytes: usize,
    /// No more pushes; the writer drains what is queued, then closes the connection.
    ending: bool,
    /// Aborted: everything queued is dropped.
    closed: bool,
}

impl ObserverQueue {
    fn new(budget: usize, stream: Stream) -> ObserverQueue {
        ObserverQueue {
            state: Mutex::new(ObsState {
                items: VecDeque::new(),
                bytes: 0,
                ending: false,
                closed: false,
            }),
            cv: Condvar::new(),
            budget,
            stream,
        }
    }

    fn state(&self) -> MutexGuard<'_, ObsState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `item` if the queue is open and it fits (an empty queue admits anything);
    /// `false` otherwise.
    fn push(&self, item: ObsItem) -> bool {
        let cost = item.cost();
        let mut st = self.state();
        if st.closed || st.ending || (st.bytes != 0 && st.bytes + cost > self.budget) {
            return false;
        }
        st.bytes += cost;
        st.items.push_back(item);
        self.cv.notify_all();
        true
    }

    /// Queues the small final `last` frame regardless of the budget, then lets the writer
    /// drain and close the connection.
    fn end(&self, last: ObsItem) {
        let mut st = self.state();
        if st.closed || st.ending {
            return;
        }
        st.bytes += last.cost();
        st.items.push_back(last);
        st.ending = true;
        self.cv.notify_all();
    }

    /// Drops everything queued and closes the connection now.
    fn abort(&self) {
        {
            let mut st = self.state();
            st.closed = true;
            st.items.clear();
            st.bytes = 0;
            self.cv.notify_all();
        }
        // Never blocks (called under the core lock): no flush towards a peer not reading.
        self.stream.abort();
    }

    /// Next item for the writer; `None` once aborted, or ended and drained.
    fn pop(&self) -> Option<ObsItem> {
        let st = self.state();
        let mut st = self
            .cv
            .wait_while(st, |st| st.items.is_empty() && !st.closed && !st.ending)
            .unwrap_or_else(PoisonError::into_inner);
        if st.closed {
            return None;
        }
        st.items.pop_front()
    }

    /// The writer finished an item of `cost`.
    fn written(&self, cost: usize) {
        let mut st = self.state();
        st.bytes = st.bytes.saturating_sub(cost);
    }
}

/// JSON control payload for observer queues, shareable between them.
fn json_payload<T: serde::Serialize>(value: &T) -> Result<Arc<[u8]>> {
    Ok(Arc::from(serde_json::to_vec(value)?))
}

struct Core {
    term: Term<EventProxy>,
    processor: Processor,
    scanner: SideScanner,
    events: Receiver<Event>,
    palette: Palette,
    size: PtySize,
    client: Option<ClientHandle>,
    /// Read-only subscribers; never affect the session.
    observers: Vec<ObserverHandle>,
    next_client: u64,
    /// Whether any client ever attached.
    ever_attached: bool,
    /// Query replies collected while locked; pushed to the PTY after unlocking
    /// ([`Shared::send_replies`]).
    replies: Vec<u8>,
    /// The TSP terminal of a `SessionSpec::tsp` session.
    tsp: Option<deck_tsp::terminal::Responder>,
}

struct Status {
    /// `Some(code)` once the session ended.
    exited: Option<Option<i32>>,
    reader_done: bool,
    /// The child process is gone; its remaining output may still be draining (`exited` is
    /// recorded once that finished).
    child_exited: bool,
    /// Threads that must deliver something before the process exits.
    busy: usize,
    /// A client received the final screen and `Exited` (or killed the session) after the
    /// child exited.
    first_attach_done: bool,
    shutting_down: bool,
}

struct Shared {
    core: Mutex<Core>,
    /// Wakes the sync-update timer (paired with `core`).
    sync_cv: Condvar,
    status: Mutex<Status>,
    status_cv: Condvar,
    input: InputQueue,
    child: PtyChild,
    listener: Arc<Listener>,
    name: String,
    argv: Vec<String>,
    cwd: std::path::PathBuf,
    term_session_id: Option<String>,
    started_ms: u64,
}

impl Shared {
    fn core(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn status(&self) -> MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update_status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status());
        self.status_cv.notify_all();
    }

    /// Waits until `done(status)` or `timeout`; returns whether it became true.
    fn wait_status(&self, timeout: Duration, done: impl Fn(&Status) -> bool) -> bool {
        let guard = self.status();
        let (guard, _) = self
            .status_cv
            .wait_timeout_while(guard, timeout, |st| !done(st))
            .unwrap_or_else(PoisonError::into_inner);
        done(&guard)
    }

    /// Pushes the query replies collected under `core` to the PTY, after unlocking it.
    fn send_replies(&self, mut core: MutexGuard<'_, Core>) {
        if core.replies.is_empty() {
            return;
        }
        let replies = std::mem::take(&mut core.replies);
        drop(core);
        if !self.input.try_push(replies) {
            log::warn!("PTY input queue full; dropping terminal query replies");
        }
    }

    /// If the child already exited, waits until its remaining output (e.g. ConPTY's teardown
    /// sequence) is in the terminal and the exit is recorded, so snapshots show the final
    /// screen and an attach is answered by the exit path.
    fn settle_exit(&self) {
        if self.status().child_exited {
            self.wait_status(DRAIN_TIMEOUT + Duration::from_secs(1), |st| {
                st.exited.is_some()
            });
        }
    }

    fn info(&self, core: &Core) -> HostInfo {
        HostInfo {
            name: self.name.clone(),
            protocol: PROTOCOL,
            host_version: crate::HOST_VERSION.to_owned(),
            daemon_pid: std::process::id(),
            child_pid: self.child.pid(),
            child_started_unix_ms: self.started_ms,
            cwd: self.cwd.clone(),
            argv: self.argv.clone(),
            tty: self.child.tty().map(str::to_owned),
            term_session_id: self.term_session_id.clone(),
            attached: core.client.is_some(),
            cols: core.size.cols,
            rows: core.size.rows,
            title: core.scanner.state().title.clone(),
        }
    }
}

impl Core {
    /// Feeds PTY output to the terminal and side scanner and collects query replies.
    fn feed(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
        self.scanner.advance(bytes);
        // Before `answer_queries`: the hello reply must precede the DA1 reply to the query
        // that follows it in the same output.
        if let Some(tsp) = &mut self.tsp {
            let out = tsp.on_output(bytes, Instant::now());
            self.replies.extend_from_slice(&out);
        }
        self.answer_queries();
    }

    /// Records the controller's palette; a light/dark flip is reported to a live TSP program.
    fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
        if let Some(tsp) = &mut self.tsp {
            let out = tsp.theme(palette_is_dark(&palette));
            self.replies.extend_from_slice(&out);
        }
    }

    /// Appends replies to queued terminal queries to `self.replies`.
    fn answer_queries(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            let reply = match event {
                Event::PtyWrite(text) => text,
                Event::ColorRequest(index, format) => {
                    let rgb = self.term.colors()[index]
                        .unwrap_or_else(|| palette_color(&self.palette, index));
                    format(rgb)
                }
                Event::TextAreaSizeRequest(format) => format(WindowSize {
                    num_lines: self.size.rows,
                    num_cols: self.size.cols,
                    cell_width: self.size.cell_width,
                    cell_height: self.size.cell_height,
                }),
                _ => continue,
            };
            self.replies.extend_from_slice(reply.as_bytes());
        }
    }

    /// Ends a pending synchronized update so the terminal reflects everything received.
    fn flush_sync(&mut self) {
        if self.processor.sync_timeout().sync_timeout().is_some() {
            self.processor.stop_sync(&mut self.term);
            self.answer_queries();
        }
    }

    fn grid(&self) -> Grid {
        Grid {
            cols: self.size.cols,
            rows: self.size.rows,
            cell_width: self.size.cell_width,
            cell_height: self.size.cell_height,
        }
    }

    /// Queues a frame for every observer. Those that are over budget or whose writer failed
    /// are disconnected and forgotten; never blocks.
    fn broadcast(&mut self, kind: Kind, payload: &Arc<[u8]>) {
        self.observers.retain(|o| {
            if o.queue.push(ObsItem::Frame(kind, Arc::clone(payload))) {
                return true;
            }
            log::warn!("observer {} fell behind or went away; dropping it", o.id);
            o.queue.abort();
            false
        });
    }

    fn remove_observer(&mut self, id: u64) {
        self.observers.retain(|o| o.id != id);
    }

    fn resize(
        &mut self,
        child: &PtyChild,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    ) {
        let before = self.size;
        if cell_width != 0 && cell_height != 0 {
            self.size.cell_width = cell_width;
            self.size.cell_height = cell_height;
        }
        if cols != 0 && rows != 0 && (cols != self.size.cols || rows != self.size.rows) {
            self.size.cols = cols.max(2);
            self.size.rows = rows;
            if let Err(e) = child.resize(self.size) {
                log::warn!("pty resize: {e}");
            }
            self.term.resize(term_size(self.size));
            self.scanner.resize(rows);
            if let Some(tsp) = &mut self.tsp {
                let out = tsp.resize(u32::from(self.size.cols));
                self.replies.extend_from_slice(&out);
            }
        }
        // Under the core lock like output, so observers apply it between the same bytes.
        if self.size != before && !self.observers.is_empty() {
            match json_payload(&self.grid()) {
                Ok(grid) => self.broadcast(Kind::Resized, &grid),
                Err(e) => {
                    // Observers would render at a stale size: end their subscriptions.
                    log::error!("encoding the grid: {e}; dropping all observers");
                    for observer in self.observers.drain(..) {
                        observer.queue.abort();
                    }
                }
            }
        }
    }

    fn snapshot(&mut self, history_lines: u32) -> Vec<u8> {
        self.flush_sync();
        let side = self.scanner.state().clone();
        snapshot::encode(&mut self.term, &side, history_lines as usize)
    }
}

/// Whether the controller's background is dark (relative luminance below one half); what a TSP
/// program is told as `dark`.
fn palette_is_dark(palette: &Palette) -> bool {
    let [r, g, b] = palette.0[Palette::BACKGROUND];
    // Rec. 709 weights on the gamma-encoded bytes: enough to tell a light theme from a dark one.
    let luminance = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
    luminance < 127.5
}

/// Color for a query the child's own overrides do not answer: the client's palette, else the
/// xterm 256-color cube and gray ramp.
fn palette_color(palette: &Palette, index: usize) -> Rgb {
    let [r, g, b] = match index {
        0..=15 => palette.0[index],
        16..=231 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            [level(i / 36), level((i / 6) % 6), level(i % 6)]
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            [v, v, v]
        }
        257 => palette.0[Palette::BACKGROUND],
        258 => palette.0[Palette::CURSOR],
        _ => palette.0[Palette::FOREGROUND],
    };
    Rgb { r, g, b }
}

fn read_loop(shared: &Shared, mut reader: pty::PtyReader) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log::warn!("pty read: {e}");
                break;
            }
        };
        let chunk = &buf[..n];
        let mut core = shared.core();
        core.feed(chunk);
        if core.processor.sync_timeout().sync_timeout().is_some() {
            shared.sync_cv.notify_one();
        }
        if core.client.is_some() || !core.observers.is_empty() {
            // One copy of the chunk, shared by the controller and every observer.
            let output: Arc<[u8]> = Arc::from(chunk);
            if let Some(client) = &core.client {
                match client.queue.try_send(Out::Output(Arc::clone(&output))) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        log::warn!("client {} is not keeping up; dropping it", client.id);
                        core.client = None;
                    }
                    Err(TrySendError::Disconnected(_)) => core.client = None,
                }
            }
            core.broadcast(Kind::Output, &output);
        }
        shared.send_replies(core);
    }
    shared.update_status(|st| st.reader_done = true);
}

/// Applies the synchronized-update timeout (150 ms in vte) even when no further output
/// arrives, so queries inside an unterminated update still get answered.
fn sync_timer(shared: &Shared) {
    let mut core = shared.core();
    loop {
        let deadline = core.processor.sync_timeout().sync_timeout();
        core = match deadline {
            None => shared
                .sync_cv
                .wait(core)
                .unwrap_or_else(PoisonError::into_inner),
            Some(at) => {
                let now = Instant::now();
                if at <= now {
                    core.flush_sync();
                    shared.send_replies(core);
                    shared.core()
                } else {
                    shared
                        .sync_cv
                        .wait_timeout(core, at - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            }
        };
    }
}

/// Ends the daemon: endpoint removed, `Exited` to the attached client and pending killers,
/// then process exit.
fn finish(shared: &Shared, code: Option<i32>) -> ! {
    shared.listener.remove();
    shared.update_status(|st| {
        st.exited = Some(code);
        st.shutting_down = true;
    });
    {
        let mut core = shared.core();
        let payload = serde_json::to_vec(&ExitedPayload { code }).unwrap_or_default();
        let exited: Arc<[u8]> = Arc::from(payload.as_slice());
        for observer in core.observers.drain(..) {
            observer
                .queue
                .end(ObsItem::Frame(Kind::Exited, Arc::clone(&exited)));
        }
        if let Some(client) = core.client.take() {
            let _ = client.queue.try_send(Out::Frame(Kind::Exited, payload));
        }
    }
    shared.wait_status(SHUTDOWN_GRACE, |st| st.busy == 0);
    log::info!("session ended");
    log::logger().flush();
    std::process::exit(0);
}

fn accept_loop(shared: &Arc<Shared>, listener: &Listener) -> ! {
    loop {
        match listener.accept() {
            Ok(stream) => {
                log::debug!("connection accepted");
                let s = Arc::clone(shared);
                if let Err(e) = spawn_thread("conn", move || {
                    if let Err(e) = serve_conn(&s, stream) {
                        log::debug!("connection: {e}");
                    }
                }) {
                    log::warn!("spawning connection thread: {e}");
                }
            }
            Err(e) => {
                log::warn!("accept: {e}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Per-connection state.
struct Conn<'a> {
    shared: &'a Arc<Shared>,
    stream: Stream,
    payload: Vec<u8>,
    out: Vec<u8>,
    /// Client id while this connection is (or was) the attached one; its writes then go
    /// through the client queue.
    attached: Option<u64>,
    /// Set by `Observe`: the connection may only `Ping` and `Detach` from then on.
    read_only: bool,
    /// Observer id and queue while subscribed; its replies then go through the queue.
    observing: Option<(u64, Arc<ObserverQueue>)>,
    peer_protocol: u32,
}

impl Conn<'_> {
    fn reply(&mut self, kind: Kind, payload: &[u8]) -> Result<()> {
        if let Some((_, queue)) = &self.observing {
            // Over budget or gone: the stream is closed and the connection ends.
            if !queue.push(ObsItem::Frame(kind, Arc::from(payload))) {
                queue.abort();
            }
            return Ok(());
        }
        if let Some(id) = self.attached {
            let core = self.shared.core();
            if let Some(client) = core.client.as_ref().filter(|c| c.id == id) {
                let _ = client.queue.try_send(Out::Frame(kind, payload.to_vec()));
            }
            return Ok(());
        }
        protocol::write_frame(&mut self.stream, &mut self.out, kind, payload)
    }

    fn reply_json<T: serde::Serialize>(&mut self, kind: Kind, value: &T) -> Result<()> {
        let payload = serde_json::to_vec(value)?;
        self.reply(kind, &payload)
    }

    fn is_current(&self, core: &Core) -> bool {
        self.attached.is_some() && core.client.as_ref().map(|c| c.id) == self.attached
    }
}

fn serve_conn(shared: &Arc<Shared>, stream: Stream) -> Result<()> {
    stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
    let mut conn = Conn {
        shared,
        stream,
        payload: Vec::new(),
        out: Vec::new(),
        attached: None,
        read_only: false,
        observing: None,
        peer_protocol: 0,
    };
    match protocol::read_frame(&mut conn.stream, &mut conn.payload)? {
        Some(Kind::Hello) => {}
        _ => return Ok(()),
    }
    let hello: Hello = protocol::parse(&conn.payload)?;
    conn.peer_protocol = hello.protocol;
    conn.reply_json(Kind::Hello, &Hello::ours())?;
    conn.stream.set_read_timeout(None)?;

    let result = conn_loop(&mut conn);
    if let Some(id) = conn.attached {
        let mut core = shared.core();
        if core.client.as_ref().is_some_and(|c| c.id == id) {
            core.client = None;
        }
    }
    if let Some((id, queue)) = conn.observing.take() {
        shared.core().remove_observer(id);
        queue.abort();
    }
    result
}

fn conn_loop(conn: &mut Conn<'_>) -> Result<()> {
    let shared = conn.shared;
    loop {
        let Some(kind) = protocol::read_frame(&mut conn.stream, &mut conn.payload)? else {
            return Ok(());
        };
        if conn.read_only {
            match kind {
                Kind::Ping => {
                    conn.reply(Kind::Pong, &[])?;
                    continue;
                }
                Kind::Detach => return Ok(()),
                _ => {
                    refuse_read_only(conn, kind);
                    return Ok(());
                }
            }
        }
        if kind == Kind::Kill {
            return kill(conn);
        }
        if kind == Kind::Ping {
            conn.reply(Kind::Pong, &[])?;
            continue;
        }
        if conn.peer_protocol != PROTOCOL {
            let msg = format!(
                "protocol {} not supported (daemon speaks {PROTOCOL})",
                conn.peer_protocol
            );
            conn.reply_json(Kind::Error, &msg)?;
            continue;
        }
        match kind {
            Kind::Info => {
                let info = shared.info(&shared.core());
                conn.reply_json(Kind::Info, &info)?;
            }
            Kind::Peek => {
                let params: PeekParams = protocol::parse(&conn.payload)?;
                shared.settle_exit();
                let mut core = shared.core();
                let snapshot = core.snapshot(params.history_lines);
                shared.send_replies(core);
                for chunk in snapshot.chunks(SNAPSHOT_CHUNK) {
                    conn.reply(Kind::Snapshot, chunk)?;
                }
                conn.reply(Kind::SnapshotEnd, &[])?;
            }
            Kind::TspPeek => {
                let core = shared.core();
                let Some(tsp) = &core.tsp else {
                    drop(core);
                    conn.reply_json(Kind::Error, &"not a TSP session")?;
                    continue;
                };
                let document = serde_json::to_vec(tsp.document());
                drop(core);
                for chunk in document?.chunks(SNAPSHOT_CHUNK) {
                    conn.reply(Kind::Snapshot, chunk)?;
                }
                conn.reply(Kind::SnapshotEnd, &[])?;
            }
            Kind::Attach => {
                let params: AttachParams = protocol::parse(&conn.payload)?;
                attach(conn, &params)?;
            }
            Kind::Observe => {
                if conn.attached.is_some() {
                    conn.reply_json(Kind::Error, &"an attached connection cannot observe")?;
                    continue;
                }
                let params: ObserveParams = protocol::parse(&conn.payload)?;
                observe(conn, &params)?;
            }
            Kind::Input => {
                // Backpressure: while the PTY input queue is over budget this connection's
                // socket is not read, which in turn blocks the client's writes.
                shared.input.wait_room(conn.payload.len());
                let current = conn.is_current(&shared.core());
                if current {
                    shared.input.push(conn.payload.clone());
                }
            }
            Kind::Resize => {
                let p: ResizeParams = protocol::parse(&conn.payload)?;
                let mut core = shared.core();
                if conn.is_current(&core) {
                    core.resize(&shared.child, p.cols, p.rows, p.cell_width, p.cell_height);
                    shared.send_replies(core);
                }
            }
            Kind::Palette => {
                let palette: Palette = protocol::parse(&conn.payload)?;
                let mut core = shared.core();
                if conn.is_current(&core) {
                    core.set_palette(palette);
                    shared.send_replies(core);
                }
            }
            Kind::Detach => {
                let mut core = shared.core();
                if conn.is_current(&core) {
                    // Dropping the queue ends the client writer, which closes the stream.
                    core.client = None;
                }
                return Ok(());
            }
            _ => {}
        }
    }
}

fn attach(conn: &mut Conn<'_>, params: &AttachParams) -> Result<()> {
    let shared = conn.shared;
    if shared.status().shutting_down {
        let payload = serde_json::to_vec(&ExitedPayload {
            code: shared.status().exited.flatten(),
        })?;
        return conn.reply(Kind::Exited, &payload);
    }
    shared.settle_exit();
    let mut core = shared.core();
    // Checked under the core lock: the child waiter reads `ever_attached` (also under it)
    // after recording the exit, so either it sees this attach or this sees the exit.
    let exited = shared.status().exited;
    if let Some(code) = exited {
        return attach_after_exit(conn, core, params.history_lines, code);
    }
    let writer_stream = conn.stream.try_clone()?;
    let (queue, rx) = mpsc::sync_channel(CLIENT_QUEUE);
    core.resize(
        &shared.child,
        params.cols,
        params.rows,
        params.cell_width,
        params.cell_height,
    );
    core.set_palette(params.palette);
    let snapshot = core.snapshot(params.history_lines);
    if let Some(old) = core.client.take() {
        // The old writer sends this, then sees the queue closed and ends its stream.
        let _ = old.queue.try_send(Out::Frame(Kind::Displaced, Vec::new()));
    }
    let id = core.next_client;
    core.next_client += 1;
    // Fresh queue with capacity ≥ 1: cannot fail.
    let _ = queue.try_send(Out::Snapshot(snapshot));
    core.client = Some(ClientHandle { id, queue });
    core.ever_attached = true;
    conn.attached = Some(id);
    shared.send_replies(core);

    shared.update_status(|st| st.busy += 1);
    let s = Arc::clone(shared);
    let spawned = spawn_thread("client-writer", move || {
        client_writer(writer_stream, &rx);
        s.update_status(|st| st.busy -= 1);
    });
    if spawned.is_err() {
        shared.update_status(|st| st.busy -= 1);
    }
    spawned
}

/// Attach while the session lingers after its child exited unseen: the final screen, then
/// `Exited`, written directly (no PTY, nothing else will follow); the daemon then ends.
fn attach_after_exit(
    conn: &mut Conn<'_>,
    mut core: MutexGuard<'_, Core>,
    history_lines: u32,
    code: Option<i32>,
) -> Result<()> {
    let shared = conn.shared;
    let snapshot = core.snapshot(history_lines);
    core.replies.clear();
    drop(core);
    shared.update_status(|st| st.busy += 1);
    let result = snapshot
        .chunks(SNAPSHOT_CHUNK)
        .try_for_each(|c| conn.reply(Kind::Snapshot, c))
        .and_then(|()| conn.reply(Kind::SnapshotEnd, &[]))
        .and_then(|()| conn.reply_json(Kind::Exited, &ExitedPayload { code }));
    shared.update_status(|st| {
        st.busy -= 1;
        st.first_attach_done |= result.is_ok();
    });
    result
}

fn write_snapshot(stream: &mut Stream, buf: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    bytes
        .chunks(SNAPSHOT_CHUNK)
        .try_for_each(|c| protocol::write_frame(stream, buf, Kind::Snapshot, c))?;
    protocol::write_frame(stream, buf, Kind::SnapshotEnd, &[])
}

fn client_writer(mut stream: Stream, rx: &Receiver<Out>) {
    let mut buf = Vec::new();
    for out in rx {
        let result = match out {
            Out::Frame(kind, payload) => {
                protocol::write_frame(&mut stream, &mut buf, kind, &payload)
            }
            Out::Output(payload) => {
                protocol::write_frame(&mut stream, &mut buf, Kind::Output, &payload)
            }
            Out::Snapshot(bytes) => write_snapshot(&mut stream, &mut buf, &bytes),
        };
        if result.is_err() {
            break;
        }
    }
    stream.shutdown();
}

/// `Observe`: registers a read-only subscriber (see the module docs for the guarantees).
fn observe(conn: &mut Conn<'_>, params: &ObserveParams) -> Result<()> {
    let shared = conn.shared;
    conn.read_only = true;
    if shared.status().shutting_down {
        let code = shared.status().exited.flatten();
        return conn.reply_json(Kind::Exited, &ExitedPayload { code });
    }
    shared.settle_exit();
    let writer_stream = conn.stream.try_clone()?;
    let abort_stream = conn.stream.try_clone()?;
    let mut core = shared.core();
    // Checked under the core lock: `finish` drains the observers under it after recording the
    // exit, so either it sees this observer or this sees the exit.
    let exited = shared.status().exited;
    if let Some(code) = exited {
        return observe_after_exit(conn, core, params.history_lines, code);
    }
    let info = match json_payload(&shared.info(&core)) {
        Ok(info) => info,
        Err(e) => {
            // Reported, not dropped: a silent close would read as an older daemon.
            drop(core);
            return conn.reply_json(Kind::Error, &format!("encoding session info: {e}"));
        }
    };
    let grid = json_payload(&core.grid())?;
    let snapshot = core.snapshot(params.history_lines);
    let queue = Arc::new(ObserverQueue::new(
        OBSERVER_BUDGET + snapshot.len(),
        abort_stream,
    ));
    // An empty queue whose budget covers the snapshot admits all three.
    queue.push(ObsItem::Frame(Kind::Info, info));
    queue.push(ObsItem::Frame(Kind::Resized, grid));
    queue.push(ObsItem::Snapshot(snapshot));
    let id = core.next_client;
    core.next_client += 1;
    core.observers.push(ObserverHandle {
        id,
        queue: Arc::clone(&queue),
    });
    conn.observing = Some((id, Arc::clone(&queue)));
    shared.send_replies(core);

    shared.update_status(|st| st.busy += 1);
    let s = Arc::clone(shared);
    let spawned = spawn_thread("observer-writer", move || {
        observer_writer(writer_stream, &queue);
        s.update_status(|st| st.busy -= 1);
    });
    if spawned.is_err() {
        shared.update_status(|st| st.busy -= 1);
    }
    spawned
}

/// Observe while the session lingers after its child exited unseen: info, grid, final screen
/// and `Exited`, written directly. Unlike a first attach this does not end the linger, so the
/// controller still gets the final screen.
fn observe_after_exit(
    conn: &mut Conn<'_>,
    mut core: MutexGuard<'_, Core>,
    history_lines: u32,
    code: Option<i32>,
) -> Result<()> {
    let shared = conn.shared;
    let info = match serde_json::to_vec(&shared.info(&core)) {
        Ok(info) => info,
        Err(e) => {
            drop(core);
            return conn.reply_json(Kind::Error, &format!("encoding session info: {e}"));
        }
    };
    let grid = core.grid();
    let snapshot = core.snapshot(history_lines);
    core.replies.clear();
    drop(core);
    shared.update_status(|st| st.busy += 1);
    let (stream, out) = (&mut conn.stream, &mut conn.out);
    let result = protocol::write_frame(stream, out, Kind::Info, &info)
        .and_then(|()| protocol::write_json(stream, out, Kind::Resized, &grid))
        .and_then(|()| write_snapshot(stream, out, &snapshot))
        .and_then(|()| protocol::write_json(stream, out, Kind::Exited, &ExitedPayload { code }));
    shared.update_status(|st| st.busy -= 1);
    result
}

fn observer_writer(mut stream: Stream, queue: &ObserverQueue) {
    let mut buf = Vec::new();
    while let Some(item) = queue.pop() {
        let result = match &item {
            ObsItem::Frame(kind, payload) => {
                protocol::write_frame(&mut stream, &mut buf, *kind, payload)
            }
            ObsItem::Snapshot(bytes) => write_snapshot(&mut stream, &mut buf, bytes),
        };
        queue.written(item.cost());
        if result.is_err() {
            queue.abort();
            return;
        }
    }
    stream.shutdown();
}

/// A frame other than `Ping`/`Detach` on an observer connection is a violation: the
/// subscription is aborted at once (queued data dropped, its writer unblocked, connection
/// closed). The disconnect is the rejection; nothing is promised to the misbehaving peer, so
/// a peer that never reads cannot pin a writer thread. Nothing reaches the session.
fn refuse_read_only(conn: &mut Conn<'_>, kind: Kind) {
    log::warn!("observer connection sent {kind:?}; disconnected");
    match conn.observing.take() {
        Some((id, queue)) => {
            conn.shared.core().remove_observer(id);
            queue.abort();
        }
        // Answered after the session ended (written directly, nothing queued).
        None => conn.stream.abort(),
    }
}

/// `Kill`: terminate the process tree, wait for the session to end, confirm with `Exited`.
fn kill(conn: &mut Conn<'_>) -> Result<()> {
    let shared = conn.shared;
    shared.update_status(|st| st.busy += 1);
    if shared.status().exited.is_none() {
        log::info!("kill requested");
        shared.child.kill_tree(KILL_GRACE);
    }
    shared.wait_status(DRAIN_TIMEOUT + KILL_GRACE, |st| st.exited.is_some());
    let code = shared.status().exited.flatten();
    // The attached client gets `Exited` through its queue from `finish`.
    let result = if conn.attached.is_none() {
        conn.reply_json(Kind::Exited, &ExitedPayload { code })
    } else {
        Ok(())
    };
    shared.update_status(|st| {
        st.busy -= 1;
        // Ends a linger after an unseen exit.
        st.first_attach_done = true;
    });
    result
}

/// Forks into a detached session and returns the child's readiness fd. The launcher parent
/// waits for readiness and exits 0, or exits 1 if the daemon dies first.
///
/// # Safety
///
/// Call only at process entry while still single-threaded, before creating worker threads.
#[cfg(unix)]
pub unsafe fn daemonize() -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let mut fds = [0; 2];
    // SAFETY: valid out array of two fds.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(Error::last_os("pipe"));
    }
    for fd in fds {
        // Still single-threaded: no fork can race this. The session child must not inherit
        // the write end, or the launcher would wait for it too.
        // SAFETY: valid fds from pipe().
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // SAFETY: freshly created fds we own.
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // SAFETY: called while the process is single-threaded (before any thread is spawned).
    match unsafe { libc::fork() } {
        -1 => Err(Error::last_os("fork")),
        0 => {
            drop(read_end);
            // SAFETY: plain syscalls in the child.
            unsafe {
                libc::setsid();
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                if null >= 0 {
                    libc::dup2(null, 0);
                    libc::dup2(null, 1);
                    libc::dup2(null, 2);
                    if null > 2 {
                        libc::close(null);
                    }
                }
            }
            Ok(write_end)
        }
        _ => {
            drop(write_end);
            let mut file = std::fs::File::from(read_end);
            let mut byte = [0u8; 1];
            let ok = matches!(file.read(&mut byte), Ok(1));
            std::process::exit(if ok { 0 } else { 1 });
        }
    }
}

/// Releases the detached launcher's readiness wait after the endpoint is accepting clients.
#[cfg(unix)]
pub fn signal_ready(fd: std::os::fd::OwnedFd) {
    let _ = std::fs::File::from(fd).write_all(b"1");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_colors_fall_back_to_palette_and_xterm_ramps() {
        let mut p = Palette::default();
        p.0[3] = [1, 2, 3];
        p.0[Palette::BACKGROUND] = [9, 8, 7];
        assert_eq!(palette_color(&p, 3), Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(palette_color(&p, 257), Rgb { r: 9, g: 8, b: 7 });
        assert_eq!(palette_color(&p, 16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(palette_color(&p, 196), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(
            palette_color(&p, 244),
            Rgb {
                r: 128,
                g: 128,
                b: 128
            }
        );
    }
}
