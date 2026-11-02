//! Native frontend link: a renderer [`crate::Terminal`] whose session terminal is owned by
//! another process (the gateway), see [`crate::Terminal::frontend`]. Output arrives through the
//! same [`RemoteRx`] events as a host session; everything sent goes through [`FrontendTx`] with
//! its metadata, never as raw backend writes. Owner-side counterparts: [`crate::Terminal`]'s
//! `subscribe_output`, `record_input_meta`, `write_frontend_reply` and [`ClipboardPolicy`].

use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use alacritty_terminal::event::WindowSize;
use serde::{Deserialize, Serialize};

use crate::out::WriteProgress;
use crate::remote::RemoteRx;

/// What the terminal's owner knows about input that its encoded bytes do not carry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InputMeta {
    /// The input is a Ctrl+C key press, also when encoded without a 0x03 byte (kitty CSI-u,
    /// win32-input-mode records): the owner's double-Ctrl+C guard must see it.
    pub ctrl_c: bool,
    /// win32-input-mode: the modifier state the child has seen after this input
    /// ([`crate::input::Mods::bits`]); `None` outside win32-input-mode.
    pub win32_modifiers: Option<u32>,
}

/// Owner state a frontend renders and encodes with, delivered in order with the output
/// ([`crate::RemoteEvent::State`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrontendState {
    /// The child is a tmux client: tmux key rules apply.
    pub tmux_client: bool,
    /// The owner holds user input (an injected sequence owns it); typing is dropped.
    pub input_held: bool,
    /// win32-input-mode modifier state the child last saw ([`crate::input::Mods::bits`]).
    pub win32_modifiers: u32,
    /// This frontend serves the child's OSC 52 clipboard stores and reads.
    pub clipboard_owner: bool,
}

/// Who serves a terminal's OSC 52 clipboard requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClipboardPolicy {
    /// The terminal's own system clipboard.
    #[default]
    Local,
    /// The designated frontend ([`FrontendState::clipboard_owner`]): the owner never touches a
    /// clipboard; loads stay outstanding until [`crate::Terminal::write_frontend_reply`].
    Frontend,
}

/// Why [`crate::Terminal::write_frontend_reply`] refused bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReplyRejected {
    /// Not the exact reply to an outstanding clipboard read.
    #[error("no outstanding clipboard read matches this reply")]
    NotOutstanding,
    /// The owner no longer writes to the child.
    #[error("the terminal is closed")]
    Closed,
}

/// A frontend connection to an owned session terminal, split by [`crate::Terminal::frontend`].
pub trait Frontend: Send {
    fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn FrontendTx>);
}

/// Sending half (runs on its own thread; calls may block). An error refuses that one call
/// (logged); the link stays up until [`Self::detach`]. Dropping it detaches.
pub trait FrontendTx: Send + Sync {
    /// Encoded user input plus what the owner must know about it.
    fn input(&self, bytes: &[u8], meta: InputMeta) -> io::Result<()>;
    /// The reply to the child's outstanding OSC 52 read (designated clipboard frontend only).
    fn clipboard_reply(&self, bytes: &[u8]) -> io::Result<()>;
    /// This view gained or lost keyboard focus.
    fn focus(&self, focused: bool) -> io::Result<()>;
    /// Requested grid and cell pixel size; the authoritative one arrives as
    /// [`crate::RemoteEvent::Resize`].
    fn resize(&self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) -> io::Result<()>;
    /// Query colors: ANSI 0..=15, foreground, background, cursor.
    fn palette(&self, palette: [[u8; 3]; 19]) -> io::Result<()>;
    /// Ends the session's process.
    fn kill(&self) -> io::Result<()>;
    /// Stops following; the session keeps running.
    fn detach(&self) -> io::Result<()>;
}

/// Work for the frontend writer thread, in order.
pub(crate) enum FrontendCmd {
    Input(Vec<u8>, InputMeta),
    ClipboardReply(Vec<u8>),
    Focus(bool),
    Resize(WindowSize),
    Palette([[u8; 3]; 19]),
    Kill,
    Detach,
}

/// Renderer-side state of a frontend terminal.
#[derive(Default)]
pub(crate) struct FrontendView {
    pub(crate) state: FrontendState,
    /// A snapshot arrived and the link is up: input may be sent.
    pub(crate) live: bool,
    /// The link ended for good.
    pub(crate) closed: bool,
    /// Last requested geometry (cols, rows, cell width, cell height).
    pub(crate) requested: Option<(u16, u16, u16, u16)>,
    /// Focus last sent.
    pub(crate) focus_sent: Option<bool>,
    /// Reconnected: desired view state is sent again once the snapshot is in.
    pub(crate) resync: bool,
}

