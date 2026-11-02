//! App-side glue for notifications. The gateway classifies turns, keeps the inbox and shows the
//! desktop pop-ups; this window reports whether it is focused and which sessions it shows, draws
//! the inbox and the bells, and raises itself when a pop-up was clicked.

use deck_service::controller::SessionTarget;
use deck_service::gateway::protocol::{ClientState, Mutation};
use egui::{UserAttentionType, ViewportCommand};

use super::App;
#[cfg(target_os = "linux")]
use crate::notify::Signal;
use crate::notify::WindowRaiser;
use crate::notify_panel::{self, PanelAction};
use crate::notify_view::NotifyAction;
use crate::settings_page::OmpDeckPage;
use crate::tabs::{ClosedTab, TabRole};

pub(super) struct NotifyUi {
    raiser: WindowRaiser,
    window_attached: bool,
    /// What the gateway was last told about this window.
    reported: Option<ClientState>,
}

impl NotifyUi {
    pub(super) fn new(ctx: &egui::Context) -> NotifyUi {
        NotifyUi {
            raiser: WindowRaiser::start(ctx.clone()),
            window_attached: false,
            reported: None,
        }
    }

    /// The link came back up: the gateway keeps no view state of a lost connection.
    pub(super) fn forget_report(&mut self) {
        self.reported = None;
    }
}

impl App {
    /// Per tick (from `App::logic`, so also while the window is minimized or on another
    /// desktop): tell the gateway whether this window is focused and which sessions it shows.
    pub(super) fn drive_notifications(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if !self.notify.window_attached {
            self.notify.window_attached = true;
            self.notify.raiser.attach_window(frame);
        }
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        let visible = self.visible_uids();
        let state = ClientState {
            focused,
            visible: self
                .tabs
                .iter()
                .filter(|t| t.role == TabRole::Session && visible.contains(&t.uid))
                .map(|t| t.target().clone())
                .collect(),
            active: self.active_target(),
        };
        if self.notify.reported.as_ref() != Some(&state) {
            self.gw.set_client_state(state.clone());
            self.notify.reported = Some(state);
        }
        #[cfg(target_os = "linux")]
        for signal in self.notify.raiser.drain() {
            match signal {
                Signal::RaiseFailed => self.raise_attention(),
            }
        }
    }

    /// The tab showing exactly this session generation, if one is open: a click on a pop-up of a
    /// replaced conversation finds nothing instead of activating the new one.
    fn tab_of_target(&self, target: &SessionTarget) -> Option<usize> {
        self.tabs.iter().position(|t| t.target() == target)
    }

    /// A finished turn of `target`: the tab's bell marker.
    pub(super) fn on_bell(&mut self, target: &SessionTarget) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.target() == target) {
            t.bell = true;
        }
    }

    /// The gateway reports a click on a desktop notification of `target` (or of the inbox):
    /// show that session's tab, then bring the window to the front.
    pub(super) fn on_activate(&mut self, target: &SessionTarget, token: Option<&str>) {
        self.show_session_of(target);
        self.raise_window(token);
    }

    /// Activates the tab of `target`; when this window has none, opens a view of the session
    /// the gateway runs (if it still runs it).
    fn show_session_of(&mut self, target: &SessionTarget) {
        if let Some(i) = self.tab_of_target(target) {
            self.activate(i);
            return;
        }
        let Some(info) = self.gw.controller(target) else {
            return;
        };
        let mux_name = match &info.backend {
            deck_service::gateway::protocol::BackendRef::Mux { name, .. } => Some(name.clone()),
            deck_service::gateway::protocol::BackendRef::Direct => None,
        };
        self.open_resolved(ClosedTab {
            mux_name,
            cwd: info.cwd.clone(),
            session_path: info.session_path.clone(),
            title: info.label.clone(),
            view: info.view,
        });
    }

    /// Bring the window to the front: the compositor's activation token on Wayland, else the
    /// windowing system's focus request.
    pub(super) fn raise_window(&mut self, token: Option<&str>) {
        self.ctx
            .send_viewport_cmd(ViewportCommand::Minimized(false));
        self.ctx.send_viewport_cmd(ViewportCommand::Focus);
        let tried = token.is_some_and(|t| self.notify.raiser.raise_with_token(t));
        if !tried {
            self.raise_attention();
        }
        self.focus_pending = true;
    }

    /// Windows' focus request is forceful enough; elsewhere ask the shell to flag the window.
    fn raise_attention(&self) {
        if !cfg!(windows) {
            self.ctx
                .send_viewport_cmd(ViewportCommand::RequestUserAttention(
                    UserAttentionType::Informational,
                ));
        }
    }

    pub(super) fn apply_notify_action(&mut self, action: NotifyAction) {
        let cfg = &mut self.cfg.notify;
        match action {
            NotifyAction::Desktop(v) => cfg.desktop = v,
            NotifyAction::TurnFinished(v) => cfg.turn_finished = v,
            NotifyAction::Attention(v) => cfg.attention = v,
            NotifyAction::Exited(v) => cfg.exited = v,
            NotifyAction::OnlyWhenUnfocused(v) => cfg.only_when_unfocused = v,
            NotifyAction::WhileViewing(v) => cfg.while_viewing = v,
            NotifyAction::Sound(v) => cfg.sound = v,
            NotifyAction::Test => {
                self.send_test_notification();
                return;
            }
        }
        self.dirty = true;
    }

    /// The inbox popover asked for something.
    pub(super) fn apply_inbox_action(&mut self, action: PanelAction) {
        match action {
            PanelAction::Open(target) => {
                // The gateway drops the entry and withdraws its pop-up; this window shows it.
                self.gw
                    .mutate(Mutation::InboxOpen(target.clone()), |app, result| {
                        if let Err(e) = result {
                            app.toasts
                                .error(&app.ctx, format!("Could not open the entry: {e}"));
                        }
                    });
                self.show_session_of(&target);
                self.raise_window(None);
            }
            PanelAction::Dismiss(target) => {
                self.gw
                    .mutate(Mutation::InboxDismiss(target), |app, result| {
                        if let Err(e) = result {
                            app.toasts
                                .error(&app.ctx, format!("Could not dismiss the entry: {e}"));
                        }
                    });
            }
            PanelAction::Settings => {
                self.open_settings_page(OmpDeckPage::Notifications);
            }
            PanelAction::ClearAll => {
                self.gw.mutate(Mutation::InboxClear, |app, result| {
                    if let Err(e) = result {
                        app.toasts
                            .error(&app.ctx, format!("Could not clear the inbox: {e}"));
                    }
                });
            }
        }
    }

    /// Opens the inbox popover under the bell.
    pub(super) fn open_inbox_panel(&self) {
        egui::Popup::open_id(&self.ctx, notify_panel::popup_id());
    }

    /// Asks the gateway for a sample desktop notification.
    pub(super) fn send_test_notification(&mut self) {
        self.gw
            .mutate(Mutation::InboxTest, |app, result| match result {
                Ok(_) => app.toasts.info(&app.ctx, "Test notification sent"),
                Err(e) => app.toasts.error(
                    &app.ctx,
                    format!("The test notification could not be sent: {e}"),
                ),
            });
    }
}
