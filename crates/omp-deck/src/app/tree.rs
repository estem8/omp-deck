//! App-side glue for the session tree window: the gateway topic that follows the session file,
//! the gates for "Go here" / "Fork", and the two operations — "Go here" through the live-control
//! request of the active tab, fork through the gateway.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use deck_core::ctl::{BranchSummary, CtlCommand, CtlReply};
use deck_core::mux::{new_session_name, session_name_for};
use deck_core::omp::SettingsCatalog;
use deck_core::tree::SessionTree;
use deck_service::gateway::protocol::{Answer, Done, Mutation, Query, Topic, TopicValue};
use egui::{Key, Modifiers};

use super::App;
use super::spawn::Focus;
use crate::agent_ctl::Link;
use crate::tabs::{ClosedTab, TabRole};
use crate::titlebar;
use crate::toast::ToastKind;
use crate::tree_view::{self, TreeAction, TreeInput, TreeState};
use deck_service::statusfmt::Activity;

/// The open tree window.
pub(super) struct TreeUi {
    state: TreeState,
    /// The session file shown.
    session: PathBuf,
    /// Working directory a forked tab starts in.
    cwd: PathBuf,
    /// The session id (running checks against tmux names).
    id: Option<String>,
    title: String,
    /// The gateway's parsed tree, shared with the view between revisions.
    tree: Option<Arc<SessionTree>>,
    error: Option<String>,
    /// The tree's own revision (the view re-reads when it moves).
    revision: u64,
    /// The cache revision of the topic last taken.
    seen: u64,
    /// omp's `branchSummary.enabled`, read when the window opened (`None` until it answers or
    /// when it cannot be read: Go here then does not ask).
    summary_enabled: Option<bool>,
    /// A label change is on its way to the session file.
    labeling: bool,
}

/// What the open tabs say about the session being forked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TabFact {
    /// No live tab shows the file.
    None,
    /// The active tab shows it with a live control link.
    ActiveLive,
    /// A tab with a control socket shows it, but it is not the active one.
    Background,
    /// A tab shows it without usable live control.
    NoControl,
}

/// What the tmux snapshot says about the session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RunFact {
    /// No snapshot yet.
    Unknown,
    /// tmux cannot be polled; nothing can be known.
    Blind,
    /// A tmux session runs it; `control` = its control socket exists.
    Running {
        control: bool,
    },
    Stopped,
}

struct ForkFacts {
    busy: bool,
    tab: TabFact,
    omp: bool,
    run: RunFact,
}

const RUNNING: &str = "Session is running in the background without live control — open it and restart with live control to fork";

/// The reason the Fork button is disabled, if any.
fn fork_gate(f: &ForkFacts) -> Result<(), &'static str> {
    if f.busy {
        return Err("Another tree action is still running");
    }
    match f.tab {
        TabFact::ActiveLive => return Ok(()),
        TabFact::Background => return Err("Switch to this session's tab to fork it"),
        TabFact::NoControl => return Err(RUNNING),
        TabFact::None => {}
    }
    if !f.omp {
        return Err("omp was not found");
    }
    match f.run {
        RunFact::Running { control: true } | RunFact::Stopped | RunFact::Blind => Ok(()),
        RunFact::Running { control: false } => Err(RUNNING),
        RunFact::Unknown => Err("Still checking which sessions are running"),
    }
}

/// Whether a live-control tree action (Go here, Re-answer) is available: the tab's control link,
/// then "nothing else running", then "omp idle". The first reason wins.
fn live_action_gate(
    link: Result<(), &'static str>,
    busy: bool,
    idle: bool,
) -> Result<(), &'static str> {
    link?;
    if busy {
        return Err("Another tree action is still running");
    }
    if idle {
        Ok(())
    } else {
        Err("omp is busy — wait for it to finish")
    }
}

/// omp's `branchSummary.enabled` in the settings catalog (its effective value, else the default).
fn branch_summary_enabled(catalog: &SettingsCatalog) -> Option<bool> {
    catalog
        .tabs
        .iter()
        .flat_map(|t| &t.settings)
        .find(|s| s.key == "branchSummary.enabled")
        .and_then(|s| s.value.as_ref().or(s.default.as_ref()))
        .and_then(serde_json::Value::as_bool)
}

/// omp's status for a Go here to the entry the tab is already on.
const ALREADY_THERE: &str = "Already at this point";

