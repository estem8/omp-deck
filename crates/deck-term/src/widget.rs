//! `TerminalView`: the egui widget wiring keyboard, mouse, focus, IME and resize to a
//! [`Terminal`], and painting it via `render`.

use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{TermMode, viewport_to_point};
use egui::{
    CursorIcon, Event, EventFilter, IMEPurpose, ImeEvent, Modifiers, MouseWheelUnit, PointerButton,
    Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2,
};

use crate::core::{Terminal, TerminalEvents};
use crate::input::{self, Key, KeyAction, KeyInput, KeyRules, Mods, MouseAction, MouseButton};
use crate::links;
use crate::render::{self, Frame, GlyphCache, Metrics, PaintParams, PaintScratch, UrlSpan};

const BLINK_HALF_PERIOD: Duration = Duration::from_millis(500);
const MULTI_CLICK_SECS: f64 = 0.4;

#[derive(Clone, Copy)]
struct Pressed {
    button: MouseButton,
    /// Reported to the app (mouse mode) vs handled locally (selection / paste).
    reporting: bool,
}

/// Per-terminal widget state, kept inside [`Terminal`] so hidden tabs keep their selection
/// gesture, blink phase and render buffers.
#[derive(Default)]
pub(crate) struct ViewState {
    frame: Frame,
    glyphs: GlyphCache,
    scratch: PaintScratch,
    metrics: Option<Metrics>,
    /// Shared with [`Terminal::set_focused`].
    pub(crate) focused: bool,
    blink_epoch: Option<Instant>,
    pressed: Option<Pressed>,
    /// Ctrl+click opened a link: swallow the matching release.
    link_press: bool,
    /// Link under the last right click: the context menu's Open/Copy link target.
    menu_link: Option<String>,
    /// Plain click waiting for a drag before it becomes a selection.
    pending_select: Option<(Point, Side)>,
    selecting: bool,
    last_click: Option<(f64, usize, usize)>,
    click_count: u8,
    last_report_cell: Option<(usize, usize)>,
    wheel_accum: f32,
    preedit: String,
    /// Key of a local clipboard shortcut whose release must not reach the PTY.
    shortcut_key: Option<egui::Key>,
    /// Keys whose press was dropped by [`Terminal::hold_input_until`]: their releases are
    /// swallowed too, so kitty event reporting never sees a lone release.
    held_keys: Vec<egui::Key>,
    /// Modifiers the child last saw in win32-input-mode (each change is sent as modifier key
    /// records, see [`input::encode_win32_modifiers`]).
    pub(crate) win32_mods: Mods,
}

impl ViewState {
    /// Restarts the blink phase; dropping focus abandons any IME composition.
    pub(crate) fn on_focus_change(&mut self, focused: bool) {
        self.blink_epoch = Some(Instant::now());
        if !focused {
            self.preedit.clear();
            // Keys released while unfocused are never seen; start over from "none held".
            self.win32_mods = Mods::NONE;
        }
    }
}

/// Embedded terminal widget. Fills the available rect.
pub struct TerminalView<'a> {
    term: &'a mut Terminal,
    font_size: f32,
    /// Hash of [`Self::id_salt`]: the interaction id comes from it instead of the layout's
    /// automatic (draw-order dependent) id.
    id_salt: Option<u64>,
}

pub struct TerminalResponse {
    pub response: egui::Response,
    /// Merged `poll()` result for this frame.
    pub events: TerminalEvents,
    /// Ctrl+click on a hyperlink (OSC 8 target or a plain URL in the text); the app opens it.
    pub open_url: Option<String>,
}

/// Grid geometry of this frame.
#[derive(Clone, Copy)]
struct Geometry {
    origin: Pos2,
    cell_w: f32,
    cell_h: f32,
    cols: usize,
    rows: usize,
}

impl Geometry {
    /// Cell under `pos`, clamped to the grid, plus which half of the cell.
    fn cell(&self, pos: Pos2) -> (usize, usize, Side) {
        let fx = ((pos.x - self.origin.x) / self.cell_w).max(0.0);
        let fy = ((pos.y - self.origin.y) / self.cell_h).max(0.0);
        let col = (fx as usize).min(self.cols.saturating_sub(1));
        let row = (fy as usize).min(self.rows.saturating_sub(1));
        let side = if fx.fract() < 0.5 || fx as usize >= self.cols {
            Side::Left
        } else {
            Side::Right
        };
        (row, col, side)
    }

