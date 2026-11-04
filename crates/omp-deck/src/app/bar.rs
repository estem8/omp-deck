//! App-side glue for the status bar: the status the gateway follows for the active tab, the
//! model-switch observations, command submission and the small dialogs.
//!
//! Every command that writes into a session (status-bar actions, the palette, review text,
//! worktree move/finish) is submitted to the gateway's controller of that session with the
//! [`SessionTarget`] the user acted on; this module only turns the controller's events into
//! dialogs and toasts.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use deck_core::ctl::CtlCommand;
use deck_core::platform;
use deck_core::sessions::{SessionEntry, abbreviate_home};
use deck_core::status::SessionStats;
use deck_core::status::SessionStatus;
use deck_service::controller::{
    ActionFacts, Command, CommandKind, ControllerEvent, Delivery, Refusal, Rejected, RequestId,
    SessionTarget,
};
use deck_service::gateway::protocol::{
    Answer, Done, GatewayError, ModelFactsView, Mutation, Query, Topic,
};
use deck_service::inject::Requires;
use deck_service::session_actions::{self, Action, ActionCtx, CompactMode};
use egui::{RichText, TextEdit, Ui};

use super::{App, PendingConfirm};
use crate::card;
use crate::confirm::{self, Confirm, Dialog};
use crate::icons::{ICON_FOLD_VERTICAL, ICON_HANDSHAKE, ICON_PENCIL};
use crate::model_picker::{self, PickerAction, SessionExtras};
use crate::statusbar::{self, BarAction, BarInput};
use crate::tabs::{Tab, TabRole};
use crate::theme::Palette;

/// Terminal input is dropped for this long (renewed every frame) while a dialog is open.
const MODAL_HOLD: Duration = Duration::from_millis(250);

/// A text prompt opened from the bar's menus.
pub enum ActionDialog {
    Handoff { focus: String, first: bool },
    CompactFocus { focus: String, first: bool },
    Rename { title: String, first: bool },
}

/// The status the gateway follows for this client's active session, unwrapped once per gateway
/// revision (the bar compares snapshots by identity).
#[derive(Default)]
pub(super) struct StatusView {
    revision: u64,
    snapshot: Option<(SessionTarget, Arc<SessionStatus>)>,
}

impl App {
    /// The gateway's status of the active session tab (`None` while it is not the session the
    /// gateway currently follows for this client).
    pub(super) fn status_snapshot(&self) -> Option<Arc<SessionStatus>> {
        let target = self.active_target()?;
        let (t, s) = self.status.snapshot.as_ref()?;
        (t.key == target.key).then(|| s.clone())
    }

    /// Per frame, before drawing: follow the active tab, feed the bar and report what it shows
    /// about the model to the gateway (which resolves the optimistic switches).
    pub(super) fn drive_status(&mut self) {
        let want_uid = match self.tabs.get(self.active) {
            Some(t) if t.role == TabRole::Session => Some(t.uid),
            _ => None,
        };
        if want_uid != self.status_uid {
            self.status_uid = want_uid;
            self.bar_view = statusbar::BarView::default();
        }
        let revision = self.gw.revision(&Topic::Status);
        if revision != self.status.revision {
            self.status.revision = revision;
            self.status.snapshot = self.gw.status().map(|(t, s)| (t, Arc::new(s)));
        }
        let snap = self.status_snapshot();
        let live = self.live_snapshot();
        let mut observed = None;
        if let Some(tab) = self.tabs.get(self.active)
            && tab.role == TabRole::Session
        {
            let live_model = live.as_deref().and_then(|l| l.model.as_ref());
            let no_stats = SessionStats::default();
            let stats = snap.as_deref().map_or(&no_stats, |s| &s.stats);
            let state = statusbar::model_state(stats, live_model);
            self.bar_view.set_defaults(&self.omp_defaults);
            self.bar_view.set_live_model(live_model);
            self.bar_view
                .set_context(live.as_deref().and_then(|l| l.context.as_ref()));
            self.bar_view
                .set_account(live.as_deref().and_then(|l| l.account.as_ref()));
            self.bar_view.set_usage(self.usage.summary());
            self.bar_view.update(snap.as_ref(), &tab.cwd, &self.models);
            if tab.copy.is_none() {
                observed = Some((
                    tab.target().clone(),
                    ModelFactsView {
                        model: state.model.map(str::to_owned),
                        thinking: state.thinking.map(str::to_owned),
                        configured: state.configured.map(str::to_owned),
                        shown: self.bar_view.model_selector.clone(),
                        shown_is_default: self.bar_view.model_is_default,
                    },
                ));
            }
        }
        // Sent when it changed (and again after the link came back up: the gateway keeps no
        // observation across a reconnect).
        if let Some((target, facts)) = observed
            && self.model_sent.as_ref() != Some(&(target.clone(), facts.clone()))
        {
            self.gw.observe_model(target.clone(), facts.clone());
            self.model_sent = Some((target, facts));
        }
        self.hold_input_during_modals();
    }

