//! Terminal state: alacritty `Term` + the event listener, fed either by a local PTY child
//! (Unix: alacritty tty + our PTY loop) or by a remote session host ([`crate::Remote`]) or, for
//! a gateway frontend, by the gateway ([`crate::Frontend`]). Every output path runs through the
//! terminal's [`Engine`]. No egui types except `egui::Context` (used only to wake the UI).

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{ClipboardType, Config, Osc52, Term, TermMode};

use deck_host::run_state::RunState;

use crate::clipboard::Clipboard;
use crate::engine::{Engine, OutputEvent, OutputNotify, OutputSubscription};
use crate::frontend::{
    self, ClipboardPolicy, Frontend, FrontendState, FrontendView, InputMeta, ReplyRejected,
};
use crate::graphics::{Graphics, ImageReplay};
use crate::input::{KeyRules, Mods};
use crate::out::{PtyOut, Sink, WriteProgress, register_detaching};
use crate::remote::{self, Feeder, Remote, ScrollKeep};
use crate::scan::{KeyModes, OutputScan, RunShared};
use crate::theme::{self, Palette, TermTheme};
use crate::tsp::{TspHandle, TspRefused, TspShared};
use crate::wake::{Wake, WakeCallback};
use crate::widget::ViewState;

/// Formats the reply to an OSC 52 read from the clipboard text.
type ClipboardFormat = Arc<dyn Fn(&str) -> String + Sync + Send + 'static>;

/// OSC 52 reads kept outstanding for the designated frontend (the oldest is dropped beyond).
const MAX_OUTSTANDING_LOADS: usize = 8;

/// What to run in a local PTY (Unix).
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: std::path::PathBuf,
    pub env: Vec<(String, String)>,
    /// true when `program` is a tmux client (omp-deck's normal mode). Selects key-encoding rules
    /// because tmux, not the inner app, is the outer terminal's peer.
    pub tmux_client: bool,
}

#[derive(Clone)]
pub struct TermOptions {
    /// Local scrollback lines (tmux and the session host keep their own).
    pub scrollback: usize,
    pub theme: TermTheme,
    /// Shared system clipboard (one per app).
    pub clipboard: Clipboard,
    /// Honor kitty keyboard protocol requests (`CSI > flags u`). A local terminal also answers
    /// `CSI ? u` with it; for a remote one the host answers and this only selects the encoder.
    pub kitty_keyboard: bool,
}

impl Default for TermOptions {
    fn default() -> Self {
        TermOptions {
            scrollback: 10_000,
            theme: TermTheme::Dark,
            clipboard: Clipboard::new(),
            kitty_keyboard: true,
        }
    }
}

/// Events drained by [`Terminal::poll`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalEvents {
    /// Some("") = title reset.
    pub title_changed: Option<String>,
    /// Reported exactly once.
    pub exited: Option<i32>,
    pub bell: bool,
}

/// Visible text of the active screen, see [`Terminal::screen_text`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScreenText {
    /// One entry per screen row, top to bottom.
    pub lines: Vec<String>,
    /// Cursor row, index into `lines`.
    pub cursor_row: usize,
    /// Cursor grid column (cells, 0-based). Equals the char index into `lines[cursor_row]`
    /// only while the row left of the cursor holds no wide chars or combining marks.
    pub cursor_col: usize,
}

/// Minimum spacing of output-driven repaints while the terminal is visible (one 60 Hz frame).
const VISIBLE_REPAINT: Duration = Duration::from_millis(16);
/// Spacing of output-driven repaints while hidden: enough for title/bell/exit via `poll`.
const HIDDEN_REPAINT: Duration = Duration::from_millis(250);

/// Repaint slots, µs since [`RepaintGate::epoch`].
#[derive(Default)]
struct Slots {
    /// Earliest time the next repaint may happen.
    next: u64,
    /// Time of the last scheduled (delayed) repaint; pending while in the future.
    scheduled: u64,
}

/// Coalesces repaint requests caused by PTY output (shared by the PTY thread's listener
/// and [`Terminal::set_visible`]): at most one repaint per interval. After a quiet interval
/// the repaint is immediate (visible) so typing echo is instant; output within an interval
/// schedules one delayed repaint at the interval's end, which occupies that slot.
pub(crate) struct RepaintGate {
    epoch: Instant,
    visible: AtomicBool,
    slots: Mutex<Slots>,
}

impl RepaintGate {
    fn new() -> Self {
        RepaintGate {
            epoch: Instant::now(),
            visible: AtomicBool::new(true),
            slots: Mutex::new(Slots::default()),
        }
    }