    fn cell_rect(&self, row: usize, col: usize) -> Rect {
        Rect::from_min_size(
            pos2(
                self.origin.x + col as f32 * self.cell_w,
                self.origin.y + row as f32 * self.cell_h,
            ),
            egui::vec2(self.cell_w, self.cell_h),
        )
    }
}

fn mods_of(m: Modifiers) -> Mods {
    Mods {
        shift: m.shift,
        alt: m.alt,
        ctrl: m.ctrl,
    }
}

/// Maps an egui key to the encoder's key. Shifted symbols use the physical key's base char
/// when Shift is held (Shift+; is logical `Colon`, base `;`).
fn map_key(key: egui::Key, physical: Option<egui::Key>, shift: bool) -> Option<Key> {
    use egui::Key as K;
    let named = match key {
        K::Enter => Key::Enter,
        K::Tab => Key::Tab,
        K::Backspace => Key::Backspace,
        K::Escape => Key::Escape,
        K::ArrowUp => Key::ArrowUp,
        K::ArrowDown => Key::ArrowDown,
        K::ArrowLeft => Key::ArrowLeft,
        K::ArrowRight => Key::ArrowRight,
        K::Home => Key::Home,
        K::End => Key::End,
        K::PageUp => Key::PageUp,
        K::PageDown => Key::PageDown,
        K::Insert => Key::Insert,
        K::Delete => Key::Delete,
        K::Space => Key::Char(' '),
        _ => {
            let name = key.name();
            if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
                return Some(Key::F(n));
            }
            let c = key_char(key)?;
            let shifted_symbol = matches!(
                key,
                K::Colon
                    | K::Pipe
                    | K::Questionmark
                    | K::Exclamationmark
                    | K::OpenCurlyBracket
                    | K::CloseCurlyBracket
                    | K::Plus
            );
            if shift
                && shifted_symbol
                && let Some(base) = physical.and_then(key_char)
            {
                return Some(Key::Char(base));
            }
            Key::Char(c)
        }
    };
    Some(named)
}

fn key_char(key: egui::Key) -> Option<char> {
    use egui::Key as K;
    Some(match key {
        K::A => 'a',
        K::B => 'b',
        K::C => 'c',
        K::D => 'd',
        K::E => 'e',
        K::F => 'f',
        K::G => 'g',
        K::H => 'h',
        K::I => 'i',
        K::J => 'j',
        K::K => 'k',
        K::L => 'l',
        K::M => 'm',
        K::N => 'n',
        K::O => 'o',
        K::P => 'p',
        K::Q => 'q',
        K::R => 'r',
        K::S => 's',
        K::T => 't',
        K::U => 'u',
        K::V => 'v',
        K::W => 'w',
        K::X => 'x',
        K::Y => 'y',
        K::Z => 'z',
        K::Num0 => '0',
        K::Num1 => '1',
        K::Num2 => '2',
        K::Num3 => '3',
        K::Num4 => '4',
        K::Num5 => '5',
        K::Num6 => '6',
        K::Num7 => '7',
        K::Num8 => '8',
        K::Num9 => '9',
        K::Colon => ':',
        K::Comma => ',',
        K::Backslash => '\\',
        K::Slash => '/',
        K::Pipe => '|',
        K::Questionmark => '?',
        K::Exclamationmark => '!',
        K::OpenBracket => '[',
        K::CloseBracket => ']',
        K::OpenCurlyBracket => '{',
        K::CloseCurlyBracket => '}',
        K::Backtick => '`',
        K::Minus => '-',
        K::Period => '.',
        K::Plus => '+',
        K::Equals => '=',
        K::Semicolon => ';',
        K::Quote => '\'',
        K::Space => ' ',
        _ => return None,
    })
}

fn pointer_button(b: PointerButton) -> Option<MouseButton> {
    match b {
        PointerButton::Primary => Some(MouseButton::Left),
        PointerButton::Middle => Some(MouseButton::Middle),
        PointerButton::Secondary => Some(MouseButton::Right),
        _ => None,
    }
}

impl<'a> TerminalView<'a> {
    pub fn new(term: &'a mut Terminal) -> Self {
        TerminalView {
            term,
            font_size: 14.0,
            id_salt: None,
        }
    }

    pub fn font_size(mut self, points: f32) -> Self {
        self.font_size = points;
        self
    }

