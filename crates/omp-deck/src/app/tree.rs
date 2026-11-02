//! App-side glue for the session tree window: the gateway topic that follows the session file,
//! the gates for "Go here" / "Fork", and the two operations — "Go here" through the live-control
//! request of the active tab, fork through the gateway.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use deck_core::ctl::{CtlCommand, CtlReply};
use deck_core::mux::{new_session_name, session_name_for};
use deck_core::tree::SessionTree;
use deck_service::gateway::protocol::{Done, Mutation, Topic, TopicValue};
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TreeOp {
    Fork,
    Navigate { user: bool },
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
            session,
            cwd,
            id,
            title,
            tree: None,
            error: None,
            revision: 0,
            seen: 0,
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
                return Err("Switch to this session's tab to use Go here");
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

    fn tree_navigate_gate(&self, session: &Path) -> Result<(), &'static str> {
        self.tree_link(session)?;
        if self.tree_busy.is_some() {
            return Err("Another tree action is still running");
        }
        let idle = self
            .live_snapshot()
            .and_then(|s| s.main_idle)
            .or_else(|| {
                self.tabs
                    .get(self.active)
                    .map(|t| Activity::of_terminal(t.terminal()) == Activity::Idle)
            })
            .unwrap_or(false);
        if idle {
            Ok(())
        } else {
            Err("omp is busy — wait for it to finish")
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
                now,
            },
        );
        for a in actions {
            match a {
                TreeAction::Close => self.close_tree(),
                TreeAction::Fork { entry, what } => self.tree_fork(&session, entry, what),
                TreeAction::Navigate { entry, user } => self.tree_navigate(&session, entry, user),
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

    fn tree_navigate(&mut self, session: &Path, entry: String, user: bool) {
        if self.tree_navigate_gate(session).is_err() {
            return;
        }
        let command = CtlCommand::Navigate {
            root: session.to_path_buf(),
            entry_id: entry,
        };
        if self.ctl_command(command, |app, r| app.on_tree_reply(r)) {
            self.tree_busy = Some(TreeBusy {
                op: TreeOp::Navigate { user },
                cwd: PathBuf::new(),
                title: String::new(),
                what: String::new(),
                seq: self.activation_seq,
            });
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
            self.tree_failed(busy.op, &why);
            return;
        }
        // Only "Go here" settles over the live link; a fork goes through the gateway.
        let TreeOp::Navigate { user } = busy.op else {
            return;
        };
        self.close_tree();
        self.toasts.success(
            &self.ctx,
            if user {
                "Moved before that prompt — its text is back in the editor"
            } else {
                "Moved there — new prompts continue from this point"
            },
        );
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
}