    fn slots(&self) -> MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn request(&self, wake: &Wake) {
        let now = u64::try_from(self.epoch.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.request_at(now, wake);
    }

    /// [`Self::request`] at `now` (µs since `epoch`).
    fn request_at(&self, now: u64, wake: &Wake) {
        let visible = self.visible.load(Ordering::Relaxed);
        let interval = if visible {
            VISIBLE_REPAINT
        } else {
            HIDDEN_REPAINT
        };
        let step = interval.as_micros() as u64;
        let mut s = self.slots();
        if s.scheduled > now {
            return;
        }
        if now >= s.next {
            s.next = now.saturating_add(step);
            if visible {
                drop(s);
                wake.now();
                return;
            }
            s.scheduled = s.next;
        } else {
            s.scheduled = s.next;
            s.next = s.next.saturating_add(step);
        }
        let delay = Duration::from_micros(s.scheduled - now);
        drop(s);
        wake.after(delay);
    }

    /// Shown again: a hidden-mode slot must not delay the next output repaint.
    fn reset(&self) {
        *self.slots() = Slots::default();
    }
}

/// What the output side tells [`Terminal::poll`].
pub(crate) enum Notice {
    /// An event from the `Term` (or, locally, the PTY event loop).
    Term(Event),
    /// Remote: the child exited.
    Exited(Option<i32>),
    /// Remote: another client attached; input has stopped.
    Displaced,
    /// Remote: the connection ended.
    Closed,
    /// Remote: a snapshot replaced the `Term` (the title starts over).
    Reset,
    /// Authoritative geometry received by an output observer.
    Resized(WindowSize),
    /// Frontend: new owner state.
    State(FrontendState),
    /// Frontend: the link to the owner is lost until the next snapshot.
    Disconnected,
}

/// OSC 52 traffic of an owner whose clipboard is a designated frontend's
/// ([`ClipboardPolicy::Frontend`]). Reads are recorded by the output thread while it parses,
/// before the output is published, so a frontend's reply can never outrun its record.
#[derive(Default)]
pub(crate) struct RoutedClipboard {
    routed: AtomicBool,
    loads: Mutex<VecDeque<ClipboardFormat>>,
}

impl RoutedClipboard {
    fn loads(&self) -> MutexGuard<'_, VecDeque<ClipboardFormat>> {
        self.loads.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Called from the output thread (and from `Term` methods on the UI thread while the Term is
/// locked), so it never locks the Term: PtyWrite goes to [`PtyOut`], everything else to the
/// channel + a coalesced repaint request (query events needing an answer from `poll` repaint
/// at once). Remote terminals never answer queries (the host does), except clipboard loads.
#[derive(Clone)]
pub(crate) struct Listener {
    tx: Sender<Notice>,
    out: Arc<PtyOut>,
    wake: Wake,
    gate: Arc<RepaintGate>,
    /// Local terminal: this side answers terminal queries.
    answers: bool,
    clipboard: Arc<RoutedClipboard>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(s) => {
                if self.answers {
                    self.out.reply(s.into_bytes());
                }
            }
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {
                self.gate.request(&self.wake);
            }
            Event::ColorRequest(..) | Event::TextAreaSizeRequest(_) if !self.answers => {}
            // The designated frontend's own Term stores; its reply is checked against this.
            Event::ClipboardStore(..) if self.clipboard.routed.load(Ordering::Acquire) => {}
            Event::ClipboardLoad(_, fmt) if self.clipboard.routed.load(Ordering::Acquire) => {
                let mut loads = self.clipboard.loads();
                if loads.len() == MAX_OUTSTANDING_LOADS {
                    loads.pop_front();
                }
                loads.push_back(fmt);
            }
            e @ (Event::ColorRequest(..)
            | Event::TextAreaSizeRequest(_)
            | Event::ClipboardLoad(..)) => {
                let _ = self.tx.send(Notice::Term(e));
                self.wake.now();
            }
            e => {
                let _ = self.tx.send(Notice::Term(e));
                self.gate.request(&self.wake);
            }
        }
    }
}

pub(crate) type SharedTerm = Arc<FairMutex<Term<Listener>>>;

/// Shared construction of every terminal kind.
struct Parts {
    term: SharedTerm,
    listener: Listener,
    out: Arc<PtyOut>,
    progress: Arc<WriteProgress>,
    tx: Sender<Notice>,
    rx: Receiver<Notice>,
    gate: Arc<RepaintGate>,
    graphics: Arc<Graphics>,
    key_modes: Arc<KeyModes>,
    run_state: Arc<RunShared>,
    engine: Arc<Engine>,
    tsp: Arc<TspShared>,
    window_size: WindowSize,
    wake: Wake,
    /// The owner of its output (publishes to subscribers).
    serve: bool,
    scroll_keep: Arc<ScrollKeep>,
}

impl Parts {
    /// Initial size 80x24 until the first `TerminalView::show`. `serve`: an owner (retains
    /// snapshot side state and images for output subscribers).
    fn new(
        opts: &TermOptions,
        ctx: &egui::Context,
        answers: bool,
        wake: Wake,
        serve: bool,
    ) -> Parts {
        let (cols, rows) = (80u16, 24u16);
        let window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 8,
            cell_height: 16,
        };
        let (tx, rx) = mpsc::channel();
        let progress = Arc::new(WriteProgress::default());
        let out = Arc::new(PtyOut::new(progress.clone(), wake.clone()));
        let gate = Arc::new(RepaintGate::new());
        let listener = Listener {
            tx: tx.clone(),
            out: out.clone(),
            wake: wake.clone(),
            gate: gate.clone(),
            answers,
            clipboard: Arc::new(RoutedClipboard::default()),
        };
        let config = Config {
            kitty_keyboard: opts.kitty_keyboard,
            osc52: Osc52::CopyPaste,
            scrolling_history: opts.scrollback,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(
            config.clone(),
            &TermSize::new(usize::from(cols), usize::from(rows)),
            listener.clone(),
        )));
        // A headless owner never paints: its renderers decode the replayed uploads.
        let decode = !(serve && matches!(wake, Wake::Headless(_)));
        let graphics = Arc::new(Graphics::new(ctx.clone(), wake.clone(), decode));
        let key_modes = Arc::new(KeyModes::default());
        let run_state = Arc::new(RunShared::default());
        let tsp = Arc::new(TspShared::new());
        let scan = OutputScan::new(
            key_modes.clone(),
            run_state.clone(),
            graphics.clone(),
            serve.then(ImageReplay::default),
            tsp.clone(),
        );
        let scroll_keep = Arc::<ScrollKeep>::default();
        let engine = Engine::new(
            term.clone(),
            config,
            listener.clone(),
            scan,
            serve,
            window_size,
            scroll_keep.clone(),
        );
        Parts {
            term,
            listener,
            out,
            progress,
            tx,
            rx,
            gate,
            graphics,
            key_modes,
            run_state,
            engine,
            tsp,
            window_size,
            wake,
            serve,
            scroll_keep,
        }
    }

    fn feeder(&self, frontend: bool) -> Feeder {
        Feeder {
            engine: self.engine.clone(),
            out: self.out.clone(),
            notices: self.tx.clone(),
            wake: self.wake.clone(),
            frontend,
        }
    }

    fn into_terminal(
        self,
        opts: TermOptions,
        tmux_client: bool,
        remote: bool,
        writer: Option<JoinHandle<()>>,
    ) -> Terminal {
        Terminal {
            term: self.term,
            out: self.out,
            rx: self.rx,
            window_size: self.window_size,
            size_sent: !remote,
            clipboard: opts.clipboard,
            routed_clipboard: self.listener.clipboard.clone(),
            palette: Palette::for_theme(opts.theme),
            tmux_client,
            key_modes: self.key_modes,
            run_state: self.run_state,
            scroll_keep: self.scroll_keep,
            title: None,
            exit_code: None,
            exit_reported: false,
            banner: None,
            displaced: false,
            shut_down: false,
            read_only: false,
            frontend: None,
            writer,
            last_ctrl_c: Cell::new(None),
            input_hold: Cell::new(None),
            gate: self.gate,
            graphics: self.graphics,
            engine: self.engine,
            tsp: TspHandle(self.tsp),
            serving: self.serve,
            wake: self.wake,
            view: ViewState::default(),
        }
    }
}