    /// Derives the widget's interaction id from `salt` (unique among the views of one id
    /// scope, e.g. a tab uid), so several views drawn in one frame keep their own focus,
    /// pointer and drag state whatever order they are drawn in. Without it the id is the
    /// layout's automatic one.
    pub fn id_salt(mut self, salt: impl Hash) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        salt.hash(&mut hasher);
        self.id_salt = Some(hasher.finish());
        self
    }

    pub fn show(self, ui: &mut Ui) -> TerminalResponse {
        let TerminalView {
            term,
            font_size,
            id_salt,
        } = self;
        let events = term.poll();
        let ctx = ui.ctx().clone();
        let ppp = ctx.pixels_per_point();

        let metrics = match &term.view.metrics {
            Some(m) if m.ppp == ppp && m.font_size == font_size => m.clone(),
            _ => {
                let m = Metrics::compute(&ctx, font_size);
                term.view.metrics = Some(m.clone());
                m
            }
        };

        let sense = Sense::click_and_drag().union(Sense::focusable_noninteractive());
        let (rect, response) = match id_salt {
            None => ui.allocate_exact_size(ui.available_size(), sense),
            Some(salt) => {
                let (_, rect) = ui.allocate_space(ui.available_size());
                let id = ui.make_persistent_id(("deck_term::TerminalView", salt));
                (rect, ui.interact(rect, id, sense))
            }
        };
        let origin = pos2(
            (rect.min.x * ppp).round() / ppp,
            (rect.min.y * ppp).round() / ppp,
        );
        let cols =
            ((((rect.max.x - origin.x) * ppp) / metrics.cell_w_px).floor() as usize).clamp(2, 4096);
        let rows =
            ((((rect.max.y - origin.y) * ppp) / metrics.cell_h_px).floor() as usize).clamp(1, 4096);
        term.resize(
            cols as u16,
            rows as u16,
            metrics.cell_w_px as u16,
            metrics.cell_h_px as u16,
        );
        let (cols, rows) = if term.follows_remote_geometry() {
            let state = term.term.lock();
            (state.columns(), state.screen_lines())
        } else {
            (cols, rows)
        };
        let geo = Geometry {
            origin,
            cell_w: metrics.cell_w,
            cell_h: metrics.cell_h,
            cols,
            rows,
        };

        // Focus.
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        let focused = response.has_focus();
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    response.id,
                    EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }
        term.set_focused(focused);
        let mut mode = term.mode();

        let input_events = ui.input(|i| i.events.clone());
        let mut open_url = None;
        if focused {
            let modifiers = ui.input(|i| i.modifiers);
            handle_keyboard(term, &input_events, modifiers, mode);
            mode = term.mode();
        }
        handle_pointer(
            term,
            &input_events,
            &response,
            rect,
            geo,
            mode,
            &mut open_url,
        );

        // File drops (winit on X11/Windows, the app's bridge on Wayland) paste the paths like
        // Konsole/kitty. The view under the pointer takes them; the focused one when the
        // platform reports no pointer position during the drag.
        let drop = ui.input(|i| {
            let dropped = input::encode_dropped_paths(i.raw.dropped_files.iter().map(|f| f.path()));
            let hovering = !i.raw.hovered_files.is_empty();
            (hovering || !dropped.is_empty())
                .then(|| (i.pointer.hover_pos().is_some(), hovering, dropped))
        });
        let mut drop_hover = false;
        if let Some((has_pos, hovering, dropped)) = drop
            && (if has_pos {
                response.contains_pointer()
            } else {
                focused
            })
        {
            drop_hover = hovering;
            if !dropped.is_empty() && !term.input_held() {
                let mode = term.mode();
                paste(term, Some(dropped), mode);
            }
        }

        // Snapshot under the lock, paint without it.
        {
            let term_guard = term.term.lock();
            let palette = term.palette().clone();
            term.view.frame.capture(&term_guard, &palette);
        }

        // Link hover (Ctrl held): an OSC 8 link, else a plain URL in the text.
        let hover = response.hover_pos().filter(|p| rect.contains(*p));
        let ctrl = ui.input(|i| i.modifiers.ctrl);
        let mut hover_link = 0;
        let mut hover_url = None;
        if let (Some(pos), true) = (hover, ctrl) {
            let (row, col, _) = geo.cell(pos);
            hover_link = term.view.frame.cell(row, col).map_or(0, |c| c.link);
            if hover_link == 0 {
                let t = term.term.lock();
                let offset = t.grid().display_offset();
                let point = viewport_to_point(offset, Point::new(row, Column(col)));
                hover_url = links::url_at(&*t, point).map(|hit| {
                    let vp = |p: Point| (p.line.0 + offset as i32, p.column.0);
                    UrlSpan {
                        start: vp(hit.start),
                        end: vp(hit.end),
                    }
                });
            }
        }
        if response.contains_pointer() {
            ctx.set_cursor_icon(if hover_link != 0 || hover_url.is_some() {
                CursorIcon::PointingHand
            } else {
                CursorIcon::Text
            });
        }

        // Cursor blink: own timer, repaint only while focused and blinking.
        let blinking = focused && term.view.frame.blinking && term.view.frame.cursor.is_some();
        let mut cursor_visible = true;
        if blinking {
            let epoch = *term.view.blink_epoch.get_or_insert_with(Instant::now);
            let elapsed = epoch.elapsed().as_millis();
            let half = BLINK_HALF_PERIOD.as_millis();
            cursor_visible = (elapsed / half).is_multiple_of(2);
            let to_next = half - elapsed % half;
            ctx.request_repaint_after(Duration::from_millis(to_next as u64));
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, term.view.frame.bg);
        let view = &mut term.view;
        let params = PaintParams {
            origin,
            metrics: &metrics,
            cursor_visible,
            focused,
            hover_link,
            hover_url,
            graphics: &term.graphics,
            preedit: &view.preedit,
            banner: term.banner.as_deref(),
        };
        render::paint(
            &view.frame,
            &painter,
            &mut view.glyphs,
            &mut view.scratch,
            &params,
        );
        if drop_hover {
            let accent = ui.visuals().selection.stroke.color;
            painter.rect_stroke(rect, 0.0, Stroke::new(2.0, accent), StrokeKind::Inside);
        }

        if focused {
            let cursor_rect = view.frame.cursor.map_or(
                Rect::from_min_size(origin, egui::vec2(1.0, metrics.cell_h)),
                |c| geo.cell_rect(c.row, c.col),
            );
            ctx.output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    purpose: IMEPurpose::Terminal,
                    rect,
                    cursor_rect,
                    should_interrupt_composition: false,
                })
            });
        }

        TerminalResponse {
            response,
            events,
            open_url,
        }
    }
}