/// Writer thread: forwards commands until `Detach` (or every sender is gone, which detaches
/// too). Refused input still counts as handled for the bounded detach flush, never as
/// acknowledged, and does not end the link.
fn run_writer(tx: Box<dyn FrontendTx>, cmds: Receiver<FrontendCmd>, progress: Arc<WriteProgress>) {
    let report = |what: &str, result: io::Result<()>| {
        if let Err(e) = result {
            log::debug!("frontend {what} refused: {e}");
        }
    };
    while let Ok(cmd) = cmds.recv() {
        match cmd {
            FrontendCmd::Input(bytes, meta) => {
                let result = tx.input(&bytes, meta);
                if result.is_ok() {
                    progress.record(bytes.len(), true);
                } else {
                    progress.record_refused(bytes.len());
                }
                report("input", result);
            }
            FrontendCmd::ClipboardReply(bytes) => {
                report("clipboard reply", tx.clipboard_reply(&bytes))
            }
            FrontendCmd::Focus(focused) => report("focus", tx.focus(focused)),
            FrontendCmd::Resize(ws) => report(
                "resize",
                tx.resize(ws.num_cols, ws.num_lines, ws.cell_width, ws.cell_height),
            ),
            FrontendCmd::Palette(p) => report("palette", tx.palette(p)),
            FrontendCmd::Kill => report("kill", tx.kill()),
            FrontendCmd::Detach => return report("detach", tx.detach()),
        }
    }
    report("detach", tx.detach());
}

/// Starts the writer thread; returns its queue and handle.
pub(crate) fn spawn_writer(
    tx: Box<dyn FrontendTx>,
    progress: Arc<WriteProgress>,
) -> (Sender<FrontendCmd>, Option<std::thread::JoinHandle<()>>) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let writer = std::thread::Builder::new()
        .name("deck-term-frontend-tx".into())
        .spawn(move || run_writer(tx, cmd_rx, progress));
    match writer {
        Ok(handle) => (cmd_tx, Some(handle)),
        Err(e) => {
            log::warn!("frontend writer thread: {e}");
            (cmd_tx, None)
        }
    }
}

