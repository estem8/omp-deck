//! Session snapshots: a VT replay that turns a fresh alacritty `Term` of the same size into the
//! daemon's terminal state, plus the side state `Term` does not expose.

use std::io::Write as _;

use crate::run_state::{self, RunState};
use alacritty_terminal::Term;
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::vte::ansi::{
    CharsetIndex, Color, CursorShape, CursorStyle, NamedColor, StandardCharset,
};
use alacritty_terminal::vte::{Params, Parser, Perform};

/// Terminal state outside alacritty's public `Term` API, tracked from the output stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideState {
    /// Window title (OSC 0/2, title stack `CSI 22/23 t`, reset by RIS).
    pub title: Option<String>,
    /// xterm modifyOtherKeys level from `CSI > 4 ; n m` (0 = off).
    pub modify_other_keys: u8,
    /// win32-input-mode `CSI ? 9001 h/l` (ConPTY asks for it).
    pub win32_input_mode: bool,
    /// Color-scheme change reports `CSI ? 2031 h/l`.
    pub color_scheme_reports: bool,
    /// Synchronized output `CSI ? 2026 h/l` in progress.
    pub sync_update: bool,
    /// DECSTBM region as 0-based `[top, bottom)` screen lines; `None` = whole screen.
    pub scroll_region: Option<(u16, u16)>,
    /// omp's run status: the last OSC 7501 report that was not cleared.
    pub run_state: Option<RunState>,
}

/// Maximum title stack depth (same as alacritty).
const TITLE_STACK_MAX: usize = 4096;

/// Feeds output bytes through a VT parser to maintain [`SideState`].
pub struct SideScanner {
    parser: Parser,
    tracker: Tracker,
}

struct Tracker {
    state: SideState,
    title_stack: Vec<Option<String>>,
    rows: u16,
}

impl SideScanner {
    pub fn new(rows: u16) -> Self {
        SideScanner {
            parser: Parser::new(),
            tracker: Tracker {
                state: SideState::default(),
                title_stack: Vec::new(),
                rows,
            },
        }
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.tracker, bytes);
    }

    /// The terminal was resized (alacritty resets the scroll region on resize).
    pub fn resize(&mut self, rows: u16) {
        self.tracker.rows = rows;
        self.tracker.state.scroll_region = None;
    }

    pub fn state(&self) -> &SideState {
        &self.tracker.state
    }

    /// The child ended; its last program status no longer describes a running program.
    pub fn child_exited(&mut self) {
        self.tracker.state.run_state = None;
    }
}

