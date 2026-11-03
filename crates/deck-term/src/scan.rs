//! Byte-level scanners for child output that alacritty_terminal parses but ignores: xterm
//! modifyOtherKeys requests (`CSI > 4 ; n m`), win32-input-mode (`CSI ? 9001 h/l`) and kitty
//! graphics commands (APC strings). Every output path (the local PTY loop, the remote and
//! frontend feeders) runs each chunk through one [`OutputScan`] inside the terminal's engine,
//! right after the `Term` saw it; the bytes themselves pass unchanged.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use deck_host::run_state::{Report, RunState, RunStateScanner};
use deck_tsp::stream::CHUNK_IDLE;

use crate::graphics::{ApcScanner, Command, Graphics, ImageReplay};
use crate::tsp::{TspShared, may_touch_mirror};

const ESC: u8 = 0x1b;

/// Keyboard modes requested by the child that `TermMode` does not carry, published by the
/// output thread and read by the UI thread's key encoder.
#[derive(Debug, Default)]
pub(crate) struct KeyModes {
    /// modifyOtherKeys level (0 = off).
    pub(crate) modify_other_keys: AtomicU8,
    /// win32-input-mode (`?9001`).
    pub(crate) win32_input: AtomicBool,
}

/// omp's run status (OSC 7501) as the output thread last saw it, read by the UI thread.
#[derive(Debug, Default)]
pub(crate) struct RunShared(Mutex<Option<RunState>>);