/// One child with its terminal state: a local PTY child ([`Terminal::spawn`], Unix), a
/// session on a remote host ([`Terminal::connect`]) or a gateway-owned session
/// ([`Terminal::frontend`]). All behave alike towards the widget and the app.
pub struct Terminal {
    pub(crate) term: SharedTerm,
    /// Every byte (and resize) to the child goes through here.
    out: Arc<PtyOut>,
    rx: Receiver<Notice>,
    window_size: WindowSize,
    /// The child knows `window_size` (false for a remote terminal until the first resize: the
    /// host's size is unknown here).
    size_sent: bool,
    clipboard: Clipboard,
    /// OSC 52 routing to a designated frontend (see [`Terminal::set_clipboard_policy`]).
    routed_clipboard: Arc<RoutedClipboard>,
    palette: Palette,
    tmux_client: bool,
    /// Keyboard modes requested by the child that `TermMode` lacks (set by the output thread).
    key_modes: Arc<KeyModes>,
    /// omp's run status from its OSC 7501 reports (set by the output thread).
    run_state: Arc<RunShared>,
    /// Holds a scrolled-back view across the redraw an app answers a resize with.
    pub(crate) scroll_keep: Arc<ScrollKeep>,
    title: Option<String>,
    exit_code: Option<i32>,
    exit_reported: bool,
    /// Text drawn over the bottom row once the session ended here (exit, displaced, lost).
    pub(crate) banner: Option<String>,
    displaced: bool,
    shut_down: bool,
    read_only: bool,
    /// Gateway frontend state (`None` for owners and observers).
    frontend: Option<FrontendView>,
    /// Remote or frontend writer thread (joined via [`crate::wait_detached`] after drop).
    writer: Option<JoinHandle<()>>,
    /// When the last byte 0x03 was sent to the PTY (see [`Terminal::last_ctrl_c`]).
    last_ctrl_c: Cell<Option<Instant>>,
    /// User keyboard/paste/IME input is dropped before this instant.
    input_hold: Cell<Option<Instant>>,
    gate: Arc<RepaintGate>,
    /// Kitty graphics images (fed by the output thread, painted by the widget).
    pub(crate) graphics: Arc<Graphics>,
    /// Output parsing and publication.
    engine: Arc<Engine>,
    /// The program's TSP surfaces (passive mirror).
    tsp: TspHandle,
    /// This terminal owns its output (output subscribers, state publication).
    serving: bool,
    wake: Wake,
    /// Per-terminal widget state (selection drag, blink timer, render buffers).
    pub(crate) view: ViewState,
}

impl Terminal {
    /// Spawns the PTY child (alacritty tty + our PTY loop thread). Initial size 80x24 until
    /// the first `TerminalView::show`.
    #[cfg(unix)]
    pub fn spawn(
        spec: SpawnSpec,
        opts: TermOptions,
        ctx: egui::Context,
    ) -> std::io::Result<Terminal> {
        Self::spawn_with_wake(spec, opts, ctx.clone(), Wake::Gui(ctx))
    }

    /// Spawns without a GUI event loop. The driver polls on output wakeups and the deadlines
    /// returned by its command controller.
    #[cfg(unix)]
    pub fn spawn_headless(
        spec: SpawnSpec,
        opts: TermOptions,
        wake: WakeCallback,
    ) -> std::io::Result<Terminal> {
        Self::spawn_with_wake(spec, opts, egui::Context::default(), Wake::Headless(wake))
    }

    #[cfg(unix)]
    fn spawn_with_wake(
        spec: SpawnSpec,
        opts: TermOptions,
        ctx: egui::Context,
        wake: Wake,
    ) -> std::io::Result<Terminal> {
        use alacritty_terminal::tty::{self, Options, Shell};

        let parts = Parts::new(&opts, &ctx, true, wake, true);
        let tty_opts = Options {
            shell: Some(Shell::new(spec.program, spec.args)),
            working_directory: Some(spec.cwd),
            drain_on_exit: true,
            env: spec.env.into_iter().collect(),
        };
        let pty = tty::new(&tty_opts, parts.window_size, 0)?;
        let (pty_loop, sender) = crate::pty::PtyLoop::new(
            pty,
            parts.engine.clone(),
            parts.listener.clone(),
            parts.progress.clone(),
        )?;
        // Set before the loop starts parsing, so no reply can be dropped.
        parts.out.set_sink(Sink::Local(sender));
        // Detached: the thread owns the Pty and drops it (SIGHUP + wait) after Msg::Shutdown.
        pty_loop.spawn()?;
        Ok(parts.into_terminal(opts, spec.tmux_client, false, None))
    }

    /// Attaches to a session on a remote host. The host's `Term` answers every terminal query;
    /// this side only renders, except that OSC 52 clipboard reads are answered here (the
    /// clipboard lives here, see [`Self::set_clipboard_policy`]). A
    /// [`crate::RemoteEvent::Snapshot`] resets the local `Term` and replays; the palette of
    /// `opts.theme` is sent at once (and again on [`Self::set_theme`]), the grid size on the
    /// first `TerminalView::show`. `Displaced` shows `[attached elsewhere]` and stops input.
    /// Dropping the terminal detaches (queued input is sent first), [`Self::kill`] kills the
    /// session.
    pub fn connect(remote: Box<dyn Remote>, opts: TermOptions, ctx: egui::Context) -> Terminal {
        Self::connect_with_wake(remote, opts, ctx.clone(), Wake::Gui(ctx), false)
    }

    /// Connects a controller without exposing an egui context to the headless caller.
    pub fn connect_headless(
        remote: Box<dyn Remote>,
        opts: TermOptions,
        wake: WakeCallback,
    ) -> Terminal {
        Self::connect_with_wake(
            remote,
            opts,
            egui::Context::default(),
            Wake::Headless(wake),
            false,
        )
    }

    /// Follows output without acquiring input, focus, palette, resize or process-kill rights.
    /// Host geometry arrives through `RemoteEvent::Resize`; widget layout cannot change it.
    pub fn observe(
        observer: Box<dyn remote::Observer>,
        opts: TermOptions,
        ctx: egui::Context,
    ) -> Terminal {
        Self::connect_with_wake(
            Box::new(remote::ObserverRemote(observer)),
            opts,
            ctx.clone(),
            Wake::Gui(ctx),
            true,
        )
    }

    /// Read-only output subscription for a headless caller.
    pub fn observe_headless(
        observer: Box<dyn remote::Observer>,
        opts: TermOptions,
        wake: WakeCallback,
    ) -> Terminal {
        Self::connect_with_wake(
            Box::new(remote::ObserverRemote(observer)),
            opts,
            egui::Context::default(),
            Wake::Headless(wake),
            true,
        )
    }

    fn connect_with_wake(
        remote: Box<dyn Remote>,
        opts: TermOptions,
        ctx: egui::Context,
        wake: Wake,
        read_only: bool,
    ) -> Terminal {
        let parts = Parts::new(&opts, &ctx, false, wake, !read_only);
        let (rx, tx) = remote.split();
        let (cmds, writer) = remote::spawn_writer(tx, parts.progress.clone());
        // Before the feeder runs, so no clipboard reply can be dropped.
        parts.out.set_sink(Sink::Remote(cmds));
        remote::start(rx, parts.feeder(false));
        if !read_only {
            parts
                .out
                .palette(Palette::for_theme(opts.theme).host_colors());
        }
        let mut terminal = parts.into_terminal(opts, false, true, writer);
        terminal.read_only = read_only;
        terminal
    }