fn after_input(term: &mut Terminal) {
    term.view.blink_epoch = Some(Instant::now());
    term.scroll_keep.cancel();
    let mut t = term.term.lock();
    if t.grid().display_offset() != 0 {
        t.scroll_display(Scroll::Bottom);
    }
}

fn paste(term: &mut Terminal, text: Option<String>, mode: TermMode) {
    let Some(text) = text else { return };
    let bytes = term.paste_encoding(&text, mode);
    if !bytes.is_empty() {
        term.write(&bytes);
        after_input(term);
    }
}

/// Clipboard and link actions behind the app's terminal context menu.
impl Terminal {
    /// Sends this frame's keyboard events (`events`, with `modifiers` the state at the end of
    /// the frame) to the child exactly as [`TerminalView`] does while focused: held input,
    /// win32 modifier records, kitty press/repeat/release reporting, the Ctrl+Shift+C/V and
    /// Shift+Insert clipboard shortcuts, plain Ctrl+C/X/V clipboard events, IME preedit and
    /// commits. For views that draw a TSP surface instead of the grid.
    pub fn route_keyboard(&mut self, events: &[Event], modifiers: Modifiers) {
        let mode = self.mode();
        handle_keyboard(self, events, modifiers, mode);
    }

    /// The IME composition in progress (shown at the caret by whoever draws the input).
    pub fn preedit(&self) -> &str {
        &self.view.preedit
    }

    /// A local (alacritty) selection exists: Shift+drag, or any drag without mouse reporting.
    pub fn has_selection(&self) -> bool {
        self.term
            .lock()
            .selection
            .as_ref()
            .is_some_and(|s| !s.is_empty())
    }

    /// Copies the local selection to the clipboard; false when there is nothing to copy.
    pub fn copy_selection(&self) -> bool {
        let text = self.term.lock().selection_to_string();
        match text.filter(|t| !t.is_empty()) {
            Some(text) => {
                self.clipboard().set_text(&text);
                true
            }
            None => false,
        }
    }

    /// Pastes the clipboard as input (bracketed when the app enabled bracketed paste).
    pub fn paste_clipboard(&mut self) {
        if self.input_held() {
            return;
        }
        let text = self.clipboard().get_text();
        let mode = self.mode();
        paste(self, text, mode);
    }

    /// The exact bytes the widget's own paste sends for `text` under the current modes
    /// (bracketed; filtered like ConPTY's paste while the child speaks win32-input-mode);
    /// `None` while the application has not enabled bracketed paste (?2004). Empty text
    /// sends nothing (`Some` of no bytes).
    pub fn paste_bytes(&self, text: &str) -> Option<Vec<u8>> {
        let mode = self.mode();
        (!self.is_read_only() && mode.contains(TermMode::BRACKETED_PASTE))
            .then(|| self.paste_encoding(text, mode))
    }

