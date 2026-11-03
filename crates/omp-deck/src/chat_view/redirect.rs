//! Redirects: omp's own screens (`/settings`, the Alt+P model picker, `/usage`, `/resume`,
//! `/tree`) are answered by OmpDeck's own counterparts, which do everything omp's do.
//!
//! Three ways in, one outcome ([`Redirect`] on the frame's response):
//!
//! * **Typed** ([`intercept`]): a plain Enter on a sendable composer whose command is one of
//!   those, resolved from the autocomplete's selected item (Enter accepts it) or else from the
//!   first token of the text, with what was typed in the same frame. The press and its release
//!   never reach omp and the composer is cleared by one `edit`; omp opens nothing, so nothing of
//!   what it does on open (a usage fetch) happens. Arguments that make omp act directly
//!   (`/switch <model>`, `/resume <id>`, `/usage reset <provider>/<id>`) pass through.
//! * **Caught** ([`catch_page`], [`catch_layer`], [`Latch`]): every other way omp can open one of
//!   them (a remapped chord, double Esc, a slash-menu click, an omp action) is recognised in the
//!   pre-draw snapshot, redirected and closed with Esc in the same frame; the surface is never
//!   drawn. The model hub (Alt+M) and every other picker or overlay are never caught.
//! * **Clicked** (`Cx::click`): the composer's model and cost facts, drawn in split panes.
//!
//! The identifications are those of omp 18.16.0's recorded TSP (`redirect-fixtures.json`).

use std::time::{Duration, Instant};

use deck_core::omp::ForeignSource;
use deck_tsp::wire::Event as TspEvent;
use egui::{Event, ImeEvent, Key};

use super::dock::utf16_len;
use super::events;
use super::select;
use super::snap::{SNode, Snap};

/// An omp screen answered by OmpDeck's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redirect {
    /// `/settings`. `page` is omp's current settings page when the redirect saw one open.
    OmpSettings { page: Option<String> },
    /// Alt+P, `/switch` without an argument, the composer's model fact.
    ModelPicker,
    /// `/usage`, the composer's cost fact; `reset` for `/usage reset` without a target.
    Usage { reset: bool },
    /// `/resume`; `foreign` for `/resume @claude` / `@codex`.
    Resume { foreign: Option<ForeignSource> },
    /// `/tree`, double Esc with `doubleEscapeAction: tree`.
    Tree,
}

// ---- typed ----------------------------------------------------------------------------------

/// The redirect `text` stands for when Enter submits it. `popup`: the name of the slash menu's
/// selected item while the menu is open (Enter accepts it); only valid while the menu was
/// drawn for exactly this `text`, i.e. when nothing was typed since.
fn command(text: &str, popup: Option<&str>) -> Option<Redirect> {
    let rest = text.strip_prefix('/')?;
    if rest.contains('\n') {
        return None;
    }
    let (token, args) = match rest.split_once(char::is_whitespace) {
        Some((token, args)) => (token, args.trim()),
        None => (rest, ""),
    };
    let name = match popup {
        // The menu of a command's arguments: Enter accepts the argument selected in it, which is
        // what is typed only when the selection (`configure `) names those very arguments.
        Some(selected) if rest.contains(char::is_whitespace) => {
            if selected.trim() != args {
                return None;
            }
            token
        }
        Some(selected) => selected,
        None => token,
    };
    match (name, args) {
        ("advisor", "configure") => Some(Redirect::OmpSettings {
            page: Some(ADVISOR_PAGE.to_owned()),
        }),
        ("settings", "") => Some(Redirect::OmpSettings { page: None }),
        ("switch", "") => Some(Redirect::ModelPicker),
        ("usage", "" | "show") => Some(Redirect::Usage { reset: false }),
        ("usage", "reset") => Some(Redirect::Usage { reset: true }),
        ("resume", "") => Some(Redirect::Resume { foreign: None }),
        ("resume", "@claude") => Some(Redirect::Resume {
            foreign: Some(ForeignSource::Claude),
        }),
        ("resume", "@codex") => Some(Redirect::Resume {
            foreign: Some(ForeignSource::Codex),
        }),
        ("tree", "") => Some(Redirect::Tree),
        _ => None,
    }
}

/// The name of the slash menu's selected item (`list/<name>`, the item's key after the list's own
/// id), while omp's autocomplete is open.
fn popup_item(snap: &Snap) -> Option<&str> {
    let layer = snap.region("layer")?;
    let popup = snap.kids(layer).find(|n| n.role() == "omp.autocomplete")?;
    let list = snap.find(popup, &|n| n.kind == "list")?;
    list.str("selected")?
        .strip_prefix(list.id.as_str())?
        .strip_prefix('/')
}