    /// A native renderer of a session terminal owned elsewhere (the gateway's
    /// [`Self::subscribe_output`] on the other end). Its own `Term` parses the owner's
    /// snapshot and output for display, local selection/scrollback and the key, mouse, paste
    /// encoders; it never answers terminal queries (the owner does). Input goes out with its
    /// [`InputMeta`] and is dropped (never queued) until the first snapshot, while the owner
    /// holds input ([`FrontendState::input_held`]) and while the link is
    /// [`crate::RemoteEvent::Disconnected`]. Geometry follows the owner's
    /// [`crate::RemoteEvent::Resize`]; widget layout only requests a size. Focus and palette
    /// are sent explicitly. OSC 52 stores/reads are served only while this frontend is the
    /// designated clipboard owner. A lost link leaves a stale view, never an exit; dropping
    /// the terminal only detaches this view.
    pub fn frontend(
        frontend: Box<dyn Frontend>,
        opts: TermOptions,
        ctx: egui::Context,
    ) -> Terminal {
        let parts = Parts::new(&opts, &ctx, false, Wake::Gui(ctx.clone()), false);
        let (rx, tx) = frontend.split();
        let (cmds, writer) = frontend::spawn_writer(tx, parts.progress.clone());
        parts.out.set_sink(Sink::Frontend(cmds));
        remote::start(rx, parts.feeder(true));
        let mut terminal = parts.into_terminal(opts, false, true, writer);
        // Focus, palette and requested size go out with the first snapshot.
        terminal.frontend = Some(FrontendView {
            resync: true,
            ..FrontendView::default()
        });
        terminal
    }

    /// Drains pending terminal events: answers ColorRequest / TextAreaSizeRequest /
    /// ClipboardLoad, stores ClipboardStore, records title / bell / exit / session state.
    pub fn poll(&mut self) -> TerminalEvents {
        let mut out = TerminalEvents::default();
        while let Ok(notice) = self.rx.try_recv() {
            match notice {
                Notice::Term(event) => self.on_event(event, &mut out),
                Notice::Exited(code) => self.set_exit(code.unwrap_or(-1)),
                Notice::Displaced => {
                    if self.exit_code.is_none() && !self.displaced {
                        self.displaced = true;
                        self.shut_down = true;
                        self.banner = Some("[attached elsewhere]".to_owned());
                    }
                }
                Notice::Closed => self.on_closed(),
                Notice::Disconnected => match &mut self.frontend {
                    Some(view) => {
                        view.live = false;
                        view.resync = true;
                        if self.exit_code.is_none() && !self.displaced && !view.closed {
                            self.banner = Some("[gateway disconnected]".to_owned());
                        }
                    }
                    None => self.on_closed(),
                },
                Notice::Reset => self.on_frontend_snapshot(),
                Notice::Resized(size) => {
                    self.window_size = size;
                    self.size_sent = true;
                }
                Notice::State(state) => {
                    if let Some(view) = &mut self.frontend {
                        view.state = state;
                        self.tmux_client = state.tmux_client;
                        self.view.win32_mods = Mods::from_bits(state.win32_modifiers);
                    }
                }
            }
        }
        self.out.flush_due();
        self.publish_state();
        if !self.exit_reported && self.exit_code.is_some() {
            self.exit_reported = true;
            out.exited = self.exit_code;
        }
        out
    }

    /// The connection ended. A frontend keeps its stale view and never reports an exit (the
    /// session may live on in the gateway); the other kinds end here.
    fn on_closed(&mut self) {
        if let Some(view) = &mut self.frontend {
            view.live = false;
            view.closed = true;
            if self.exit_code.is_none() && !self.displaced {
                self.banner = Some("[gateway connection closed]".to_owned());
            }
            return;
        }
        if self.exit_code.is_none() && !self.displaced && !(self.read_only && self.shut_down) {
            self.exit_code = Some(-1);
            self.banner = Some("[connection to the session host lost]".to_owned());
        }
    }

    /// A frontend's view is current again: input passes, and after a reconnect the view's
    /// focus, palette and requested size reach the owner again.
    fn on_frontend_snapshot(&mut self) {
        let Some(view) = &mut self.frontend else {
            return;
        };
        if view.closed {
            return;
        }
        view.live = true;
        if self.exit_code.is_none() && !self.displaced {
            self.banner = None;
        }
        if std::mem::take(&mut view.resync) {
            view.focus_sent = Some(self.view.focused);
            self.out.frontend_focus(self.view.focused);
            self.out.palette(self.palette.host_colors());
            if let Some((cols, rows, cell_width, cell_height)) = view.requested {
                self.out.resize(WindowSize {
                    num_lines: rows,
                    num_cols: cols,
                    cell_width,
                    cell_height,
                });
            }
        }
    }

    /// This terminal acts on OSC 52: a designated, live frontend; a terminal that keeps its
    /// clipboard local. Never an observer, nor an owner that routes to a frontend (its
    /// listener records those reads).
    fn serves_clipboard(&self) -> bool {
        if self.read_only || self.shut_down {
            return false;
        }
        match &self.frontend {
            Some(view) => view.live && view.state.clipboard_owner,
            None => !self.routed_clipboard.routed.load(Ordering::Acquire),
        }
    }

    fn on_event(&mut self, event: Event, out: &mut TerminalEvents) {
        match event {
            Event::Title(t) => {
                out.title_changed = Some(t.clone());
                self.title = Some(t);
            }
            Event::ResetTitle => {
                self.title = None;
                out.title_changed = Some(String::new());
            }
            Event::Bell => out.bell = true,
            Event::ClipboardStore(..) | Event::ClipboardLoad(..) if !self.serves_clipboard() => {}
            Event::ClipboardStore(ty, text) => match ty {
                ClipboardType::Clipboard => self.clipboard.set_text(&text),
                ClipboardType::Selection => self.clipboard.set_primary(&text),
            },
            Event::ClipboardLoad(ty, fmt) => {
                // Answered from the clipboard worker once the read completes; a frontend's
                // answer goes to its owner, which checks it against the read it recorded.
                let out = self.out.clone();
                let frontend = self.frontend.is_some();
                self.clipboard
                    .load(ty == ClipboardType::Selection, move |text| {
                        let reply = fmt(text.as_deref().unwrap_or("")).into_bytes();
                        if frontend {
                            out.clipboard_reply(reply);
                        } else {
                            out.reply(reply);
                        }
                    });
            }
            Event::ColorRequest(index, fmt) => {
                let rgb = {
                    let term = self.term.lock();
                    theme::indexed(term.colors(), &self.palette, index)
                };
                self.out.reply(fmt(rgb).into_bytes());
            }
            Event::TextAreaSizeRequest(fmt) => {
                self.out.reply(fmt(self.window_size).into_bytes());
            }
            Event::PtyWrite(s) => self.out.reply(s.into_bytes()),
            Event::ChildExit(status) => self.set_exit(status.code().unwrap_or(-1)),
            Event::Exit => self.set_exit(-1),
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {}
        }
    }

