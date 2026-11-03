//! Selection and clipboard behaviour of the chat view.
//!
//! **Transcript text** (prose, code, tool output, diffs) is selected by the view's own model
//! (`tsel.rs`): a position is (node pre-order index, run, char), so a selection can span blocks the
//! virtualized transcript never lays out, and a copy renders the selected items invisibly to read
//! their text. This module only decides when a copy chord belongs to that selection.
//!
//! **Composer text** is selected by the view itself ([`ComposerSel`]): omp's `anchor` is emitted for
//! vim's visual mode only, so the selection lives in the terminal. Acting on it goes back to omp as
//! `edit` events (replace `[from, to)`, caret after the new text) when its hello advertises `edit`;
//! without `edit` the selection is local and only copies, and typing simply collapses it.
//!
//! **What Ctrl+C does** (decided once, here): with a selection (transcript or composer) it copies
//! and clears it, never reaching omp; without one it is the plain `0x03` omp expects (interrupt /
//! clear the draft). Ctrl+Shift+C always copies the selection and never interrupts. Shift+arrows,
//! Home/End and every other key stay omp's: they clear a composer selection and pass through.

use egui::{Event, ImeEvent, Key, Modifiers};

use super::dock::{char_to_utf16, utf16_len};
use super::events;

// ---- composer selection ---------------------------------------------------------------------

/// A selection in the composer's text, in char indices of the text it was made on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerSel {
    /// The `editor` node.
    pub node: String,
    /// The text the indices refer to; the selection ends when omp's text is another.
    pub text: String,
    pub anchor: usize,
    /// The moving end (where omp's caret goes).
    pub head: usize,
}

impl ComposerSel {
    pub fn new(node: &str, text: &str, anchor: usize, head: usize) -> ComposerSel {
        let n = text.chars().count();
        ComposerSel {
            node: node.to_owned(),
            text: text.to_owned(),
            anchor: anchor.min(n),
            head: head.min(n),
        }
    }

    /// Ordered `(from, to)` char range.
    pub fn range(&self) -> (usize, usize) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Whether the selection still stands on `node`'s `text`.
    pub fn is_for(&self, node: &str, text: &str) -> bool {
        self.node == node && self.text == text
    }

    /// The selected text.
    pub fn selected(&self) -> String {
        let (a, b) = self.range();
        self.text.chars().skip(a).take(b - a).collect()
    }

    /// The selection as UTF-16 offsets `(from, to)` (what omp's events carry).
    pub fn utf16(&self) -> (u32, u32) {
        let (a, b) = self.range();
        (
            char_to_utf16(&self.text, a) as u32,
            char_to_utf16(&self.text, b) as u32,
        )
    }

    /// The `edit` that replaces the selection with `with`, the caret after it.
    pub fn replace(&self, sf: &str, with: &str) -> TspEvent {
        let (from, to) = self.utf16();
        events::edit(
            sf,
            &self.node,
            from,
            to,
            with,
            from + utf16_len(with) as u32,
            utf16_len(&self.text) as u32,
        )
    }
}

pub use deck_tsp::wire::Event as TspEvent;

/// A pure caret move to char index `at` of `text` (selection collapsed).
pub fn caret_edit(sf: &str, node: &str, text: &str, at: usize) -> TspEvent {
    let u = char_to_utf16(text, at) as u32;
    events::edit(sf, node, u, u, "", u, utf16_len(text) as u32)
}

/// `(from, to)` of the word at char index `at` (a run of letters/digits/`_`, or of other
/// non-space characters, or of spaces); the whole text when empty is impossible, `(0, 0)`.
pub fn word_at(text: &str, at: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return (0, 0);
    }
    let i = at.min(chars.len() - 1);
    // A click after the last character selects the last word.
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            0
        } else if c.is_whitespace() {
            1
        } else {
            2
        }
    };
    let k = class(chars[i]);
    let mut a = i;
    while a > 0 && class(chars[a - 1]) == k {
        a -= 1;
    }
    let mut b = i + 1;
    while b < chars.len() && class(chars[b]) == k {
        b += 1;
    }
    (a, b)
}

/// `(from, to)` of the logical line (between newlines) holding char index `at`.
pub fn line_at(text: &str, at: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let at = at.min(chars.len());
    let from = chars[..at]
        .iter()
        .rposition(|c| *c == '\n')
        .map_or(0, |i| i + 1);
    let to = chars[at..]
        .iter()
        .position(|c| *c == '\n')
        .map_or(chars.len(), |i| at + i);
    (from, to)
}