    /// Paste encoding shared by every paste path: the active key rules pick the encoder.
    fn paste_encoding(&self, text: &str, mode: TermMode) -> Vec<u8> {
        if text.is_empty() {
            Vec::new()
        } else if self.key_rules() == KeyRules::Win32 {
            input::encode_paste_win32(text, mode)
        } else {
            input::encode_paste(text, mode)
        }
    }

    /// Link (OSC 8 target or plain URL) under the last right click.
    pub fn context_link(&self) -> Option<&str> {
        self.view.menu_link.as_deref()
    }
}

impl Terminal {
    /// Injects one complete key stroke encoded exactly as the same key typed by the user under
    /// the child's current key protocol (kitty CSI-u, win32-input-mode records, tmux,
    /// modifyOtherKeys, legacy), see [`input::encode_stroke`]. Sent at once like
    /// [`Self::write`], never held. Apps must inject keys this way, not as raw VT bytes: once
    /// the outer side has spoken win32-input-mode, ConPTY keeps a trailing raw `ESC` as the
    /// start of a sequence and merges it with the next write (`ESC` + `\r` = Alt+Enter).
    pub fn send_key(&self, key: Key, mods: Mods) {
        let bytes = input::encode_stroke(
            key,
            mods,
            self.mode(),
            self.key_rules(),
            self.view.win32_mods,
        );
        // Kitty CSI-u and win32 records encode Ctrl+C without a 0x03 byte; omp still sees it.
        self.write_input(&bytes, key == Key::Char('c') && mods == Mods::CTRL);
    }
}

/// Link under viewport cell (`row`, `col`): its OSC 8 target, else a plain URL in the text.
fn link_at(term: &Terminal, row: usize, col: usize) -> Option<String> {
    let t = term.term.lock();
    let point = viewport_to_point(t.grid().display_offset(), Point::new(row, Column(col)));
    t.grid()[point]
        .hyperlink()
        .map(|h| h.uri().to_owned())
        .or_else(|| links::url_at(&*t, point).map(|hit| hit.uri))
}

/// win32-input-mode: sends modifier key records for the changes since the child last saw
/// the modifiers.
fn sync_win32_mods(term: &mut Terminal, now: Mods) {
    let bytes = input::encode_win32_modifiers(term.view.win32_mods, now);
    term.view.win32_mods = now;
    if !bytes.is_empty() {
        term.write(&bytes);
    }
}