/// The text of an OSC 52 read reply (`ESC ] 52 ; <c> ; <base64> <ST|BEL>`), if well-formed.
pub(crate) fn osc52_reply_text(bytes: &[u8]) -> Option<String> {
    let rest = bytes.strip_prefix(b"\x1b]52;")?;
    let (_, rest) = rest.split_first()?;
    let rest = rest.strip_prefix(b";")?;
    let payload = rest
        .strip_suffix(b"\x07")
        .or_else(|| rest.strip_suffix(b"\x1b\\"))?;
    let data = crate::graphics::base64_decode(payload)?;
    String::from_utf8(data).ok()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    #[cfg(unix)]
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
    use egui::Color32;

    use super::*;
    use crate::engine::{OutputEvent, OutputSubscription};
    #[cfg(unix)]
    use crate::input::{Key, Mods};
    use crate::remote::RemoteEvent;
    use crate::remote::tests::{Log, Sent, connect};
    use crate::{BlockedKind, RunPhase, RunState, TermOptions, Terminal, wait_detached};

    const TIMEOUT: Duration = Duration::from_secs(10);

    /// What the frontend sent towards its owner.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Cmd {
        Input(Vec<u8>, InputMeta),
        ClipboardReply(Vec<u8>),
        Focus(bool),
        Resize(u16, u16, u16, u16),
        Palette([[u8; 3]; 19]),
        Kill,
        Detach,
    }

    /// Test control of the gateway side of the link.
    enum Ctl {
        Disconnect,
        Attach(OutputSubscription),
    }

    /// The gateway's job in miniature: an owner subscription mapped onto frontend events.
    struct Bridge {
        sub: Option<OutputSubscription>,
        ctl: Receiver<Ctl>,
    }

    impl RemoteRx for Bridge {
        fn recv(&mut self) -> io::Result<RemoteEvent> {
            loop {
                match self.ctl.try_recv() {
                    Ok(Ctl::Disconnect) => {
                        self.sub = None;
                        return Ok(RemoteEvent::Disconnected);
                    }
                    Ok(Ctl::Attach(sub)) => self.sub = Some(sub),
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(RemoteEvent::Closed),
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                match &self.sub {
                    Some(sub) => {
                        if let Some(event) = sub.recv_timeout(Duration::from_millis(20)) {
                            return Ok(remote_event(event));
                        }
                    }
                    None => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        }
    }

    fn remote_event(event: OutputEvent) -> RemoteEvent {
        match event {
            OutputEvent::Resize {
                cols,
                rows,
                cell_width,
                cell_height,
            } => RemoteEvent::Resize {
                cols,
                rows,
                cell_width,
                cell_height,
            },
            OutputEvent::Snapshot(bytes) => RemoteEvent::Snapshot(bytes.to_vec()),
            OutputEvent::Output(bytes) => RemoteEvent::Output(bytes.to_vec()),
            OutputEvent::State(state) => RemoteEvent::State(state),
            OutputEvent::Exited(code) => RemoteEvent::Exited(code),
            OutputEvent::Overflow => RemoteEvent::Disconnected,
            OutputEvent::Closed => RemoteEvent::Closed,
        }
    }

    struct Tx(Sender<Cmd>);

    impl Tx {
        fn send(&self, cmd: Cmd) -> io::Result<()> {
            self.0
                .send(cmd)
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    impl FrontendTx for Tx {
        fn input(&self, bytes: &[u8], meta: InputMeta) -> io::Result<()> {
            self.send(Cmd::Input(bytes.to_vec(), meta))
        }
        fn clipboard_reply(&self, bytes: &[u8]) -> io::Result<()> {
            self.send(Cmd::ClipboardReply(bytes.to_vec()))
        }
        fn focus(&self, focused: bool) -> io::Result<()> {
            self.send(Cmd::Focus(focused))
        }
        fn resize(
            &self,
            cols: u16,
            rows: u16,
            cell_width: u16,
            cell_height: u16,
        ) -> io::Result<()> {
            self.send(Cmd::Resize(cols, rows, cell_width, cell_height))
        }
        fn palette(&self, palette: [[u8; 3]; 19]) -> io::Result<()> {
            self.send(Cmd::Palette(palette))
        }
        fn kill(&self) -> io::Result<()> {
            self.send(Cmd::Kill)
        }
        fn detach(&self) -> io::Result<()> {
            self.send(Cmd::Detach)
        }
    }

    struct Link {
        rx: Bridge,
        tx: Tx,
    }

    impl Frontend for Link {
        fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn FrontendTx>) {
            (Box::new(self.rx), Box::new(self.tx))
        }
    }

    /// An owner and a frontend joined by a [`Bridge`]; [`Pair::pump`] plays the gateway.
    struct Pair {
        owner: Terminal,
        front: Terminal,
        cmds: Receiver<Cmd>,
        ctl: Sender<Ctl>,
        sent: Vec<Cmd>,
    }

    impl Pair {
        fn new(owner: Terminal) -> Pair {
            let (ctl, ctl_rx) = mpsc::channel();
            let (cmd_tx, cmds) = mpsc::channel();
            let link = Link {
                rx: Bridge {
                    sub: Some(owner.subscribe_output(Arc::new(|| {}))),
                    ctl: ctl_rx,
                },
                tx: Tx(cmd_tx),
            };
            let front = Terminal::frontend(
                Box::new(link),
                TermOptions::default(),
                egui::Context::default(),
            );
            Pair {
                owner,
                front,
                cmds,
                ctl,
                sent: Vec::new(),
            }
        }

        /// Polls both terminals and applies what the frontend sent to the owner.
        fn pump(&mut self) {
            self.owner.poll();
            self.front.poll();
            while let Ok(cmd) = self.cmds.try_recv() {
                match &cmd {
                    Cmd::Input(bytes, meta) => {
                        self.owner.record_input_meta(*meta);
                        self.owner.write(bytes);
                    }
                    Cmd::ClipboardReply(bytes) => {
                        let _ = self.owner.write_frontend_reply(bytes);
                    }
                    Cmd::Focus(focused) => self.owner.set_focused(*focused),
                    Cmd::Resize(cols, rows, w, h) => self.owner.resize(*cols, *rows, *w, *h),
                    Cmd::Palette(colors) => self.owner.set_palette(*colors),
                    Cmd::Kill => self.owner.kill(),
                    Cmd::Detach => {}
                }
                self.sent.push(cmd);
            }
        }

        fn until(&mut self, what: &str, mut done: impl FnMut(&Pair) -> bool) {
            let deadline = Instant::now() + TIMEOUT;
            loop {
                self.pump();
                if done(self) {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn inputs(&self) -> Vec<Vec<u8>> {
            self.sent
                .iter()
                .filter_map(|c| match c {
                    Cmd::Input(bytes, _) => Some(bytes.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    fn screen(t: &Terminal) -> String {
        t.screen_text().lines.join("\n")
    }

    fn until_owner(t: &mut Terminal, what: &str, mut done: impl FnMut(&Terminal) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done(t) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            t.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn drain(sub: &OutputSubscription) -> Vec<OutputEvent> {
        std::iter::from_fn(|| sub.try_recv()).collect()
    }

    fn output(host: &Sender<RemoteEvent>, bytes: &[u8]) {
        host.send(RemoteEvent::Output(bytes.to_vec()))
            .expect("feeder alive");
    }

    fn host_inputs(log: &Log) -> Vec<u8> {
        log.lock()
            .expect("log")
            .iter()
            .filter_map(|s| match s {
                Sent::Input(bytes) => Some(bytes.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[cfg(unix)]
    type CellView = (char, Color, Color, Flags, Vec<char>, Option<String>);

    #[cfg(unix)]
    /// Every cell of scrollback and screen, hyperlinks by target (ids are per terminal), and
    /// the cursor.
    fn cells(t: &Terminal) -> (Vec<CellView>, usize, (i32, usize)) {
        let term = t.term.lock();
        let grid = term.grid();
        let history = grid.history_size();
        let mut out = Vec::new();
        for line in -(history as i32)..term.screen_lines() as i32 {
            for cell in &grid[Line(line)][..] {
                out.push((
                    cell.c,
                    cell.fg,
                    cell.bg,
                    cell.flags,
                    cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
                    cell.hyperlink().map(|h| h.uri().to_owned()),
                ));
            }
        }
        let cursor = grid.cursor.point;
        (out, history, (cursor.line.0, cursor.column.0))
    }

    #[cfg(unix)]
    fn owner_sh(script: &str) -> Terminal {
        Terminal::spawn(
            crate::SpawnSpec {
                program: "sh".into(),
                args: vec!["-c".into(), script.into()],
                cwd: std::env::temp_dir(),
                env: vec![("TERM".into(), "xterm-256color".into())],
                tmux_client: false,
            },
            TermOptions::default(),
            egui::Context::default(),
        )
        .expect("spawn sh")
    }

    #[cfg(unix)]
    #[test]
    fn frontend_replays_the_owner_terminal_and_carries_semantic_ctrl_c() {
        // Kitty keyboard flags, SGR, wide and combining chars, an OSC 8 link, scrollback: all
        // produced before the frontend exists.
        let script = r"printf '\033[>1u\033[1;31mred\033[0m \346\274\242\345\255\227 e\314\201 \033]8;;https://x.io\033\\link\033]8;;\033\\\r\n'; i=0; while [ $i -lt 40 ]; do echo line$i; i=$((i+1)); done; printf ready; exec cat";
        let mut owner = owner_sh(script);
        until_owner(&mut owner, "owner output", |t| screen(t).contains("ready"));
        let mut pair = Pair::new(owner);
        pair.until("an identical replay", |p| {
            !p.front.is_stale() && cells(&p.front) == cells(&p.owner)
        });
        assert!(cells(&pair.front).1 > 0, "scrollback replayed");

        // Live output after the join stays identical.
        pair.front.write(b"live\r");
        pair.until("echoed input", |p| {
            screen(&p.front).matches("live").count() == 2 && cells(&p.front) == cells(&p.owner)
        });

        // Kitty Ctrl+C has no 0x03 byte; its metadata still moves the owner's guard.
        assert!(pair.owner.last_ctrl_c().is_none());
        pair.front.send_key(Key::Char('c'), Mods::CTRL);
        pair.until("Ctrl+C at the owner", |p| p.owner.last_ctrl_c().is_some());
        let ctrl_c = pair
            .sent
            .iter()
            .find_map(|c| match c {
                Cmd::Input(bytes, meta) if meta.ctrl_c => Some(bytes.clone()),
                _ => None,
            })
            .expect("Ctrl+C input");
        assert!(
            ctrl_c.starts_with(b"\x1b[") && !ctrl_c.contains(&0x03),
            "{ctrl_c:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_the_owner_answers_terminal_queries() {
        let script = r"stty raw -echo; dd bs=1 count=1 >/dev/null 2>&1; printf '\033[c\033[6n'; dd bs=1 count=5 >/dev/null 2>&1; printf done; exec sleep 30";
        let mut pair = Pair::new(owner_sh(script));
        pair.until("live", |p| !p.front.is_stale());
        pair.front.write(b"g");
        pair.until("the child read its replies", |p| {
            screen(&p.front).contains("done")
        });
        std::thread::sleep(Duration::from_millis(50));
        pair.pump();
        assert_eq!(pair.inputs(), vec![b"g".to_vec()]);
    }

    /// A fresh renderer fed `events` the way a frontend applies them.
    fn replay(events: &[OutputEvent]) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &TermSize::new(80, 24), VoidListener);
        let mut parser: Processor = Processor::new();
        for event in events {
            match event {
                OutputEvent::Resize { cols, rows, .. } => {
                    term.resize(TermSize::new(usize::from(*cols), usize::from(*rows)));
                }
                OutputEvent::Snapshot(bytes) => {
                    let size = TermSize::new(term.columns(), term.screen_lines());
                    term = Term::new(Config::default(), &size, VoidListener);
                    parser = Processor::new();
                    parser.advance(&mut term, bytes);
                }
                OutputEvent::Output(bytes) => parser.advance(&mut term, bytes),
                _ => {}
            }
        }
        term
    }

    fn first_row(term: &Term<VoidListener>, cols: usize) -> String {
        let row = &term.grid()[Line(0)];
        (0..cols).map(|c| row[Column(c)].c).collect()
    }

    #[test]
    fn a_join_inside_a_sequence_replays_its_unfinished_prefix() {
        let (mut owner, host, _log) = connect();
        output(&host, b"A\x1b[3");
        until_owner(&mut owner, "A", |t| screen(t).starts_with('A'));
        let csi = owner.subscribe_output(Arc::new(|| {}));
        output(&host, b"1mX\xe6\xbc");
        until_owner(&mut owner, "X", |t| screen(t).starts_with("AX"));
        let utf8 = owner.subscribe_output(Arc::new(|| {}));
        // Silence well past any join deadline while both subscribers keep reading.
        let deadline = Instant::now() + Duration::from_millis(250);
        let (mut csi_events, mut utf8_events) = (Vec::new(), Vec::new());
        while Instant::now() < deadline {
            csi_events.extend(drain(&csi));
            utf8_events.extend(drain(&utf8));
            std::thread::sleep(Duration::from_millis(10));
        }
        output(&host, b"\xa2Z");
        until_owner(&mut owner, "the completed sequences", |t| {
            screen(t).starts_with("AX\u{6f22}Z")
        });
        csi_events.extend(drain(&csi));
        utf8_events.extend(drain(&utf8));
        for events in [&csi_events, &utf8_events] {
            assert!(
                matches!(events[0], OutputEvent::Resize { .. }),
                "{events:?}"
            );
            assert!(matches!(events[1], OutputEvent::Snapshot(_)), "{events:?}");
            let term = replay(events);
            assert_eq!(first_row(&term, 6), "AX\u{6f22} Z ");
            let row = &term.grid()[Line(0)];
            assert_eq!(row[Column(1)].fg, Color::Named(NamedColor::Red));
            assert_eq!(row[Column(4)].fg, Color::Named(NamedColor::Red));
        }
    }

    #[test]
    fn a_join_never_ends_a_synchronized_update_early() {
        let (mut owner, host, _log) = connect();
        let sent = Instant::now();
        output(&host, b"M\x1b[?2026hS");
        until_owner(&mut owner, "M", |t| screen(t).starts_with('M'));
        let sub = owner.subscribe_output(Arc::new(|| {}));
        let shown = screen(&owner);
        let early = drain(&sub);
        // Before vte's own 150 ms timeout nothing may have applied the buffered `S`.
        if sent.elapsed() < Duration::from_millis(140) {
            assert!(shown.starts_with("M\n") || shown == "M", "{shown:?}");
            assert!(early.is_empty(), "joined inside the update: {early:?}");
        }
        output(&host, b"\x1b[?2026lT");
        until_owner(&mut owner, "the update", |t| screen(t).starts_with("MST"));
        let mut events = early;
        events.extend(drain(&sub));
        let OutputEvent::Snapshot(snapshot) = &events[1] else {
            panic!("{events:?}");
        };
        assert!(!snapshot.ends_with(b"\x1b[?2026h"));
        assert_eq!(first_row(&replay(&events), 3), "MST");
    }

    #[test]
    fn a_join_after_an_update_timed_out_is_not_held_in_one() {
        let (mut owner, host, _log) = connect();
        output(&host, b"\x1b[?2026hS");
        // Shown only once vte's timeout ended the update (no `?2026l` ever came).
        until_owner(&mut owner, "the timed-out update", |t| {
            screen(t).starts_with('S')
        });
        let sub = owner.subscribe_output(Arc::new(|| {}));
        output(&host, b"T");
        until_owner(&mut owner, "T", |t| screen(t).starts_with("ST"));
        let events = drain(&sub);
        let OutputEvent::Snapshot(snapshot) = &events[1] else {
            panic!("{events:?}");
        };
        assert!(!snapshot.ends_with(b"\x1b[?2026h"));
        // A replaying parser left in an update would still hold `T` back.
        assert_eq!(first_row(&replay(&events), 2), "ST");
    }

    #[test]
    fn state_and_snapshot_floods_count_against_the_budget() {
        let (mut owner, host, _log) = connect();
        let states = owner.subscribe_output(Arc::new(|| {}));
        let far = Instant::now() + Duration::from_secs(3600);
        let past = Instant::now();
        for _ in 0..40_000 {
            owner.hold_input_until(far);
            owner.hold_input_until(past);
        }
        let events = drain(&states);
        assert_eq!(events.last(), Some(&OutputEvent::Overflow));
        assert!(events.len() < 70_000, "{} events queued", events.len());

        let snapshots = owner.subscribe_output(Arc::new(|| {}));
        let page = vec![b'b'; 0x1_0000];
        for _ in 0..80 {
            host.send(RemoteEvent::Snapshot(page.clone()))
                .expect("feeder alive");
        }
        output(&host, b"\x1b[H\x1b[2JEND");
        until_owner(&mut owner, "END", |t| screen(t).starts_with("END"));
        assert_eq!(drain(&snapshots).last(), Some(&OutputEvent::Overflow));
    }

    #[test]
    fn kitty_images_uploaded_before_the_join_reach_the_frontend() {
        let (mut owner, host, _log) = connect();
        // Image 11 complete and placed; image 12 still mid-upload; then a marker.
        output(
            &host,
            b"\x1b_Ga=t,q=2,i=11,f=24,s=2,v=1;/wAAAP8A\x1b\\\
              \x1b_Ga=p,q=2,i=11,p=7,U=1,c=2,r=1\x1b\\\
              \x1b_Ga=t,q=2,i=12,f=24,s=2,v=1,m=1;/wAA\x1b\\M",
        );
        until_owner(&mut owner, "marker", |t| screen(t).starts_with('M'));
        let mut pair = Pair::new(owner);
        pair.until("image 11 decoded", |p| {
            p.front.graphics.ready_pixels(11).is_some()
        });
        assert_eq!(
            pair.front.graphics.ready_pixels(11),
            Some(vec![Color32::RED, Color32::GREEN])
        );
        assert!(pair.front.graphics.has_placement(11, 7));
        // The rest of the pending upload arrives live.
        output(&host, b"\x1b_Gm=0;AP8A\x1b\\");
        pair.until("image 12 decoded", |p| {
            p.front.graphics.ready_pixels(12).is_some()
        });
        assert_eq!(
            pair.front.graphics.ready_pixels(12),
            Some(vec![Color32::RED, Color32::GREEN])
        );
    }

    #[test]
    fn routed_clipboard_reads_accept_only_the_exact_outstanding_reply() {
        let (mut owner, host, log) = connect();
        owner.set_clipboard_policy(ClipboardPolicy::Frontend);
        let reply = b"\x1b]52;c;aGk=\x07";
        assert_eq!(
            owner.write_frontend_reply(reply),
            Err(ReplyRejected::NotOutstanding)
        );
        output(&host, b"\x1b]52;c;?\x07R");
        until_owner(&mut owner, "read parsed", |t| screen(t).starts_with('R'));
        // Any clipboard text is a valid answer; selector and terminator must match the read.
        for wrong in [
            &b"\x1b]52;p;aGk=\x07"[..],
            b"\x1b]52;c;aGk=\x1b\\",
            b"\x1b[?6c",
        ] {
            assert_eq!(
                owner.write_frontend_reply(wrong),
                Err(ReplyRejected::NotOutstanding)
            );
        }
        assert_eq!(owner.write_frontend_reply(reply), Ok(()));
        assert_eq!(
            owner.write_frontend_reply(reply),
            Err(ReplyRejected::NotOutstanding)
        );
        until_owner(&mut owner, "reply to the host", |_| {
            host_inputs(&log) == reply
        });
    }

    #[test]
    fn a_subscriber_beyond_its_budget_overflows_and_can_resubscribe() {
        let (mut owner, host, _log) = connect();
        let sub = owner.subscribe_output(Arc::new(|| {}));
        let chunk = vec![b'a'; 1 << 20];
        for _ in 0..5 {
            output(&host, &chunk);
        }
        output(&host, b"\x1b[2J\x1b[HEND");
        until_owner(&mut owner, "END", |t| screen(t).starts_with("END"));
        let events = drain(&sub);
        assert_eq!(events.last(), Some(&OutputEvent::Overflow));
        assert!(sub.try_recv().is_none());
        let again = owner.subscribe_output(Arc::new(|| {}));
        assert!(
            matches!(
                drain(&again).as_slice(),
                [
                    OutputEvent::Resize { .. },
                    OutputEvent::Snapshot(_),
                    OutputEvent::State(_)
                ]
            ),
            "a fresh snapshot after overflow"
        );
    }

    #[test]
    fn hold_and_resize_reach_subscribers_in_order() {
        let (mut owner, _host, _log) = connect();
        let sub = owner.subscribe_output(Arc::new(|| {}));
        assert!(matches!(
            drain(&sub).as_slice(),
            [
                OutputEvent::Resize { .. },
                OutputEvent::Snapshot(_),
                OutputEvent::State(s)
            ] if !s.input_held
        ));
        owner.hold_input_until(Instant::now() + Duration::from_millis(100));
        owner.resize(100, 30, 9, 18);
        assert_eq!(
            drain(&sub),
            vec![
                OutputEvent::State(FrontendState {
                    input_held: true,
                    ..FrontendState::default()
                }),
                OutputEvent::Resize {
                    cols: 100,
                    rows: 30,
                    cell_width: 9,
                    cell_height: 18
                },
            ]
        );
        std::thread::sleep(Duration::from_millis(150));
        owner.poll();
        assert_eq!(
            drain(&sub),
            vec![OutputEvent::State(FrontendState::default())]
        );
    }

    #[test]
    fn a_lost_link_leaves_a_stale_view_and_dropping_the_frontend_only_detaches() {
        let (owner, _host, log) = connect();
        let mut pair = Pair::new(owner);
        // Requested before the first snapshot: sent once the view is current.
        pair.front.resize(100, 30, 8, 16);
        pair.until("live", |p| !p.front.is_stale());
        pair.until("owner geometry", |p| {
            let term = p.front.term.lock();
            (term.columns(), term.screen_lines()) == (100, 30)
        });
        assert!(pair.sent.iter().any(|c| matches!(c, Cmd::Palette(_))));
        assert!(pair.sent.iter().any(|c| matches!(c, Cmd::Focus(_))));

        pair.ctl.send(Ctl::Disconnect).expect("bridge alive");
        pair.until("stale", |p| p.front.is_stale());
        assert!(!pair.front.is_exited());
        assert!(pair.front.input_held());
        assert!(!pair.front.accepts_input());
        pair.front.write(b"lost");
        pair.pump();
        assert!(pair.inputs().is_empty());

        pair.sent.clear();
        let sub = pair.owner.subscribe_output(Arc::new(|| {}));
        pair.ctl.send(Ctl::Attach(sub)).expect("bridge alive");
        pair.until("current again", |p| !p.front.is_stale());
        pair.until("view state sent again", |p| {
            p.sent.contains(&Cmd::Resize(100, 30, 8, 16))
                && p.sent.iter().any(|c| matches!(c, Cmd::Palette(_)))
                && p.sent.iter().any(|c| matches!(c, Cmd::Focus(_)))
        });
        pair.front.write(b"ok");
        pair.until("input delivered", |_| host_inputs(&log) == b"ok");

        let Pair {
            owner, front, cmds, ..
        } = pair;
        drop(front);
        wait_detached(TIMEOUT);
        let rest: Vec<Cmd> = cmds.try_iter().collect();
        assert!(rest.contains(&Cmd::Detach), "{rest:?}");
        assert!(!rest.contains(&Cmd::Kill), "{rest:?}");
        assert!(!owner.is_exited());
        assert!(!log.lock().expect("log").contains(&Sent::Kill));
    }

    #[test]
    fn a_frontend_joining_late_holds_the_owners_document_and_sends_events_through_it() {
        use crate::tsp::TspRefused;
        use crate::tsp::tests::{doc_json, event, frame1, frame2_chunks, hello, open, send, text};

        let (mut owner, host, log) = connect();
        let mut head = hello();
        head.extend(open());
        head.extend(frame1());
        let chunks = frame2_chunks();
        send(&host, &head);
        send(&host, &chunks[0]);
        until_owner(&mut owner, "the first frame", |t| {
            text(t).as_deref() == Some("hello")
        });

        // The frontend joins inside a chunked message: it stays stale until that ends.
        let mut pair = Pair::new(owner);
        std::thread::sleep(Duration::from_millis(200));
        pair.pump();
        assert!(
            pair.front.is_stale(),
            "no snapshot half way through a frame"
        );
        assert!(!pair.front.tsp().live());
        assert_eq!(pair.front.send_tsp(&event()), Err(TspRefused::Detached));
        for chunk in &chunks[1..] {
            send(&host, chunk);
        }
        pair.until("the replayed document", |p| {
            !p.front.is_stale() && text(&p.front).as_deref() == Some("hello and more of it")
        });
        assert!(pair.front.tsp().live());
        assert_eq!(doc_json(&pair.front), doc_json(&pair.owner));

        // Live output after the join keeps them equal, and moves the revision.
        let rev = pair.front.tsp().revision();
        send(
            &host,
            &deck_tsp::stream::encode_message(
                deck_tsp::stream::Verb::Frame,
                &[],
                br#"{"sf":"s1","s":3,"ops":[["text","t","append","!"]]}"#,
            ),
        );
        pair.until("the append", |p| {
            text(&p.front).as_deref() == Some("hello and more of it!")
        });
        assert!(pair.front.tsp().revision() > rev);
        assert_eq!(doc_json(&pair.front), doc_json(&pair.owner));

        // Neither side answered the program; the frontend's event reaches it once, exact.
        assert_eq!(host_inputs(&log), b"");
        pair.front.send_tsp(&event()).expect("accepted");
        let expected =
            deck_tsp::terminal::encode_event(&event(), deck_tsp::terminal::InputEncoding::native());
        pair.until("the event at the host", |_| host_inputs(&log) == expected);
        assert_eq!(pair.inputs(), vec![expected.clone()]);

        // The owner's hold reaches the frontend and refuses the event there.
        pair.owner
            .hold_input_until(Instant::now() + Duration::from_secs(3600));
        pair.until("the hold", |p| p.front.input_held());
        assert_eq!(pair.front.send_tsp(&event()), Err(TspRefused::Held));

        // A lost link: stale, detached.
        pair.ctl.send(Ctl::Disconnect).expect("bridge alive");
        pair.until("stale", |p| p.front.is_stale());
        assert_eq!(pair.front.send_tsp(&event()), Err(TspRefused::Detached));
        assert_eq!(host_inputs(&log), expected, "refusals wrote nothing");
    }

    #[test]
    fn a_frontend_joining_mid_turn_learns_omp_run_state_and_follows_it() {
        let (mut owner, host, _log) = connect();
        // The turn started before the frontend joined; a title omp drops under TSP is absent.
        output(&host, b"\x1b]7501;state=work");
        output(&host, b"ing:app=omp\x1b\\M");
        until_owner(&mut owner, "M", |t| screen(t).starts_with('M'));
        assert_eq!(owner.run_state(), Some(RunState::new(RunPhase::Working)));

        let mut pair = Pair::new(owner);
        pair.until("the replayed state", |p| {
            p.front.run_state() == Some(RunState::new(RunPhase::Working))
        });

        // Live transitions reach it through the output stream.
        output(
            &host,
            b"\x1b]7501;state=blocked:kind=permission:app=omp:msg=T2s=\x07",
        );
        let blocked = RunState {
            state: RunPhase::Blocked {
                kind: Some(BlockedKind::Permission),
            },
            msg: Some("Ok".into()),
        };
        pair.until("blocked", |p| p.front.run_state() == Some(blocked.clone()));
        output(
            &host,
            b"\x1b]7501;state=done:app=omp\x07\x1b]7501;state=idle:app=omp\x07",
        );
        pair.until("idle", |p| {
            p.front.run_state() == Some(RunState::new(RunPhase::Idle))
        });
        output(&host, b"\x1b]7501;state=clear\x1b\\");
        pair.until("cleared", |p| p.front.run_state().is_none());
        assert_eq!(pair.owner.run_state(), None);

        // A frontend joining after the clear has nothing to learn.
        let late = Pair::new(pair.owner);
        assert_eq!(late.front.run_state(), None);
    }
}