    /// The gateway reported that omp did not confirm a model switch.
    pub(super) fn on_model_unconfirmed(
        &mut self,
        switch: &deck_service::gateway::protocol::SwitchInfo,
    ) {
        let pending = deck_service::model_switch::Pending {
            selector: switch.selector.clone(),
            level: switch.level.clone(),
            since: std::time::Instant::now(),
        };
        let desc = pending.describe(&|s| self.models.get(s).map(|m| m.name.clone()));
        self.toasts.error(
            &self.ctx,
            format!(
                "omp did not confirm the switch to {desc} — it may be unavailable or omp is busy; check the terminal"
            ),
        );
    }

    /// While a dialog or menu is up the terminal may still own the keyboard: Enter/Esc meant for
    /// the dialog must not reach omp.
    fn hold_input_during_modals(&mut self) {
        let now = std::time::Instant::now();
        if self.confirm.is_some()
            || self.folder_dialog.is_some()
            || self.wt.dialog.is_some()
            || self.popup.is_some()
            || self.tree.is_some()
            || self.settings_open
            || self.usage_open
            || self.sessions.open
            || self.palette.is_some()
            || self.mobile_modal_open()
            || egui::Popup::is_any_open(&self.ctx)
        {
            let visible = self.visible_uids();
            for t in self.tabs.iter() {
                if visible.contains(&t.uid) {
                    t.terminal().hold_input_until(now + MODAL_HOLD);
                }
            }
        }
    }

    /// Carries out what a controller reported to this client: the draft prompt, and the end of
    /// each command it submitted.
    pub(super) fn on_controller_event(&mut self, target: &SessionTarget, event: ControllerEvent) {
        let Some(uid) = self
            .tabs
            .iter()
            .find(|t| t.target().key == target.key)
            .map(|t| t.uid)
        else {
            // The tab was closed since: a draft question no one can see is declined.
            if let ControllerEvent::Confirm { id, .. } = event {
                self.answer_target(target.clone(), id, false);
            }
            return;
        };
        match event {
            ControllerEvent::Confirm { id, label, draft } => {
                if self.confirm.is_none() {
                    self.confirm = Some((
                        Confirm {
                            title: "Clear your unsent message?".to_owned(),
                            body: format!(
                                "The composer holds “{}”. Running {} clears it first.",
                                deck_service::statusfmt::ellipsize_left(&draft, 80),
                                label
                            ),
                            confirm_label: "Clear and run".to_owned(),
                            danger: true,
                        },
                        PendingConfirm::ClearDraft { uid, id },
                    ));
                } else {
                    self.answer_draft(uid, id, false);
                }
            }
            ControllerEvent::Ended {
                kind,
                label,
                delivery,
                ..
            } => self.command_ended(uid, kind, &label, &delivery),
        }
    }