// ---- keyboard -------------------------------------------------------------------------------

/// What the keyboard pre-pass decided for one frame.
#[derive(Debug, Default)]
pub struct Plan {
    /// Events for omp (`edit`), sent before the frame's key bytes.
    pub send: Vec<TspEvent>,
    /// The events omp's keyboard routing still sees.
    pub keys: Vec<Event>,
    /// Text for the clipboard (a composer selection copied or cut).
    pub copy: Option<String>,
    /// A copy was asked for while the transcript holds a selection (`tsel::copy` builds the text).
    pub copy_transcript: bool,
    /// Esc with a transcript selection: drop it (the Esc is not forwarded to omp).
    pub clear_transcript: bool,
    /// Paste the clipboard at omp's caret (after `send` removed the selection it replaces).
    pub paste: bool,
}

/// Whether `modifiers` leave a key press plain text input (shift allowed).
fn plain(m: Modifiers) -> bool {
    !(m.ctrl || m.alt || m.command || m.mac_cmd)
}

/// A modifier (or lock) key on its own: it moves nothing and types nothing, so it must not end a
/// selection the user is about to type over (Shift for a capital).
fn is_modifier(key: Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::AltLeft
            | Key::AltRight
            | Key::SuperLeft
            | Key::SuperRight
            | Key::CapsLock
            | Key::NumLock
    )
}

/// The bare clipboard chord `key` (Ctrl, or Cmd on a Mac; no Shift/Alt): the terminal's own
/// Ctrl+C/X/V, which the app does not rewrite into `Event::Copy/Cut/Paste` while the chat has
/// focus. It belongs to the selection only while one is live; otherwise omp gets the control byte.
fn is_chord(key: Key, m: Modifiers, want: Key) -> bool {
    key == want && m.command && !m.shift && !m.alt
}

/// Takes the key press for the selection: its release is dropped too (once).
pub(super) fn swallow(swallowed: &mut Vec<Key>, key: Key) {
    if !swallowed.contains(&key) {
        swallowed.push(key);
    }
}

fn is_copy_chord(key: Key, m: Modifiers) -> bool {
    key == Key::C && m.ctrl && m.shift && !m.alt
}

fn is_paste_chord(key: Key, m: Modifiers) -> bool {
    (key == Key::V && m.ctrl && m.shift && !m.alt)
        || (key == Key::Insert && m.shift && !m.ctrl && !m.alt)
}