/// Whether `modifiers` make Enter a plain submit (Shift+Enter is a newline).
fn plain(modifiers: egui::Modifiers) -> bool {
    !(modifiers.shift || modifiers.ctrl || modifiers.alt || modifiers.command || modifiers.mac_cmd)
}

fn is_modifier_key(key: Key) -> bool {
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

/// Takes a redirecting Enter out of the frame's `keys`.
///
/// Returns the redirect and the `edit` that clears the composer when a plain Enter submits one of
/// the redirected commands: everything the frame typed up to and including that Enter is removed
/// from `keys` (the composer is cleared, so none of it may land in it), its release is removed
/// too or, when it comes in a later frame, remembered in `swallowed`.
///
/// A frame that edits the text in a way the keys do not show (Backspace, arrows, shortcuts
/// before the Enter) is left alone: omp opens the screen and [`catch_layer`] closes it.
pub fn intercept(
    snap: &Snap,
    keys: &mut Vec<Event>,
    swallowed: &mut Vec<Key>,
) -> Option<(Redirect, Option<TspEvent>)> {
    let composer = snap.composer()?;
    if !snap.can_edit || !composer.flag("sendable") {
        return None;
    }
    // Plain typing: a key press followed by its text, or text alone (IME, paste is not typing).
    let mut typed = String::new();
    let mut enter = None;
    let mut i = 0;
    while i < keys.len() {
        match &keys[i] {
            Event::Key { key, .. } if is_modifier_key(*key) => {}
            Event::Key {
                key: Key::Enter,
                pressed: true,
                repeat,
                modifiers,
                ..
            } => {
                if !plain(*modifiers) || (*repeat && swallowed.contains(&Key::Enter)) {
                    return None;
                }
                enter = Some(i);
                break;
            }
            Event::Key {
                pressed: true,
                modifiers,
                ..
            } => match keys.get(i + 1) {
                Some(Event::Text(t)) if !modifiers.ctrl && !modifiers.alt && !modifiers.command => {
                    typed.push_str(t);
                    i += 1;
                }
                // Any other key edits or moves in ways the text above does not show.
                _ => return None,
            },
            Event::Key { .. } => {}
            Event::Text(t) | Event::Ime(ImeEvent::Commit(t)) => typed.push_str(t),
            Event::Paste(_) | Event::Cut | Event::Copy => return None,
            _ => {}
        }
        i += 1;
    }
    let enter = enter?;
    // Typing lands at omp's caret, over its selection: only an append at the end (no selection)
    // is what `text + typed` says. Anything else is omp's to apply; a screen it opens is caught.
    if !typed.is_empty() {
        let end = utf16_len(composer.text());
        let cursor = composer.num("cursor").map_or(end, |c| c as usize);
        let anchor = composer.num("anchor").map_or(cursor, |a| a as usize);
        if cursor != end || anchor != end {
            return None;
        }
    }
    let mut text = String::with_capacity(composer.text().len() + typed.len());
    text.push_str(composer.text());
    text.push_str(&typed);
    let redirect = command(&text, popup_item(snap).filter(|_| typed.is_empty()))?;

    // Everything typed up to the Enter is the redirect's: the keys, their text, the Enter itself.
    // Releases of earlier keys and everything that is no keyboard input stay.
    let mut released = false;
    let mut at = 0;
    keys.retain(|e| {
        let before = at <= enter;
        at += 1;
        match e {
            Event::Key { key, .. } if is_modifier_key(*key) => true,
            Event::Key {
                key: Key::Enter,
                pressed: false,
                ..
            } if !released => {
                released = true;
                false
            }
            Event::Key { pressed: false, .. } => true,
            Event::Key { .. } | Event::Text(_) | Event::Ime(ImeEvent::Commit(_)) => !before,
            _ => true,
        }
    });
    if !released {
        select::swallow(swallowed, Key::Enter);
    }
    let len = utf16_len(composer.text()) as u32;
    // Nothing in omp's composer (the text was typed in this very frame): nothing to clear.
    let clear = (len > 0).then(|| events::edit(snap.sf.as_str(), &composer.id, 0, len, "", 0, len));
    Some((redirect, clear))
}

// ---- caught ---------------------------------------------------------------------------------

/// What a recognised omp surface is, to hide it while it closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Surface {
    /// omp's settings page: the `screen` surface, not drawn in the session's place.
    Page(String),
    /// A node of the session's `layer`: skipped when the layer is drawn.
    Node(String),
}