    /// A command of tab `uid` reached its final delivery state. "Delivered" means the transport
    /// accepted the bytes, not that omp (or the host's PTY) executed anything; "Unobserved" means
    /// they were queued but no acknowledgement was seen before the view closed. Only the waiting UI
    /// state (review comments, worktree moves) acts on "Delivered".
    fn command_ended(&mut self, uid: u64, kind: CommandKind, label: &str, delivery: &Delivery) {
        let finish = kind == CommandKind::WorktreeFinish;
        // Both retry by themselves; only the final failure is announced.
        let quiet = finish || kind == CommandKind::MoveTo;
        // The finish prompt retries by itself; only its final failure is announced.
        let note = match delivery {
            Delivery::Refused(Refusal::NoBracketedPaste) => Some(format!(
                "{label} refused: omp has not enabled bracketed paste"
            )),
            Delivery::Refused(Refusal::Precondition(Requires::Idle)) if !quiet => Some(format!(
                "{label} needs omp to be idle — try again when it finishes"
            )),
            Delivery::Refused(Refusal::DialogOpen) if !quiet => {
                Some("Close omp's open dialog first".to_owned())
            }
            Delivery::Aborted if !quiet => Some(format!(
                "{label} cancelled: omp started working — check the composer"
            )),
            Delivery::Stalled if !quiet => Some(format!(
                "{label} cancelled: omp did not show it in time — check the composer"
            )),
            // Never a success claim and never retried: it may still be written.
            Delivery::Unobserved => Some(format!(
                "{label} may not have reached omp — check the session"
            )),
            _ => None,
        };
        if let Some(note) = note {
            self.toasts.info(&self.ctx, note);
        }
        let lost = matches!(
            delivery,
            Delivery::Refused(Refusal::Precondition(_) | Refusal::DialogOpen)
                | Delivery::Aborted
                | Delivery::Stalled
        ) || (matches!(kind, CommandKind::WorktreeMove | CommandKind::MoveTo)
            && matches!(delivery, Delivery::Cancelled(_)));
        // A move that never went in is not waited for.
        if lost && kind == CommandKind::WorktreeMove {
            self.forget_move(uid);
        }
    }

    /// Close tab `i` (the user's close click): this window detaches its view. The gateway lets a
    /// command that is mid-way finish and drops the queued ones before it closes the controller
    /// (closing must not cut a paste in half).
    pub(super) fn request_close(&mut self, i: usize) {
        self.close_tab(i, true);
    }

    /// The user's answer to the draft prompt.
    pub(super) fn answer_draft(&mut self, uid: u64, id: RequestId, clear: bool) {
        let target = self
            .tabs
            .iter()
            .find(|t| t.uid == uid)
            .map(|t| t.target().clone());
        if let Some(target) = target {
            self.answer_target(target, id, clear);
        }
        self.ctx.request_repaint();
    }

    /// An answer for a confirmation the controller no longer waits for (the session changed, the
    /// request was dropped) is refused there.
    fn answer_target(&mut self, target: SessionTarget, id: RequestId, clear: bool) {
        self.gw.mutate(
            Mutation::Answer { target, id, clear },
            |_, result| match result {
                Ok(Done::Answered(false)) => log::debug!("the draft question is gone"),
                Ok(_) => {}
                Err(e) => log::debug!("the draft answer was refused: {e}"),
            },
        );
    }

    /// What the availability rules of the session actions see for `tab` (status bar and palette).
    pub(super) fn action_ctx(&self, tab: &Tab) -> ActionCtx {
        tab.action_ctx(ActionFacts {
            last_turn_failed: self.bar_view.last_turn_failed,
            has_last_reply: self.bar_view.has_reply,
            omp_found: self.omp_found(),
        })
    }

