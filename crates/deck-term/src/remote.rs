//! Remote terminals: the child runs under a session host (`omp-deck-host`) that owns the PTY
//! and its own headless `Term`; this side renders and sends input. The host answers every
//! terminal query, so the local `Term` here only displays (see [`crate::Terminal::connect`]).
//!
//! Threads per remote terminal: a reader (blocking [`RemoteRx::recv`]), a feeder (parses
//! output into the shared `Term` and runs the output scanners; owns the synchronized-update
//! timeout) and a writer (drains [`RemoteCmd`]s into [`RemoteTx`], so the UI thread never
//! blocks on the connection).

use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::Term;

use crate::core::Notice;
use crate::engine::{Engine, OutputEvent};
use crate::frontend::FrontendState;
use crate::out::{PtyOut, WriteProgress};
use crate::wake::Wake;

/// What a session host (or, for a frontend, the gateway) sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteEvent {
    /// Whole-screen replay (VT bytes) that a fresh terminal turns into the host's state; the
    /// local terminal is reset before it is applied. A frontend accepts input from its first
    /// snapshot on, and again after [`Self::Disconnected`] once a new one arrived.
    Snapshot(Vec<u8>),
    /// Child output.
    Output(Vec<u8>),
    /// Authoritative host geometry, sent before an observer snapshot and after a controller
    /// resize. It changes only this renderer, never the host.
    Resize {
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    },
    /// Frontend only: owner state (key rules, input hold, win32 modifiers, clipboard role).
    State(FrontendState),
    /// The child exited (`None`: no exit code, e.g. killed by a signal).
    Exited(Option<i32>),
    /// Another client attached to the session; this connection is over.
    Displaced,
    /// Frontend only: the link to the owner is lost. The view stays, stale and read-only,
    /// until a fresh [`Self::Snapshot`] (a re-established, identity-checked attach). Not an
    /// exit. Other remotes treat it as [`Self::Closed`].
    Disconnected,
    /// The connection ended.
    Closed,
}

/// A connection to a session host, split into its receiving and sending halves by
/// [`crate::Terminal::connect`].
pub trait Remote: Send {
    fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn RemoteTx>);
}

/// Receiving half (runs on its own thread).
pub trait RemoteRx: Send {
    /// Blocks for the next event. After [`RemoteTx::detach`] or [`RemoteTx::kill`] it must
    /// return [`RemoteEvent::Closed`] (or an error) promptly; an error counts as `Closed`.
    fn recv(&mut self) -> io::Result<RemoteEvent>;
}

/// Sending half (runs on its own thread; calls may block). Dropping it detaches.
pub trait RemoteTx: Send + Sync {
    /// Input bytes for the child (keys, paste, focus reports, OSC 52 replies).
    fn input(&self, bytes: &[u8]) -> io::Result<()>;
    /// New grid size and cell pixel size.
    fn resize(&self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) -> io::Result<()>;
    /// Colors the host answers OSC 4/10/11/12 queries with: ANSI 0..=15, foreground,
    /// background, cursor.
    fn palette(&self, palette: [[u8; 3]; 19]) -> io::Result<()>;
    /// Ends the child (and the session).
    fn kill(&self) -> io::Result<()>;
    /// Leaves the session running.
    fn detach(&self) -> io::Result<()>;
}

/// Read-only connection to a session's output. It cannot acquire controller rights.
pub trait Observer: Send {
    fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn ObserverTx>);
}

/// The only operation available to an observer: stop following, leaving the child running.
pub trait ObserverTx: Send + Sync {
    /// Must promptly unblock the receiving half. Dropping the last sending half also detaches.
    fn detach(&self) -> io::Result<()>;
}

pub(crate) struct ObserverRemote(pub(crate) Box<dyn Observer>);

impl Remote for ObserverRemote {
    fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn RemoteTx>) {
        let (rx, tx) = self.0.split();
        (rx, Box::new(ReadOnlyTx(tx)))
    }
}

struct ReadOnlyTx(Box<dyn ObserverTx>);

fn read_only() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "terminal observer is read-only",
    ))
}

impl RemoteTx for ReadOnlyTx {
    fn input(&self, _: &[u8]) -> io::Result<()> {
        read_only()
    }
    fn resize(&self, _: u16, _: u16, _: u16, _: u16) -> io::Result<()> {
        read_only()
    }
    fn palette(&self, _: [[u8; 3]; 19]) -> io::Result<()> {
        read_only()
    }
    fn kill(&self) -> io::Result<()> {
        read_only()
    }
    fn detach(&self) -> io::Result<()> {
        self.0.detach()
    }
}

