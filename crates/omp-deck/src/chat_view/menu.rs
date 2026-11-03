//! The right-click menu of the chat view.
//!
//! One menu, opened where the click landed, whose content depends on what was clicked: a tool or
//! agent card (its actions, see [`tools::card_acts`]), the composer (cut, copy, paste, select all)
//! or the transcript's text (copy the selection, select all). Every card action is also a hover
//! button on the card's head.
//!
//! While an egui popup is up the app holds the terminal's input back (keys meant for the menu must
//! not reach omp), so a clicked entry first ends that hold ([`Cx::release`]): its events are the
//! user's deliberate act.

use egui::{Align, Layout, Popup, PopupAnchor, PopupKind, SetOpenCommand, Ui};

use super::ctx::{Claim, CtxMenu, Cx};
use super::select::{self, ComposerSel};
use super::{glyphs, tools};
use crate::icons::{ICON_CLIPBOARD_LIST, ICON_COPY, Icon};
use crate::widgets::menu_item;

const WIDTH: f32 = 220.0;

/// A Lucide icon by Tern name, for `menu_item`.
fn glyph(name: &str) -> Icon {
    Icon::new(glyphs::lucide(name))
}

/// Takes this frame's right click: opens the menu when it landed on a card, the composer, the
/// transcript's text, or a selection. Returns whether it opened (the popup then asks to be opened
/// too).
pub fn open(cx: &Cx) -> bool {
    let Some(pos) = cx.loc(|l| l.ctx_click.take()) else {
        return false;
    };
    let claim = cx.loc(|l| l.ctx_claim.take());
    let on_text =
        cx.loc(|l| l.tsel.has_selection() || l.tsel.view.is_some_and(|v| v.contains(pos)));
    if claim.is_none() && !on_text {
        return false;
    }
    cx.loc(|l| l.menu = Some(CtxMenu { pos, target: claim }));
    true
}

/// Shows the open menu (if any). `opened` is [`open`]'s answer for this frame.
pub fn show(cx: &Cx, ui: &Ui, id: egui::Id, opened: bool) {
    let Some(menu) = cx.loc(|l| l.menu.clone()) else {
        return;
    };
    let shown = Popup::new(
        id.with("chat-menu"),
        ui.ctx().clone(),
        PopupAnchor::Position(menu.pos),
        ui.layer_id(),
    )
    .kind(PopupKind::Menu)
    .layout(Layout::top_down_justified(Align::Min))
    .style(crate::theme::menu_style)
    .open_memory(opened.then_some(SetOpenCommand::Bool(true)))
    .width(WIDTH)
    .show(|ui| body(cx, ui, &menu));
    if cx.loc(|l| std::mem::take(&mut l.refocus)) {
        ui.memory_mut(|m| m.request_focus(id));
    }
    if shown.is_none() {
        cx.loc(|l| l.menu = None);
    }
}

fn body(cx: &Cx, ui: &mut Ui, menu: &CtxMenu) {
    match &menu.target {
        Some(Claim::Composer(id)) => composer(cx, ui, id),
        Some(Claim::Card(id)) => card(cx, ui, id),
        None => text_entries(cx, ui),
    }
}

/// A clicked entry: the user acted, so the hold the open popup put on the terminal ends.
fn done(cx: &Cx, ui: &Ui) {
    cx.release();
    cx.loc(|l| l.refocus = true);
    ui.close();
}

/// The transcript's "Copy" and "Select all". The selection is OmpDeck's own and a press on this
/// menu leaves it alone, so a click acts at once: the copy is made next frame, before the
/// transcript draws ([`super::tsel::copy`]).
fn text_entries(cx: &Cx, ui: &mut Ui) {
    copy_selection(cx, ui);
    if menu_item(ui, glyph("list"), "Select all", None, true, false).clicked() {
        cx.loc(|l| l.tsel.select_all(cx.snap));
        done(cx, ui);
    }
}

/// The "Copy" entry for the transcript's selection.
fn copy_selection(cx: &Cx, ui: &mut Ui) {
    let has = cx.loc(|l| l.tsel.has_selection());
    if menu_item(ui, ICON_COPY, "Copy", Some("Ctrl+Shift+C"), has, false).clicked() && has {
        cx.loc(|l| l.tsel.want_copy = true);
        ui.ctx().request_repaint();
        done(cx, ui);
    }
}

fn card(cx: &Cx, ui: &mut Ui, id: &str) {
    let Some(n) = cx.snap.get(id) else {
        ui.close();
        return;
    };
    let ink = cx.ink;
    let title = super::text::clean(n.str("name").unwrap_or(n.kind.as_str())).into_owned();
    ui.label(crate::theme::section_caption(&title, ink.faint()));
    ui.separator();
    if cx.loc(|l| l.tsel.has_selection()) {
        copy_selection(cx, ui);
        ui.separator();
    }
    for act in tools::card_acts(cx, n).iter() {
        let r = menu_item(
            ui,
            glyph(act.icon),
            &act.label,
            act.keys.as_deref(),
            !act.disabled,
            act.danger,
        );
        if r.clicked() {
            tools::perform(cx, n, act);
            done(cx, ui);
        }
    }
    if n.flag("collapsible") {
        ui.separator();
        let collapsed = cx.collapsed(n);
        let (icon, label) = if collapsed {
            (glyph("chev"), "Expand")
        } else {
            (glyph("chev-up"), "Collapse")
        };
        if menu_item(ui, icon, label, None, true, false).clicked() {
            cx.toggle(n);
            done(cx, ui);
        }
    }
}

fn composer(cx: &Cx, ui: &mut Ui, id: &str) {
    let Some(n) = cx.snap.get(id) else {
        ui.close();
        return;
    };
    let text = n.text();
    let sel = cx
        .loc(|l| l.sel.clone())
        .filter(|s| !s.is_empty() && s.is_for(id, text));
    let has = sel.is_some();
    let editable = cx.can_edit && !n.flag("readonly");
    ui.label(crate::theme::section_caption("Composer", cx.ink.faint()));
    ui.separator();
    if menu_item(
        ui,
        glyph("scissors"),
        "Cut",
        Some("Ctrl+X"),
        has && editable,
        false,
    )
    .clicked()
        && let Some(s) = &sel
    {
        cx.copy(s.selected());
        cx.send(s.replace(cx.sf(), ""));
        cx.loc(|l| l.sel = None);
        done(cx, ui);
    }
    if menu_item(ui, ICON_COPY, "Copy", Some("Ctrl+Shift+C"), has, false).clicked()
        && let Some(s) = &sel
    {
        cx.copy(s.selected());
        done(cx, ui);
    }
    if menu_item(
        ui,
        ICON_CLIPBOARD_LIST,
        "Paste",
        Some("Ctrl+Shift+V"),
        !n.flag("readonly"),
        false,
    )
    .clicked()
    {
        // The pasted text replaces the selection: delete it first, then paste at omp's caret.
        if let Some(s) = sel.as_ref().filter(|_| editable) {
            cx.send(s.replace(cx.sf(), ""));
        }
        cx.loc(|l| l.sel = None);
        cx.paste();
        done(cx, ui);
    }
    ui.separator();
    if menu_item(
        ui,
        glyph("list"),
        "Select all",
        None,
        !text.is_empty(),
        false,
    )
    .clicked()
    {
        let all = ComposerSel::new(id, text, 0, text.chars().count());
        if editable {
            cx.send(select::caret_edit(cx.sf(), id, text, text.chars().count()));
        }
        cx.loc(|l| l.sel = Some(all));
        done(cx, ui);
    }
}