impl RunShared {
    pub(crate) fn get(&self) -> Option<RunState> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn apply(&self, report: Report) {
        report.apply(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner));
    }

    pub(crate) fn clear(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// The scanners of one terminal's output plus where their results go.
pub(crate) struct OutputScan {
    mok: MokScanner,
    win32: Win32ModeScanner,
    apc: ApcScanner,
    modes: Arc<KeyModes>,
    run: RunStateScanner,
    run_shared: Arc<RunShared>,
    /// A snapshot replaces the terminal and no report of it arrived yet: the held state is
    /// the old session's until one does (or [`Self::end_reset`] finds it carried none).
    reset_pending: bool,
    graphics: Arc<Graphics>,
    /// An owner's retained images, replayed to renderers attaching later.
    replay: Option<ImageReplay>,
    /// The program's TSP surfaces (passive; shared with the UI through `TspHandle`).
    tsp: Arc<TspShared>,
    /// The mirror is between TSP messages (the last chunk left it so).
    tsp_idle: bool,
    /// The last chunk fed to the mirror ended in ESC (the next may continue a sequence).
    tsp_esc_tail: bool,
    /// The last time a chunk was fed to the mirror that left it not idle.
    tsp_busy_at: Instant,
}

impl OutputScan {
    pub(crate) fn new(
        modes: Arc<KeyModes>,
        run_shared: Arc<RunShared>,
        graphics: Arc<Graphics>,
        replay: Option<ImageReplay>,
        tsp: Arc<TspShared>,
    ) -> OutputScan {
        OutputScan {
            mok: MokScanner::default(),
            win32: Win32ModeScanner::default(),
            apc: ApcScanner::default(),
            modes,
            run: RunStateScanner::default(),
            run_shared,
            reset_pending: false,
            graphics,
            replay,
            tsp,
            tsp_idle: true,
            tsp_esc_tail: false,
            tsp_busy_at: Instant::now(),
        }
    }

    /// Scans one chunk of child output (sequences may span chunks).
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        if let Some(level) = self.mok.feed(bytes) {
            self.modes.modify_other_keys.store(level, Ordering::Relaxed);
        }
        if let Some(on) = self.win32.feed(bytes) {
            self.modes.win32_input.store(on, Ordering::Relaxed);
        }
        if let Some(report) = self.run.feed(bytes) {
            self.reset_pending = false;
            self.run_shared.apply(report);
        }
        let graphics = &self.graphics;
        let replay = &mut self.replay;
        self.apc.feed(bytes, |cmd| match cmd {
            Command::Transmit(up) => {
                let up = Arc::new(up);
                if let Some(replay) = replay.as_mut() {
                    replay.transmit(up.clone());
                }
                graphics.transmit(up);
            }
            cmd => {
                if let Some(replay) = replay.as_mut() {
                    replay.apply(&cmd);
                }
                graphics.apply(cmd);
            }
        });
        // Plain output (no ESC, no TSP string half read) never touches the mirror or its lock.
        if may_touch_mirror(self.tsp_idle && !self.tsp_esc_tail, bytes) {
            let now = Instant::now();
            self.tsp_idle = self.tsp.feed(bytes, now);
            self.tsp_esc_tail = bytes.last() == Some(&ESC);
            if !self.tsp_idle {
                self.tsp_busy_at = now;
            }
        }
    }

    /// The mirror is between TSP messages: no APC string is half read and no chunked message
    /// waits for its next chunk. A subscriber joining now misses nothing.
    pub(crate) fn tsp_idle(&self) -> bool {
        self.tsp_idle
    }

    /// When a mirror that is not idle can no longer be waiting for a chunk (`None`: idle). It
    /// is the time of the last chunk fed while not idle plus [`CHUNK_IDLE`]; the mirror's own
    /// sequence is never younger than that, so it has expired by then.
    pub(crate) fn tsp_deadline(&self) -> Option<Instant> {
        (!self.tsp_idle).then(|| self.tsp_busy_at + CHUNK_IDLE)
    }

    /// Lets an abandoned chunk sequence of the mirror expire at `now` (the splitter has no
    /// clock; expiry otherwise only happens when more output arrives). Returns whether the
    /// mirror is idle afterwards.
    pub(crate) fn expire_tsp(&mut self, now: Instant) -> bool {
        if !self.tsp_idle {
            self.tsp_idle = self.tsp.feed(&[], now);
        }
        self.tsp_idle
    }

    /// Appends the TSP replay of the program's surfaces (empty when it has none).
    pub(crate) fn tsp_replay(&self, now: Instant, out: &mut Vec<u8>) {
        self.tsp.replay(now, out);
    }

    /// Stores a blob the session host supplied for the mirror, already hashed to `id`. Not part
    /// of the byte stream: see `Engine::insert_blob`.
    pub(crate) fn tsp_put_blob(
        &self,
        id: &str,
        mime: Option<String>,
        bytes: std::sync::Arc<[u8]>,
    ) -> bool {
        self.tsp.put_verified_blob(id, mime, bytes)
    }

    /// The host holds no blob `id`: the mirror stops wanting it.
    pub(crate) fn tsp_forget_blob(&self, id: &str) {
        self.tsp.forget_blob(id);
    }

    /// Appends the APC replay of the retained images (nothing unless this is an owner).
    pub(crate) fn image_replay(&self, out: &mut Vec<u8>) {
        if let Some(replay) = &self.replay {
            replay.encode(self.apc.pending(), out);
        }
    }

    /// Forgets partial sequences and every mode (a remote snapshot replays them). Stored images
    /// stay: the replayed grid may still reference them through placeholder cells.
    pub(crate) fn reset(&mut self) {
        self.mok = MokScanner::default();
        self.win32 = Win32ModeScanner::default();
        self.apc = ApcScanner::default();
        self.run.reset();
        self.reset_pending = true;
        self.modes.modify_other_keys.store(0, Ordering::Relaxed);
        self.modes.win32_input.store(false, Ordering::Relaxed);
        self.tsp.reset();
        self.tsp_idle = true;
        self.tsp_esc_tail = false;
    }

    /// The snapshot after [`Self::reset`] was fed completely: a run state it did not carry is
    /// gone (the replay re-sends the held report, so a missing one means none is held).
    pub(crate) fn end_reset(&mut self) {
        if std::mem::take(&mut self.reset_pending) {
            self.run_shared.clear();
        }
    }

    /// Output ended: no program remains to own the held run status.
    pub(crate) fn finish(&mut self) {
        self.run.reset();
        self.reset_pending = false;
        self.run_shared.clear();
    }
}

