//! egui has no system-clipboard backend in this build (see docs/notes/egui-input.md), so plain
//! Ctrl+C/X/V in egui widgets are bridged to the shared `deck_term::Clipboard` by hand.

use deck_term::Clipboard;
use egui::{Event, Key, Modifiers, OutputCommand};

fn is_plain_ctrl(m: &Modifiers) -> bool {
    m.ctrl && !m.shift && !m.alt && !m.mac_cmd
}

fn clipboard_key(ev: &Event) -> Option<Key> {
    match ev {
        Event::Key {
            key: key @ (Key::C | Key::X | Key::V),
            pressed: true,
            modifiers,
            ..
        } if is_plain_ctrl(modifiers) => Some(*key),
        _ => None,
    }
}

/// Rewrite plain Ctrl+C/X/V key presses into `Event::Copy/Cut/Paste`. `paste` is only called
/// when a Ctrl+V is present; `None` (empty clipboard) drops the key, like egui-winit does.
pub fn rewrite_events(events: &mut Vec<Event>, mut paste: impl FnMut() -> Option<String>) {
    if !events.iter().any(|e| clipboard_key(e).is_some()) {
        return;
    }
    let old = std::mem::take(events);
    events.reserve(old.len());
    for ev in old {
        match clipboard_key(&ev) {
            Some(Key::C) => events.push(Event::Copy),
            Some(Key::X) => events.push(Event::Cut),
            Some(_) => {
                if let Some(text) = paste().filter(|t| !t.is_empty()) {
                    events.push(Event::Paste(text.replace("\r\n", "\n")));
                }
            }
            None => events.push(ev),
        }
    }
}

/// Call from `App::raw_input_hook` only while no terminal has focus.
pub fn rewrite_raw_input(raw: &mut egui::RawInput, clipboard: &Clipboard) {
    rewrite_events(&mut raw.events, || clipboard.get_text());
}

/// Sends `OutputCommand::CopyText` to the system clipboard.
///
/// It is an egui plugin acting on the finished frame's output, because some copies are only
/// issued while the pass ends: a selection in labels (the chat transcript) is copied by egui's
/// own end-of-pass step, after the app's `ui` has returned.
struct CopyOutput {
    sink: Box<dyn Fn(&str) + Send + Sync>,
}

impl egui::Plugin for CopyOutput {
    fn debug_name(&self) -> &'static str {
        "CopyOutput"
    }

    fn output_hook(&mut self, _ctx: &egui::Context, output: &mut egui::FullOutput) {
        for cmd in &output.platform_output.commands {
            if let OutputCommand::CopyText(text) = cmd {
                (self.sink)(text);
            }
        }
    }
}

/// Routes widget copies (`OutputCommand::CopyText`) to `clipboard`, from now on. Call once, when
/// the egui context is created; a second call keeps the first sink.
pub fn install(ctx: &egui::Context, clipboard: &Clipboard) {
    let clipboard = clipboard.clone();
    install_with(ctx, move || {
        let clipboard = clipboard.clone();
        Box::new(move |text| clipboard.set_text(text))
    });
}

fn install_with(ctx: &egui::Context, sink: impl FnOnce() -> Box<dyn Fn(&str) + Send + Sync>) {
    if ctx.plugin_opt::<CopyOutput>().is_none() {
        ctx.add_plugin(CopyOutput { sink: sink() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: Key, m: Modifiers) -> Event {
        Event::Key {
            key: k,
            physical_key: Some(k),
            pressed: true,
            repeat: false,
            modifiers: m,
        }
    }

    fn ctrl() -> Modifiers {
        Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::NONE
        }
    }

    #[test]
    fn rewrites_plain_ctrl_only() {
        let mut ev = vec![
            key(Key::C, ctrl()),
            key(Key::X, ctrl()),
            key(Key::V, ctrl()),
            key(
                Key::V,
                Modifiers {
                    shift: true,
                    ..ctrl()
                },
            ),
            key(Key::A, ctrl()),
        ];
        rewrite_events(&mut ev, || Some("a\r\nb".into()));
        assert_eq!(ev[0], Event::Copy);
        assert_eq!(ev[1], Event::Cut);
        assert_eq!(ev[2], Event::Paste("a\nb".into()));
        assert!(matches!(ev[3], Event::Key { key: Key::V, .. }));
        assert!(matches!(ev[4], Event::Key { key: Key::A, .. }));
        assert_eq!(ev.len(), 5);
    }

    #[test]
    fn empty_clipboard_drops_paste() {
        let mut ev = vec![key(Key::V, ctrl())];
        rewrite_events(&mut ev, || None);
        assert!(ev.is_empty());
    }

    #[test]
    fn untouched_without_clipboard_keys() {
        let mut ev = vec![Event::Text("x".into())];
        rewrite_events(&mut ev, || panic!("must not read clipboard"));
        assert_eq!(ev, vec![Event::Text("x".into())]);
    }

    #[test]
    fn copies_issued_in_any_pass_reach_the_clipboard_once() {
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let seen = Arc::clone(&seen);
            install_with(&ctx, move || {
                Box::new(move |t| seen.lock().expect("test mutex").push(t.to_owned()))
            });
        }
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.ctx().copy_text("hello".to_owned());
        });
        out.textures_delta.clear();
        assert_eq!(*seen.lock().expect("test mutex"), ["hello"]);
    }
}