/// What a settled Go here means for the window.
#[derive(Debug, PartialEq, Eq)]
struct NavigateOutcome {
    /// Close the tree and return to the tab (not when nothing moved).
    close: bool,
    /// The toast text, and whether it is a success (otherwise a plain notice).
    toast: String,
    success: bool,
}

/// omp's own `message` is shown as it is; "Already at this point" keeps the window open.
fn navigate_outcome(user: bool, message: Option<&str>) -> NavigateOutcome {
    match message {
        Some(m) => NavigateOutcome {
            close: m != ALREADY_THERE,
            toast: m.to_owned(),
            success: false,
        },
        None => NavigateOutcome {
            close: true,
            toast: if user {
                "Moved before that prompt — its text is back in the editor"
            } else {
                "Moved there — new prompts continue from this point"
            }
            .to_owned(),
            success: true,
        },
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TreeOp {
    Fork,
    /// `summarizing`: omp is writing a summary of the branch left (can take a while).
    Navigate {
        user: bool,
        summarizing: bool,
    },
    /// omp asks the question again in the tab; settles when it is answered or cancelled.
    Reanswer,
}
/// The one operation in flight (a second one is refused until it settles).
pub(super) struct TreeBusy {
    op: TreeOp,
    cwd: PathBuf,
    title: String,
    what: String,
    /// `activation_seq` when requested: the result may take focus only if still unchanged.
    seq: u64,
}

impl App {
    /// Open the tree of `session` (file-based; works for any session).
    pub(super) fn open_tree(
        &mut self,
        session: PathBuf,
        cwd: PathBuf,
        title: String,
        id: Option<String>,
    ) {
        self.close_tree_topic();
        self.gw.subscribe(Topic::Tree(session.clone()));
        self.tree = Some(TreeUi {
            state: TreeState::new(title.clone()),
            session: session.clone(),
            cwd,
            id,
            title,
            tree: None,
            error: None,
            revision: 0,
            seen: 0,
            summary_enabled: None,
            labeling: false,
        });
        // Go here asks about a branch summary only while omp's setting is on.
        self.gw.query(Query::OmpSettings, move |app, result| {
            let enabled = match result {
                Ok(Answer::OmpSettings(c)) => branch_summary_enabled(&c),
                _ => None,
            };
            if let Some(t) = app.tree.as_mut().filter(|t| t.session == session) {
                t.summary_enabled = enabled;
                app.ctx.request_repaint();
            }
        });
        self.ctx.request_repaint();
    }

    /// Stops following the open tree's session file.
    fn close_tree_topic(&mut self) {
        if let Some(t) = self.tree.take() {
            self.gw.unsubscribe(&Topic::Tree(t.session));
        }
    }

    pub(super) fn close_tree(&mut self) {
        self.close_tree_topic();
        self.focus_pending = true;
    }

    /// The tree of the active session tab.
    pub(super) fn open_tree_active(&mut self) {
        let Some(t) = self.tabs.get(self.active) else {
            return;
        };
        if t.role != TabRole::Session {
            return;
        }
        let Some(session) = t.session_path_owned() else {
            self.toasts.info(
                &self.ctx,
                "The session has no file yet — send a prompt first",
            );
            return;
        };
        let cwd = t.cwd.clone();
        let title = titlebar::strip_omp_prefix(t.title(&self.labels)).to_owned();
        let id = self.groups().and_then(|groups| {
            groups
                .iter()
                .flat_map(|g| &g.sessions)
                .find(|e| e.path == session)
                .map(|e| e.id.clone())
        });
        self.open_tree(session, cwd, title, id);
    }

    /// Ctrl+Shift+B: same gating as the agent hub.
    pub(super) fn toggle_tree(&mut self) {
        if self.tree.is_some() {
            self.close_tree();
        } else if self.active_session_tab() {
            self.open_tree_active();
        }
    }

    /// Esc is handled inside the window; it only needs to know a menu is not open.
    pub(super) fn tree_shortcut(&mut self, ctx: &egui::Context) -> bool {
        ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::B))
    }

    /// Whether a live (not exited) tab shows `session`.
    fn tree_has_tab(&self, session: &Path) -> bool {
        self.tabs
            .iter()
            .any(|t| t.session_path() == Some(session) && !t.terminal().is_exited())
    }

    /// The active tab shows `session` and has a live control link.
    fn tree_link(&self, session: &Path) -> Result<(), &'static str> {
        let active = match self.tabs.get(self.active) {
            Some(t) if t.role == TabRole::Session && t.session_path() == Some(session) => t,
            _ if self.tree_has_tab(session) => {
                return Err("Switch to this session's tab first");
            }
            _ => return Err("Open the session in a tab with live control first"),
        };
        if active.terminal().is_exited() {
            return Err("omp has exited in this tab");
        }
        if self.ctl_link() != Link::Live {
            return Err(
                "Live control is not connected for this tab (restart it from the Agents panel)",
            );
        }
        Ok(())
    }

    /// Go here and Re-answer: both drive the active tab's omp, which must be idle.
    fn tree_navigate_gate(&self, session: &Path) -> Result<(), &'static str> {
        let idle = self
            .live_snapshot()
            .and_then(|s| s.main_idle)
            .or_else(|| {
                self.tabs
                    .get(self.active)
                    .map(|t| Activity::of_terminal(t.terminal()) == Activity::Idle)
            })
            .unwrap_or(false);
        live_action_gate(self.tree_link(session), self.tree_busy.is_some(), idle)
    }

    /// Labels go through the gateway (live tab or file), one at a time.
    fn tree_label_gate(&self) -> Result<(), &'static str> {
        if self.tree_busy.is_some() || self.tree.as_ref().is_some_and(|t| t.labeling) {
            Err("Another tree action is still running")
        } else {
            Ok(())
        }
    }

    /// The session running `session` in a backend while no tab shows it has a live control
    /// endpoint (the gateway forks over it).
    fn tree_bg_control(&self, session: &Path) -> bool {
        let snap = self.running_snapshot();
        let name = snap.holder(session).map(str::to_owned).or_else(|| {
            let id = self.tree.as_ref()?.id.as_deref()?;
            let canonical = session_name_for(id);
            (snap.is_alive(&canonical) && snap.files_of(&canonical).is_empty()).then_some(canonical)
        });
        name.is_some_and(|n| snap.has_ctl(&n))
    }

    /// Why a fork of `session` is not offered right now (the gateway decides the fork itself).
    fn tree_fork_gate(&self, session: &Path) -> Result<(), &'static str> {
        let tab = self
            .tabs
            .iter()
            .enumerate()
            .find(|(_, t)| t.session_path() == Some(session) && !t.terminal().is_exited());
        let tab = match tab {
            None => TabFact::None,
            Some((i, _)) if i == self.active && self.tree_link(session).is_ok() => {
                TabFact::ActiveLive
            }
            Some((i, t)) if i != self.active && t.info().is_some_and(|i| i.live_control) => {
                TabFact::Background
            }
            Some(_) => TabFact::NoControl,
        };
        let run = match self.gw.running() {
            Some(snap) => {
                let id = self.tree.as_ref().and_then(|u| u.id.as_deref());
                if snap.holder(session).is_some()
                    || id.is_some_and(|id| snap.is_running(id, session))
                {
                    RunFact::Running {
                        control: self.tree_bg_control(session),
                    }
                } else {
                    RunFact::Stopped
                }
            }
            None if self.gw.running_blind() => RunFact::Blind,
            None => RunFact::Unknown,
        };
        fork_gate(&ForkFacts {
            busy: self.tree_busy.is_some(),
            tab,
            omp: self.omp_found(),
            run,
        })
    }

    /// Takes a newer tree of the open window's session from the gateway's cache.
    fn take_tree(&mut self) {
        let Some(session) = self.tree.as_ref().map(|t| t.session.clone()) else {
            return;
        };
        let topic = Topic::Tree(session);
        let rev = self.gw.revision(&topic);
        if self.tree.as_ref().is_none_or(|t| t.seen == rev) {
            return;
        }
        let Some(TopicValue::Tree {
            revision,
            tree,
            error,
        }) = self.gw.topic(&topic)
        else {
            return;
        };
        if let Some(t) = self.tree.as_mut() {
            t.seen = rev;
            t.revision = revision;
            t.tree = tree.map(Arc::new);
            t.error = error;
        }
    }

    /// Per frame: draw the window and run what it asks for.
    pub(super) fn show_tree(&mut self, ctx: &egui::Context) {
        let Some(session) = self.tree.as_ref().map(|t| t.session.clone()) else {
            return;
        };
        self.take_tree();
        let navigate = self.tree_navigate_gate(&session);
        let fork = self.tree_fork_gate(&session);
        let label = self.tree_label_gate();
        let busy = self.tree_busy.as_ref().map(|b| match b.op {
            TreeOp::Navigate {
                summarizing: true, ..
            } => "Summarizing the branch… this can take a while",
            TreeOp::Navigate { .. } => "Moving to the entry…",
            TreeOp::Reanswer => "Waiting for your answer in the tab…",
            TreeOp::Fork => "Forking…",
        });
        let live = self
            .live_snapshot()
            .filter(|_| self.ctl_root() == Some(session.as_path()));
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        let Some(ui_state) = self.tree.as_mut() else {
            return;
        };
        let actions = tree_view::show(
            ctx,
            &mut ui_state.state,
            &TreeInput {
                tree: ui_state.tree.as_ref(),
                revision: ui_state.revision,
                error: ui_state.error.as_deref(),
                live_leaf: live.as_ref().and_then(|s| s.leaf.as_deref()),
                navigate,
                fork,
                summary_prompt: ui_state.summary_enabled == Some(true),
                label,
                reanswer: navigate,
                busy,
                now,
            },
        );
        for a in actions {
            match a {
                TreeAction::Close => self.close_tree(),
                TreeAction::Fork { entry, what } => self.tree_fork(&session, entry, what),
                TreeAction::Navigate {
                    entry,
                    user,
                    summary,
                } => self.tree_navigate(&session, entry, user, summary),
                TreeAction::SetLabel { entry, label } => {
                    self.tree_set_label(&session, entry, label)
                }
                TreeAction::Reanswer { entry } => self.tree_reanswer(&session, entry),
                TreeAction::Copy { text, what } => self.copy_text(&text, what),
                TreeAction::Notice(text) => self.toasts.info(&self.ctx, text),
            }
        }
    }

    fn tree_fork(&mut self, session: &Path, entry: String, what: String) {
        let Some((cwd, title)) = self.tree.as_ref().map(|t| (t.cwd.clone(), t.title.clone()))
        else {
            return;
        };
        if self.tree_busy.is_some() {
            return;
        }
        if self.tree_fork_gate(session).is_err() {
            return;
        }
        let busy = TreeBusy {
            op: TreeOp::Fork,
            cwd,
            title,
            what,
            seq: self.activation_seq,
        };
        // The gateway forks over the running session's control socket when there is one, headless
        // when nothing runs the file, and refuses a session that runs without live control.
        self.tree_busy = Some(busy);
        self.toasts.info(&self.ctx, "Forking…");
        self.gw.mutate(
            Mutation::ForkAt {
                file: session.to_path_buf(),
                entry_id: entry,
            },
            |app, r| {
                let result = r.and_then(|d| match d {
                    Done::Path(file) => Ok(file),
                    _ => Err("the gateway answered with something else".to_owned()),
                });
                app.on_tree_forked(result);
            },
        );
    }

    fn tree_navigate(&mut self, session: &Path, entry: String, user: bool, summary: BranchSummary) {
        if self.tree_navigate_gate(session).is_err() {
            return;
        }
        let summarizing = summary != BranchSummary::None;
        let command = CtlCommand::Navigate {
            root: session.to_path_buf(),
            entry_id: entry,
            summary,
        };
        if self.ctl_command(command, |app, r| app.on_tree_reply(r)) {
            self.tree_busy = Some(TreeBusy {
                op: TreeOp::Navigate { user, summarizing },
                cwd: PathBuf::new(),
                title: String::new(),
                what: String::new(),
                seq: self.activation_seq,
            });
        }
    }

    /// Set or remove the label of `entry`; the tree topic brings the change back into the view.
    fn tree_set_label(&mut self, session: &Path, entry: String, label: Option<String>) {
        if self.tree_label_gate().is_err() {
            return;
        }
        if let Some(t) = self.tree.as_mut() {
            t.labeling = true;
        }
        let removing = label.is_none();
        self.gw.mutate(
            Mutation::SessionLabel {
                file: session.to_path_buf(),
                entry_id: entry,
                label,
            },
            move |app, result| app.on_tree_labeled(removing, result),
        );
    }

    fn on_tree_labeled(&mut self, removing: bool, result: Result<Done, String>) {
        if let Some(t) = self.tree.as_mut() {
            t.labeling = false;
        }
        // A live tab answers with omp's reply, a closed session with nothing.
        let failure = match result {
            Ok(Done::Ctl(r)) if !r.ok => Some(r.error.unwrap_or_else(|| "omp refused".to_owned())),
            Ok(_) => None,
            Err(e) => Some(crate::summary::strip_context(&e).to_owned()),
        };
        match failure {
            Some(why) => self.toasts.notify(
                &self.ctx,
                ToastKind::Error,
                format!("Could not change the label: {why}"),
            ),
            None => self.toasts.success(
                &self.ctx,
                if removing {
                    "Label removed"
                } else {
                    "Label saved"
                },
            ),
        }
    }

    /// Re-answer an `ask` result: omp asks the question again in the tab, so the tree gives way
    /// to it. The reply comes when the question is answered (or cancelled).
    fn tree_reanswer(&mut self, session: &Path, entry: String) {
        if self.tree_navigate_gate(session).is_err() {
            return;
        }
        let command = CtlCommand::ReanswerAsk {
            root: session.to_path_buf(),
            entry_id: entry,
        };
        if self.ctl_command(command, |app, r| app.on_tree_reply(r)) {
            self.tree_busy = Some(TreeBusy {
                op: TreeOp::Reanswer,
                cwd: PathBuf::new(),
                title: String::new(),
                what: String::new(),
                seq: self.activation_seq,
            });
            self.close_tree();
            self.toasts
                .info(&self.ctx, "Answer the question in the tab");
        }
    }

    /// The live-control reply that settles the tree operation.
    fn on_tree_reply(&mut self, reply: Result<CtlReply, String>) {
        let Some(busy) = self.tree_busy.take() else {
            return;
        };
        let reply = match reply {
            Ok(r) => r,
            Err(e) => {
                self.tree_failed(busy.op, &e);
                return;
            }
        };
        if !reply.ok {
            let why = reply
                .error
                .clone()
                .unwrap_or_else(|| "omp refused".to_owned());
            // Cancelling the question is the user's own choice, not a failure.
            if busy.op == TreeOp::Reanswer && why == "cancelled" {
                self.toasts.info(&self.ctx, "Re-answer cancelled");
            } else {
                self.tree_failed(busy.op, &why);
            }
            return;
        }
        match busy.op {
            // A fork goes through the gateway, not this link.
            TreeOp::Fork => {}
            TreeOp::Reanswer => self.toasts.success(&self.ctx, "Answer replaced"),
            TreeOp::Navigate { user, .. } => {
                let outcome = navigate_outcome(user, reply.message.as_deref());
                if outcome.close {
                    self.close_tree();
                }
                if outcome.success {
                    self.toasts.success(&self.ctx, outcome.toast);
                } else {
                    self.toasts.info(&self.ctx, outcome.toast);
                }
            }
        }
    }

    /// The gateway's fork finished.
    fn on_tree_forked(&mut self, result: Result<PathBuf, String>) {
        let Some(busy) = self.tree_busy.take() else {
            return;
        };
        match result {
            Ok(file) => self.tree_forked(&busy, file),
            Err(e) => self.tree_failed(TreeOp::Fork, &e),
        }
    }

    fn tree_forked(&mut self, busy: &TreeBusy, file: PathBuf) {
        self.close_tree();
        let name = self
            .backends_available()
            .then(|| new_session_name(SystemTime::now()));
        // Like every queued launch: focus only while the user has not switched tabs since asking.
        let focus = if busy.seq == self.activation_seq {
            Focus::Claim
        } else {
            Focus::Never
        };
        self.open_resolved_focus(
            ClosedTab {
                mux_name: name,
                cwd: busy.cwd.clone(),
                session_path: Some(file),
                title: format!("{} (fork)", busy.title),
                view: self.cfg.session_view,
            },
            focus,
        );
        self.toasts
            .success(&self.ctx, format!("{} — opened in a new tab", busy.what));
        self.refresh_sessions();
    }

    fn tree_failed(&mut self, op: TreeOp, why: &str) {
        let what = match op {
            TreeOp::Fork => "fork",
            TreeOp::Navigate { .. } => "go there",
            TreeOp::Reanswer => "re-answer",
        };
        let text = if why.eq_ignore_ascii_case("busy") {
            "omp is busy — try again when it is idle".to_owned()
        } else {
            why.to_owned()
        };
        self.toasts.notify(
            &self.ctx,
            ToastKind::Error,
            format!("Could not {what}: {text}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(tab: TabFact, run: RunFact) -> ForkFacts {
        ForkFacts {
            busy: false,
            tab,
            omp: true,
            run,
        }
    }

    #[test]
    fn closed_tab_with_a_live_session_needs_its_control_socket_to_fork() {
        assert_eq!(
            fork_gate(&facts(TabFact::None, RunFact::Running { control: false })),
            Err(RUNNING)
        );
        assert_eq!(
            fork_gate(&facts(TabFact::None, RunFact::Running { control: true })),
            Ok(())
        );
    }

    #[test]
    fn idle_closed_session_may_fork() {
        assert_eq!(fork_gate(&facts(TabFact::None, RunFact::Stopped)), Ok(()));
        assert_eq!(fork_gate(&facts(TabFact::None, RunFact::Blind)), Ok(()));
        assert!(fork_gate(&facts(TabFact::None, RunFact::Unknown)).is_err());
    }

    #[test]
    fn open_tabs_decide_the_gate() {
        let run = RunFact::Stopped;
        assert_eq!(fork_gate(&facts(TabFact::ActiveLive, run)), Ok(()));
        assert!(fork_gate(&facts(TabFact::Background, run)).is_err());
        assert_eq!(fork_gate(&facts(TabFact::NoControl, run)), Err(RUNNING));
    }

    #[test]
    fn busy_and_missing_omp_refuse() {
        let mut f = facts(TabFact::ActiveLive, RunFact::Stopped);
        f.busy = true;
        assert!(fork_gate(&f).is_err());
        let mut f = facts(TabFact::None, RunFact::Stopped);
        f.omp = false;
        assert!(fork_gate(&f).is_err());
    }

    #[test]
    fn live_actions_report_the_first_reason() {
        assert_eq!(live_action_gate(Ok(()), false, true), Ok(()));
        assert_eq!(
            live_action_gate(Err("no link"), true, false),
            Err("no link"),
            "no link beats busy beats not-idle"
        );
        assert_eq!(
            live_action_gate(Ok(()), true, false),
            Err("Another tree action is still running")
        );
        assert_eq!(
            live_action_gate(Ok(()), false, false),
            Err("omp is busy — wait for it to finish")
        );
    }

    #[test]
    fn omp_status_is_shown_and_only_a_move_closes_the_tree() {
        let moved = navigate_outcome(true, None);
        assert!(moved.close && moved.success);
        assert!(moved.toast.contains("text is back in the editor"));
        // Nothing moved: say so and stay in the tree.
        let same = navigate_outcome(false, Some("Already at this point"));
        assert!(!same.close && !same.success);
        assert_eq!(same.toast, "Already at this point");
        // Any other status of omp is shown as it is, after the move.
        let other = navigate_outcome(false, Some("Summary written"));
        assert!(other.close);
        assert_eq!(other.toast, "Summary written");
    }

    #[test]
    fn the_branch_summary_prompt_follows_the_effective_setting() {
        use deck_core::omp::{Setting, SettingKind, SettingsTab};
        use serde_json::json;
        let catalog = |value: Option<serde_json::Value>, default: Option<serde_json::Value>| {
            SettingsCatalog {
                tabs: vec![SettingsTab {
                    id: "t".to_owned(),
                    label: "T".to_owned(),
                    settings: vec![Setting {
                        key: "branchSummary.enabled".to_owned(),
                        kind: SettingKind::Bool,
                        value,
                        default,
                        description: String::new(),
                        secret: false,
                    }],
                }],
            }
        };
        assert_eq!(
            branch_summary_enabled(&catalog(Some(json!(true)), Some(json!(false)))),
            Some(true)
        );
        assert_eq!(
            branch_summary_enabled(&catalog(None, Some(json!(true)))),
            Some(true)
        );
        assert_eq!(
            branch_summary_enabled(&catalog(Some(json!(false)), None)),
            Some(false)
        );
        assert_eq!(branch_summary_enabled(&catalog(None, None)), None);
        assert_eq!(
            branch_summary_enabled(&SettingsCatalog { tabs: Vec::new() }),
            None
        );
    }
}