/// Runs the frame's `events` past the selections: copies, cuts and replacements are taken here,
/// everything else is handed on.
///
/// * `sel`: the composer selection (cleared by any key that is not copy/cut/paste/typing);
/// * `can_edit`: omp's hello advertises `edit`;
/// * `transcript`: the transcript holds a selection;
/// * `swallowed`: keys whose press was taken last frame (their releases are dropped too).
pub fn plan(
    events: &[Event],
    sel: &mut Option<ComposerSel>,
    sf: &str,
    can_edit: bool,
    transcript: bool,
    swallowed: &mut Vec<Key>,
) -> Plan {
    let mut out = Plan::default();
    let mut i = 0;
    while i < events.len() {
        let live = sel.as_ref().filter(|s| !s.is_empty()).cloned();
        let ev = &events[i];
        i += 1;
        match ev {
            Event::Key { key, .. } if is_modifier(*key) => out.keys.push(ev.clone()),
            Event::Copy if live.is_some() => {
                out.copy = live.map(|s| s.selected());
                *sel = None;
            }
            Event::Copy if transcript => out.copy_transcript = true,
            Event::Key {
                key: Key::Escape,
                pressed: true,
                modifiers,
                ..
            } if transcript && live.is_none() && !modifiers.any() => {
                // The selection ends; whether this Esc is omp's too (it interrupts a running
                // turn) is for the caller to decide.
                out.clear_transcript = true;
                out.keys.push(ev.clone());
            }
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if is_chord(*key, *modifiers, Key::C) && (live.is_some() || transcript) => {
                match live {
                    Some(s) => {
                        out.copy = Some(s.selected());
                        *sel = None;
                    }
                    None => out.copy_transcript = true,
                }
                swallow(swallowed, *key);
            }
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if is_chord(*key, *modifiers, Key::X) && live.is_some() && can_edit => {
                if let Some(s) = live {
                    out.copy = Some(s.selected());
                    out.send.push(s.replace(sf, ""));
                }
                *sel = None;
                swallow(swallowed, *key);
            }
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if is_chord(*key, *modifiers, Key::V) && live.is_some() && can_edit => {
                if let Some(s) = live {
                    out.send.push(s.replace(sf, ""));
                }
                *sel = None;
                out.paste = true;
                swallow(swallowed, *key);
            }
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if is_copy_chord(*key, *modifiers) => {
                match live {
                    Some(s) => {
                        out.copy = Some(s.selected());
                        *sel = None;
                    }
                    None => out.copy_transcript = transcript,
                }
                // The routing meets its own copy shortcut here (no grid selection to copy).
                out.keys.push(ev.clone());
            }
            Event::Cut if live.is_some() && can_edit => {
                if let Some(s) = live {
                    out.copy = Some(s.selected());
                    out.send.push(s.replace(sf, ""));
                }
                *sel = None;
            }
            Event::Paste(text) if live.is_some() && can_edit => {
                if let Some(s) = live {
                    out.send.push(s.replace(sf, text));
                }
                *sel = None;
            }
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if is_paste_chord(*key, *modifiers) => {
                // The routing pastes the clipboard at omp's caret: the selection goes first.
                if let Some(s) = live.filter(|_| can_edit) {
                    out.send.push(s.replace(sf, ""));
                }
                *sel = None;
                out.keys.push(ev.clone());
            }
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if live.is_some() && can_edit && plain(*modifiers) => {
                // Typing (the key press is directly followed by its text) or deleting replaces
                // the selection.
                let typed = match events.get(i) {
                    Some(Event::Text(t)) if !t.is_empty() => Some(t.as_str()),
                    _ => None,
                };
                let replacement = match (key, typed) {
                    (Key::Backspace | Key::Delete, _) => Some(""),
                    (_, Some(t)) => Some(t),
                    _ => None,
                };
                match (live, replacement) {
                    (Some(s), Some(with)) => {
                        out.send.push(s.replace(sf, with));
                        *sel = None;
                        swallow(swallowed, *key);
                        if typed.is_some() && !matches!(key, Key::Backspace | Key::Delete) {
                            i += 1;
                        }
                    }
                    _ => {
                        // Any other key moves omp's caret or leaves the field.
                        *sel = None;
                        out.keys.push(ev.clone());
                    }
                }
            }
            Event::Key { pressed: true, .. } => {
                if live.is_some() {
                    *sel = None;
                }
                out.keys.push(ev.clone());
            }
            Event::Key {
                key,
                pressed: false,
                ..
            } => {
                if let Some(p) = swallowed.iter().position(|k| k == key) {
                    swallowed.swap_remove(p);
                } else {
                    out.keys.push(ev.clone());
                }
            }
            Event::Text(t) if live.is_some() && can_edit && !t.is_empty() => {
                if let Some(s) = live {
                    out.send.push(s.replace(sf, t));
                }
                *sel = None;
            }
            Event::Ime(ImeEvent::Commit(t)) if live.is_some() && can_edit && !t.is_empty() => {
                if let Some(s) = live {
                    out.send.push(s.replace(sf, t));
                }
                *sel = None;
                // The routing still ends the composition (an empty commit sends nothing).
                out.keys.push(Event::Ime(ImeEvent::Commit(String::new())));
            }
            Event::Text(_) | Event::Ime(ImeEvent::Commit(_)) if live.is_some() => {
                *sel = None;
                out.keys.push(ev.clone());
            }
            other => out.keys.push(other.clone()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key, mods: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: mods,
        }
    }

    fn release(k: Key) -> Event {
        Event::Key {
            key: k,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn ctrl_shift() -> Modifiers {
        Modifiers {
            ctrl: true,
            shift: true,
            command: false,
            ..Modifiers::NONE
        }
    }

    fn sel(text: &str, a: usize, b: usize) -> Option<ComposerSel> {
        Some(ComposerSel::new("ed", text, a, b))
    }

    fn json(e: &TspEvent) -> serde_json::Value {
        serde_json::to_value(e).expect("event JSON")
    }

    #[test]
    fn drag_ranges_map_to_utf16_offsets_across_astral_characters() {
        // "a😀bc": the emoji is two UTF-16 units; chars 1..3 are "😀b".
        let s = ComposerSel::new("ed", "a😀bc", 3, 1);
        assert_eq!(s.range(), (1, 3), "a backwards drag is ordered");
        assert_eq!(s.selected(), "😀b");
        assert_eq!(s.utf16(), (1, 4));
        assert_eq!(
            json(&s.replace("s:1", "XY")),
            serde_json::json!({"ev":"edit","sf":"s:1","id":"ed","from":1,"to":4,"text":"XY","cursor":3,"len":5})
        );
        // Indices past the end clamp to the text.
        let s = ComposerSel::new("ed", "ab", 9, 0);
        assert_eq!(s.range(), (0, 2));
    }

    #[test]
    fn a_collapsed_caret_move_is_an_edit_with_no_text() {
        assert_eq!(
            json(&caret_edit("s:1", "ed", "a😀bc", 2)),
            serde_json::json!({"ev":"edit","sf":"s:1","id":"ed","from":3,"to":3,"text":"","cursor":3,"len":5})
        );
    }

    #[test]
    fn double_click_picks_the_word_the_punctuation_run_or_the_space() {
        let t = "foo_bar  (baz)";
        assert_eq!(word_at(t, 1), (0, 7));
        assert_eq!(word_at(t, 7), (7, 9));
        assert_eq!(word_at(t, 9), (9, 10));
        assert_eq!(word_at(t, 11), (10, 13));
        // At or after the end: the last unit.
        assert_eq!(word_at(t, 99), (13, 14));
        assert_eq!(word_at("", 0), (0, 0));
    }

    #[test]
    fn triple_click_takes_the_logical_line() {
        let t = "one\ntwo words\n\nlast";
        assert_eq!(line_at(t, 5), (4, 13));
        assert_eq!(line_at(t, 3), (0, 3), "at the newline: the line before it");
        assert_eq!(line_at(t, 14), (14, 14), "an empty line");
        assert_eq!(line_at(t, 99), (15, 19));
    }

    #[test]
    fn typing_over_a_selection_becomes_one_replacing_edit_and_the_keys_never_reach_omp() {
        let mut s = sel("hello world", 0, 5);
        let mut swallowed = Vec::new();
        let events = [
            key(Key::X, Modifiers::NONE),
            Event::Text("x".into()),
            release(Key::X),
        ];
        let p = plan(&events, &mut s, "s:1", true, false, &mut swallowed);
        assert_eq!(p.send.len(), 1);
        assert_eq!(
            json(&p.send[0]),
            serde_json::json!({"ev":"edit","sf":"s:1","id":"ed","from":0,"to":5,"text":"x","cursor":1,"len":11})
        );
        assert!(p.keys.is_empty(), "{:?}", p.keys);
        assert!(s.is_none());
        assert!(
            swallowed.is_empty(),
            "the release in the same frame is dropped"
        );
    }

    #[test]
    fn a_release_in_a_later_frame_is_dropped_with_its_swallowed_press() {
        let mut s = sel("hello", 1, 3);
        let mut swallowed = Vec::new();
        let p = plan(
            &[key(Key::Backspace, Modifiers::NONE)],
            &mut s,
            "s:1",
            true,
            false,
            &mut swallowed,
        );
        assert_eq!(
            json(&p.send[0]),
            serde_json::json!({"ev":"edit","sf":"s:1","id":"ed","from":1,"to":3,"text":"","cursor":1,"len":5})
        );
        assert_eq!(swallowed, [Key::Backspace]);
        let p = plan(
            &[release(Key::Backspace)],
            &mut s,
            "s:1",
            true,
            false,
            &mut swallowed,
        );
        assert!(p.keys.is_empty() && swallowed.is_empty());
    }

    #[test]
    fn without_edit_support_typing_collapses_the_selection_and_goes_to_omp() {
        let mut s = sel("hello", 0, 3);
        let events = [key(Key::A, Modifiers::NONE), Event::Text("a".into())];
        let p = plan(&events, &mut s, "s:1", false, false, &mut Vec::new());
        assert!(p.send.is_empty());
        assert_eq!(p.keys.len(), 2);
        assert!(s.is_none());
    }

    #[test]
    fn ctrl_c_copies_a_selection_and_interrupts_only_without_one() {
        // Composer selection: copied, cleared, the Copy event is not handed to omp.
        let mut s = sel("hello world", 6, 11);
        let p = plan(&[Event::Copy], &mut s, "s:1", true, false, &mut Vec::new());
        assert_eq!(p.copy.as_deref(), Some("world"));
        assert!(p.keys.is_empty() && s.is_none() && !p.copy_transcript);
        // Transcript selection: the transcript copies it; omp does not see Ctrl+C.
        let mut none = None;
        let p = plan(
            &[Event::Copy],
            &mut none,
            "s:1",
            true,
            true,
            &mut Vec::new(),
        );
        assert!(p.copy.is_none() && p.copy_transcript && p.keys.is_empty());
        // Nothing selected: the Copy event stays for omp (the plain 0x03).
        let p = plan(
            &[Event::Copy],
            &mut none,
            "s:1",
            true,
            false,
            &mut Vec::new(),
        );
        assert_eq!(p.keys, [Event::Copy]);
        assert!(p.copy.is_none() && !p.copy_transcript);
    }

    fn ctrl() -> Modifiers {
        // What the app's input sees for Ctrl on Linux/Windows.
        Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::NONE
        }
    }

    #[test]
    fn the_real_ctrl_chords_act_on_a_live_selection_and_swallow_press_and_release() {
        // Ctrl+C on the composer selection.
        let mut s = sel("hello world", 6, 11);
        let mut swallowed = Vec::new();
        let p = plan(
            &[key(Key::C, ctrl()), release(Key::C)],
            &mut s,
            "s:1",
            true,
            false,
            &mut swallowed,
        );
        assert_eq!(p.copy.as_deref(), Some("world"));
        assert!(p.keys.is_empty() && s.is_none() && swallowed.is_empty());
        // Ctrl+C on a transcript selection: the transcript copies it, omp sees no key at all; the
        // release of a later frame is dropped too.
        let mut none = None;
        let p = plan(
            &[key(Key::C, ctrl())],
            &mut none,
            "s:1",
            true,
            true,
            &mut swallowed,
        );
        assert!(p.copy_transcript && p.copy.is_none() && p.keys.is_empty());
        assert_eq!(swallowed, [Key::C]);
        let p = plan(
            &[release(Key::C)],
            &mut none,
            "s:1",
            true,
            false,
            &mut swallowed,
        );
        assert!(p.keys.is_empty() && swallowed.is_empty());
        // A held key repeats its press: still one release to drop.
        plan(
            &[key(Key::C, ctrl()), key(Key::C, ctrl())],
            &mut none,
            "s:1",
            true,
            true,
            &mut swallowed,
        );
        assert_eq!(swallowed, [Key::C]);
        // Ctrl+X cuts the composer selection.
        let mut s = sel("one two", 4, 7);
        let p = plan(
            &[key(Key::X, ctrl()), release(Key::X)],
            &mut s,
            "s:1",
            true,
            false,
            &mut Vec::new(),
        );
        assert_eq!(p.copy.as_deref(), Some("two"));
        assert_eq!(
            json(&p.send[0]),
            serde_json::json!({"ev":"edit","sf":"s:1","id":"ed","from":4,"to":7,"text":"","cursor":4,"len":7})
        );
        assert!(p.keys.is_empty() && !p.paste);
        // Ctrl+V replaces the selection with the clipboard: delete it, then paste at the caret.
        let mut s = sel("one two", 0, 3);
        let p = plan(
            &[key(Key::V, ctrl()), release(Key::V)],
            &mut s,
            "s:1",
            true,
            false,
            &mut Vec::new(),
        );
        assert_eq!(p.send.len(), 1);
        assert!(p.paste && p.keys.is_empty());
    }

    #[test]
    fn the_real_ctrl_chords_without_a_selection_reach_omp_untouched() {
        let mut none = None;
        for k in [Key::C, Key::X, Key::V] {
            let events = [key(k, ctrl()), release(k)];
            let p = plan(&events, &mut none, "s:1", true, false, &mut Vec::new());
            assert_eq!(p.keys, events, "{k:?}");
            assert!(p.copy.is_none() && p.send.is_empty() && !p.copy_transcript && !p.paste);
        }
        // A selection that omp cannot edit is not cut or replaced either.
        let mut s = sel("abc", 0, 2);
        let p = plan(
            &[key(Key::X, ctrl())],
            &mut s,
            "s:1",
            false,
            false,
            &mut Vec::new(),
        );
        assert!(p.copy.is_none() && p.send.is_empty());
        assert_eq!(p.keys.len(), 1);
        // Ctrl+Alt+C is somebody else's chord.
        let mut s = sel("abc", 0, 2);
        let alt = Modifiers {
            alt: true,
            ..ctrl()
        };
        let p = plan(
            &[key(Key::C, alt)],
            &mut s,
            "s:1",
            true,
            true,
            &mut Vec::new(),
        );
        assert!(p.copy.is_none() && !p.copy_transcript);
    }

    #[test]
    fn ctrl_shift_c_copies_the_composer_selection_else_asks_the_transcript() {
        let mut s = sel("abc", 0, 2);
        let events = [key(Key::C, ctrl_shift())];
        let mut swallowed = Vec::new();
        let p = plan(&events, &mut s, "s:1", true, true, &mut swallowed);
        assert_eq!(p.copy.as_deref(), Some("ab"));
        assert!(!p.copy_transcript && s.is_none());
        let mut none = None;
        let p = plan(&events, &mut none, "s:1", true, true, &mut swallowed);
        assert!(p.copy.is_none() && p.copy_transcript);
    }

    #[test]
    fn cut_copies_and_deletes_and_pasting_replaces_the_selection() {
        let mut s = sel("one two", 4, 7);
        let p = plan(&[Event::Cut], &mut s, "s:1", true, false, &mut Vec::new());
        assert_eq!(p.copy.as_deref(), Some("two"));
        assert_eq!(
            json(&p.send[0]),
            serde_json::json!({"ev":"edit","sf":"s:1","id":"ed","from":4,"to":7,"text":"","cursor":4,"len":7})
        );
        let mut s = sel("one two", 0, 3);
        let p = plan(
            &[Event::Paste("1".into())],
            &mut s,
            "s:1",
            true,
            false,
            &mut Vec::new(),
        );
        assert_eq!(
            json(&p.send[0]),
            serde_json::json!({"ev":"edit","sf":"s:1","id":"ed","from":0,"to":3,"text":"1","cursor":1,"len":7})
        );
        assert!(p.keys.is_empty());
        // Ctrl+Shift+V pastes the clipboard at omp's caret: the selection is deleted first.
        let mut s = sel("one two", 0, 3);
        let p = plan(
            &[key(Key::V, ctrl_shift())],
            &mut s,
            "s:1",
            true,
            false,
            &mut Vec::new(),
        );
        assert_eq!(p.send.len(), 1);
        assert_eq!(p.keys.len(), 1);
    }

    #[test]
    fn a_modifier_key_alone_keeps_the_selection_so_a_capital_replaces_it() {
        let mut s = sel("hello world", 6, 11);
        let mut swallowed = Vec::new();
        let shift = Modifiers {
            shift: true,
            ..Modifiers::NONE
        };
        let events = [
            key(Key::ShiftLeft, Modifiers::NONE),
            key(Key::X, shift),
            Event::Text("X".to_owned()),
            release(Key::X),
            release(Key::ShiftLeft),
        ];
        let plan = plan(&events, &mut s, "s:1", true, false, &mut swallowed);
        assert_eq!(plan.send.len(), 1, "one replacing edit");
        assert_eq!(
            json(&plan.send[0])["text"],
            "X",
            "the capital replaced the selection"
        );
        assert!(s.is_none());
    }

    #[test]
    fn shift_arrows_and_other_keys_stay_omps_and_end_the_selection() {
        let mut s = sel("hello", 0, 3);
        let shift = Modifiers {
            shift: true,
            ..Modifiers::NONE
        };
        let p = plan(
            &[key(Key::ArrowRight, shift)],
            &mut s,
            "s:1",
            true,
            false,
            &mut Vec::new(),
        );
        assert!(p.send.is_empty());
        assert_eq!(p.keys.len(), 1);
        assert!(s.is_none());
        // An empty selection (a click) does not take typing: omp inserts at its caret.
        let mut s = sel("hello", 2, 2);
        let events = [key(Key::A, Modifiers::NONE), Event::Text("a".into())];
        let p = plan(&events, &mut s, "s:1", true, false, &mut Vec::new());
        assert!(p.send.is_empty() && p.keys.len() == 2);
    }

    #[test]
    fn an_ime_commit_replaces_the_selection_and_still_ends_the_composition() {
        let mut s = sel("hello", 0, 5);
        let p = plan(
            &[Event::Ime(ImeEvent::Commit("你好".into()))],
            &mut s,
            "s:1",
            true,
            false,
            &mut Vec::new(),
        );
        assert_eq!(
            json(&p.send[0]),
            serde_json::json!({"ev":"edit","sf":"s:1","id":"ed","from":0,"to":5,"text":"你好","cursor":2,"len":5})
        );
        assert_eq!(p.keys, [Event::Ime(ImeEvent::Commit(String::new()))]);
    }

    #[test]
    fn a_selection_is_only_valid_on_the_text_it_was_made_on() {
        let s = ComposerSel::new("ed", "abc", 0, 1);
        assert!(s.is_for("ed", "abc"));
        assert!(!s.is_for("ed", "abcd"), "omp changed the text");
        assert!(!s.is_for("other", "abc"));
    }
}