    /// Records the exit code (the first one wins).
    fn set_exit(&mut self, code: i32) {
        if self.exit_code.is_none() && !self.displaced {
            self.run_state.clear();
            self.exit_code = Some(code);
            self.banner = Some(format!("[process exited: {code}]"));
        }
    }

    /// Last OSC 0/2 title (None after reset).
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Some once the child exited (signal → -1; a lost remote connection → -1).
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    pub fn is_exited(&self) -> bool {
        self.exit_code.is_some()
    }

    /// Remote: another client attached to the session (input has stopped; the session lives
    /// on, so this is not an exit).
    pub fn is_displaced(&self) -> bool {
        self.displaced
    }

    /// Whether the terminal is on screen (default true). While visible, output-driven
    /// repaints are coalesced to one per 16 ms (immediate after a quiet slot, else one delayed
    /// repaint per slot); while hidden, to one delayed repaint per 250 ms (enough for
    /// `poll` to surface title/bell/exit). Query events always repaint at once.
    pub fn set_visible(&self, visible: bool) {
        if self.gate.visible.swap(visible, Ordering::Relaxed) != visible && visible {
            self.gate.reset();
        }
    }

    /// Switches the palette; a remote host (or a frontend's owner) gets the new query colors.
    pub fn set_theme(&mut self, theme: TermTheme) {
        let palette = Palette::for_theme(theme);
        if palette != self.palette {
            let link_up = self.frontend.as_ref().is_none_or(|v| v.live);
            if !self.shut_down && !self.read_only && link_up {
                self.out.palette(palette.host_colors());
            }
            self.palette = palette;
        }
    }

    /// Owner: the query colors (OSC 4/10/11/12 answers) of the controlling frontend, in the
    /// layout of [`Palette::host_colors`]. A remote owner passes them on to its host.
    pub fn set_palette(&mut self, colors: [[u8; 3]; 19]) {
        if self.frontend.is_some() || self.read_only {
            return;
        }
        let palette = self.palette.with_host_colors(colors);
        if palette != self.palette {
            if !self.shut_down {
                self.out.palette(colors);
            }
            self.palette = palette;
        }
    }

    /// Owner: where OSC 52 clipboard stores and reads go. [`ClipboardPolicy::Frontend`] (set
    /// before the first [`Self::poll`]) leaves both to the subscriber marked
    /// [`OutputSubscription::set_clipboard_owner`]; reads wait for its
    /// [`Self::write_frontend_reply`]. Switching back to `Local` forgets outstanding reads.
    pub fn set_clipboard_policy(&mut self, policy: ClipboardPolicy) {
        let routed = matches!(policy, ClipboardPolicy::Frontend);
        self.routed_clipboard
            .routed
            .store(routed, Ordering::Release);
        if !routed {
            self.routed_clipboard.loads().clear();
        }
    }

    /// Owner: subscribes to this terminal's output. The subscription starts with the current
    /// size, a snapshot (grids, scrollback, modes, hyperlinks, retained kitty images) and the
    /// owner state, then every later output byte exactly once, in order (see
    /// [`OutputEvent`]). The initial events are queued at once, or — while a synchronized
    /// update is buffered, or inside an unfinished sequence over 4 MiB — when it ends. A
    /// subscriber more than 4 MiB of queued events behind gets [`OutputEvent::Overflow`].
    /// `notify` runs whenever an event was queued. Observers and frontends
    /// own no output: their subscription ends at once with [`OutputEvent::Closed`].
    pub fn subscribe_output(&self, notify: OutputNotify) -> OutputSubscription {
        self.publish_state();
        self.engine.subscribe(notify)
    }

    /// Owner: the metadata of a frontend's input, applied right before its bytes are written
    /// with [`Self::write`]: a semantic Ctrl+C (kitty/win32 encodings carry no 0x03) moves the
    /// [`Self::last_ctrl_c`] guard; win32 modifier state is what the child now believes held.
    pub fn record_input_meta(&mut self, meta: InputMeta) {
        if meta.ctrl_c {
            self.note_ctrl_c();
        }
        if let Some(bits) = meta.win32_modifiers {
            self.view.win32_mods = Mods::from_bits(bits);
        }
        self.publish_state();
    }

    /// Owner: a designated frontend's reply to an OSC 52 read. Accepted only if it is exactly
    /// the reply to an outstanding read (which it consumes); it then goes to the child like any
    /// query reply (lone-ESC quiet time applies, the input hold does not).
    pub fn write_frontend_reply(&self, bytes: &[u8]) -> Result<(), ReplyRejected> {
        if self.shut_down || self.read_only || self.out.is_closed() {
            return Err(ReplyRejected::Closed);
        }
        let text = frontend::osc52_reply_text(bytes).ok_or(ReplyRejected::NotOutstanding)?;
        let mut loads = self.routed_clipboard.loads();
        let pos = loads
            .iter()
            .position(|fmt| fmt(&text).as_bytes() == bytes)
            .ok_or(ReplyRejected::NotOutstanding)?;
        loads.remove(pos);
        drop(loads);
        self.out.reply(bytes.to_vec());
        Ok(())
    }

    /// Owner: publishes the state frontends follow (deduplicated by the engine).
    fn publish_state(&self) {
        if !self.serving {
            return;
        }
        self.engine.set_state(FrontendState {
            tmux_client: self.tmux_client,
            input_held: self.input_hold.get().is_some_and(|u| Instant::now() < u),
            win32_modifiers: self.view.win32_mods.bits(),
            clipboard_owner: false,
        });
    }

    /// Records keyboard focus. On a change it updates the Term and, when the child enabled
    /// focus reporting (DECSET 1004), writes `ESC[I` / `ESC[O`. [`crate::TerminalView`] uses
    /// the same state, so calling `set_focused(false)` on a tab being hidden and showing it
    /// again later yields exactly one out/in pair. A frontend sends its focus to the owner
    /// instead (once live; again after a reconnect), which reports it in the child's modes.
    ///
    /// Like every terminal-generated byte (query replies too), a report due less than 100 ms
    /// after a [`Self::write`] that ended with ESC is queued until that quiet time passed (sent
    /// by a later [`Self::poll`], which the widget runs every frame; a repaint is requested)
    /// or until the next write that does not end with ESC, right behind its bytes. An out/in
    /// pair falling into the same quiet time cancels out.
    pub fn set_focused(&mut self, focused: bool) {
        if focused == self.view.focused {
            return;
        }
        self.view.focused = focused;
        let mode = {
            let mut term = self.term.lock();
            term.is_focused = focused;
            *term.mode()
        };
        if let Some(view) = &mut self.frontend {
            if view.live && !self.shut_down && view.focus_sent != Some(focused) {
                view.focus_sent = Some(focused);
                self.out.frontend_focus(focused);
            }
        } else if !self.read_only
            && let Some(bytes) = crate::input::encode_focus(focused, mode)
        {
            self.out.focus_report(bytes.to_vec());
        }
        self.view.on_focus_change(focused);
    }

