//! Gestures → TSP events. Pure: every clickable node sends exactly the event its `actions` (or
//! its kind) declares; `copy`/`open`/`zoom` stay local, `toggle` also flips the local fold.

use deck_tsp::wire::Event;

use super::snap::{SNode, Snap};

/// What a click on a node resolves to.
#[derive(Debug, Clone, PartialEq)]
pub enum Click {
    /// Fold toggle: the event carries the *new* `collapsed`.
    Toggle(Event),
    /// Copy the node's text.
    Copy,
    /// Open the node's `href` (or text).
    Open,
    /// Enlarge an image.
    Zoom,
    /// Send this event to omp.
    Send(Event),
}

pub fn toggle(sf: &str, id: &str, key: Option<&str>, collapsed: bool) -> Event {
    Event::Toggle {
        sf: sf.to_owned(),
        id: id.to_owned(),
        key: key.map(str::to_owned),
        collapsed,
    }
}

pub fn select(sf: &str, id: &str, item: &str) -> Event {
    Event::Select {
        sf: sf.to_owned(),
        id: id.to_owned(),
        item: item.to_owned(),
    }
}

pub fn activate(sf: &str, id: &str, item: &str) -> Event {
    Event::Activate {
        sf: sf.to_owned(),
        id: id.to_owned(),
        item: item.to_owned(),
    }
}

/// `act` may carry its value as `act=value`; the first `=` splits.
pub fn action(sf: &str, id: &str, act: &str) -> Event {
    match act.split_once('=') {
        Some((a, v)) => action_value(sf, id, a, Some(v)),
        None => action_value(sf, id, act, None),
    }
}

pub fn action_value(sf: &str, id: &str, act: &str, value: Option<&str>) -> Event {
    Event::Action {
        sf: sf.to_owned(),
        id: id.to_owned(),
        act: act.to_owned(),
        value: value.map(str::to_owned),
        mods: None,
    }
}

/// A native edit: replace the UTF-16 range `[from, to)` with `text` and put the caret at
/// `cursor`; `len` is the text length (UTF-16) the terminal saw.
pub fn edit(sf: &str, id: &str, from: u32, to: u32, text: &str, cursor: u32, len: u32) -> Event {
    Event::Edit {
        sf: sf.to_owned(),
        id: id.to_owned(),
        from,
        to,
        text: text.to_owned(),
        cursor,
        len,
    }
}

/// The nearest ancestor-or-self that is a `list`, `tabs` or `picker` (the id `select`/`activate`
/// events address); `None` when `node` is not inside one.
pub fn list_of<'a>(snap: &'a Snap, node: &'a SNode) -> Option<&'a SNode> {
    if matches!(node.kind.as_str(), "list" | "tabs" | "picker") {
        return Some(node);
    }
    let mut cur = node;
    // Parents are not stored: find the owner by scanning up from the root only for items.
    while let Some(p) = snap.parent_of(&cur.id) {
        if matches!(p.kind.as_str(), "list" | "tabs" | "picker") {
            return Some(p);
        }
        cur = p;
    }
    None
}

/// Resolves the click action `name` declared on `node`. `collapsed` is the node's current
/// fold state (what `toggle` flips).
pub fn resolve_click(snap: &Snap, node: &SNode, name: &str, collapsed: bool) -> Click {
    let sf = snap.sf.as_str();
    match name {
        "toggle" => Click::Toggle(toggle(sf, &node.id, node.str("key"), !collapsed)),
        "copy" => Click::Copy,
        "open" => Click::Open,
        "zoom" => Click::Zoom,
        "select" | "activate" => {
            let list = list_of(snap, node).map_or(node.id.as_str(), |l| l.id.as_str());
            Click::Send(if name == "select" {
                select(sf, list, &node.id)
            } else {
                activate(sf, list, &node.id)
            })
        }
        custom => Click::Send(action(sf, &node.id, custom)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn j(e: &Event) -> Value {
        serde_json::to_value(e).expect("event JSON")
    }

    #[test]
    fn toggle_carries_the_new_fold_state_and_the_nodes_key() {
        assert_eq!(
            j(&toggle("s:1", "7", Some("tool:7"), true)),
            json!({"ev":"toggle","sf":"s:1","id":"7","key":"tool:7","collapsed":true})
        );
        assert_eq!(
            j(&toggle("s:1", "7", None, false)),
            json!({"ev":"toggle","sf":"s:1","id":"7","collapsed":false})
        );
    }

    #[test]
    fn list_gestures_address_the_list_with_the_item_node_id() {
        assert_eq!(
            j(&select("s:1", "L", "L.2")),
            json!({"ev":"select","sf":"s:1","id":"L","item":"L.2"})
        );
        assert_eq!(
            j(&activate("s:1", "L", "L.2")),
            json!({"ev":"activate","sf":"s:1","id":"L","item":"L.2"})
        );
    }

    #[test]
    fn custom_actions_split_act_and_value_at_the_first_equals() {
        assert_eq!(
            j(&action("s:1", "b", "approve")),
            json!({"ev":"action","sf":"s:1","id":"b","act":"approve"})
        );
        assert_eq!(
            j(&action("s:1", "b", "scope=all=1")),
            json!({"ev":"action","sf":"s:1","id":"b","act":"scope","value":"all=1"})
        );
        assert_eq!(
            j(&action_value("s:1", "P", "tab", Some("t2"))),
            json!({"ev":"action","sf":"s:1","id":"P","act":"tab","value":"t2"})
        );
    }

    #[test]
    fn clicks_resolve_per_declared_action() {
        let m = crate::chat_view::snap::tests::fixture_mirror(usize::MAX);
        let mut s = Snap::default();
        crate::chat_view::snap::tests::sync_doc(&mut s, &m);
        let main = s.region("main").expect("main");
        let copy = s
            .find(main, &|n| n.click() == Some("copy-message"))
            .expect("user copy icon");
        assert_eq!(
            resolve_click(&s, copy, copy.click().unwrap_or_default(), false),
            Click::Send(action("s:1", &copy.id, "copy-message"))
        );
        assert_eq!(resolve_click(&s, copy, "copy", false), Click::Copy);
        assert_eq!(resolve_click(&s, copy, "open", false), Click::Open);
        assert_eq!(resolve_click(&s, copy, "zoom", false), Click::Zoom);
        assert_eq!(
            resolve_click(&s, copy, "toggle", false),
            Click::Toggle(toggle("s:1", &copy.id, None, true))
        );
        // `select` on a node outside any list addresses the node itself.
        assert_eq!(
            resolve_click(&s, copy, "select", false),
            Click::Send(select("s:1", &copy.id, &copy.id))
        );
    }
}