/// An omp surface that has a counterpart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caught {
    pub redirect: Redirect,
    pub surface: Surface,
}

/// OmpDeck's Settings page for omp's advisor configuration (`/advisor configure`).
const ADVISOR_PAGE: &str = "advisor";

/// What omp's `prefs` sheet titled `title`, showing its page `page`, is, if it has a counterpart:
/// the settings sheet ("omp settings"; omp's current page, e.g. `plugins` for its Installed
/// tab, is OmpDeck's page of that id) or the advisor editor ("Advisors", "Advisors · unsaved").
fn prefs_screen(title: &str, page: Option<&str>) -> Option<Redirect> {
    match title {
        "omp settings" => Some(Redirect::OmpSettings {
            page: page.map(str::to_owned),
        }),
        "Advisors" | "Advisors · unsaved" => Some(Redirect::OmpSettings {
            page: Some(ADVISOR_PAGE.to_owned()),
        }),
        _ => None,
    }
}

/// omp's settings page or advisor editor: a `prefs` sheet on the page surface `page`.
pub fn catch_page(page: &Snap) -> Option<Caught> {
    let main = page.region("main")?;
    let redirect = page
        .find(main, &|n| {
            n.kind == "prefs"
                && n.str("title")
                    .is_some_and(|t| prefs_screen(t, None).is_some())
        })
        .and_then(|n| prefs_screen(n.str("title")?, n.str("page")))?;
    Some(Caught {
        redirect,
        surface: Surface::Page(page.sf.clone()),
    })
}

/// What the layer node `n` is, if one of the redirected screens: the Alt+P picker ("Switch
/// model"; the task and role pickers carry other titles), the usage overlay, the session picker
/// of `/resume`, the tree picker.
fn layer_screen(n: &SNode) -> Option<Redirect> {
    match n.kind.as_str() {
        "picker" => match (n.str("title"), n.str("noun"), n.str("icon")) {
            (Some("Switch model"), ..) => Some(Redirect::ModelPicker),
            (Some("Session tree"), ..) if n.str("layout") == Some("tree") => Some(Redirect::Tree),
            (_, Some("sessions"), Some("history")) => Some(Redirect::Resume { foreign: None }),
            _ => None,
        },
        "overlay" if n.role() == "omp.overlay.usage" => Some(Redirect::Usage { reset: false }),
        _ => None,
    }
}

/// The first redirected screen open in the session's `layer`.
pub fn catch_layer(session: &Snap) -> Option<Caught> {
    let layer = session.region("layer")?;
    session.kids(layer).filter(|n| !n.hidden()).find_map(|n| {
        Some(Caught {
            redirect: layer_screen(n)?,
            surface: Surface::Node(n.id.clone()),
        })
    })
}

/// How long omp gets to close a surface before the Esc is sent again.
const RETRY: Duration = Duration::from_millis(1500);

/// The surface being closed: omp needs a moment (a frame or two) to take the Esc, and the surface
/// stays in the snapshot until then. It is redirected once, hidden meanwhile, and its Esc repeated
/// if omp keeps it.
#[derive(Debug, Default)]
pub struct Latch {
    closing: Option<(Surface, Instant)>,
}

/// What this frame does about the surface omp has open.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// The surface not to draw.
    pub hide: Option<Surface>,
    /// The redirect to report (once per surface).
    pub redirect: Option<Redirect>,
    /// Send omp an Esc.
    pub esc: bool,
}