/// Work for the writer thread, in order.
pub(crate) enum RemoteCmd {
    Input(Vec<u8>),
    Resize(WindowSize),
    Palette([[u8; 3]; 19]),
    Detach,
    Kill,
}

/// How long after a grid resize the scroll position is held (see [`ScrollKeep`]).
const KEEP_FOR: Duration = Duration::from_millis(1500);

/// Keeps a scrolled-back view where it was across a resize.
///
/// Apps that draw into the main screen (omp's TUI) answer a width change by clearing the screen
/// and the scrollback (`CSI 2 J`, `CSI 3 J`) and printing everything again. Clearing resets the
/// `Term`'s view to the bottom, and the reprint leaves it there. For a short window after a
/// resize that started scrolled back, the [`Engine`] puts the view back at the same distance from
/// the bottom after every chunk it parses. The distance is in lines, so rewrapped text can
/// shift the view slightly; it does not jump to the end.
#[derive(Default)]
pub(crate) struct ScrollKeep(Mutex<Option<(usize, Instant)>>);

impl ScrollKeep {
    /// A resize is about to change the grid while the view is `offset` lines above the bottom.
    /// A view already at the bottom arms nothing (and leaves a running window alone: the
    /// clearing redraw of the previous resize may have put it there).
    pub(crate) fn arm(&self, offset: usize) {
        if offset > 0 {
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) =
                Some((offset, Instant::now() + KEEP_FOR));
        }
    }

    /// The user moved the view or typed: stop holding it.
    pub(crate) fn cancel(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Puts the view back at the held distance (clamped to the scrollback now present).
    pub(crate) fn pin<T: EventListener>(&self, term: &mut Term<T>) {
        let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((offset, until)) = *slot else { return };
        if Instant::now() >= until {
            *slot = None;
            return;
        }
        let want = offset.min(term.grid().history_size());
        let now = term.grid().display_offset();
        if want != now {
            term.scroll_display(Scroll::Delta(want as i32 - now as i32));
        }
    }
}

/// Writer thread: forwards commands until `Detach`/`Kill` (or every sender is gone).
/// Handled bytes advance even on failure for bounded detach; only successful input calls
/// advance the separate delivery acknowledgement. A failed input closes the connection.
pub(crate) fn run_writer(
    tx: Box<dyn RemoteTx>,
    cmds: Receiver<RemoteCmd>,
    progress: Arc<WriteProgress>,
) {
    let mut failed = false;
    let mut report = |what: &str, result: io::Result<()>| {
        if let Err(e) = result
            && !failed
        {
            failed = true;
            log::debug!("remote terminal {what} failed: {e}");
        }
    };
    while let Ok(cmd) = cmds.recv() {
        match cmd {
            RemoteCmd::Input(bytes) => {
                let result = tx.input(&bytes);
                progress.record(bytes.len(), result.is_ok());
                if result.is_err() {
                    report("input", result);
                    let _ = tx.detach();
                    return;
                }
            }
            RemoteCmd::Resize(ws) => report(
                "resize",
                tx.resize(ws.num_cols, ws.num_lines, ws.cell_width, ws.cell_height),
            ),
            RemoteCmd::Palette(p) => report("palette", tx.palette(p)),
            RemoteCmd::Detach => return report("detach", tx.detach()),
            RemoteCmd::Kill => return report("kill", tx.kill()),
        }
    }
}

/// Reader thread: forwards events to the feeder until the connection ends.
pub(crate) fn run_reader(mut rx: Box<dyn RemoteRx>, events: SyncSender<RemoteEvent>) {
    loop {
        let event = rx.recv().unwrap_or_else(|e| {
            log::debug!("remote terminal connection lost: {e}");
            RemoteEvent::Closed
        });
        let closed = event == RemoteEvent::Closed;
        if events.send(event).is_err() || closed {
            return;
        }
    }
}

/// Feeds remote output through the terminal's [`Engine`] and reports session state changes.
pub(crate) struct Feeder {
    pub(crate) engine: Arc<Engine>,
    pub(crate) out: Arc<PtyOut>,
    pub(crate) notices: Sender<Notice>,
    pub(crate) wake: Wake,
    /// A gateway frontend (state and connectivity events apply).
    pub(crate) frontend: bool,
}