    pub(super) fn show_status_bar(&mut self, ui: &mut Ui) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.role != TabRole::Session {
            return;
        }
        let input = BarInput {
            catalog: &self.models,
            favorites: &self.cfg.favorite_models,
            provider: self.cfg.model_provider_filter.as_deref(),
            activity: tab.activity(),
            pending: tab.pending_switch(),
            assumed: tab.assumed_switch(),
            actions: self.action_ctx(tab),
            agents: statusbar::AgentsChip {
                counts: self.agents_view.counts,
                open: self.dash.open,
            },
            session: SessionExtras {
                tab: tab.uid,
                task_unavailable: model_picker::task_unavailable(self.ctl_link()),
            },
        };
        let actions = statusbar::show(ui, &self.bar_view, &mut self.picker, &input);
        if actions.is_empty() {
            return;
        }
        self.focus_pending = true;
        for a in actions {
            self.apply_bar_action(a);
        }
    }

    fn apply_bar_action(&mut self, action: BarAction) {
        match action {
            BarAction::Picker(PickerAction::Refresh) => self.reload_models(),
            BarAction::Picker(PickerAction::Switch { selector, level }) => {
                self.queue_change(&selector, level.as_deref());
            }
            BarAction::Picker(PickerAction::ToggleFavorite(selector)) => {
                self.cfg.toggle_favorite(&selector);
                self.dirty = true;
            }
            BarAction::Picker(PickerAction::SetProvider(provider)) => {
                self.cfg.model_provider_filter = provider;
                self.dirty = true;
            }
            BarAction::Picker(PickerAction::Effort(level)) => self.queue_effort(level),
            BarAction::TaskModel(model) => self.set_task_model(model),
            BarAction::Run(a) => self.run_action(a),
            BarAction::AskHandoff => self.open_action_dialog(ActionDialog::Handoff {
                focus: String::new(),
                first: true,
            }),
            BarAction::AskCompactFocus => self.open_action_dialog(ActionDialog::CompactFocus {
                focus: String::new(),
                first: true,
            }),
            BarAction::AskRename => self.open_action_dialog(ActionDialog::Rename {
                title: String::new(),
                first: true,
            }),
            BarAction::CopyPath(p) => self.copy_text(&p, "path"),
            BarAction::OpenUrl(u) => self.open_external(u),
            BarAction::ToggleAgents => self.toggle_dashboard(true),
            BarAction::ToggleChanges => self.toggle_changes_panel(),
            BarAction::OpenTree => self.open_tree_active(),
            BarAction::OpenUsage => self.open_usage(),
        }
    }

    /// Asks the gateway for omp's model catalog and, beside it, its settings (the picker's roles).
    pub(super) fn reload_models(&mut self) {
        if !self.omp_found() {
            return;
        }
        self.reload_roles();
        if self.models.is_loading() {
            return;
        }
        self.models.begin();
        self.gw.query(Query::Models, |app, result| {
            let models = result.and_then(|a| match a {
                Answer::Models(m) => Ok(m),
                other => Err(format!("unexpected answer {other:?}")),
            });
            app.models.finish(models);
            app.ctx.request_repaint();
        });
    }

    /// Re-reads omp's settings for the picker's quick roles (`modelRoles`) and the configured
    /// task-agent model; a failed read keeps the roles known so far.
    fn reload_roles(&mut self) {
        self.gw
            .query(Query::OmpSettings, |app, result| match result {
                Ok(Answer::OmpSettings(catalog)) => {
                    app.models.set_roles(&catalog);
                    app.ctx.request_repaint();
                }
                Ok(other) => log::debug!("roles: unexpected answer {other:?}"),
                Err(e) => log::debug!("roles: omp settings unavailable: {e}"),
            });
    }

    /// Task mode of the model picker: sets (`Some`) or clears (`None`) the active tab's
    /// session-only task-agent model override through live control, then tells the user what omp
    /// answered. The reply is remembered for the picker's summary: omp has no way to read it back.
    pub(super) fn set_task_model(&mut self, model: Option<String>) {
        let Some(uid) = self
            .tabs
            .get(self.active)
            .filter(|t| t.role == TabRole::Session)
            .map(|t| t.uid)
        else {
            return;
        };
        let root = self.ctl_root().map(PathBuf::from);
        let wanted = model.clone();
        self.ctl_command(
            CtlCommand::TaskModelOverride { root, model },
            move |app, reply| {
                match reply {
                    Ok(r) if r.ok => {
                        let now = r.value.as_ref().and_then(|v| v.as_str()).map(str::to_owned);
                        app.picker.record_task_override(uid, now.clone());
                        let text = match (wanted, now) {
                            (Some(m), _) => format!(
                                "Task agents use {m} (this session only). Use /agents to persist."
                            ),
                            (None, Some(m)) => {
                                format!("Override cleared: task agents use {m} again")
                            }
                            (None, None) => {
                                "Override cleared: task agents use the session's model".to_owned()
                            }
                        };
                        app.toasts.success(&app.ctx, text);
                    }
                    Ok(r) => app.toasts.error(
                        &app.ctx,
                        format!(
                            "Task model not changed: {}",
                            r.error.as_deref().unwrap_or("omp refused it")
                        ),
                    ),
                    Err(e) => app
                        .toasts
                        .error(&app.ctx, format!("Task model not changed: {e}")),
                }
                app.ctx.request_repaint();
            },
        );
    }

    /// The model the active session runs as far as the deck knows: an unconfirmed switch's
    /// model (omp applies it before anything queued now), else an assumed switch's model while
    /// the bar can only show omp's defaults (the same rule as the model chip).
    pub(super) fn current_model(&self) -> Option<String> {
        match self.tabs.get(self.active) {
            Some(t) => t.current_model(),
            _ => self.bar_view.model_selector.clone(),
        }
    }

    /// The session the status bar and palette act on now: the active session tab's current target.
    pub(super) fn active_target(&self) -> Option<SessionTarget> {
        match self.tabs.get(self.active) {
            Some(t) if t.role == TabRole::Session => Some(t.target().clone()),
            _ => None,
        }
    }

    /// Opens a text prompt for the active session. The dialog is bound to the session (and
    /// generation) it was opened on: if that session changed or went away by the time the user
    /// confirms, nothing is sent.
    pub(super) fn open_action_dialog(&mut self, dialog: ActionDialog) {
        let Some(target) = self.active_target() else {
            return;
        };
        self.dialog = Some(dialog);
        self.dialog_target = Some(target);
    }

    /// Submits `command` to the session `target` names. A refusal is told to the user (one that
    /// only restates a no-op stays silent). The final delivery arrives as a controller event,
    /// except for a change the gateway made over live control without typing anything: that one
    /// ends with omp's own message (a quiet toast) or its error.
    pub(super) fn send_command(&mut self, target: &SessionTarget, command: Command) {
        // Actions and model switches that find omp gone stay silent, as the bar always did.
        let quiet_exit = matches!(
            command,
            Command::Action(_) | Command::SwitchModel { .. } | Command::SetEffort { .. }
        );
        let direct = Rc::new(Cell::new(false));
        let handled = Rc::clone(&direct);
        self.gw.submit(
            target.clone(),
            command,
            move |app, _key, result| match result {
                Ok(submitted) => {
                    handled.set(submitted.is_none());
                    app.ctx.request_repaint();
                }
                Err(GatewayError::Rejected(why)) => app.report_rejected(&why, quiet_exit),
                Err(e) => app.toasts.error(&app.ctx, format!("Nothing was sent: {e}")),
            },
            move |app, result| {
                if direct.get() {
                    match direct_notice(result, quiet_exit) {
                        DirectNotice::Success(text) => app.toasts.success(&app.ctx, text),
                        DirectNotice::Failure(text) => app.toasts.error(&app.ctx, text),
                        DirectNotice::Quiet => {}
                    }
                    return;
                }
                // The delivery itself is reported by the controller's events; what is left is a
                // request whose fate the gateway could not record.
                if let Err(e) = result
                    && e != "cancelled"
                {
                    app.toasts.info(
                        &app.ctx,
                        "A command may not have reached omp — check the session",
                    );
                }
            },
        );
    }

    fn report_rejected(&mut self, why: &Rejected, quiet_exit: bool) {
        match why {
            // Nothing to do: the request restated the state the session already has.
            Rejected::NoChange | Rejected::NotInjected => {}
            Rejected::Exited if quiet_exit => {}
            Rejected::Exited => self
                .toasts
                .error(&self.ctx, "omp has exited in this tab; nothing was sent"),
            Rejected::ModelUnknown => self
                .toasts
                .info(&self.ctx, "The current model is not known yet"),
            other => self.toasts.error(&self.ctx, other.to_string()),
        }
    }

    /// Send `/switch` or `/effort` to the active tab; the gateway shows the optimistic state.
    pub(super) fn queue_change(&mut self, selector: &str, level: Option<&str>) {
        let Some(target) = self.active_target() else {
            return;
        };
        self.send_command(
            &target,
            Command::SwitchModel {
                selector: selector.to_owned(),
                level: level.map(str::to_owned),
            },
        );
    }

    /// `/effort` on the active session's current model.
    pub(super) fn queue_effort(&mut self, level: String) {
        let Some(target) = self.active_target() else {
            return;
        };
        self.send_command(&target, Command::SetEffort { level });
    }

    pub(super) fn run_action(&mut self, action: Action) {
        match action {
            Action::Fork => self.fork_active(),
            Action::CopyLast => match self
                .status_snapshot()
                .and_then(|s| s.stats.last_assistant_text.clone())
            {
                Some(text) => self.copy_text(&text, "last response"),
                None => self
                    .toasts
                    .info(&self.ctx, "There is no assistant reply to copy yet"),
            },
            Action::ExportHtml => self.export_active(),
            other => {
                if let Some(target) = self.active_target() {
                    self.send_command(&target, Command::Action(other));
                }
            }
        }
    }

    pub(super) fn active_session(&self) -> Option<PathBuf> {
        self.tabs
            .get(self.active)
            .and_then(Tab::session_path)
            .map(PathBuf::from)
    }

    pub(super) fn find_entry(&self, path: &std::path::Path) -> Option<SessionEntry> {
        let groups = self.groups()?;
        groups
            .iter()
            .flat_map(|g| g.sessions.iter())
            .find(|e| e.path == path)
            .cloned()
    }

    /// Fork = `omp --fork <file>` in a new tab, linked like a copy once its file appears.
    fn fork_active(&mut self) {
        if !self.omp_found() {
            return;
        }
        let Some(path) = self.active_session() else {
            return;
        };
        match self.find_entry(&path) {
            Some(entry) => {
                let dest = entry.cwd.clone();
                self.copy_session(&entry, &dest);
            }
            None => {
                self.refresh_sessions();
                self.toasts.info(
                    &self.ctx,
                    "That session is not in the sidebar yet — try again in a moment",
                );
            }
        }
    }

    /// The gateway exports the session to HTML; the result opens with the desktop's default
    /// handler.
    fn export_active(&mut self) {
        let (true, Some(file)) = (self.omp_found(), self.active_session()) else {
            return;
        };
        self.toasts.info(&self.ctx, "Exporting session to HTML…");
        let dest = session_actions::exports_dir();
        self.gw
            .mutate(Mutation::ExportHtml { file, dest }, |app, result| {
                let path = match result {
                    Ok(Done::Path(p)) => p,
                    Ok(other) => {
                        app.toasts.error(
                            &app.ctx,
                            format!("Export failed: unexpected answer {other:?}"),
                        );
                        return;
                    }
                    Err(e) => {
                        app.toasts.error(&app.ctx, e);
                        return;
                    }
                };
                let shown = abbreviate_home(&path);
                app.gw.native(
                    move || platform::open_path(path.as_os_str()).map_err(|e| e.to_string()),
                    move |app, opened| match opened {
                        Ok(()) => app.toasts.success(&app.ctx, format!("Exported to {shown}")),
                        Err(e) => app.toasts.error(
                            &app.ctx,
                            format!("Exported to {shown} but opening it failed: {e}"),
                        ),
                    },
                );
            });
    }

    // ------------------------------------------------------------ dialogs

    pub(super) fn show_action_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.dialog.as_mut() else {
            return;
        };
        // The tab the dialog acts on (it was active when the dialog opened; modals keep it so).
        let active = self.tabs.get(self.active);
        let session = active.map_or("", |t| {
            crate::titlebar::strip_omp_prefix(t.title(&self.labels))
        });
        let project = active.map(|t| abbreviate_home(&t.cwd)).unwrap_or_default();
        let facts = |ui: &mut Ui| {
            let pal = Palette::of_ui(ui);
            card::kv(ui, 84.0, "Session", |ui| {
                ui.add(
                    egui::Label::new(crate::theme::strong(session).color(pal.text_strong))
                        .truncate(),
                );
            });
            if !project.is_empty() {
                card::kv(ui, 84.0, "Project", |ui| {
                    ui.add(egui::Label::new(RichText::new(&project).color(pal.text)).truncate());
                });
            }
        };
        let result = match dialog {
            ActionDialog::Handoff { focus, first } => confirm::show_dialog(
                ctx,
                &Dialog {
                    id: "handoff",
                    title: "Hand off to a fresh context",
                    width: 440.0,
                    confirm_label: "Hand off",
                    confirm_enabled: true,
                    danger: false,
                    enter_confirms: false,
                    ctrl_enter_confirms: true,
                    icon: Some(ICON_HANDSHAKE),
                    subtitle: Some("Continues in a new context from a summary"),
                    ..Default::default()
                },
                |ui| {
                    facts(ui);
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("omp summarizes the session into a handoff document and continues from it. Optionally tell it what to focus on.")
                            .color(Palette::of_ui(ui).weak),
                    );
                    focus_editor(ui, focus, first, true, "Focus instructions (optional)");
                },
            ),
            ActionDialog::CompactFocus { focus, first } => confirm::show_dialog(
                ctx,
                &Dialog {
                    id: "compact-focus",
                    title: "Compact with focus",
                    width: 440.0,
                    confirm_label: "Compact",
                    confirm_enabled: !focus.trim().is_empty(),
                    danger: false,
                    enter_confirms: false,
                    icon: Some(ICON_FOLD_VERTICAL),
                    subtitle: Some("Summarizes older turns, keeps what you name"),
                    ..Default::default()
                },
                |ui| {
                    facts(ui);
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("What should the compacted context keep?")
                            .color(Palette::of_ui(ui).weak),
                    );
                    focus_editor(ui, focus, first, true, "e.g. the auth refactor decisions");
                },
            ),
            ActionDialog::Rename { title, first } => confirm::show_dialog(
                ctx,
                &Dialog {
                    id: "rename",
                    title: "Rename session",
                    width: 420.0,
                    confirm_label: "Rename",
                    confirm_enabled: !title.trim().is_empty(),
                    danger: false,
                    enter_confirms: true,
                    icon: Some(ICON_PENCIL),
                    subtitle: Some("Shown in tabs and session lists"),
                    ..Default::default()
                },
                |ui| {
                    facts(ui);
                    focus_editor(ui, title, first, false, "New title");
                },
            ),
        };
        let Some(ok) = result else {
            return;
        };
        self.focus_pending = true;
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        let target = self.dialog_target.take();
        if !ok {
            return;
        }
        let action = match dialog {
            ActionDialog::Handoff { focus, .. } => {
                Action::Handoff(Some(focus).filter(|f| !f.trim().is_empty()))
            }
            ActionDialog::CompactFocus { focus, .. } => Action::Compact(CompactMode::Focus(focus)),
            ActionDialog::Rename { title, .. } => Action::Rename(title),
        };
        if let Some(target) = target {
            self.send_command(&target, Command::Action(action));
        }
    }
}