    /// Ends the child at once. Local: Msg::Shutdown → the PTY thread drops the Pty, which
    /// SIGHUPs the child. Remote: the host kills the session. Input not yet written is
    /// discarded (unlike drop, see [`Terminal`]'s `Drop`). A frontend asks its owner to kill
    /// the session; the exit arrives from the owner like any other.
    pub fn kill(&mut self) {
        if self.read_only {
            self.shut_down = true;
            self.out.displaced();
            self.banner = Some("[observer detached]".to_owned());
            return;
        }
        if self.frontend.is_some() {
            if !self.shut_down {
                self.out.kill();
            }
            return;
        }
        if !self.shut_down {
            self.shut_down = true;
            self.engine.finish(OutputEvent::Exited(None));
            self.out.kill();
            if let Some(writer) = self.writer.take() {
                register_detaching(writer);
            }
        }
        self.set_exit(-1);
    }

    /// Raw input to the child (keys, paste, injected text): sent at once, in order, never held
    /// by [`Self::hold_input_until`] or by the lone-ESC quiet time (the caller's
    /// responsibility). A frontend drops it while its link is not live.
    pub fn write(&self, bytes: &[u8]) {
        self.write_input(bytes, false);
    }

    /// [`Self::write`] of input that may encode a Ctrl+C without a 0x03 byte (kitty CSI-u,
    /// win32 records): `ctrl_c` marks it, so the quiet-time guard (here, or at a frontend's
    /// owner) still moves. A frontend's input carries the win32 modifier state it leaves.
    pub(crate) fn write_input(&self, bytes: &[u8], ctrl_c: bool) {
        if bytes.is_empty() || self.shut_down || self.read_only {
            return;
        }
        if self.frontend.as_ref().is_some_and(|v| !v.live) {
            return;
        }
        let ctrl_c = ctrl_c || bytes.contains(&0x03);
        if ctrl_c {
            self.note_ctrl_c();
        }
        let meta = InputMeta {
            ctrl_c,
            win32_modifiers: (self.key_rules() == KeyRules::Win32)
                .then(|| self.view.win32_mods.bits()),
        };
        self.out.write(bytes, meta);
    }

    /// omp's run status as its OSC 7501 reports say (`None` before the first report, after
    /// `state=clear`, RIS, or child exit). Replayed with the rest of the side state, so a terminal
    /// attaching mid-turn knows it. omp sends it in classic and in TSP mode; under TSP it is
    /// the only state omp publishes besides the document, the `π` title being dropped.
    pub fn run_state(&self) -> Option<RunState> {
        if self.is_exited() {
            None
        } else {
            self.run_state.get()
        }
    }

    /// The program's TSP surfaces, mirrored from its output (never answered here: the
    /// session host is the only TSP terminal that replies).
    pub fn tsp(&self) -> TspHandle {
        self.tsp.clone()
    }

    /// Sends a terminal → program TSP event to the child's input, through the same checks as
    /// user input (an observer, a stale or displaced connection and an input hold refuse it).
    /// Refused instead of silently dropped, so the UI can say so. A frontend can only check
    /// its own state: its owner may still refuse the bytes later.
    pub fn send_tsp(&self, event: &deck_tsp::wire::Event) -> Result<(), TspRefused> {
        self.tsp_gate()?;
        if self.input_held() {
            return Err(TspRefused::Held);
        }
        self.write_tsp(event);
        Ok(())
    }

    /// [`Self::send_tsp`] for an injected command: the injector holds the user's input while its
    /// job runs, and that hold must not refuse the job's own event (raw [`Self::write`] and
    /// [`Self::send_key`] bypass it likewise). Every other check applies.
    pub fn inject_tsp(&self, event: &deck_tsp::wire::Event) -> Result<(), TspRefused> {
        self.tsp_gate()?;
        self.write_tsp(event);
        Ok(())
    }

    fn tsp_gate(&self) -> Result<(), TspRefused> {
        if self.read_only {
            return Err(TspRefused::ReadOnly);
        }
        if self.is_exited() {
            return Err(TspRefused::Exited);
        }
        if self.displaced || !self.accepts_input() {
            return Err(TspRefused::Detached);
        }
        if !self.tsp.live() {
            return Err(TspRefused::NotLive);
        }
        Ok(())
    }

    fn write_tsp(&self, event: &deck_tsp::wire::Event) {
        let bytes =
            deck_tsp::terminal::encode_event(event, deck_tsp::terminal::InputEncoding::native());
        self.write_input(&bytes, false);
    }