/// `modifiers`: the state at the end of this frame's events.
fn handle_keyboard(term: &mut Terminal, events: &[Event], modifiers: Modifiers, mode: TermMode) {
    let rules = term.key_rules();
    let win32 = rules == KeyRules::Win32;
    if !win32 {
        term.view.win32_mods = Mods::NONE;
    }
    let held = term.input_held();
    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            Event::Key {
                key,
                physical_key,
                pressed,
                repeat,
                modifiers,
            } => {
                // Text produced by this key press directly follows it.
                let text = match (pressed, events.get(i + 1)) {
                    (true, Some(Event::Text(t))) => {
                        i += 1;
                        Some(t.as_str())
                    }
                    _ => None,
                };
                if held {
                    // Everything is swallowed. Only dropped fresh presses are remembered (their
                    // releases are swallowed too); repeats of a key pressed before the hold
                    // leave no state, so its release still reaches the child. A release within
                    // the hold ends the bookkeeping, so a later press/release pair is delivered.
                    if *pressed {
                        if !*repeat && !term.view.held_keys.contains(key) {
                            term.view.held_keys.push(*key);
                        }
                    } else if let Some(pos) = term.view.held_keys.iter().position(|k| k == key) {
                        term.view.held_keys.swap_remove(pos);
                    }
                    i += 1;
                    continue;
                }
                if !*pressed && let Some(pos) = term.view.held_keys.iter().position(|k| k == key) {
                    term.view.held_keys.swap_remove(pos);
                    i += 1;
                    continue;
                }
                let m = *modifiers;
                if win32 {
                    sync_win32_mods(term, mods_of(m));
                }
                let copy = m.ctrl && m.shift && !m.alt && *key == egui::Key::C;
                let paste_key = (m.ctrl && m.shift && !m.alt && *key == egui::Key::V)
                    || (m.shift && !m.ctrl && !m.alt && *key == egui::Key::Insert);
                // Local shortcuts act on press (auto-repeat repeats them, as in VTE/Konsole).
                // Their releases are swallowed too — also when a modifier was let go first —
                // so kitty event reporting never sees half of the combo.
                if *pressed && (copy || paste_key) {
                    term.view.shortcut_key = Some(*key);
                    if copy {
                        term.copy_selection();
                    } else {
                        term.paste_clipboard();
                    }
                } else if !*pressed && (copy || paste_key || term.view.shortcut_key == Some(*key)) {
                    if term.view.shortcut_key == Some(*key) {
                        term.view.shortcut_key = None;
                    }
                } else if let Some(k) = map_key(*key, *physical_key, m.shift) {
                    let action = match (pressed, repeat) {
                        (false, _) => KeyAction::Release,
                        (true, true) => KeyAction::Repeat,
                        (true, false) => KeyAction::Press,
                    };
                    let mods = mods_of(m);
                    let bytes = input::encode_key(
                        &KeyInput {
                            key: k,
                            mods,
                            text,
                            action,
                        },
                        mode,
                        rules,
                    );
                    if !bytes.is_empty() {
                        // Kitty CSI-u and win32 records encode Ctrl+C without a 0x03 byte;
                        // omp still sees it.
                        term.write_input(
                            &bytes,
                            k == Key::Char('c') && mods == Mods::CTRL && *pressed,
                        );
                        after_input(term);
                    }
                } else if let Some(t) = text {
                    term.write(&input::encode_text(t));
                    after_input(term);
                }
            }
            Event::Text(t) if !held => {
                term.write(&input::encode_text(t));
                after_input(term);
            }
            // Only produced when the app left egui's clipboard shortcuts on: these are the
            // plain Ctrl+C/X/V presses, which belong to the PTY.
            Event::Copy | Event::Cut | Event::Paste(_) if !held => {
                let c = match &events[i] {
                    Event::Copy => 'c',
                    Event::Cut => 'x',
                    _ => 'v',
                };
                if win32 {
                    sync_win32_mods(term, Mods::CTRL);
                }
                let bytes =
                    input::encode_key(&KeyInput::press(Key::Char(c), Mods::CTRL), mode, rules);
                term.write_input(&bytes, c == 'c');
                after_input(term);
            }
            Event::Ime(ImeEvent::Preedit { text, .. }) => {
                term.view.preedit.clone_from(text);
            }
            Event::Ime(ImeEvent::Commit(text)) => {
                term.view.preedit.clear();
                if !text.is_empty() && !held {
                    term.write(&input::encode_text(text));
                    after_input(term);
                }
            }
            _ => {}
        }
        i += 1;
    }
    if win32 && !held {
        sync_win32_mods(term, mods_of(modifiers));
    }
}