impl Feeder {
    /// Runs until the reader is gone or the connection closed.
    pub(crate) fn run(self, events: Receiver<RemoteEvent>) {
        loop {
            let event = match self.engine.sync_deadline() {
                Some(deadline) => {
                    match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(event) => event,
                        Err(RecvTimeoutError::Timeout) => {
                            // The app never ended its synchronized update: show what it drew.
                            self.engine.flush_sync_due();
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                None => match events.recv() {
                    Ok(event) => event,
                    Err(_) => return,
                },
            };
            match event {
                RemoteEvent::Snapshot(bytes) => {
                    self.engine.reset(&bytes);
                    self.notify(Notice::Reset);
                }
                RemoteEvent::Output(bytes) => self.engine.feed(&bytes),
                RemoteEvent::Resize {
                    cols,
                    rows,
                    cell_width,
                    cell_height,
                } => {
                    if cols == 0 || rows == 0 {
                        return self.close();
                    }
                    let size = WindowSize {
                        num_cols: cols,
                        num_lines: rows,
                        cell_width,
                        cell_height,
                    };
                    self.engine.resize(size);
                    self.notify(Notice::Resized(size));
                }
                RemoteEvent::State(state) => {
                    if self.frontend {
                        self.notify(Notice::State(state));
                    }
                }
                RemoteEvent::Exited(code) => {
                    self.engine.finish(OutputEvent::Exited(code));
                    self.notify(Notice::Exited(code));
                }
                RemoteEvent::Displaced => {
                    self.out.displaced();
                    self.engine.finish(OutputEvent::Closed);
                    self.notify(Notice::Displaced);
                }
                RemoteEvent::Disconnected if self.frontend => {
                    self.notify(Notice::Disconnected);
                }
                RemoteEvent::Disconnected | RemoteEvent::Closed => return self.close(),
            }
        }
    }

    fn close(&self) {
        self.out.displaced();
        self.engine.finish(OutputEvent::Closed);
        self.notify(Notice::Closed);
    }

    fn notify(&self, notice: Notice) {
        let _ = self.notices.send(notice);
        self.wake.now();
    }
}

/// Starts the reader and feeder threads of a remote terminal. A thread that cannot start ends
/// the connection (`Closed`).
pub(crate) fn start(rx: Box<dyn RemoteRx>, feeder: Feeder) {
    // Bound decoded-but-unparsed frames. Without backpressure here a slow observer renderer
    // could drain the host's bounded socket queue into unbounded frontend memory.
    let (ev_tx, ev_rx) = mpsc::sync_channel(4);
    let notices = feeder.notices.clone();
    let wake = feeder.wake.clone();
    let started = std::thread::Builder::new()
        .name("deck-term-remote-feed".into())
        .spawn(move || feeder.run(ev_rx))
        .and_then(|_| {
            std::thread::Builder::new()
                .name("deck-term-remote-rx".into())
                .spawn(move || run_reader(rx, ev_tx))
        });
    if let Err(e) = started {
        log::warn!("remote terminal reader threads: {e}");
        let _ = notices.send(Notice::Closed);
        wake.now();
    }
}

/// Starts the writer thread of a remote terminal; returns its command queue and handle.
pub(crate) fn spawn_writer(
    tx: Box<dyn RemoteTx>,
    progress: Arc<WriteProgress>,
) -> (Sender<RemoteCmd>, Option<std::thread::JoinHandle<()>>) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let writer = std::thread::Builder::new()
        .name("deck-term-remote-tx".into())
        .spawn(move || run_writer(tx, cmd_rx, progress));
    match writer {
        Ok(handle) => (cmd_tx, Some(handle)),
        Err(e) => {
            log::warn!("remote terminal writer thread: {e}");
            (cmd_tx, None)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::term::TermMode;

    use super::*;
    use crate::input::KeyRules;
    use crate::theme::{Palette, TermTheme};
    use crate::{BlockedKind, RunPhase, RunState, TermOptions, Terminal, wait_detached};

    const TIMEOUT: Duration = Duration::from_secs(10);

    /// What the terminal sent to the fake host.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum Sent {
        Input(Vec<u8>),
        Resize(u16, u16, u16, u16),
        Palette([[u8; 3]; 19]),
        Kill,
        Detach,
    }

    pub(crate) type Log = Arc<Mutex<Vec<Sent>>>;

    /// In-memory host: events come from the test, everything sent is logged.
    struct Fake {
        events: Receiver<RemoteEvent>,
        log: Log,
    }

    struct FakeRx(Receiver<RemoteEvent>);

    struct FakeTx(Log);

    impl Remote for Fake {
        fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn RemoteTx>) {
            (Box::new(FakeRx(self.events)), Box::new(FakeTx(self.log)))
        }
    }

    impl RemoteRx for FakeRx {
        fn recv(&mut self) -> io::Result<RemoteEvent> {
            Ok(self.0.recv().unwrap_or(RemoteEvent::Closed))
        }
    }

    impl FakeTx {
        fn push(&self, sent: Sent) -> io::Result<()> {
            self.0.lock().expect("log").push(sent);
            Ok(())
        }
    }

    impl RemoteTx for FakeTx {
        fn input(&self, bytes: &[u8]) -> io::Result<()> {
            self.push(Sent::Input(bytes.to_vec()))
        }

        fn resize(&self, cols: u16, rows: u16, cell_w: u16, cell_h: u16) -> io::Result<()> {
            self.push(Sent::Resize(cols, rows, cell_w, cell_h))
        }

        fn palette(&self, palette: [[u8; 3]; 19]) -> io::Result<()> {
            self.push(Sent::Palette(palette))
        }

        fn kill(&self) -> io::Result<()> {
            self.push(Sent::Kill)
        }

        fn detach(&self) -> io::Result<()> {
            self.push(Sent::Detach)
        }
    }

    pub(crate) fn connect() -> (Terminal, Sender<RemoteEvent>, Log) {
        let (host, events) = mpsc::channel();
        let log = Log::default();
        let fake = Fake {
            events,
            log: log.clone(),
        };
        let t = Terminal::connect(
            Box::new(fake),
            TermOptions::default(),
            egui::Context::default(),
        );
        (t, host, log)
    }

    fn output(host: &Sender<RemoteEvent>, bytes: &[u8]) {
        host.send(RemoteEvent::Output(bytes.to_vec()))
            .expect("feeder alive");
    }

    /// Polls `t` until `f` holds, failing after [`TIMEOUT`].
    fn until(t: &mut Terminal, what: &str, mut f: impl FnMut(&mut Terminal) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !f(t) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            t.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn logged(log: &Log) -> Vec<Sent> {
        log.lock().expect("log").clone()
    }

    fn inputs(log: &Log) -> Vec<u8> {
        logged(log)
            .into_iter()
            .filter_map(|s| match s {
                Sent::Input(b) => Some(b),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// Output the feeder has certainly processed: a marker drawn after it is on screen.
    fn settle(t: &mut Terminal, host: &Sender<RemoteEvent>, marker: &str) {
        output(host, format!("\r\n{marker}").as_bytes());
        until(t, marker, |t| {
            t.screen_text().lines.iter().any(|l| l.contains(marker))
        });
    }

    #[test]
    fn snapshot_resets_and_replays_then_output_follows() {
        let (mut t, host, _log) = connect();
        // State before the snapshot: text, scrollback, modes, title, key modes.
        let mut before = Vec::new();
        for i in 0..40 {
            before.extend_from_slice(format!("old{i}\r\n").as_bytes());
        }
        before.extend_from_slice(b"\x1b]0;old title\x07\x1b[?1004h\x1b[>4;2m\x1b[?9001h");
        output(&host, &before);
        until(&mut t, "pre-snapshot title", |t| {
            t.title() == Some("old title")
        });
        until(&mut t, "win32 mode", |t| t.key_rules() == KeyRules::Win32);
        assert!(t.mode().contains(TermMode::FOCUS_IN_OUT));

        host.send(RemoteEvent::Snapshot(
            b"\x1b]2;snap\x07hello\x1b[?2004h\x1b[2;5H".to_vec(),
        ))
        .expect("send");
        until(&mut t, "snapshot replay", |t| {
            t.screen_text().lines[0] == "hello"
        });
        until(&mut t, "snapshot title", |t| t.title() == Some("snap"));
        let text = t.screen_text();
        assert!(text.lines[1..].iter().all(String::is_empty), "{text:?}");
        assert_eq!((text.cursor_row, text.cursor_col), (1, 4));
        {
            let term = t.term.lock();
            assert_eq!(term.grid().history_size(), 0, "scrollback reset");
        }
        let mode = t.mode();
        assert!(mode.contains(TermMode::BRACKETED_PASTE));
        assert!(!mode.contains(TermMode::FOCUS_IN_OUT), "modes reset");
        // Key modes reset too (the snapshot re-sends the ones still active).
        assert_eq!(t.key_rules(), KeyRules::Legacy);

        // Live output continues on the replayed state.
        output(&host, b"world");
        until(&mut t, "output after snapshot", |t| {
            t.screen_text().lines[1] == "    world"
        });
        // The modifyOtherKeys and win32 scanners run on remote output, across chunks.
        output(&host, b"\x1b[?90");
        output(&host, b"01h");
        until(&mut t, "win32 mode again", |t| {
            t.key_rules() == KeyRules::Win32
        });
        output(&host, b"\x1b[?9001l\x1b[>4;");
        output(&host, b"2m");
        until(&mut t, "modifyOtherKeys", |t| {
            t.key_rules() == KeyRules::ModifyOtherKeys
        });
    }

    #[test]
    fn replies_suppressed_but_clipboard_load_answered() {
        let (mut t, host, log) = connect();
        // DA1, DA2, DSR, CPR, kitty query, OSC 10/11/4 color queries, CSI 14/18 t.
        output(
            &host,
            b"\x1b[c\x1b[>c\x1b[5n\x1b[6n\x1b[?u\x1b]10;?\x07\x1b]11;?\x07\x1b]4;1;?\x07\x1b[14t\x1b[18t",
        );
        settle(&mut t, &host, "queried");
        for _ in 0..5 {
            t.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(inputs(&log), b"", "the host answers queries, not this side");

        // OSC 52 read: answered here (the clipboard lives here).
        output(&host, b"\x1b]52;c;?\x07");
        until(&mut t, "clipboard reply", |_| !inputs(&log).is_empty());
        let sent = inputs(&log);
        assert!(sent.starts_with(b"\x1b]52;c;"), "{sent:?}");
        assert!(sent.ends_with(b"\x07"), "{sent:?}");
        assert_eq!(sent.iter().filter(|&&b| b == 0x1b).count(), 1, "{sent:?}");
    }

    #[test]
    fn resize_and_palette_forwarded() {
        let (mut t, _host, log) = connect();
        // The theme's palette goes out at once.
        let dark = Palette::for_theme(TermTheme::Dark).host_colors();
        until(&mut t, "initial palette", |_| !logged(&log).is_empty());
        assert_eq!(logged(&log), [Sent::Palette(dark)]);
        // The first size is always sent (the host's size is unknown here), even if it
        // equals the initial one; unchanged sizes are not repeated.
        t.resize(80, 24, 8, 16);
        t.resize(80, 24, 8, 16);
        t.resize(100, 30, 9, 18);
        t.resize(100, 30, 10, 20);
        {
            let term = t.term.lock();
            assert_eq!((term.columns(), term.screen_lines()), (100, 30));
        }
        t.set_theme(TermTheme::Light);
        t.set_theme(TermTheme::Light);
        let light = Palette::for_theme(TermTheme::Light).host_colors();
        assert_ne!(light, dark);
        let want = [
            Sent::Palette(dark),
            Sent::Resize(80, 24, 8, 16),
            Sent::Resize(100, 30, 9, 18),
            Sent::Resize(100, 30, 10, 20),
            Sent::Palette(light),
        ];
        until(&mut t, "forwarded", |_| logged(&log).len() >= want.len());
        assert_eq!(logged(&log), want);
        // Light palette layout: ANSI 0..=15, foreground, background, cursor.
        let p = Palette::for_theme(TermTheme::Light);
        let rgb = |c: alacritty_terminal::vte::ansi::Rgb| [c.r, c.g, c.b];
        assert_eq!(light[1], rgb(p.ansi[1]));
        assert_eq!(
            light[16..],
            [rgb(p.foreground), rgb(p.background), rgb(p.cursor)]
        );
    }

    #[test]
    fn displaced_stops_input_without_exit() {
        let (mut t, host, log) = connect();
        t.write(b"before");
        until(&mut t, "input", |_| inputs(&log) == b"before");
        host.send(RemoteEvent::Displaced).expect("send");
        until(&mut t, "displaced", |t| t.is_displaced());
        assert_eq!(t.banner.as_deref(), Some("[attached elsewhere]"));
        // The connection is let go; a following Closed is no exit.
        until(&mut t, "detach", |_| {
            logged(&log).last() == Some(&Sent::Detach)
        });
        host.send(RemoteEvent::Closed).expect("send");
        std::thread::sleep(Duration::from_millis(50));
        let ev = t.poll();
        assert_eq!(ev.exited, None);
        assert!(!t.is_exited());
        t.write(b"after");
        t.resize(90, 20, 8, 16);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(inputs(&log), b"before");
        assert!(!logged(&log).iter().any(|s| matches!(s, Sent::Resize(..))));
    }

    #[test]
    fn exit_is_reported_once_and_closed_after_it_ignored() {
        let (mut t, host, _log) = connect();
        host.send(RemoteEvent::Exited(Some(3))).expect("send");
        host.send(RemoteEvent::Closed).expect("send");
        let mut exited = None;
        until(&mut t, "exit", |t| {
            exited = exited.or(t.poll().exited);
            exited.is_some()
        });
        assert_eq!(exited, Some(3));
        assert_eq!(t.exit_code(), Some(3));
        assert_eq!(t.banner.as_deref(), Some("[process exited: 3]"));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(t.poll().exited, None);

        let (mut t, host, _log) = connect();
        host.send(RemoteEvent::Exited(None)).expect("send");
        until(&mut t, "signal exit", |t| t.exit_code() == Some(-1));

        // Connection lost without an exit status.
        let (mut t, host, _log) = connect();
        drop(host);
        until(&mut t, "lost", |t| t.exit_code() == Some(-1));
        assert_eq!(
            t.banner.as_deref(),
            Some("[connection to the session host lost]")
        );
    }

    #[test]
    fn kill_and_drop_reach_the_host() {
        let (mut t, _host, log) = connect();
        t.kill();
        assert_eq!(t.exit_code(), Some(-1));
        until(&mut t, "kill", |_| logged(&log).last() == Some(&Sent::Kill));

        // Drop: input written before is delivered, then the session is detached (not killed).
        let (t, _host, log) = connect();
        t.write(b"typed");
        drop(t);
        wait_detached(TIMEOUT);
        let sent = logged(&log);
        assert_eq!(sent.last(), Some(&Sent::Detach));
        assert!(!sent.contains(&Sent::Kill));
        assert_eq!(inputs(&log), b"typed");
    }

    #[test]
    fn synchronized_update_flushes_after_timeout() {
        let (mut t, host, _log) = connect();
        output(&host, b"\x1b[?2026hpending");
        // Held back while the update is open; shown once the timeout (150 ms) passed even
        // though no further output arrives.
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(t.screen_text().lines[0], "");
        until(&mut t, "sync timeout", |t| {
            t.screen_text().lines[0] == "pending"
        });
    }

    #[test]
    fn paste_bytes_follow_bracketed_paste_and_key_rules() {
        let (mut t, host, _log) = connect();
        assert_eq!(t.paste_bytes("hi"), None, "?2004 off");

        output(&host, b"\x1b[?2004h");
        until(&mut t, "?2004", |t| {
            t.mode().contains(TermMode::BRACKETED_PASTE)
        });
        assert_eq!(
            t.paste_bytes("one\ntwo\r\nthree").as_deref(),
            Some(&b"\x1b[200~one\ntwo\r\nthree\x1b[201~"[..])
        );
        // An embedded end marker cannot close the bracket early.
        assert_eq!(
            t.paste_bytes("a\x1b[201~b").as_deref(),
            Some(&b"\x1b[200~ab\x1b[201~"[..])
        );
        assert_eq!(t.paste_bytes("").as_deref(), Some(&b""[..]));

        // win32-input-mode: ConPTY's paste filter (newlines to CR, ESC dropped), bracketed.
        output(&host, b"\x1b[?9001h");
        until(&mut t, "win32 mode", |t| t.key_rules() == KeyRules::Win32);
        assert_eq!(
            t.paste_bytes("a\nb\x1b[201~c").as_deref(),
            Some(&b"\x1b[200~a\rb[201~c\x1b[201~"[..])
        );

        output(&host, b"\x1b[?2004l");
        until(&mut t, "?2004 off", |t| {
            !t.mode().contains(TermMode::BRACKETED_PASTE)
        });
        assert_eq!(t.paste_bytes("a\nb"), None);
    }

    struct Follow {
        events: Receiver<RemoteEvent>,
        end: Sender<RemoteEvent>,
    }

    struct FollowTx(Sender<RemoteEvent>);

    impl Observer for Follow {
        fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn ObserverTx>) {
            (Box::new(FakeRx(self.events)), Box::new(FollowTx(self.end)))
        }
    }

    impl ObserverTx for FollowTx {
        fn detach(&self) -> io::Result<()> {
            self.0.send(RemoteEvent::Closed).map_err(io::Error::other)
        }
    }

    #[test]
    fn observer_uses_host_geometry_without_acquiring_input_or_resize_rights() {
        use crate::input::{Key, Mods};
        let (host, events) = mpsc::channel();
        let mut t = Terminal::observe_headless(
            Box::new(Follow {
                events,
                end: host.clone(),
            }),
            TermOptions::default(),
            Arc::new(|_| {}),
        );
        host.send(RemoteEvent::Resize {
            cols: 100,
            rows: 30,
            cell_width: 9,
            cell_height: 18,
        })
        .expect("send geometry");
        host.send(RemoteEvent::Snapshot(
            b"\x1b[?2004h\x1b[?1000h\x1b[?1004h\x1b[30;96Hedge".to_vec(),
        ))
        .expect("send snapshot");
        until(&mut t, "snapshot at host edge", |t| {
            let screen = t.screen_text();
            screen.lines.len() == 30 && screen.lines[29].ends_with("edge")
        });
        t.resize(20, 4, 8, 16);
        assert_eq!(t.screen_text().lines.len(), 30);
        assert_eq!(t.term.lock().columns(), 100);
        assert!(t.is_read_only());
        assert!(!t.accepts_input());
        assert!(t.input_held());
        assert_eq!(t.paste_bytes("must not submit"), None);
        assert!(
            !t.mode()
                .intersects(TermMode::MOUSE_MODE | TermMode::ALTERNATE_SCROLL)
        );
        t.send_key(Key::Char('c'), Mods::CTRL);
        t.set_focused(true);
        host.send(RemoteEvent::Resize {
            cols: 60,
            rows: 12,
            cell_width: 8,
            cell_height: 16,
        })
        .expect("send controller resize");
        output(&host, b"\x1b[2J\x1b[12;55Hnew");
        until(&mut t, "controller resize output", |t| {
            let screen = t.screen_text();
            screen.lines.len() == 12 && screen.lines[11].ends_with("new")
        });
        assert_eq!(t.term.lock().columns(), 60);
        drop(t);
        wait_detached(TIMEOUT);
    }

    #[test]
    fn detaching_an_observer_is_not_a_process_exit() {
        let (host, events) = mpsc::channel();
        let (wake, awakened) = mpsc::channel();
        let mut t = Terminal::observe_headless(
            Box::new(Follow { events, end: host }),
            TermOptions::default(),
            Arc::new(move |delay| {
                let _ = wake.send(delay);
            }),
        );
        t.kill();
        awakened
            .recv_timeout(TIMEOUT)
            .expect("detach closed the receiver");
        let events = t.poll();
        assert_eq!(events.exited, None);
        assert_eq!(t.exit_code(), None);
        assert_eq!(t.banner.as_deref(), Some("[observer detached]"));
        drop(t);
        wait_detached(TIMEOUT);
    }

    #[test]
    fn an_observer_mirrors_tsp_but_never_writes() {
        use deck_tsp::stream::{Verb, encode_message};
        let (host, events) = mpsc::channel();
        let mut t = Terminal::observe_headless(
            Box::new(Follow {
                events,
                end: host.clone(),
            }),
            TermOptions::default(),
            Arc::new(|_| {}),
        );
        let event = deck_tsp::wire::Event::Theme { dark: true };
        assert_eq!(t.send_tsp(&event), Err(crate::TspRefused::ReadOnly));
        output(
            &host,
            &encode_message(
                Verb::Open,
                &[],
                br#"{"id":"s1","mode":"inline","role":"omp.session"}"#,
            ),
        );
        until(&mut t, "the surface", |t| t.tsp().live());
        assert_eq!(t.send_tsp(&event), Err(crate::TspRefused::ReadOnly));
        drop(t);
        wait_detached(TIMEOUT);
    }

    /// What omp answers a width change with: wipe screen and scrollback, print everything again.
    /// Ends with a title change, which `until(.. title ..)` sees only after the lines are parsed.
    fn redraw(t: &mut Terminal, host: &Sender<RemoteEvent>, done: &str) {
        output(host, b"\x1b[2J\x1b[3J\x1b[H");
        for half in 0..2 {
            let mut lines = Vec::new();
            for i in 0..50 {
                lines.extend_from_slice(format!("line {}\r\n", half * 50 + i).as_bytes());
            }
            output(host, &lines);
        }
        output(host, format!("\x1b]0;{done}\x07").as_bytes());
        until(t, done, |t| t.title() == Some(done));
    }

    fn scroll_back(t: &Terminal, lines: i32) {
        t.term
            .lock()
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
    }

    fn display_offset(t: &Terminal) -> usize {
        t.term.lock().grid().display_offset()
    }

    #[test]
    fn a_scrolled_back_view_survives_the_redraw_a_resize_triggers() {
        let (mut t, host, _log) = connect();
        t.resize(40, 10, 8, 16);
        redraw(&mut t, &host, "first");
        scroll_back(&t, 7);
        assert_eq!(display_offset(&t), 7);

        // The redraw clears the scrollback (view to the bottom) and rebuilds it in chunks.
        t.resize(30, 10, 8, 16);
        redraw(&mut t, &host, "second");
        assert!(t.term.lock().grid().history_size() >= 90);
        assert_eq!(display_offset(&t), 7);
    }

    #[test]
    fn a_view_at_the_bottom_stays_there_through_a_resize_redraw() {
        let (mut t, host, _log) = connect();
        t.resize(40, 10, 8, 16);
        redraw(&mut t, &host, "first");
        t.resize(30, 10, 8, 16);
        redraw(&mut t, &host, "second");
        assert_eq!(display_offset(&t), 0);
    }

    #[test]
    fn scrolling_by_hand_ends_the_hold() {
        let (mut t, host, _log) = connect();
        t.resize(40, 10, 8, 16);
        redraw(&mut t, &host, "first");
        scroll_back(&t, 7);
        t.resize(30, 10, 8, 16);
        t.scroll_keep.cancel();
        redraw(&mut t, &host, "second");
        assert_eq!(display_offset(&t), 0);
    }

    #[test]
    fn run_state_follows_output_and_a_snapshot_replaces_it() {
        let (mut t, host, _log) = connect();
        assert_eq!(t.run_state(), None);
        // Split across reads, ST-terminated.
        output(&host, b"x\x1b]7501;state=work");
        output(&host, b"ing:app=omp\x1b\\");
        until(&mut t, "working", |t| {
            t.run_state() == Some(RunState::new(RunPhase::Working))
        });
        output(&host, b"\x1b]7501;state=error:app=omp:msg=Ym9vbQ==\x07");
        until(&mut t, "error", |t| {
            t.run_state()
                == Some(RunState {
                    state: RunPhase::Error,
                    msg: Some("boom".into()),
                })
        });

        // A host snapshot that carries a report replaces the state with it ...
        host.send(RemoteEvent::Snapshot(
            b"\x1bcsnap\x1b]7501;state=blocked:kind=question:app=omp\x1b\\".to_vec(),
        ))
        .expect("send");
        until(&mut t, "snapshot state", |t| {
            t.run_state()
                == Some(RunState::new(RunPhase::Blocked {
                    kind: Some(BlockedKind::Question),
                }))
        });
        // ... and one that carries none leaves none: the old session's state does not survive.
        host.send(RemoteEvent::Snapshot(b"\x1bcagain".to_vec()))
            .expect("send");
        until(&mut t, "snapshot without state", |t| {
            t.screen_text().lines[0] == "again" && t.run_state().is_none()
        });
    }

    /// Waits for the subscriber's snapshot, returning it and how long it took.
    fn snapshot_of(
        sub: &crate::OutputSubscription,
        limit: Duration,
    ) -> Option<(Arc<[u8]>, Duration)> {
        let start = Instant::now();
        while start.elapsed() < limit {
            if let Some(crate::OutputEvent::Snapshot(bytes)) =
                sub.recv_timeout(Duration::from_millis(20))
            {
                return Some((bytes, start.elapsed()));
            }
        }
        None
    }

    /// An abandoned chunk sequence ends a joiner's wait on its own: nothing more arrives from an
    /// idle program, and the splitter's expiry only runs when bytes do.
    #[test]
    fn a_join_behind_an_abandoned_chunk_sequence_completes_after_the_chunk_idle_time() {
        use crate::tsp::tests::{frame1, frame2_chunks, hello, open};
        let (mut t, host, _log) = connect();
        let mut head = hello();
        head.extend(open());
        head.extend(frame1());
        let chunks = frame2_chunks();
        output(&host, &head);
        output(&host, &chunks[0]);
        output(&host, &chunks[1]);
        settle(&mut t, &host, "started");

        let sub = t.subscribe_output(Arc::new(|| {}));
        assert!(
            snapshot_of(&sub, Duration::from_millis(300)).is_none(),
            "the sequence has not expired yet"
        );
        // No further output: the join ends when the sequence expires.
        let (snapshot, _) = snapshot_of(&sub, Duration::from_secs(5)).expect("the join completes");
        let mut mirror = deck_tsp::mirror::Mirror::new();
        mirror.feed(&snapshot, Instant::now());
        assert!(mirror.is_idle(), "the replay holds no half message");
        assert!(mirror.live());

        // Later chunks of the dropped sequence are skipped; the stream goes on after it.
        for chunk in &chunks[2..] {
            output(&host, chunk);
        }
        output(&host, b"\r\nafter");
        until(&mut t, "output after the dropped sequence", |t| {
            t.screen_text().lines.iter().any(|l| l.contains("after"))
        });
        assert_eq!(
            t.tsp().with(|d| d
                .surface("s1")
                .and_then(|s| s.node("t"))
                .and_then(|n| n.text().map(str::to_owned))),
            Some("hello".to_owned())
        );
    }

    #[test]
    fn a_join_after_the_chunk_idle_time_needs_no_wait() {
        use crate::tsp::tests::{frame1, frame2_chunks, hello, open};
        let (mut t, host, _log) = connect();
        let mut head = hello();
        head.extend(open());
        head.extend(frame1());
        let chunks = frame2_chunks();
        output(&host, &head);
        output(&host, &chunks[0]);
        settle(&mut t, &host, "started");
        std::thread::sleep(deck_tsp::stream::CHUNK_IDLE + Duration::from_millis(200));

        let sub = t.subscribe_output(Arc::new(|| {}));
        let (_, took) = snapshot_of(&sub, Duration::from_secs(5)).expect("joined at once");
        assert!(took < Duration::from_millis(500), "{took:?}");
    }
}