/// Whether a byte stream is between sequences, as vte's parser sees it: not inside an escape,
/// control or string sequence and not inside a UTF-8 character. A renderer joining the stream
/// at such a boundary parses the following bytes exactly like the owner did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum GroundTracker {
    #[default]
    Ground,
    /// Continuation bytes still expected.
    Utf8(u8),
    Esc,
    /// `ESC` + intermediates.
    EscIntermediate,
    Csi,
    /// OSC: ends with BEL or ST.
    Osc,
    /// DCS, SOS, PM, APC: end with ST.
    String,
}

impl GroundTracker {
    pub(crate) fn is_ground(self) -> bool {
        self == GroundTracker::Ground
    }

    /// Advances over `bytes`; returns the offset after the last byte that left the stream
    /// between sequences, if any did.
    pub(crate) fn feed_last_ground(&mut self, bytes: &[u8]) -> Option<usize> {
        let mut last = None;
        for (i, &b) in bytes.iter().enumerate() {
            self.step(b);
            if self.is_ground() {
                last = Some(i + 1);
            }
        }
        last
    }

    /// Advances up to the first ground boundary in `bytes`; returns its offset (bytes before
    /// it), or `None` with all of `bytes` consumed.
    pub(crate) fn feed_to_ground(&mut self, bytes: &[u8]) -> Option<usize> {
        for (i, &b) in bytes.iter().enumerate() {
            self.step(b);
            if self.is_ground() {
                return Some(i + 1);
            }
        }
        None
    }

    fn step(&mut self, b: u8) {
        use GroundTracker::*;
        // Transitions from any state (C0 CAN/SUB abort, ESC starts over).
        match b {
            0x18 | 0x1a => {
                *self = Ground;
                return;
            }
            ESC => {
                *self = Esc;
                return;
            }
            _ => {}
        }
        *self = match *self {
            Ground => match b {
                0xc2..=0xdf => Utf8(1),
                0xe0..=0xef => Utf8(2),
                0xf0..=0xf4 => Utf8(3),
                _ => Ground,
            },
            Utf8(n) => match b {
                0x80..=0xbf if n > 1 => Utf8(n - 1),
                0x80..=0xbf => Ground,
                // An invalid sequence ends; the byte starts afresh.
                _ => {
                    *self = Ground;
                    return self.step(b);
                }
            },
            Esc => match b {
                0x20..=0x2f => EscIntermediate,
                b'[' => Csi,
                b']' => Osc,
                b'P' | b'X' | b'^' | b'_' => String,
                0x30..=0x7e => Ground,
                _ => Esc,
            },
            EscIntermediate => match b {
                0x30..=0x7e => Ground,
                _ => EscIntermediate,
            },
            Csi => match b {
                0x40..=0x7e => Ground,
                _ => Csi,
            },
            Osc => match b {
                0x07 => Ground,
                _ => Osc,
            },
            String => String,
        };
    }
}

/// Recogniser for `CSI > Pp ; Pv m` and RIS (`ESC c`). Keeps its state between reads, so
/// sequences split across reads are recognised.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MokScanner {
    state: MokState,
    /// Parameters of the current `CSI >` sequence (only the first two matter).
    params: [u16; 2],
    /// Index of the parameter being parsed.
    index: u8,
    /// Any parameter byte seen (distinguishes `CSI > m` from `CSI > 0 m`).
    has_params: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum MokState {
    #[default]
    Ground,
    Esc,
    /// After `ESC [`.
    Csi,
    /// After `ESC [ >`: collecting parameters.
    CsiGt,
}

impl MokScanner {
    /// Feeds child output; returns the new modifyOtherKeys level if a request was completed
    /// (the last one wins).
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Option<u8> {
        let mut level = None;
        for &b in bytes {
            if let Some(l) = self.step(b) {
                level = Some(l);
            }
        }
        level
    }