impl Latch {
    pub fn judge(&mut self, caught: Option<Caught>, now: Instant) -> Verdict {
        let Some(caught) = caught else {
            self.closing = None;
            return Verdict::default();
        };
        match &mut self.closing {
            Some((surface, sent)) if *surface == caught.surface => {
                let again = now.saturating_duration_since(*sent) >= RETRY;
                if again {
                    *sent = now;
                }
                Verdict {
                    hide: Some(caught.surface),
                    redirect: None,
                    esc: again,
                }
            }
            _ => {
                self.closing = Some((caught.surface.clone(), now));
                Verdict {
                    hide: Some(caught.surface),
                    redirect: Some(caught.redirect),
                    esc: true,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_resolve_from_the_text_or_the_menus_selection() {
        let none = |t: &str| command(t, None);
        assert_eq!(
            none("/settings"),
            Some(Redirect::OmpSettings { page: None })
        );
        assert_eq!(none("/switch"), Some(Redirect::ModelPicker));
        assert_eq!(none("/switch  "), Some(Redirect::ModelPicker));
        assert_eq!(none("/usage"), Some(Redirect::Usage { reset: false }));
        assert_eq!(none("/usage show"), Some(Redirect::Usage { reset: false }));
        assert_eq!(none("/usage reset"), Some(Redirect::Usage { reset: true }));
        assert_eq!(none("/resume"), Some(Redirect::Resume { foreign: None }));
        assert_eq!(
            none("/resume @codex"),
            Some(Redirect::Resume {
                foreign: Some(ForeignSource::Codex)
            })
        );
        assert_eq!(none("/tree"), Some(Redirect::Tree));
        // Arguments that make omp act directly pass through.
        for text in [
            "/switch fake/fake-2",
            "/switch @smol",
            "/resume 01a1712d",
            "/resume /tmp/a.jsonl",
            "/usage reset openai-codex/acct",
            "/tree now",
            "/model",
            "/settingsx",
            "settings",
            "/jobs",
            "/tree\n/usage",
        ] {
            assert_eq!(none(text), None, "{text}");
        }
        // Enter accepts the open menu's selection, which is not what was typed so far.
        assert_eq!(
            command("/set", Some("settings")),
            Some(Redirect::OmpSettings { page: None })
        );
        assert_eq!(command("/se", Some("security")), None);
        // The menu of a command's arguments is not a menu of commands: Enter takes its selection.
        assert_eq!(command("/switch ", Some("fake%2Ffake-1 ")), None);
        assert_eq!(command("/switch ", None), Some(Redirect::ModelPicker));
        assert_eq!(command("/resume x", Some("tree")), None);
    }

    #[test]
    fn the_advisor_editor_is_redirected_only_when_the_menu_selects_what_was_typed() {
        let advisor = Some(Redirect::OmpSettings {
            page: Some("advisor".into()),
        });
        assert_eq!(command("/advisor configure", None), advisor);
        // omp's menu of the argument names is open: `configure ` is selected and typed.
        assert_eq!(command("/advisor configure", Some("configure ")), advisor);
        // Another argument selected than typed: Enter takes the selected one, omp's.
        assert_eq!(command("/advisor conf", Some("configure ")), None);
        for text in ["/advisor", "/advisor on", "/advisor dump raw"] {
            assert_eq!(command(text, None), None, "{text}");
        }
    }

    #[test]
    fn omps_prefs_sheets_are_told_by_title_and_land_on_their_page() {
        let settings = |page: Option<&str>| {
            Some(Redirect::OmpSettings {
                page: page.map(str::to_owned),
            })
        };
        assert_eq!(
            prefs_screen("omp settings", Some("plugins")),
            settings(Some("plugins"))
        );
        assert_eq!(prefs_screen("omp settings", None), settings(None));
        // The advisor editor's pages (`advisor:0`, `shared`) are one OmpDeck page.
        for title in ["Advisors", "Advisors · unsaved"] {
            assert_eq!(
                prefs_screen(title, Some("advisor:1")),
                settings(Some("advisor"))
            );
        }
        assert_eq!(prefs_screen("Settings", Some("a")), None);
    }

    fn page(id: &str) -> Caught {
        Caught {
            redirect: Redirect::Tree,
            surface: Surface::Node(id.to_owned()),
        }
    }

    #[test]
    fn a_surface_is_redirected_once_hidden_while_it_stays_and_closed_again_only_after_a_wait() {
        let mut latch = Latch::default();
        let t0 = Instant::now();
        let first = latch.judge(Some(page("p")), t0);
        assert_eq!(first.redirect, Some(Redirect::Tree));
        assert!(first.esc && first.hide == Some(Surface::Node("p".into())));
        // omp still shows it a frame later: hidden, no second redirect, no second Esc.
        let next = latch.judge(Some(page("p")), t0 + Duration::from_millis(40));
        assert_eq!((next.redirect, next.esc), (None, false));
        assert_eq!(next.hide, Some(Surface::Node("p".into())));
        // omp ignored the Esc: it is sent again, still without a redirect.
        let retry = latch.judge(Some(page("p")), t0 + RETRY);
        assert_eq!((retry.redirect, retry.esc), (None, true));
        // The surface closes and a new one opens: a new redirect.
        assert_eq!(latch.judge(None, t0 + RETRY), Verdict::default());
        assert!(
            latch
                .judge(Some(page("p")), t0 + RETRY * 2)
                .redirect
                .is_some()
        );
    }
}