impl Perform for Tracker {
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let mut iter = params.iter().map(|p| p[0]);
        // Same defaulting as vte's ansi parser: a missing or zero parameter takes the default.
        let mut next_or = |default: u16| match iter.next() {
            Some(v) if v != 0 => v,
            _ => default,
        };
        match (action, intermediates) {
            ('h' | 'l', [b'?']) => {
                let on = action == 'h';
                for mode in params.iter().map(|p| p[0]) {
                    match mode {
                        9001 => self.state.win32_input_mode = on,
                        2031 => self.state.color_scheme_reports = on,
                        2026 => self.state.sync_update = on,
                        // DECCOLM clears the scroll region.
                        3 => self.state.scroll_region = None,
                        _ => {}
                    }
                }
            }
            ('m', [b'>']) => {
                if next_or(1) == 4 {
                    let level = next_or(0);
                    if level <= 2 {
                        self.state.modify_other_keys = level as u8;
                    }
                }
            }
            ('r', []) => {
                let top = next_or(1);
                let bottom = next_or(self.rows);
                if top < bottom {
                    let start = (top - 1).min(self.rows);
                    let end = bottom.min(self.rows);
                    self.state.scroll_region =
                        (start != 0 || end != self.rows).then_some((start, end));
                }
            }
            ('t', []) => match next_or(1) {
                22 => {
                    if self.title_stack.len() >= TITLE_STACK_MAX {
                        self.title_stack.remove(0);
                    }
                    self.title_stack.push(self.state.title.clone());
                }
                23 => {
                    if let Some(title) = self.title_stack.pop() {
                        self.state.title = title;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if let Some(report) = run_state::parse_osc(params) {
            report.apply(&mut self.state.run_state);
            return;
        }
        if let [b"0" | b"2", rest @ ..] = params
            && !rest.is_empty()
        {
            let parts: Vec<&str> = rest
                .iter()
                .filter_map(|p| std::str::from_utf8(p).ok())
                .collect();
            self.state.title = Some(parts.join(";").trim().to_owned());
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        // RIS resets what alacritty's `reset_state` resets; win32-input-mode, modifyOtherKeys
        // and 2031 are connection properties requested once (ConPTY) and survive.
        if !ignore && intermediates.is_empty() && byte == b'c' {
            self.state.title = None;
            self.state.run_state = None;
            self.title_stack.clear();
            self.state.scroll_region = None;
        }
    }
}

/// Encodes `term` (plus `side`) as a VT replay for a fresh `Term` of the same size: reset,
/// up to `history_lines` scrollback rows and the screen of the primary grid, the alternate
/// grid if active, cursors (incl. DECSC), cursor style, modes, kitty keyboard flags, colors,
/// title, scroll region and side state.
///
/// Takes `&mut` only to read the inactive primary grid while the alternate screen is active
/// (`swap_alt` there and back); the terminal is left as it was.
pub fn encode<T>(term: &mut Term<T>, side: &SideState, history_lines: usize) -> Vec<u8> {
    let mut w = Writer::default();
    // RIS keeps color overrides; clear them explicitly.
    w.out
        .extend_from_slice(b"\x1bc\x1b]104\x1b\\\x1b]110\x1b\\\x1b]111\x1b\\\x1b]112\x1b\\");

    let mode = *term.mode();
    let origin = mode.contains(TermMode::ORIGIN);
    let region_top = side.scroll_region.map_or(0, |(top, _)| top);

    if mode.contains(TermMode::ALT_SCREEN) {
        let alt = term.grid().clone();
        term.swap_alt();
        // Primary is active now; its keyboard flags are the top-level mode bits.
        w.kitty_flags(*term.mode());
        w.grid(term.grid(), history_lines);
        // The primary's DECSC slot equals its cursor while the alternate screen is up, and
        // `?1049h` stores it again on replay.
        w.cursor(term.grid(), false, 0);
        term.swap_alt();
        *term.grid_mut() = alt;

        w.out.extend_from_slice(b"\x1b[?1049h\x1b[H");
        w.kitty_flags(mode);
        w.grid(term.grid(), 0);
    } else {
        w.kitty_flags(mode);
        w.grid(term.grid(), history_lines);
    }

    let grid = term.grid();
    w.sgr_reset();
    if let Some((top, bottom)) = side.scroll_region {
        let _ = write!(w.out, "\x1b[{};{}r", top + 1, bottom);
    }
    w.saved_cursor(grid);
    if origin {
        w.out.extend_from_slice(b"\x1b[?6h");
    }
    w.cursor(grid, origin, region_top);

    w.modes(mode);
    let style = term.cursor_style();
    if style != CursorStyle::default() {
        let code = match (style.shape, style.blinking) {
            (CursorShape::Block, true) => Some(1),
            (CursorShape::Block, false) => Some(2),
            (CursorShape::Underline, true) => Some(3),
            (CursorShape::Underline, false) => Some(4),
            (CursorShape::Beam, true) => Some(5),
            (CursorShape::Beam, false) => Some(6),
            _ => None,
        };
        if let Some(code) = code {
            let _ = write!(w.out, "\x1b[{code} q");
        }
    }

    let colors = term.colors();
    for index in 0..259 {
        if let Some(rgb) = colors[index] {
            let (r, g, b) = (rgb.r, rgb.g, rgb.b);
            let _ = match index {
                0..=255 => write!(w.out, "\x1b]4;{index};rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\"),
                _ => write!(
                    w.out,
                    "\x1b]{};rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\",
                    index - 246
                ),
            };
        }
    }

    if let Some(title) = &side.title {
        w.out.extend_from_slice(b"\x1b]2;");
        w.out
            .extend(title.bytes().filter(|b| !b.is_ascii_control()));
        w.out.extend_from_slice(b"\x1b\\");
    }
    if side.modify_other_keys != 0 {
        let _ = write!(w.out, "\x1b[>4;{}m", side.modify_other_keys);
    }
    if side.win32_input_mode {
        w.out.extend_from_slice(b"\x1b[?9001h");
    }
    if let Some(run_state) = &side.run_state {
        run_state::encode_report(run_state, &mut w.out);
    }
    if side.color_scheme_reports {
        w.out.extend_from_slice(b"\x1b[?2031h");
    }
    // Last: the replaying parser buffers everything after it until the live `?2026l`.
    if side.sync_update {
        w.out.extend_from_slice(b"\x1b[?2026h");
    }
    w.out
}

/// SGR-relevant cell attributes.
#[derive(Clone, PartialEq)]
struct Attrs {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
}

const STYLE_FLAGS: Flags = Flags::INVERSE
    .union(Flags::BOLD)
    .union(Flags::ITALIC)
    .union(Flags::DIM)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT)
    .union(Flags::ALL_UNDERLINES);

impl Attrs {
    fn of(cell: &Cell) -> Attrs {
        Attrs {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & STYLE_FLAGS,
            underline: cell.underline_color(),
        }
    }

    fn default_attrs() -> Attrs {
        Attrs::of(&Cell::default())
    }
}

#[derive(Default)]
struct Writer {
    out: Vec<u8>,
    /// Attributes last emitted (`None` = unknown, emit next time).
    attrs: Option<Attrs>,
    link: Option<Hyperlink>,
}

impl Writer {
    fn sgr_reset(&mut self) {
        self.out.extend_from_slice(b"\x1b[0m");
        self.attrs = Some(Attrs::default_attrs());
    }

    fn set_attrs(&mut self, cell: &Cell) {
        let attrs = Attrs::of(cell);
        if self.attrs.as_ref() == Some(&attrs) {
            return;
        }
        self.out.extend_from_slice(b"\x1b[0");
        let f = attrs.flags;
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
            (Flags::UNDERLINE, "4"),
            (Flags::DOUBLE_UNDERLINE, "4:2"),
            (Flags::UNDERCURL, "4:3"),
            (Flags::DOTTED_UNDERLINE, "4:4"),
            (Flags::DASHED_UNDERLINE, "4:5"),
        ] {
            if f.contains(flag) {
                self.out.push(b';');
                self.out.extend_from_slice(code.as_bytes());
            }
        }
        color_sgr(&mut self.out, attrs.fg, 30, 90, 38);
        color_sgr(&mut self.out, attrs.bg, 40, 100, 48);
        if let Some(color) = attrs.underline {
            color_sgr(&mut self.out, color, 0, 0, 58);
        }
        self.out.push(b'm');
        self.attrs = Some(attrs);
    }

    fn set_link(&mut self, link: Option<Hyperlink>) {
        if self.link == link {
            return;
        }
        match &link {
            Some(l) => {
                let _ = write!(self.out, "\x1b]8;id={};{}\x1b\\", l.id(), l.uri());
            }
            None => self.out.extend_from_slice(b"\x1b]8;;\x1b\\"),
        }
        self.link = link;
    }

    fn cell(&mut self, cell: &Cell) {
        self.set_attrs(cell);
        self.set_link(cell.hyperlink());
        let mut utf8 = [0u8; 4];
        self.out
            .extend_from_slice(cell.c.encode_utf8(&mut utf8).as_bytes());
        for &c in cell.zerowidth().unwrap_or(&[]) {
            self.out
                .extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
        }
    }

    fn kitty_flags(&mut self, mode: TermMode) {
        let bits = [
            TermMode::DISAMBIGUATE_ESC_CODES,
            TermMode::REPORT_EVENT_TYPES,
            TermMode::REPORT_ALTERNATE_KEYS,
            TermMode::REPORT_ALL_KEYS_AS_ESC,
            TermMode::REPORT_ASSOCIATED_TEXT,
        ]
        .iter()
        .enumerate()
        .filter(|(_, m)| mode.contains(**m))
        .fold(0u8, |acc, (i, _)| acc | (1 << i));
        // Pushed (not `CSI = … u`) so the flags live on the per-screen stack and survive
        // `?1049` swaps on the replaying side.
        if bits != 0 {
            let _ = write!(self.out, "\x1b[>{bits}u");
        }
    }

    /// Writes `history` scrollback rows (capped) and all screen rows from the cursor's
    /// current position at the top-left.
    ///
    /// Lines created by scrolling are filled with the current background (BCE), so newlines
    /// are only emitted with default attributes, and a continuation row (entered by
    /// auto-wrap with the wrapping cell's attributes) clears its untouched tail explicitly.
    fn grid(&mut self, grid: &Grid<Cell>, history: usize) {
        let cols = grid.columns();
        let lines = grid.screen_lines() as i32;
        let history = history.min(grid.history_size()) as i32;
        let mut continuation = false;
        for line in -history..lines {
            let row = &grid[Line(line)];
            let wraps = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            let end = if wraps {
                cols
            } else {
                (0..cols)
                    .rev()
                    .find(|&c| !is_blank(&row[Column(c)]))
                    .map_or(0, |c| c + 1)
            };
            for col in 0..end {
                let cell = &row[Column(col)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                self.cell(cell);
            }
            if continuation && end < cols {
                self.default_attrs();
                if end == 0 {
                    // Still wrap-pending at the end of the previous row: `CSI K` there would
                    // cancel the wrap (and erase that row's last cell). A blank realizes the
                    // wrap; CR returns to the start of the continuation row.
                    self.out.extend_from_slice(b" \r");
                }
                self.out.extend_from_slice(b"\x1b[K");
            }
            if line != lines - 1 && !wraps {
                self.default_attrs();
                self.out.extend_from_slice(b"\r\n");
            }
            continuation = wraps;
        }
    }

    /// Ensures default SGR attributes (and no hyperlink) are active.
    fn default_attrs(&mut self) {
        if self.attrs.as_ref() != Some(&Attrs::default_attrs()) {
            self.sgr_reset();
        }
        self.set_link(None);
    }

    fn charsets(&mut self, charsets: impl Fn(CharsetIndex) -> StandardCharset, reset: bool) {
        for (index, final_byte) in [
            (CharsetIndex::G0, b'('),
            (CharsetIndex::G1, b')'),
            (CharsetIndex::G2, b'*'),
            (CharsetIndex::G3, b'+'),
        ] {
            if charsets(index) == StandardCharset::SpecialCharacterAndLineDrawing {
                self.out
                    .extend_from_slice(&[0x1b, final_byte, if reset { b'B' } else { b'0' }]);
            }
        }
    }

    /// DECSC slot of the active grid (absolute position; origin mode is not on yet).
    fn saved_cursor(&mut self, grid: &Grid<Cell>) {
        let saved = &grid.saved_cursor;
        let _ = write!(
            self.out,
            "\x1b[{};{}H",
            saved.point.line.0 + 1,
            saved.point.column.0 + 1
        );
        self.set_attrs(&saved.template);
        self.set_link(saved.template.hyperlink());
        self.charsets(|i| saved.charsets[i], false);
        self.out.extend_from_slice(b"\x1b7");
        self.charsets(|i| saved.charsets[i], true);
    }

    /// Cursor position (incl. a pending wrap), template attributes and charsets.
    fn cursor(&mut self, grid: &Grid<Cell>, origin: bool, region_top: u16) {
        let cursor = &grid.cursor;
        let line = cursor.point.line.0 - if origin { i32::from(region_top) } else { 0 };
        let cols = grid.columns();
        if cursor.input_needs_wrap && cursor.point.column.0 + 1 == cols {
            // Rewrite the last cell so the replaying cursor ends with the same pending wrap.
            let row = &grid[cursor.point.line];
            let mut col = cols - 1;
            if row[Column(col)].flags.contains(Flags::WIDE_CHAR_SPACER) && col > 0 {
                col -= 1;
            }
            let _ = write!(self.out, "\x1b[{};{}H", line.max(0) + 1, col + 1);
            self.cell(&row[Column(col)]);
        } else {
            let _ = write!(
                self.out,
                "\x1b[{};{}H",
                line.max(0) + 1,
                cursor.point.column.0 + 1
            );
        }
        self.set_attrs(&cursor.template);
        self.set_link(cursor.template.hyperlink());
        self.charsets(|i| cursor.charsets[i], false);
    }

    fn modes(&mut self, mode: TermMode) {
        let default = TermMode::default();
        let private = |m: TermMode, code: u16, out: &mut Vec<u8>| {
            if mode.contains(m) != default.contains(m) {
                let _ = write!(
                    out,
                    "\x1b[?{code}{}",
                    if mode.contains(m) { 'h' } else { 'l' }
                );
            }
        };
        private(TermMode::SHOW_CURSOR, 25, &mut self.out);
        private(TermMode::APP_CURSOR, 1, &mut self.out);
        private(TermMode::LINE_WRAP, 7, &mut self.out);
        private(TermMode::FOCUS_IN_OUT, 1004, &mut self.out);
        private(TermMode::BRACKETED_PASTE, 2004, &mut self.out);
        private(TermMode::ALTERNATE_SCROLL, 1007, &mut self.out);
        private(TermMode::URGENCY_HINTS, 1042, &mut self.out);
        // Mouse reporting and encodings are each mutually exclusive.
        private(TermMode::MOUSE_REPORT_CLICK, 1000, &mut self.out);
        private(TermMode::MOUSE_DRAG, 1002, &mut self.out);
        private(TermMode::MOUSE_MOTION, 1003, &mut self.out);
        private(TermMode::UTF8_MOUSE, 1005, &mut self.out);
        private(TermMode::SGR_MOUSE, 1006, &mut self.out);
        if mode.contains(TermMode::APP_KEYPAD) {
            self.out.extend_from_slice(b"\x1b=");
        }
        if mode.contains(TermMode::INSERT) {
            self.out.extend_from_slice(b"\x1b[4h");
        }
        if mode.contains(TermMode::LINE_FEED_NEW_LINE) {
            self.out.extend_from_slice(b"\x1b[20h");
        }
    }
}

fn is_blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.fg == Color::Named(NamedColor::Foreground)
        && cell.bg == Color::Named(NamedColor::Background)
        && (cell.flags & !Flags::WRAPLINE).is_empty()
        && cell.extra.is_none()
}

/// Appends `;<sgr>` for `color`; `base`/`bright` select the 8/16-color forms (0 = none).
fn color_sgr(out: &mut Vec<u8>, color: Color, base: u16, bright: u16, extended: u16) {
    let _ = match color {
        Color::Named(NamedColor::Foreground) if base == 30 => Ok(()),
        Color::Named(NamedColor::Background) if base == 40 => Ok(()),
        Color::Named(named) => {
            let n = named as usize;
            match n {
                0..=7 if base != 0 => write!(out, ";{}", base + n as u16),
                8..=15 if bright != 0 => write!(out, ";{}", bright + n as u16 - 8),
                0..=255 => write!(out, ";{extended};5;{n}"),
                // Dim/bright foreground variants are render-time colors, never stored by SGR.
                _ => Ok(()),
            }
        }
        Color::Indexed(i) => write!(out, ";{extended};5;{i}"),
        Color::Spec(rgb) => write!(out, ";{extended};2;{};{};{}", rgb.r, rgb.g, rgb.b),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::Processor;

    fn term(cols: usize, rows: usize) -> Term<VoidListener> {
        let config = Config {
            scrolling_history: 1000,
            kitty_keyboard: true,
            ..Config::default()
        };
        Term::new(config, &TermSize::new(cols, rows), VoidListener)
    }

    struct Fixture {
        term: Term<VoidListener>,
        processor: Processor,
        side: SideScanner,
    }

    impl Fixture {
        fn new(cols: usize, rows: usize) -> Self {
            Fixture {
                term: term(cols, rows),
                processor: Processor::new(),
                side: SideScanner::new(rows as u16),
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            self.processor.advance(&mut self.term, bytes);
            self.side.advance(bytes);
        }

        /// Encodes, replays into a fresh term (and scanner) and returns them.
        fn replay(&mut self, history: usize) -> Fixture {
            let side = self.side.state().clone();
            let bytes = encode(&mut self.term, &side, history);
            let mut fresh = Fixture::new(self.term.columns(), self.term.screen_lines());
            fresh.feed(&bytes);
            fresh
        }
    }

    fn rows_of(term: &Term<VoidListener>) -> Vec<Vec<Cell>> {
        let grid = term.grid();
        (grid.topmost_line().0..grid.screen_lines() as i32)
            .map(|l| {
                (0..grid.columns())
                    .map(|c| grid[Line(l)][Column(c)].clone())
                    .collect()
            })
            .collect()
    }

    /// Asserts both terminals match: every cell (primary and alternate), cursors, modes, style.
    fn assert_same(a: &mut Term<VoidListener>, b: &mut Term<VoidListener>) {
        assert_eq!(a.mode(), b.mode(), "modes");
        assert_eq!(a.cursor_style(), b.cursor_style(), "cursor style");
        for i in 0..259 {
            assert_eq!(a.colors()[i], b.colors()[i], "color {i}");
        }
        let check = |a: &Term<VoidListener>, b: &Term<VoidListener>, what: &str| {
            let (ra, rb) = (rows_of(a), rows_of(b));
            assert_eq!(ra.len(), rb.len(), "{what} row count");
            for (i, (x, y)) in ra.iter().zip(&rb).enumerate() {
                for (c, (p, q)) in x.iter().zip(y).enumerate() {
                    assert_eq!(p, q, "{what} cell at row {i} col {c}");
                }
            }
            let (ca, cb) = (&a.grid().cursor, &b.grid().cursor);
            assert_eq!(ca.point, cb.point, "{what} cursor");
            assert_eq!(ca.template, cb.template, "{what} template");
            assert_eq!(
                ca.input_needs_wrap, cb.input_needs_wrap,
                "{what} pending wrap"
            );
            for i in [
                CharsetIndex::G0,
                CharsetIndex::G1,
                CharsetIndex::G2,
                CharsetIndex::G3,
            ] {
                assert_eq!(ca.charsets[i], cb.charsets[i], "{what} charsets");
            }
            assert_eq!(
                a.grid().saved_cursor.point,
                b.grid().saved_cursor.point,
                "{what} DECSC"
            );
            assert_eq!(
                a.grid().saved_cursor.template,
                b.grid().saved_cursor.template,
                "{what} DECSC sgr"
            );
        };
        check(a, b, "active");
        if a.mode().contains(TermMode::ALT_SCREEN) {
            let (alt_a, alt_b) = (a.grid().clone(), b.grid().clone());
            a.swap_alt();
            b.swap_alt();
            assert_eq!(a.mode(), b.mode(), "primary modes");
            check(a, b, "primary");
            a.swap_alt();
            b.swap_alt();
            *a.grid_mut() = alt_a;
            *b.grid_mut() = alt_b;
        }
    }

    #[test]
    fn primary_screen_with_history_attrs_and_cursor() {
        let mut f = Fixture::new(20, 5);
        for i in 0..12 {
            f.feed(format!("line {i}\r\n").as_bytes());
        }
        f.feed(b"\x1b[1;3;4:3;38;5;196;48;2;1;2;3;58;5;33mfancy\x1b[0m ");
        f.feed(b"\x1b[2;7;9;21;94;101mmore\x1b[0m\r\n");
        f.feed("wide 漢字 e\u{301}\r\n".as_bytes());
        f.feed(b"\x1b]8;id=x1;https://example.com\x1b\\link\x1b]8;;\x1b\\ ");
        f.feed(b"wrapping text that is longer than twenty columns");
        f.feed(b"\x1b[3;5H\x1b(0\x1b[1;32m");
        let mut b = f.replay(1000);
        assert_same(&mut f.term, &mut b.term);
    }

    #[test]
    fn history_is_capped() {
        let mut f = Fixture::new(10, 3);
        for i in 0..20 {
            f.feed(format!("r{i}\r\n").as_bytes());
        }
        let b = f.replay(4);
        assert_eq!(b.term.grid().history_size(), 4);
        let first: String = (0..3)
            .map(|c| b.term.grid()[Line(-4)][Column(c)].c)
            .collect();
        assert_eq!(first, "r14");
        let last: String = (0..3)
            .map(|c| b.term.grid()[Line(1)][Column(c)].c)
            .collect();
        assert_eq!(last, "r19");
    }

    #[test]
    fn pending_wrap_and_wide_char_at_edge() {
        let mut f = Fixture::new(6, 3);
        f.feed("abcd漢".as_bytes());
        assert!(f.term.grid().cursor.input_needs_wrap);
        let mut b = f.replay(0);
        assert_same(&mut f.term, &mut b.term);
        f.feed("x漢字".as_bytes());
        let mut b = f.replay(0);
        assert_same(&mut f.term, &mut b.term);
    }

    #[test]
    fn blank_continuation_row_after_wrap() {
        let mut f = Fixture::new(8, 5);
        // Row 0 wraps into row 1, which is then erased entirely; more rows follow.
        f.feed(b"\x1b[44mabcdefghX\x1b[0m\x1b[2K\r\nnext\r\nlast");
        assert!(
            f.term.grid()[Line(0)][Column(7)]
                .flags
                .contains(Flags::WRAPLINE)
        );
        let mut b = f.replay(0);
        assert_same(&mut f.term, &mut b.term);
        assert_eq!(rows_of(&b.term)[2][0].c, 'n');
    }

    #[test]
    fn alternate_screen_with_primary_modes_and_side_state() {
        let mut f = Fixture::new(30, 8);
        f.feed(b"\x1b]2;my title\x07\x1b[>4;2m\x1b[?9001h\x1b[?2031h");
        f.feed(b"\x1b]4;1;rgb:12/34/56\x1b\\\x1b]11;rgb:01/02/03\x1b\\");
        f.feed(b"\x1b[>1uprimary text\r\nsecond\x1b[31m");
        f.feed(b"\x1b[?1049h\x1b[>3u\x1b[?1;2004;1006;1002;1004h\x1b[?25l\x1b[5 q");
        f.feed(b"\x1b[2;6r\x1b[3;3H\x1b[33msaved\x1b7\x1b[0m\x1b[?6h\x1b[2;4Halt body\x1b[4h\x1b=");
        let mut b = f.replay(100);
        assert_same(&mut f.term, &mut b.term);
        assert_eq!(b.side.state(), f.side.state());
        assert_eq!(b.side.state().title.as_deref(), Some("my title"));
        assert_eq!(b.side.state().scroll_region, Some((1, 6)));

        // Leaving the alternate screen restores the same primary on both sides.
        f.feed(b"\x1b[?6l\x1b[r\x1b[?1049l");
        b.feed(b"\x1b[?6l\x1b[r\x1b[?1049l");
        assert_same(&mut f.term, &mut b.term);
        assert!(f.term.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
    }

    #[test]
    fn primary_decsc_and_origin_mode() {
        let mut f = Fixture::new(12, 6);
        f.feed(b"top\r\n\x1b[4;2H\x1b[7mS\x1b7\x1b[0m\x1b[2;5r\x1b[?6h\x1b[3;3Hin\x1b[?7l\x1b[?1000h\x1b[?1005h");
        let mut b = f.replay(0);
        assert_same(&mut f.term, &mut b.term);
        assert_eq!(b.side.state().scroll_region, Some((1, 5)));
        // Same scroll behavior afterwards.
        f.feed(b"\x1b[10Bx\r\ny\r\nz");
        b.feed(b"\x1b[10Bx\r\ny\r\nz");
        assert_same(&mut f.term, &mut b.term);
    }

    #[test]
    fn scanner_tracks_side_state() {
        let mut s = SideScanner::new(24);
        s.advance(b"\x1b]0;one\x07\x1b[22t\x1b]2;two\x1b\\");
        assert_eq!(s.state().title.as_deref(), Some("two"));
        s.advance(b"\x1b[23t");
        assert_eq!(s.state().title.as_deref(), Some("one"));
        s.advance(b"\x1b[>4;1m\x1b[?2026h\x1b[5;10r");
        assert_eq!(s.state().modify_other_keys, 1);
        assert!(s.state().sync_update);
        assert_eq!(s.state().scroll_region, Some((4, 10)));
        s.advance(b"\x1b[>4m\x1b[?2026l\x1b[1;24r");
        assert_eq!(s.state().modify_other_keys, 0);
        assert!(!s.state().sync_update);
        assert_eq!(s.state().scroll_region, None);
        s.advance(b"\x1b[3;9r");
        s.resize(30);
        assert_eq!(s.state().scroll_region, None);
        s.advance(b"\x1b[?9001h\x1bc");
        assert_eq!(s.state().title, None);
        assert!(s.state().win32_input_mode);
    }

    #[test]
    fn run_state_is_tracked_cleared_and_replayed() {
        use crate::run_state::{BlockedKind, RunPhase};

        let mut f = Fixture::new(20, 4);
        assert_eq!(f.side.state().run_state, None);
        // Split reads, both terminators; the last report wins.
        f.feed(b"hi\x1b]7501;state=work");
        f.feed(b"ing:app=omp\x07");
        assert_eq!(
            f.side.state().run_state,
            Some(RunState::new(RunPhase::Working))
        );
        f.feed(b"\x1b]7501;state=blocked:kind=question:app=omp:msg=aGk=\x1b\\");
        let blocked = RunState {
            state: RunPhase::Blocked {
                kind: Some(BlockedKind::Question),
            },
            msg: Some("hi".into()),
        };
        assert_eq!(f.side.state().run_state, Some(blocked.clone()));

        // A late joiner learns the state from the snapshot alone.
        let b = f.replay(100);
        assert_eq!(b.side.state().run_state, Some(blocked));
        assert_eq!(b.side.state(), f.side.state());

        // A malformed report changes nothing; clear removes the record and the replay stays
        // silent about it.
        f.feed(b"\x1b]7501;state=bogus\x07");
        assert!(f.side.state().run_state.is_some());
        f.feed(b"\x1b]7501;state=clear\x1b\\");
        assert_eq!(f.side.state().run_state, None);
        assert_eq!(f.replay(100).side.state().run_state, None);
    }

    #[test]
    fn child_exit_and_ris_clear_the_held_run_state() {
        for reset in [false, true] {
            let mut f = Fixture::new(20, 4);
            f.feed(b"\x1b]7501;state=working\x07");
            assert!(f.side.state().run_state.is_some());
            if reset {
                f.feed(b"\x1bc");
            } else {
                f.side.child_exited();
            }
            assert_eq!(f.side.state().run_state, None);
            assert_eq!(f.replay(100).side.state().run_state, None);
        }
    }

    #[test]
    fn sync_update_is_replayed_last() {
        let mut f = Fixture::new(10, 3);
        f.feed(b"\x1b[?2026h");
        let side = f.side.state().clone();
        let bytes = encode(&mut f.term, &side, 0);
        assert!(bytes.ends_with(b"\x1b[?2026h"));
    }
}
