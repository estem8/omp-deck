//! Split view: several tabs side by side. The layout model is `panes::Split`; this file draws the
//! panes and keeps `App.active` equal to the focused pane's tab, so everything bound to the
//! active tab (status bar, Changes panel, control link, palette, session tree) follows pane focus.

use std::path::PathBuf;

use deck_core::sessions::SessionEntry;
use deck_term::TerminalView;
use egui::{
    Align, Align2, CornerRadius, CursorIcon, Id, Key, Layout, Modifiers, Rect, Sense, Stroke,
    StrokeKind, Ui, UiBuilder, pos2, vec2,
};

use super::{App, TermMenu, paste_from_menu, terminal_menu};
use crate::icons::{ICON_MESSAGES_SQUARE, ICON_TERMINAL, ICON_X, Icon};
use crate::panes::{self, Dir, Side};
use crate::tabs::{Tab, paint_status};
use crate::theme::{self, Palette};
use crate::widgets::paint_icon;
use deck_service::status::TabStatus;

/// Height of a pane's header strip.
const HEADER_H: f32 = 24.0;
const CLOSE_D: f32 = 18.0;

/// What the pane header reported this frame.
struct HeaderOut {
    close: bool,
    pressed: bool,
}

/// Pane header: status glyph, title, and a × that removes the pane (the tab stays open).
#[expect(
    clippy::too_many_arguments,
    reason = "one paint routine, no state worth a struct"
)]
fn pane_header(
    ui: &mut Ui,
    slot: usize,
    rect: Rect,
    pal: &Palette,
    status: TabStatus,
    title: &str,
    focused: bool,
    tab_open: bool,
    icon: Icon,
) -> HeaderOut {
    let resp = ui.interact(rect, Id::unique(("pane-header", slot)), Sense::click());
    let close_rect = Rect::from_center_size(
        pos2(rect.right() - 4.0 - CLOSE_D / 2.0, rect.center().y),
        vec2(CLOSE_D, CLOSE_D),
    );
    let close = ui.interact(close_rect, Id::unique(("pane-close", slot)), Sense::click());
    let p = ui.painter();
    let fill = if focused { pal.raised } else { pal.surface };
    p.rect_filled(
        rect,
        CornerRadius {
            nw: 3,
            ne: 3,
            sw: 0,
            se: 0,
        },
        fill,
    );
    let text_color = if focused { pal.text_strong } else { pal.text };
    let icon_c = pos2(rect.left() + 12.0, rect.center().y);
    if !paint_status(ui, icon_c, 14.0, status) {
        let col = if tab_open { pal.accent_b } else { pal.weak };
        paint_icon(
            p,
            icon_c,
            Align2::CENTER_CENTER,
            icon,
            14.0,
            if focused { col } else { pal.weak },
        );
    }
    let mut job =
        egui::text::LayoutJob::simple_singleline(title.to_owned(), theme::font(12.5), text_color);
    job.wrap.max_width = (close_rect.left() - rect.left() - 28.0 - 6.0).max(0.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = p.layout_job(job);
    p.galley(
        pos2(rect.left() + 24.0, rect.center().y - galley.size().y / 2.0),
        galley,
        text_color,
    );
    let hov = close.hovered();
    if hov {
        p.circle_filled(close_rect.center(), CLOSE_D / 2.0, theme::hover_fill(pal));
    }
    paint_icon(
        p,
        close_rect.center(),
        Align2::CENTER_CENTER,
        ICON_X,
        12.0,
        if hov { pal.text_strong } else { pal.weak },
    );
    close
        .clone()
        .on_hover_text("Close pane (the tab stays open)");
    HeaderOut {
        close: close.clicked(),
        pressed: resp.contains_pointer() && !close.contains_pointer(),
    }
}

/// What the panes asked for this frame, applied after drawing.
#[derive(Default)]
struct Requests {
    focus: Option<usize>,
    close: Option<usize>,
    dividers: Vec<(usize, f32)>,
    open_url: Option<String>,
    chat_url: Option<String>,
    /// An omp screen that OmpDeck answers with its own, and the pane that asked.
    redirect: Option<(usize, crate::chat_view::Redirect)>,
    copy_link: Option<String>,
}

/// A pane reserved for a launch still on its way, identified like the launch (session file or
/// tmux name) so concurrent launches each fill their own pane.
pub(super) struct PaneClaim {
    pub(super) session_path: Option<PathBuf>,
    pub(super) mux_name: Option<String>,
    pub(super) ticket: u64,
}

impl PaneClaim {
    fn matches(&self, path: Option<&std::path::Path>, name: Option<&str>) -> bool {
        (path.is_some() && self.session_path.as_deref() == path)
            || (name.is_some() && self.mux_name.as_deref() == name)
    }
}

impl App {
    /// The split is on screen: more than one pane, and the active tab is a session tab (the
    /// update and installer tabs take the full width).
    pub(super) fn split_shown(&self) -> bool {
        !self.dash.open
            && self.split.is_split()
            && self
                .tabs
                .get(self.active)
                .is_some_and(|t| t.pane_uid().is_some())
    }

    /// Uids of the tabs on screen: every pane of a shown split, else the active tab.
    pub(super) fn visible_uids(&self) -> Vec<u64> {
        if self.dash.open {
            Vec::new()
        } else if self.split_shown() {
            self.split.uids().collect()
        } else {
            self.tabs
                .get(self.active)
                .map(|t| t.uid)
                .into_iter()
                .collect()
        }
    }

    /// Per frame: drop panes of tabs that vanished, make sure the active session tab has a pane,
    /// and keep `active` equal to the focused pane's tab.
    pub(super) fn reconcile_panes(&mut self) {
        self.prune_pane_claims();
        let tabs = &self.tabs;
        self.split
            .retain(|uid| tabs.iter().any(|t| t.pane_uid() == Some(uid)));
        let Some(active) = self.tabs.get(self.active).and_then(Tab::pane_uid) else {
            return;
        };
        if self.split.slot_of(active).is_none() {
            self.split.show(active);
        } else if let Some(focused) = self.split.focused_uid()
            && focused != active
            && let Some(i) = self.tab_index(focused)
        {
            self.active = i;
        }
    }

    pub(super) fn tab_index(&self, uid: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.uid == uid)
    }

    /// The focused pane changed (shortcut, palette, closed pane): its tab becomes the active one.
    fn follow_pane_focus(&mut self) {
        if let Some(i) = self.split.focused_uid().and_then(|u| self.tab_index(u)) {
            self.activate(i);
        }
        self.focus_pending = true;
        self.dirty = true;
    }

    /// A click landed in pane `slot`.
    fn focus_pane(&mut self, slot: usize) {
        if self.split.focus_slot(slot) {
            self.follow_pane_focus();
        }
    }

    fn close_pane(&mut self, slot: usize) {
        self.split.close_slot(slot);
        self.follow_pane_focus();
    }

    /// Show the tab at strip index `index` in a new pane beside the focused one.
    pub(super) fn split_tab(&mut self, index: usize, side: Side) {
        let Some(uid) = self.tabs.get(index).and_then(Tab::pane_uid) else {
            return;
        };
        self.reconcile_panes();
        self.split.open_split(Some(uid), side);
        self.follow_pane_focus();
    }

    /// Open a session in a new pane beside the focused one. A session whose tab exists goes
    /// straight in. Otherwise the pane is reserved for the launch: it keeps the keyboard on the
    /// pane that has it until the session's tab exists, then that tab takes exactly this pane
    /// (see [`PaneClaim`]).
    pub(super) fn split_session(&mut self, entry: &SessionEntry, side: Side) {
        self.reconcile_panes();
        let path = entry.path.as_path();
        if let Some(i) = self
            .tabs
            .iter()
            .position(|t| t.session_path() == Some(path) && t.pane_uid().is_some())
        {
            match self.tabs[i].pane_uid() {
                Some(uid) if self.split.slot_of(uid).is_none() => self.split_tab(i, side),
                _ => self.open_session(entry),
            }
            return;
        }
        if self.spawn_queued_for(Some(path), None) {
            return; // already on its way
        }
        let ticket = self.split.reserve_split(side);
        self.pane_claims.push(PaneClaim {
            session_path: Some(entry.path.clone()),
            mux_name: None,
            ticket,
        });
        self.open_session(entry);
    }

    /// The pane reserved for the launch that produced a tab with this identity, if any.
    pub(super) fn take_pane_claim(
        &mut self,
        path: Option<&std::path::Path>,
        name: Option<&str>,
    ) -> Option<u64> {
        let at = self
            .pane_claims
            .iter()
            .position(|c| c.matches(path, name))?;
        Some(self.pane_claims.swap_remove(at).ticket)
    }

    /// A moved session's launch has a new identity: its claim follows.
    pub(super) fn retarget_pane_claim(
        &mut self,
        from: &std::path::Path,
        path: Option<PathBuf>,
        name: Option<String>,
    ) {
        for c in &mut self.pane_claims {
            if c.session_path.as_deref() == Some(from) {
                c.session_path.clone_from(&path);
                c.mux_name.clone_from(&name);
            }
        }
    }

    /// Drop claims whose launch is no longer on its way (it failed, was dropped, or the user
    /// closed the reserved pane): the reserved panes go away.
    fn prune_pane_claims(&mut self) {
        if self.pane_claims.is_empty() {
            return;
        }
        let claims = std::mem::take(&mut self.pane_claims);
        for c in claims {
            let path = c.session_path.as_deref();
            let pending = self.spawn_queued_for(path, c.mux_name.as_deref())
                || path.is_some_and(|p| {
                    self.moving.contains(p)
                        || self
                            .deferred_opens
                            .iter()
                            .any(|d| d.session_path.as_deref() == Some(p))
                });
            if !self.split.tickets().any(|t| t == c.ticket) {
                continue;
            }
            if pending {
                self.pane_claims.push(c);
            } else {
                self.split.release(c.ticket);
            }
        }
    }

    /// The open session tab to put in a new pane when the palette splits: the next session tab
    /// after the active one that no pane shows.
    fn spare_tabs(&self) -> Vec<u64> {
        let n = self.tabs.len();
        (1..=n)
            .map(|k| (self.active + k) % n)
            .filter_map(|i| self.tabs[i].pane_uid())
            .filter(|u| self.split.slot_of(*u).is_none())
            .collect()
    }

    /// Palette commands of the split view. False when `id` is not one of them.
    pub(super) fn run_split_command(&mut self, id: &str) -> bool {
        match id {
            "split.right" | "split.down" => {
                self.reconcile_panes();
                let side = if id == "split.right" {
                    Side::Right
                } else {
                    Side::Down
                };
                let spare = self.spare_tabs().first().copied();
                self.split.open_split(spare, side);
                self.follow_pane_focus();
            }
            "split.grid" => {
                self.reconcile_panes();
                let spare = self.spare_tabs();
                self.split.make_grid(&spare);
                self.follow_pane_focus();
            }
            "split.unsplit" => {
                self.split.unsplit();
                self.reconcile_panes();
                self.follow_pane_focus();
            }
            "pane.next" | "pane.prev" => {
                self.split
                    .cycle_focus(if id == "pane.next" { 1 } else { -1 });
                self.follow_pane_focus();
            }
            _ => return false,
        }
        true
    }

    /// Ctrl+Alt+Arrow moves pane focus. omp matches modifiers exactly and binds only plain
    /// Alt+Arrow, so it never sees these. Reserved only while a split is on screen.
    pub(super) fn split_shortcuts(&mut self, ctx: &egui::Context) {
        if !self.split_shown() || self.modal_open() {
            return;
        }
        let mods = Modifiers::COMMAND | Modifiers::ALT;
        let dir = ctx.input_mut(|i| {
            [
                (Key::ArrowLeft, Dir::Left),
                (Key::ArrowRight, Dir::Right),
                (Key::ArrowUp, Dir::Up),
                (Key::ArrowDown, Dir::Down),
            ]
            .into_iter()
            .find(|(key, _)| i.consume_key(mods, *key))
            .map(|(_, dir)| dir)
        });
        if let Some(dir) = dir
            && self.split.move_focus(dir)
        {
            self.follow_pane_focus();
        }
    }

    /// Draw the panes of the split: header, terminal, focus border, draggable dividers.
    pub(super) fn show_split(&mut self, ui: &mut Ui) {
        let pal = Palette::of_ui(ui);
        let mut frame = Requests::default();
        let mut refused = None;
        theme::skin::central(ui, pal, true, |ui| {
            let area = ui.max_rect();
            let rects = panes::pane_rects(self.split.layout(), area, self.split.ratios());
            let ctx = ui.ctx().clone();
            // Clicking app chrome must not toggle a terminal's focus (it sends ESC[I / ESC[O to
            // omp): only the terminal widgets ignore "click elsewhere".
            let surrender = ctx.options(|o| o.input_options.surrender_focus_on);
            ctx.options_mut(|o| o.input_options.surrender_focus_on = egui::SurrenderFocusOn::Never);
            let pressed = ui.input(|i| i.pointer.primary_pressed());
            let font_size = self.terminal_font_size();
            let focused_slot = self.split.focused();
            let mut any_focus = false;
            let mut focus_taken = false;
            for (slot, pane) in rects.iter().copied().enumerate() {
                let uid = self.split.slots()[slot];
                let index = uid.and_then(|u| self.tab_index(u));
                let is_focused = slot == focused_slot;
                let (status, title) = match index.map(|i| &self.tabs[i]) {
                    Some(tab) => (tab.status(true), tab.label(&self.labels).to_owned()),
                    None if self.split.is_reserved(slot) => {
                        (TabStatus::Idle, "Opening session…".to_owned())
                    }
                    None => (TabStatus::Idle, "Empty pane".to_owned()),
                };
                let header = pane_header(
                    ui,
                    slot,
                    panes::header_of(pane, HEADER_H),
                    pal,
                    status,
                    &title,
                    is_focused,
                    index.is_some(),
                    if index.is_some_and(|i| self.tabs[i].terminal().tsp().live()) {
                        ICON_MESSAGES_SQUARE
                    } else {
                        ICON_TERMINAL
                    },
                );
                if header.close {
                    frame.close = Some(slot);
                }
                let mut hit = pressed && header.pressed;
                let body = panes::body_of(pane, HEADER_H);
                if let Some(t) = index.map(|i| &mut self.tabs[i]) {
                    let mut child = ui.new_child(
                        UiBuilder::new()
                            .max_rect(body)
                            .layout(Layout::top_down(Align::Min)),
                    );
                    child.set_clip_rect(body.intersect(ui.clip_rect()));
                    let uid = t.uid;
                    let chat = t.terminal().tsp().live();
                    let (response, url) = if chat {
                        let out = crate::chat_view::ChatView::new(t.terminal_mut())
                            .id_salt(uid)
                            .font_size(font_size)
                            .show(&mut child);
                        refused = refused.or(out.refused);
                        if let Some(r) = out.redirect {
                            frame.redirect = Some((slot, r));
                        }
                        (out.response, out.open_url)
                    } else {
                        let out = TerminalView::new(t.terminal_mut())
                            .id_salt(uid)
                            .font_size(font_size)
                            .show(&mut child);
                        (out.response, out.open_url)
                    };
                    if self.focus_pending && is_focused {
                        response.request_focus();
                        focus_taken = true;
                    }
                    any_focus |= response.has_focus();
                    hit |= pressed && response.contains_pointer();
                    t.bell = false;
                    if url.is_some() {
                        if chat {
                            frame.chat_url = url;
                        } else {
                            frame.open_url = url;
                        }
                    }
                    match (!chat)
                        .then(|| terminal_menu(&response, t.terminal()))
                        .flatten()
                    {
                        Some(TermMenu::Copy) => {
                            t.terminal().copy_selection();
                        }
                        Some(TermMenu::Paste) => paste_from_menu(t.terminal_mut()),
                        Some(TermMenu::OpenLink(url)) => frame.open_url = Some(url),
                        Some(TermMenu::CopyLink(url)) => frame.copy_link = Some(url),
                        None => {}
                    }
                } else {
                    let waiting = self.split.is_reserved(slot);
                    ui.painter().rect_filled(body, 0, pal.bg);
                    ui.painter().text(
                        body.center(),
                        Align2::CENTER_CENTER,
                        if waiting {
                            "Starting the session…"
                        } else {
                            "Open a session here: click this pane, then a tab or a sidebar session"
                        },
                        theme::font(13.0),
                        pal.weak,
                    );
                    let resp = ui.interact(body, Id::unique(("pane-empty", slot)), Sense::click());
                    hit |= pressed && resp.contains_pointer();
                }
                let stroke = if is_focused {
                    Stroke::new(1.5, pal.accent)
                } else {
                    Stroke::new(1.0, pal.stroke)
                };
                ui.painter()
                    .rect_stroke(pane, 3, stroke, StrokeKind::Inside);
                if hit && !is_focused {
                    frame.focus = Some(slot);
                }
            }
            if focus_taken {
                self.focus_pending = false;
                ctx.request_repaint();
            }
            ctx.options_mut(|o| o.input_options.surrender_focus_on = surrender);
            // The focused pane has no terminal (an empty pane the user clicked): nothing may type
            // into the terminal that still holds egui's keyboard focus.
            if any_focus && self.split.focused_uid().is_none() {
                ctx.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
                any_focus = false;
            }
            self.terminal_focused = any_focus;

            for d in panes::dividers(self.split.layout(), area, self.split.ratios()) {
                let resp = ui.interact(
                    d.rect,
                    Id::unique(("split-divider", d.index)),
                    Sense::drag(),
                );
                let resp = resp.on_hover_cursor(if d.vertical {
                    CursorIcon::ResizeHorizontal
                } else {
                    CursorIcon::ResizeVertical
                });
                if resp.dragged()
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    frame
                        .dividers
                        .push((d.index, panes::ratio_at(&d, area, pos)));
                }
                if resp.hovered() || resp.dragged() {
                    let line = if d.vertical {
                        Rect::from_center_size(d.rect.center(), vec2(2.0, d.rect.height()))
                    } else {
                        Rect::from_center_size(d.rect.center(), vec2(d.rect.width(), 2.0))
                    };
                    ui.painter()
                        .rect_filled(line, 1, pal.accent.gamma_multiply(0.7));
                }
            }
        });
        if let Some(reason) = refused {
            self.tsp_refused(reason);
        }
        for (index, value) in frame.dividers {
            self.split.set_divider(index, value);
            self.dirty = true;
        }
        if let Some(slot) = frame.close {
            self.close_pane(slot);
        } else if let Some(slot) = frame.focus {
            self.focus_pane(slot);
        }
        // The redirect acts on the pane's tab: it is the active one first.
        if let Some((slot, redirect)) = frame.redirect {
            self.focus_pane(slot);
            self.apply_redirect(redirect);
        }
        if let Some(url) = frame.chat_url {
            self.open_chat_link(url);
        }
        if let Some(url) = frame.open_url {
            self.open_external(url);
        }
        if let Some(url) = frame.copy_link {
            self.copy_text(&url, "link");
        }
    }
}