    fn step(&mut self, b: u8) -> Option<u8> {
        match self.state {
            MokState::Ground => {
                if b == ESC {
                    self.state = MokState::Esc;
                }
            }
            MokState::Esc => {
                self.state = match b {
                    b'[' => MokState::Csi,
                    ESC => MokState::Esc,
                    b'c' => {
                        self.state = MokState::Ground;
                        return Some(0);
                    }
                    _ => MokState::Ground,
                }
            }
            MokState::Csi => {
                self.state = match b {
                    b'>' => {
                        self.params = [0; 2];
                        self.index = 0;
                        self.has_params = false;
                        MokState::CsiGt
                    }
                    ESC => MokState::Esc,
                    _ => MokState::Ground,
                }
            }
            MokState::CsiGt => match b {
                b'0'..=b'9' => {
                    self.has_params = true;
                    if let Some(p) = self.params.get_mut(usize::from(self.index)) {
                        *p = p.saturating_mul(10).saturating_add(u16::from(b - b'0'));
                    }
                }
                b';' => {
                    self.has_params = true;
                    self.index = self.index.saturating_add(1);
                }
                b'm' => {
                    self.state = MokState::Ground;
                    // `CSI > m` resets every resource; `CSI > 4 m` / `CSI > 4;0 m` reset
                    // modifyOtherKeys; `CSI > 4;n m` sets it.
                    return match self.params {
                        _ if !self.has_params => Some(0),
                        [4, n] => Some(u8::try_from(n).unwrap_or(u8::MAX)),
                        _ => None,
                    };
                }
                ESC => self.state = MokState::Esc,
                _ => self.state = MokState::Ground,
            },
        }
        None
    }
}

/// The DEC private mode number of win32-input-mode (Windows Terminal / ConPTY).
const WIN32_INPUT_MODE: u32 = 9001;

/// Recogniser for DECSET/DECRST lists containing 9001 (`CSI ? … 9001 … h` / `l`) and RIS
/// (`ESC c`, which turns the mode off). Keeps its state between reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Win32ModeScanner {
    state: Win32State,
    /// Value of the parameter being parsed (saturating).
    param: u32,
    /// 9001 appeared among the completed parameters of the current sequence.
    hit: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Win32State {
    #[default]
    Ground,
    Esc,
    /// After `ESC [`.
    Csi,
    /// After `ESC [ ?`: collecting parameters.
    Private,
}

impl Win32ModeScanner {
    /// Feeds child output; returns the new win32-input-mode state if a sequence changed it
    /// (the last one wins).
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Option<bool> {
        let mut on = None;
        for &b in bytes {
            if let Some(v) = self.step(b) {
                on = Some(v);
            }
        }
        on
    }