/// Text field for the dialogs; grabs focus the first time it is shown.
fn focus_editor(ui: &mut Ui, text: &mut String, first: &mut bool, multiline: bool, hint: &str) {
    ui.add_space(6.0);
    let edit = if multiline {
        TextEdit::multiline(text).desired_rows(4)
    } else {
        TextEdit::singleline(text)
    };
    let r = ui.add(
        edit.hint_text(hint)
            .desired_width(f32::INFINITY)
            .margin(egui::vec2(10.0, 8.0)),
    );
    if *first {
        r.request_focus();
        *first = false;
    }
}

/// What the end of a change the gateway made over live control (nothing typed into omp) tells
/// the user.
#[derive(Debug, PartialEq, Eq)]
enum DirectNotice {
    /// omp's own words about what it did (`Session-only model: p/m`).
    Success(String),
    /// omp (or the gateway) could not make the change.
    Failure(String),
    /// Nothing to say: the typed fallback reports itself through the controller's events, and a
    /// refusal that only restates the session's state stays silent as it always did.
    Quiet,
}

/// The final result of a live-control change. A typed fallback (omp must compact first) ends
/// with some other [`Done`] after its draft confirm; cancelling that confirm is not an error.
fn direct_notice(result: Result<Done, String>, quiet_exit: bool) -> DirectNotice {
    match result {
        Ok(Done::Message(text)) if !text.trim().is_empty() => DirectNotice::Success(text),
        Ok(_) => DirectNotice::Quiet,
        Err(e) => {
            let rejected = |why: Rejected| GatewayError::Rejected(why).to_string();
            let restated = e == rejected(Rejected::NoChange)
                || e == rejected(Rejected::NotInjected)
                || (quiet_exit && e == rejected(Rejected::Exited));
            if restated || e == GatewayError::Cancelled.to_string() {
                DirectNotice::Quiet
            } else {
                DirectNotice::Failure(e)
            }
        }
    }
}