    /// Whether this connection still owns its write path. Observer and displaced/lost
    /// connections must be refused by command controllers, never reported as delivered; a
    /// frontend accepts input only while its view is current.
    pub fn accepts_input(&self) -> bool {
        !self.read_only
            && !self.shut_down
            && !self.out.is_closed()
            && self.frontend.as_ref().is_none_or(|v| v.live && !v.closed)
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Frontend whose link is down (before the first snapshot, after
    /// [`crate::RemoteEvent::Disconnected`] or `Closed`): the view is stale and takes no input.
    pub fn is_stale(&self) -> bool {
        self.frontend.as_ref().is_some_and(|v| !v.live)
    }

    /// Size and focus follow another party's authority (observer, frontend): the widget only
    /// requests them.
    pub(crate) fn follows_remote_geometry(&self) -> bool {
        self.read_only || self.frontend.is_some()
    }

    /// A checkpoint for asynchronous command delivery, without waiting on the I/O thread.
    pub fn input_progress(&self) -> crate::InputProgress {
        self.out.input_progress()
    }

    /// While `Instant::now() < until`, all user keyboard/paste/IME input to this terminal is
    /// dropped (mouse reports, wheel, selection and focus reports are unaffected; [`Self::write`]
    /// is never held). Each call replaces the previous deadline. An owner publishes the hold
    /// to its frontends (and its end, through a wake-up at the deadline).
    pub fn hold_input_until(&self, until: Instant) {
        self.input_hold.set(Some(until));
        self.publish_state();
        if self.serving {
            self.wake
                .after(until.saturating_duration_since(Instant::now()));
        }
    }

    /// User keystrokes/pastes must not pass while an injected sequence owns input (here or,
    /// for a frontend, at its owner) or while a frontend's view is stale.
    /// Raw [`Self::write`] and [`Self::send_key`] deliberately bypass this gate for the injector.
    pub fn input_held(&self) -> bool {
        self.read_only
            || self.input_hold.get().is_some_and(|u| Instant::now() < u)
            || self
                .frontend
                .as_ref()
                .is_some_and(|v| !v.live || v.state.input_held)
    }

    /// Instant the last Ctrl+C was written to the child: any 0x03 byte (key input or
    /// [`Self::write`]) or a Ctrl+C key press encoded without one (kitty CSI-u,
    /// win32-input-mode). omp exits on two Ctrl+C within 500 ms; injectors must keep ≥ 650 ms
    /// distance.
    pub fn last_ctrl_c(&self) -> Option<Instant> {
        self.last_ctrl_c.get()
    }

    pub(crate) fn note_ctrl_c(&self) {
        self.last_ctrl_c.set(Some(Instant::now()));
    }

    /// Visible rows of the active screen (viewport at the bottom, i.e. ignoring the local
    /// scrollback position): wide-char spacers skipped, combining marks kept, trailing blanks
    /// trimmed.
    pub fn screen_text(&self) -> ScreenText {
        let term = self.term.lock();
        let grid = term.grid();
        let rows = term.screen_lines();
        let mut lines = Vec::with_capacity(rows);
        for line in 0..rows {
            let mut out = String::with_capacity(term.columns());
            for cell in &grid[Line(line as i32)][..] {
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                out.push(cell.c);
                if let Some(marks) = cell.zerowidth() {
                    out.extend(marks.iter());
                }
            }
            out.truncate(out.trim_end().len());
            lines.push(out);
        }
        let cursor = grid.cursor.point;
        ScreenText {
            lines,
            cursor_row: usize::try_from(cursor.line.0).unwrap_or(0),
            cursor_col: cursor.column.0,
        }
    }

    pub(crate) fn palette(&self) -> &Palette {
        &self.palette
    }

    pub(crate) fn clipboard(&self) -> &Clipboard {
        &self.clipboard
    }

    /// Non-kitty key-encoding rules: win32-input-mode while the child requested it (ConPTY);
    /// else tmux always speaks modifyOtherKeys 2 to us; a direct child gets rule 2 while it
    /// has requested level ≥ 2.
    pub(crate) fn key_rules(&self) -> KeyRules {
        if self.key_modes.win32_input.load(Ordering::Relaxed) {
            KeyRules::Win32
        } else if self.tmux_client {
            KeyRules::Tmux
        } else if self.key_modes.modify_other_keys.load(Ordering::Relaxed) >= 2 {
            KeyRules::ModifyOtherKeys
        } else {
            KeyRules::Legacy
        }
    }

    pub(crate) fn mode(&self) -> TermMode {
        let mode = *self.term.lock().mode();
        if self.read_only || self.is_stale() {
            mode.difference(TermMode::MOUSE_MODE | TermMode::ALTERNATE_SCROLL)
        } else {
            mode
        }
    }

    /// Resizes the Term and tells the child (local SIGWINCH, remote host) when the grid or the
    /// cell pixel size changed; an owner's subscribers get the new size in output order. Grids
    /// are clamped to at least 2×1 cells. Read-only observers ignore this call and follow
    /// their controller's geometry; a frontend only requests the size from its owner and
    /// follows the geometry the owner announces.
    pub fn resize(&mut self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        if self.read_only {
            return;
        }
        let cols = cols.max(2);
        let rows = rows.max(1);
        let ws = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width,
            cell_height,
        };
        if let Some(view) = &mut self.frontend {
            let requested = Some((cols, rows, cell_width, cell_height));
            if view.requested != requested {
                view.requested = requested;
                if view.live && !self.shut_down {
                    self.out.resize(ws);
                }
            }
            return;
        }
        let old = self.window_size;
        let same = (old.num_lines, old.num_cols, old.cell_width, old.cell_height)
            == (ws.num_lines, ws.num_cols, ws.cell_width, ws.cell_height);
        if same && self.size_sent {
            return;
        }
        self.window_size = ws;
        self.size_sent = true;
        self.engine.resize(ws);
        if !self.shut_down {
            self.out.resize(ws);
        }
    }
}

impl Drop for Terminal {
    /// Detaches without blocking: every byte already passed to [`Terminal::write`] (and
    /// every queued reply, after the lone-ESC quiet time) still reaches the child before the
    /// connection ends (local: the PTY thread hangs up, which SIGHUPs only the tmux client;
    /// remote: the host is told to detach). When bytes are in flight a short-lived thread
    /// waits for them to be written (bounded by 2 s) plus 50 ms after the last write for the
    /// child to read them, then ends the connection. That thread and a remote writer are
    /// registered for [`crate::wait_detached`]. Output subscribers get
    /// [`OutputEvent::Closed`]; a frontend only detaches its view.
    fn drop(&mut self) {
        self.engine.finish(OutputEvent::Closed);
        if let Some(writer) = self.writer.take() {
            register_detaching(writer);
        }
        if self.shut_down {
            return;
        }
        self.shut_down = true;
        if !self.out.close() {
            self.out.shutdown();
            return;
        }
        let out = self.out.clone();
        let spawned = std::thread::Builder::new()
            .name("deck-term-detach".into())
            .spawn(move || out.finish_detach());
        match spawned {
            Ok(handle) => register_detaching(handle),
            Err(_) => self.out.shutdown(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every repaint request the gate makes, as its delay.
    fn observed(ctx: &egui::Context) -> Arc<std::sync::Mutex<Vec<Duration>>> {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        ctx.set_repaint_observer(move |info| sink.lock().expect("lock").push(info.delay));
        seen
    }

    #[test]
    fn repaint_gate_coalesces_output_wakeups() {
        let ctx = egui::Context::default();
        let seen = observed(&ctx);
        let wake = Wake::Gui(ctx);
        let gate = RepaintGate::new();

        // Simulated clock (µs since the gate's epoch), so slot boundaries are exact.
        let v = VISIBLE_REPAINT.as_micros() as u64;
        let h = HIDDEN_REPAINT.as_micros() as u64;
        let take = || std::mem::take(&mut *seen.lock().expect("lock"));
        let burst = |from: u64| {
            for k in 0..50 {
                gate.request_at(from + k * 100, &wake);
            }
        };

        // Burst inside one slot: one immediate repaint, then one at the slot end.
        burst(1_000);
        assert_eq!(take(), [Duration::ZERO, Duration::from_micros(v - 100)]);
        // Sustained output in the following slot: delayed again (≤ 1 repaint per slot).
        burst(1_000 + v + 100);
        assert_eq!(take(), [Duration::from_micros(v - 100)]);
        // After a quiet slot: immediate again (typing echo).
        gate.request_at(1_000 + 4 * v, &wake);
        assert_eq!(take(), [Duration::ZERO]);

        // Hidden: never immediate; one repaint per 250 ms.
        gate.visible.store(false, Ordering::Relaxed);
        let t0 = 1_000 + 6 * v;
        burst(t0);
        assert_eq!(take(), [HIDDEN_REPAINT]);
        gate.request_at(t0 + h - 1, &wake);
        assert!(take().is_empty());
        gate.request_at(t0 + h + 10, &wake);
        assert_eq!(take(), [HIDDEN_REPAINT]);
    }

    /// Local PTY children (`sh`, `cat`, `sleep`).
    #[cfg(unix)]
    mod local {
        use super::*;
        use alacritty_terminal::index::{Column, Point};

        fn spawn(script: &str) -> Terminal {
            let spec = SpawnSpec {
                program: "sh".into(),
                args: vec!["-c".into(), script.into()],
                cwd: std::env::temp_dir(),
                env: vec![("TERM".into(), "xterm-256color".into())],
                tmux_client: false,
            };
            Terminal::spawn(spec, TermOptions::default(), egui::Context::default()).expect("spawn")
        }

        fn row_text(t: &Terminal, line: i32) -> String {
            let term = t.term.lock();
            let cols = term.columns();
            (0..cols)
                .map(|c| term.grid()[Point::new(Line(line), Column(c))].c)
                .collect()
        }

        fn wait_exit(t: &mut Terminal) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !t.is_exited() {
                assert!(Instant::now() < deadline, "child did not exit");
                t.poll();
                std::thread::sleep(Duration::from_millis(10));
            }
            // ChildExit can precede the drained output; wait for Exit (grid final).
            std::thread::sleep(Duration::from_millis(100));
            t.poll();
        }

        #[test]
        fn grid_contents_colors_and_wide_chars() {
            let mut t = spawn("printf 'hello \\033[31mred\\033[0m 漢x'; sleep 0.2");
            wait_exit(&mut t);
            let row = row_text(&t, 0);
            assert!(row.starts_with("hello red 漢"), "row: {row:?}");
            let term = t.term.lock();
            let grid = term.grid();
            let red = &grid[Point::new(Line(0), Column(6))];
            assert_eq!(red.c, 'r');
            assert_eq!(
                red.fg,
                alacritty_terminal::vte::ansi::Color::Named(
                    alacritty_terminal::vte::ansi::NamedColor::Red
                )
            );
            let wide = &grid[Point::new(Line(0), Column(10))];
            assert!(
                wide.flags
                    .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR)
            );
            assert!(
                grid[Point::new(Line(0), Column(11))]
                    .flags
                    .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER)
            );
            assert_eq!(grid[Point::new(Line(0), Column(12))].c, 'x');
        }

        #[test]
        fn combining_mark_kept_as_zerowidth() {
            let mut t = spawn(r"printf 'e\314\201x'; sleep 0.2");
            wait_exit(&mut t);
            let term = t.term.lock();
            let grid = term.grid();
            let e = &grid[Point::new(Line(0), Column(0))];
            assert_eq!(e.c, 'e');
            assert_eq!(e.zerowidth(), Some(&['\u{301}'][..]));
            assert_eq!(grid[Point::new(Line(0), Column(1))].c, 'x');
        }

        #[test]
        fn resize_updates_term_grid() {
            let mut t = spawn("sleep 2");
            t.resize(100, 30, 9, 18);
            {
                let term = t.term.lock();
                assert_eq!((term.columns(), term.screen_lines()), (100, 30));
            }
            t.kill();
            assert_eq!(t.poll().exited, Some(-1));
            assert_eq!(t.poll().exited, None);
        }

        #[test]
        fn screen_text_wide_chars_cursor_and_scrollback() {
            let t = spawn(
                "i=1; while [ $i -le 30 ]; do printf 'l%d\\r\\n' $i; i=$((i+1)); done; \
                 printf '> a漢be\\314\\201  \\033[2D'; sleep 3",
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut text = t.screen_text();
            while text.lines.get(23).map(String::as_str) != Some("> a漢be\u{301}") {
                assert!(Instant::now() < deadline, "screen: {text:?}");
                std::thread::sleep(Duration::from_millis(10));
                text = t.screen_text();
            }
            assert_eq!(text.lines.len(), 24);
            assert_eq!(text.lines[0], "l8");
            assert_eq!(text.lines[22], "l30");
            // Two cells for 漢, none for the combining mark; then back two columns.
            assert_eq!((text.cursor_row, text.cursor_col), (23, 7));
            // A local scrollback position does not change what is reported.
            t.term
                .lock()
                .scroll_display(alacritty_terminal::grid::Scroll::Delta(5));
            assert_eq!(t.screen_text(), text);
        }

        #[test]
        fn screen_text_empty_screen() {
            let t = spawn("sleep 2");
            let text = t.screen_text();
            assert_eq!(text.lines, vec![String::new(); 24]);
            assert_eq!((text.cursor_row, text.cursor_col), (0, 0));
        }

        #[test]
        fn set_visible_resets_hidden_window() {
            let ctx = egui::Context::default();
            let seen = observed(&ctx);
            let t = Terminal::spawn(
                SpawnSpec {
                    program: "sleep".into(),
                    args: vec!["2".into()],
                    cwd: std::env::temp_dir(),
                    env: Vec::new(),
                    tmux_client: false,
                },
                TermOptions::default(),
                ctx.clone(),
            )
            .expect("spawn");
            t.set_visible(false);
            t.gate.request(&Wake::Gui(ctx.clone()));
            t.set_visible(true);
            seen.lock().expect("lock").clear();
            t.gate.request(&Wake::Gui(ctx));
            assert_eq!(*seen.lock().expect("lock"), [Duration::ZERO]);
        }

        #[test]
        fn last_ctrl_c_tracks_etx_bytes() {
            let t = spawn("cat -v");
            assert_eq!(t.last_ctrl_c(), None);
            t.write(b"abc\x1b[A");
            assert_eq!(t.last_ctrl_c(), None);
            let before = Instant::now();
            t.write(b"x\x03");
            let first = t.last_ctrl_c().expect("0x03 recorded");
            assert!(first >= before && first <= Instant::now());
            t.write(b"plain");
            assert_eq!(t.last_ctrl_c(), Some(first));
            std::thread::sleep(Duration::from_millis(5));
            t.write(&[0x03]);
            assert!(t.last_ctrl_c().expect("second 0x03") > first);
        }
    }
}