    fn step(&mut self, b: u8) -> Option<bool> {
        match self.state {
            Win32State::Ground => {
                if b == ESC {
                    self.state = Win32State::Esc;
                }
            }
            Win32State::Esc => {
                self.state = match b {
                    b'[' => Win32State::Csi,
                    ESC => Win32State::Esc,
                    b'c' => {
                        self.state = Win32State::Ground;
                        return Some(false);
                    }
                    _ => Win32State::Ground,
                }
            }
            Win32State::Csi => {
                self.state = match b {
                    b'?' => {
                        self.param = 0;
                        self.hit = false;
                        Win32State::Private
                    }
                    ESC => Win32State::Esc,
                    _ => Win32State::Ground,
                }
            }
            Win32State::Private => match b {
                b'0'..=b'9' => {
                    self.param = self
                        .param
                        .saturating_mul(10)
                        .saturating_add(u32::from(b - b'0'));
                }
                b';' => {
                    self.hit |= self.param == WIN32_INPUT_MODE;
                    self.param = 0;
                }
                b'h' | b'l' => {
                    self.state = Win32State::Ground;
                    let hit = self.hit || self.param == WIN32_INPUT_MODE;
                    return hit.then_some(b == b'h');
                }
                ESC => self.state = Win32State::Esc,
                _ => self.state = Win32State::Ground,
            },
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels(chunks: &[&[u8]]) -> Vec<Option<u8>> {
        let mut s = MokScanner::default();
        chunks.iter().map(|c| s.feed(c)).collect()
    }

    #[test]
    fn set_and_reset() {
        assert_eq!(levels(&[b"\x1b[>4;2m"]), [Some(2)]);
        assert_eq!(levels(&[b"\x1b[>4;1m"]), [Some(1)]);
        assert_eq!(levels(&[b"\x1b[>4m"]), [Some(0)]);
        assert_eq!(levels(&[b"\x1b[>4;0m"]), [Some(0)]);
        assert_eq!(levels(&[b"\x1b[>m"]), [Some(0)]);
        assert_eq!(levels(&[b"\x1bc"]), [Some(0)]);
        // Last request in a chunk wins.
        assert_eq!(levels(&[b"\x1b[>4;2mtext\x1b[>4m"]), [Some(0)]);
    }

    #[test]
    fn split_across_reads() {
        assert_eq!(
            levels(&[b"ab\x1b", b"[", b">4", b";", b"2", b"m"]),
            [None, None, None, None, None, Some(2)]
        );
        assert_eq!(levels(&[b"x\x1b", b"c"]), [None, Some(0)]);
    }

    #[test]
    fn ignores_other_sequences() {
        // SGR, kitty push, DA2 query, other `CSI >` resources, private modes.
        assert_eq!(
            levels(&[b"\x1b[4;2m\x1b[>1u\x1b[>c\x1b[>1;2m\x1b[?4;2m\x1b[>4;2x"]),
            [None]
        );
        // An ESC aborts a sequence and starts a new one.
        assert_eq!(levels(&[b"\x1b[>4\x1b[>4;2m"]), [Some(2)]);
        // Huge parameters saturate instead of overflowing.
        assert_eq!(levels(&[b"\x1b[>4;99999999m"]), [Some(255)]);
        // `CSI > 0 m` resets modifyKeyboard, not modifyOtherKeys.
        assert_eq!(levels(&[b"\x1b[>4;2m\x1b[>0m"]), [Some(2)]);
    }

    fn win32(chunks: &[&[u8]]) -> Vec<Option<bool>> {
        let mut s = Win32ModeScanner::default();
        chunks.iter().map(|c| s.feed(c)).collect()
    }

    #[test]
    fn win32_mode_set_reset_and_lists() {
        assert_eq!(win32(&[b"\x1b[?9001h"]), [Some(true)]);
        assert_eq!(win32(&[b"\x1b[?9001l"]), [Some(false)]);
        // Inside a mode list, first, middle or last.
        assert_eq!(win32(&[b"\x1b[?9001;1004h"]), [Some(true)]);
        assert_eq!(win32(&[b"\x1b[?25;9001;1004h"]), [Some(true)]);
        assert_eq!(win32(&[b"\x1b[?1004;9001l"]), [Some(false)]);
        // RIS turns it off; the last change in a chunk wins.
        assert_eq!(win32(&[b"\x1bc"]), [Some(false)]);
        assert_eq!(win32(&[b"\x1b[?9001hab\x1b[?9001l"]), [Some(false)]);
        assert_eq!(win32(&[b"\x1b[?9001l\x1b[?9001h"]), [Some(true)]);
    }

    #[test]
    fn win32_mode_split_across_chunks() {
        assert_eq!(
            win32(&[b"out\x1b", b"[", b"?", b"90", b"01", b"h", b"x"]),
            [None, None, None, None, None, Some(true), None]
        );
        assert_eq!(
            win32(&[b"\x1b[?1004;", b"9001", b";2004", b"l"]),
            [None, None, None, Some(false)]
        );
        assert_eq!(win32(&[b"\x1b", b"c"]), [None, Some(false)]);
    }

    #[test]
    fn win32_mode_ignores_other_sequences() {
        // Other private modes, a non-private 9001, a prefix/suffix of 9001, other finals.
        assert_eq!(
            win32(&[b"\x1b[?1004h\x1b[9001h\x1b[?900h\x1b[?90011h\x1b[?19001h\x1b[?9001$p"]),
            [None]
        );
        // An ESC aborts a sequence and starts a new one.
        assert_eq!(win32(&[b"\x1b[?9001\x1b[?9001h"]), [Some(true)]);
        // Huge parameters saturate (no overflow, no false match).
        assert_eq!(win32(&[b"\x1b[?99999999999999h"]), [None]);
    }
}