fn handle_pointer(
    term: &mut Terminal,
    events: &[Event],
    response: &egui::Response,
    rect: Rect,
    geo: Geometry,
    mode: TermMode,
    open_url: &mut Option<String>,
) {
    let over = response.contains_pointer();
    let mouse_mode = mode.intersects(TermMode::MOUSE_MODE);
    for ev in events {
        match ev {
            Event::PointerButton {
                pos,
                button,
                pressed: true,
                modifiers,
            } => {
                if !(over && rect.contains(*pos)) {
                    continue;
                }
                let Some(btn) = pointer_button(*button) else {
                    continue;
                };
                response.request_focus();
                let mods = mods_of(*modifiers);
                let (row, col, side) = geo.cell(*pos);

                if btn == MouseButton::Left
                    && mods.ctrl
                    && !mods.shift
                    && let Some(uri) = link_at(term, row, col)
                {
                    *open_url = Some(uri);
                    term.view.link_press = true;
                    continue;
                }
                // Right click belongs to the app's context menu (egui opens it on release), never
                // to the PTY: remember the link under it for Open/Copy link.
                if btn == MouseButton::Right {
                    term.view.menu_link = link_at(term, row, col);
                    continue;
                }

                if mouse_mode && !mods.shift {
                    term.view.pressed = Some(Pressed {
                        button: btn,
                        reporting: true,
                    });
                    term.view.last_report_cell = Some((row, col));
                    term.write(&input::encode_mouse(
                        btn,
                        MouseAction::Press,
                        mods,
                        col,
                        row,
                        mode,
                    ));
                    continue;
                }
                term.view.pressed = Some(Pressed {
                    button: btn,
                    reporting: false,
                });
                match btn {
                    MouseButton::Left => {
                        let now = response.ctx.input(|i| i.time);
                        let count = match term.view.last_click {
                            Some((t, r, c))
                                if now - t < MULTI_CLICK_SECS && (r, c) == (row, col) =>
                            {
                                term.view.click_count % 3 + 1
                            }
                            _ => 1,
                        };
                        term.view.click_count = count;
                        term.view.last_click = Some((now, row, col));
                        let mut t = term.term.lock();
                        let point = viewport_to_point(
                            t.grid().display_offset(),
                            Point::new(row, Column(col)),
                        );
                        term.view.selecting = true;
                        match count {
                            1 => {
                                t.selection = None;
                                term.view.pending_select = Some((point, side));
                            }
                            2 => {
                                t.selection =
                                    Some(Selection::new(SelectionType::Semantic, point, side));
                                term.view.pending_select = None;
                            }
                            _ => {
                                t.selection =
                                    Some(Selection::new(SelectionType::Lines, point, side));
                                term.view.pending_select = None;
                            }
                        }
                    }
                    // A paste, held like keyboard input (no clipboard read either).
                    MouseButton::Middle if !term.input_held() => {
                        let text = term.clipboard().get_primary();
                        paste(term, text, mode);
                    }
                    _ => {}
                }
            }
            Event::PointerButton {
                pos,
                button,
                pressed: false,
                modifiers,
            } => {
                let Some(btn) = pointer_button(*button) else {
                    continue;
                };
                if btn == MouseButton::Left && term.view.link_press {
                    term.view.link_press = false;
                    continue;
                }
                let Some(pressed) = term.view.pressed.filter(|p| p.button == btn) else {
                    continue;
                };
                term.view.pressed = None;
                let (row, col, _) = geo.cell(*pos);
                if pressed.reporting {
                    if mode.intersects(TermMode::MOUSE_MODE) {
                        let mods = mods_of(*modifiers);
                        term.write(&input::encode_mouse(
                            btn,
                            MouseAction::Release,
                            mods,
                            col,
                            row,
                            mode,
                        ));
                    }
                } else if btn == MouseButton::Left && term.view.selecting {
                    term.view.selecting = false;
                    term.view.pending_select = None;
                    let text = term.term.lock().selection_to_string();
                    if let Some(text) = text.filter(|t| !t.is_empty()) {
                        term.clipboard().set_primary(&text);
                    }
                }
            }
            Event::PointerMoved(pos) => {
                let (row, col, side) = geo.cell(*pos);
                match term.view.pressed {
                    Some(Pressed {
                        button,
                        reporting: true,
                    }) => {
                        if input::mouse_report_wanted(MouseAction::Motion, true, mode)
                            && term.view.last_report_cell != Some((row, col))
                        {
                            term.view.last_report_cell = Some((row, col));
                            let mods = mods_of(response.ctx.input(|i| i.modifiers));
                            term.write(&input::encode_mouse(
                                button,
                                MouseAction::Motion,
                                mods,
                                col,
                                row,
                                mode,
                            ));
                        }
                    }
                    Some(Pressed {
                        button: MouseButton::Left,
                        reporting: false,
                    }) if term.view.selecting => {
                        let mut t = term.term.lock();
                        // Drag past the edges scrolls the local scrollback.
                        if pos.y < rect.top() {
                            term.scroll_keep.cancel();
                            t.scroll_display(Scroll::Delta(1));
                        } else if pos.y > rect.bottom() {
                            term.scroll_keep.cancel();
                            t.scroll_display(Scroll::Delta(-1));
                        }
                        let point = viewport_to_point(
                            t.grid().display_offset(),
                            Point::new(row, Column(col)),
                        );
                        if let Some((start, start_side)) = term.view.pending_select.take() {
                            t.selection =
                                Some(Selection::new(SelectionType::Simple, start, start_side));
                        }
                        if let Some(sel) = t.selection.as_mut() {
                            sel.update(point, side);
                        }
                    }
                    None if over
                        && rect.contains(*pos)
                        && input::mouse_report_wanted(MouseAction::Motion, false, mode)
                        && term.view.last_report_cell != Some((row, col)) =>
                    {
                        term.view.last_report_cell = Some((row, col));
                        let mods = mods_of(response.ctx.input(|i| i.modifiers));
                        term.write(&input::encode_mouse(
                            MouseButton::NoButton,
                            MouseAction::Motion,
                            mods,
                            col,
                            row,
                            mode,
                        ));
                    }
                    _ => {}
                }
            }
            Event::MouseWheel {
                unit,
                delta,
                modifiers,
                ..
            } => {
                if !over {
                    continue;
                }
                let (lines, notch) = match unit {
                    MouseWheelUnit::Line => (delta.y, true),
                    MouseWheelUnit::Point => (delta.y / geo.cell_h, false),
                    MouseWheelUnit::Page => (delta.y * geo.rows as f32, false),
                };
                term.view.wheel_accum += lines;
                let steps = term.view.wheel_accum.trunc();
                term.view.wheel_accum -= steps;
                let steps = steps as i32;
                if steps == 0 {
                    continue;
                }
                let mods = mods_of(*modifiers);
                let pos = response.hover_pos().unwrap_or(geo.origin);
                let (row, col, _) = geo.cell(pos);
                if mouse_mode && !mods.shift {
                    let btn = if steps > 0 {
                        MouseButton::WheelUp
                    } else {
                        MouseButton::WheelDown
                    };
                    let report = input::encode_mouse(btn, MouseAction::Press, mods, col, row, mode);
                    for _ in 0..steps.unsigned_abs() {
                        term.write(&report);
                    }
                } else if input::wants_alt_scroll(mode) && !mods.shift {
                    let per = if notch { 3 } else { 1 };
                    term.write(&input::encode_alt_scroll(steps * per, mode));
                } else {
                    let per = if notch { 3 } else { 1 };
                    term.scroll_keep.cancel();
                    term.term.lock().scroll_display(Scroll::Delta(steps * per));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use egui::{Context, RawInput, UiBuilder, vec2};

    use super::*;
    use crate::remote::tests::connect;

    const LEFT: Pos2 = pos2(100.0, 100.0);
    const RIGHT: Pos2 = pos2(500.0, 100.0);

    /// Two side-by-side views in one frame (`a` left, `b` right), drawn in the given order.
    struct Split {
        ctx: Context,
        time: f64,
        salted: bool,
    }

    impl Split {
        fn frame(&mut self, events: Vec<Event>, a: &mut Terminal, b: &mut Terminal, b_first: bool) {
            self.time += 0.1;
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 400.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let salted = self.salted;
            let output = self.ctx.run_ui(input, |ui| {
                let full = ui.max_rect();
                let left = Rect::from_min_max(full.min, pos2(full.center().x, full.max.y));
                let right = Rect::from_min_max(pos2(full.center().x, full.min.y), full.max);
                let mut draw = |term: &mut Terminal, rect: Rect, uid: u64| {
                    let mut child = ui.new_child(UiBuilder::new().max_rect(rect));
                    let view = TerminalView::new(term);
                    let view = if salted { view.id_salt(uid) } else { view };
                    view.show(&mut child);
                };
                if b_first {
                    draw(b, right, 2);
                    draw(a, left, 1);
                } else {
                    draw(a, left, 1);
                    draw(b, right, 2);
                }
            });
            output.drop_without_applying_deltas();
        }

        /// Press and release the primary button at `pos` over two frames.
        fn click(&mut self, pos: Pos2, a: &mut Terminal, b: &mut Terminal, b_first: bool) {
            let button = |pressed| Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            self.frame(vec![Event::PointerMoved(pos), button(true)], a, b, b_first);
            self.frame(vec![button(false)], a, b, b_first);
        }
    }

    fn split(salted: bool) -> Split {
        Split {
            ctx: Context::default(),
            time: 0.0,
            salted,
        }
    }

    #[test]
    fn salted_views_keep_their_focus_whatever_the_draw_order() {
        let (mut a, _ha, _la) = connect();
        let (mut b, _hb, _lb) = connect();
        let mut s = split(true);
        s.frame(Vec::new(), &mut a, &mut b, false);
        s.click(LEFT, &mut a, &mut b, false);
        for b_first in [true, false, true, true, false] {
            s.frame(Vec::new(), &mut a, &mut b, b_first);
            assert!(a.view.focused && !b.view.focused, "b_first={b_first}");
        }
        // Clicking the other view moves focus there, drawn in either order.
        s.click(RIGHT, &mut a, &mut b, true);
        for b_first in [false, true, false] {
            s.frame(Vec::new(), &mut a, &mut b, b_first);
            assert!(b.view.focused && !a.view.focused, "b_first={b_first}");
        }
    }

    #[test]
    fn unsalted_view_ids_follow_draw_order() {
        // The case the salt exists for: automatic ids belong to the draw slot, so swapping
        // the order hands the focus to the other terminal.
        let (mut a, _ha, _la) = connect();
        let (mut b, _hb, _lb) = connect();
        let mut s = split(false);
        s.frame(Vec::new(), &mut a, &mut b, false);
        s.click(LEFT, &mut a, &mut b, false);
        s.frame(Vec::new(), &mut a, &mut b, false);
        assert!(a.view.focused && !b.view.focused);
        s.frame(Vec::new(), &mut a, &mut b, true);
        assert!(b.view.focused && !a.view.focused);
    }
}