#[cfg(test)]
mod direct_tests {
    use super::*;

    #[test]
    fn omps_message_of_a_live_control_change_is_a_quiet_success() {
        assert_eq!(
            direct_notice(
                Ok(Done::Message("Session-only model: fake/fake-2".into())),
                true
            ),
            DirectNotice::Success("Session-only model: fake/fake-2".into())
        );
        assert_eq!(
            direct_notice(Ok(Done::Message("  ".into())), true),
            DirectNotice::Quiet
        );
    }

    #[test]
    fn a_failed_live_control_change_is_told_and_a_restated_state_is_not() {
        assert_eq!(
            direct_notice(Err("Unknown model: nope".into()), true),
            DirectNotice::Failure("Unknown model: nope".into())
        );
        let text = |why| GatewayError::Rejected(why).to_string();
        for quiet in [Rejected::NoChange, Rejected::NotInjected] {
            assert_eq!(direct_notice(Err(text(quiet)), true), DirectNotice::Quiet);
        }
        assert_eq!(
            direct_notice(Err(text(Rejected::Exited)), true),
            DirectNotice::Quiet
        );
        assert_eq!(
            direct_notice(Err(text(Rejected::Exited)), false),
            DirectNotice::Failure(text(Rejected::Exited))
        );
        // The typed fallback: its draft confirm is the report, and declining it is not an error.
        assert_eq!(
            direct_notice(Err("cancelled".into()), true),
            DirectNotice::Quiet
        );
    }
}
