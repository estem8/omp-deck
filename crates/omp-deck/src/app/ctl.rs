//! App-side glue for live subagent control: the gateway's live-control state of the active tab,
//! the commands the hub and popup send through it, and "restart with live control" for tabs that
//! were started without the extension.
//!
//! The gateway owns the control sockets; this window only subscribes to the active session's
//! `Topic::Live` and sends `Mutation::Ctl` commands, whose replies arrive as closures.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use deck_core::config::SessionView;
use deck_core::ctl::{CtlCommand, CtlReply, LiveSnapshot};
use deck_core::sessions::SessionEntry;
use deck_service::controller::{SessionKey, SessionTarget};
use deck_service::gateway::protocol::{Done, LaunchSpec, Mutation, SessionRole, Topic, TopicValue};
use deck_service::statusfmt::Activity;

use super::{App, PendingConfirm, RestartTarget};
use crate::agent_ctl::{self, Link, LinkFacts, Req, ReqKind, Restart, RestartFacts};
use crate::confirm::Confirm;
use crate::tabs::{ClosedTab, TabRole};

/// The session whose live-control state this window follows, and what was last read from it.
#[derive(Default)]
pub(super) struct LiveFollow {
    key: Option<SessionKey>,
    revision: u64,
    snapshot: Option<Arc<LiveSnapshot>>,
}

impl App {
    /// Per frame: follow the active tab's live-control topic (one session at a time).
    pub(super) fn drive_ctl(&mut self) {
        let want: Option<SessionKey> = match self.tabs.get(self.active) {
            Some(t)
                if t.role == TabRole::Session && t.info().is_some_and(|i| i.ctl_key.is_some()) =>
            {
                Some(t.target().key.clone())
            }
            _ => None,
        };
        if want != self.live.key {
            // Requests belong to the old session; their replies are ignored from now on.
            self.ctl_reqs.clear();
            self.ctl_errors.clear();
            if let Some(old) = self.live.key.take() {
                self.gw.unsubscribe(&Topic::Live(old));
            }
            self.live = LiveFollow::default();
            if let Some(key) = &want {
                self.gw.subscribe(Topic::Live(key.clone()));
            }
            self.live.key = want;
        }
        if let Some(key) = &self.live.key {
            let topic = Topic::Live(key.clone());
            let revision = self.gw.revision(&topic);
            if revision != self.live.revision {
                self.live.revision = revision;
                self.live.snapshot = match self.gw.topic(&topic) {
                    Some(TopicValue::Live {
                        connected: true,
                        snapshot: Some(snap),
                        ..
                    }) => Some(Arc::from(snap)),
                    _ => None,
                };
            }
        }
        if self.ctl_link() == Link::Connecting {
            // The grace period ends without any other event.
            self.ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    /// omp's own agent states, while the gateway's socket of the active tab is up and reports the
    /// session file the tab shows (a snapshot of another root session — `/new`, `/resume`, a
    /// stale socket — must not be overlaid). A session that is not persisted yet (no message
    /// sent) has no file of its own: then the file omp announced for this tab must match.
    pub(super) fn live_snapshot(&self) -> Option<Arc<LiveSnapshot>> {
        let snap = self.live.snapshot.clone()?;
        let file = snap.session_file.as_deref()?;
        let ours = match self.ctl_root() {
            Some(root) => root == file,
            None => self
                .status_snapshot()
                .is_some_and(|s| s.fresh && s.session.as_deref() == Some(file)),
        };
        ours.then_some(snap)
    }

    /// The session file of the active session tab: the root commands are addressed to.
    pub(super) fn ctl_root(&self) -> Option<&std::path::Path> {
        match self.tabs.get(self.active) {
            Some(t) if t.role == TabRole::Session => t.session_path(),
            _ => None,
        }
    }

    pub(super) fn ctl_link(&self) -> Link {
        let Some(t) = self.tabs.get(self.active) else {
            return Link::ReadOnly;
        };
        agent_ctl::link_state(&LinkFacts {
            connected: t.info().is_some_and(|i| i.live_control),
            ended: t.terminal().is_exited(),
            seen: t.ctl_seen,
            waited: t.ctl_expect().then(|| t.spawned.elapsed()),
        })
    }

    /// Whether the active tab can be restarted into a controlled session.
    pub(super) fn restart_offer(&self) -> Restart {
        let Some(t) = self.tabs.get(self.active) else {
            return Restart::Hidden;
        };
        if t.role != TabRole::Session || t.terminal().is_exited() {
            return Restart::Hidden;
        }
        agent_ctl::restart_offer(&RestartFacts {
            link: self.ctl_link(),
            backend: self.backends_available() && t.mux_name.is_some(),
            has_session_file: t.session_path().is_some(),
            extension: self.gw.gateway().is_some_and(|g| g.live_control),
            idle: Activity::of_terminal(t.terminal()) == Activity::Idle,
        })
    }

    // ---------------------------------------------------------------- agent commands

    /// Ask before killing `id`.
    pub(super) fn request_kill_agent(&mut self, id: String) {
        // Bind the question to the tab (and session) it is asked about.
        let Some((uid, key, session)) = (match self.tabs.get(self.active) {
            Some(t) if t.role == TabRole::Session => t
                .info()
                .and_then(|i| i.ctl_key.clone())
                .map(|key| (t.uid, key, t.session_path_owned())),
            _ => None,
        }) else {
            self.toasts
                .error(&self.ctx, "Live control is not connected for this tab");
            return;
        };
        let running = self
            .agents_view
            .agents()
            .iter()
            .position(|a| a.id == id)
            .and_then(|i| self.agents_view.cards.get(i))
            .is_some_and(|c| c.live == Some(deck_core::ctl::LiveStatus::Running));
        let body = if running {
            format!(
                "“{id}” is working right now. Killing it aborts the step it is on and releases \
                 the agent. Its transcript stays readable."
            )
        } else {
            format!("Release “{id}”? Its transcript stays readable.")
        };
        self.confirm = Some((
            Confirm {
                title: format!("Kill agent “{id}”?"),
                body,
                confirm_label: "Kill agent".to_owned(),
                danger: true,
            },
            PendingConfirm::KillAgent {
                id,
                uid,
                key,
                session,
            },
        ));
    }

    /// The user confirmed a kill: send it for the tab it was asked in, but only while that tab,
    /// its control key and its session file are unchanged.
    pub(super) fn confirm_kill_agent(
        &mut self,
        id: String,
        uid: u64,
        key: &str,
        session: Option<&std::path::Path>,
    ) {
        let same_tab = self.tabs.get(self.active).is_some_and(|t| {
            t.uid == uid
                && t.role == TabRole::Session
                && !t.terminal().is_exited()
                && t.info().and_then(|i| i.ctl_key.as_deref()) == Some(key)
                && t.session_path() == session
        });
        if !same_tab {
            self.toasts.error(
                &self.ctx,
                format!("Kill of “{id}” cancelled: the session it was asked about changed"),
            );
            return;
        }
        self.send_ctl(id, ReqKind::Kill, |id, root| CtlCommand::Kill { id, root });
    }

    pub(super) fn send_ctl_revive(&mut self, id: String) {
        self.send_ctl(id, ReqKind::Revive, |id, root| CtlCommand::Revive {
            id,
            root,
        });
    }

    pub(super) fn send_ctl_chat(&mut self, agent: String, text: String) {
        self.send_ctl(agent, ReqKind::Chat, |id, root| CtlCommand::Chat {
            id,
            text,
            root,
        });
    }

    /// The active session tab's target, when live control can be addressed.
    fn ctl_target(&self) -> Option<SessionTarget> {
        match self.tabs.get(self.active) {
            Some(t)
                if t.role == TabRole::Session && t.info().is_some_and(|i| i.ctl_key.is_some()) =>
            {
                Some(t.target().clone())
            }
            _ => None,
        }
    }

    /// Sends `command` to the active session's live-control socket; `done` gets omp's reply (or
    /// why none came). `false` (with a toast) when this tab has no live control.
    pub(super) fn ctl_command(
        &mut self,
        command: CtlCommand,
        done: impl FnOnce(&mut App, Result<CtlReply, String>) + 'static,
    ) -> bool {
        let Some(target) = self.ctl_target() else {
            self.toasts
                .error(&self.ctx, "Live control is not connected for this tab");
            return false;
        };
        self.gw
            .mutate(Mutation::Ctl { target, command }, move |app, result| {
                done(
                    app,
                    result.and_then(|d| match d {
                        Done::Ctl(reply) => Ok(reply),
                        other => Err(format!("unexpected answer {other:?}")),
                    }),
                );
            });
        true
    }

    /// Send a command addressed to the active tab's current session file.
    fn send_ctl(
        &mut self,
        agent: String,
        kind: ReqKind,
        build: impl FnOnce(String, PathBuf) -> CtlCommand,
    ) {
        let Some(root) = self.ctl_root().map(std::path::Path::to_path_buf) else {
            self.toasts
                .error(&self.ctx, "Live control is not connected for this tab");
            return;
        };
        let cmd = build(agent.clone(), root);
        if kind == ReqKind::Chat {
            self.ctl_errors.remove(&agent);
        }
        self.ctl_seq += 1;
        let seq = self.ctl_seq;
        let sent = self.ctl_command(cmd, move |app, reply| app.on_ctl_reply(seq, reply));
        if sent {
            self.ctl_reqs.insert(seq, Req { agent, kind });
        }
    }

    fn on_ctl_reply(&mut self, seq: u64, reply: Result<CtlReply, String>) {
        // A reply for a request of a session this window no longer follows is dropped.
        let Some(Req { agent, kind }) = self.ctl_reqs.remove(&seq) else {
            return;
        };
        let failure = match reply {
            Ok(r) if r.ok => None,
            Ok(r) => Some(r.error.unwrap_or_else(|| "omp refused".to_owned())),
            Err(e) => Some(e),
        };
        let Some(why) = failure else {
            match kind {
                ReqKind::Chat => {
                    self.ctl_errors.remove(&agent);
                    if let Some(p) = self.popup.as_mut().filter(|p| p.agent_id == agent) {
                        p.draft.clear();
                    }
                }
                ReqKind::Kill => self.toasts.success(&self.ctx, format!("Killed “{agent}”")),
                ReqKind::Revive => self.toasts.success(&self.ctx, format!("Revived “{agent}”")),
            }
            return;
        };
        let what = match kind {
            ReqKind::Kill => "kill",
            ReqKind::Revive => "revive",
            ReqKind::Chat => "message",
        };
        self.toasts
            .error(&self.ctx, format!("Could not {what} “{agent}”: {why}"));
        if kind == ReqKind::Chat {
            self.ctl_errors.insert(agent, why);
        }
    }

    // ---------------------------------------------------------------- restart with control

    pub(super) fn request_restart_live(&mut self) {
        if self.restart_offer() != Restart::Ready {
            return;
        }
        let Some(t) = self.tabs.get(self.active) else {
            return;
        };
        let Some(session) = t.session_path_owned() else {
            return;
        };
        let (uid, title) = (t.uid, t.title(&self.labels).to_owned());
        self.confirm = Some((
            Confirm {
                title: "Restart with live control?".to_owned(),
                body: format!(
                    "Stops the omp process behind “{title}” and resumes the same session in \
                     this tab with the control extension, so agents can be killed, messaged and \
                     revived from the hub. The conversation is kept."
                ),
                confirm_label: "Restart".to_owned(),
                danger: false,
            },
            PendingConfirm::RestartLive { uid, session },
        ));
    }

    /// The gateway kills the tab's session and relaunches it with the extension; the tab stays
    /// where it is and takes a view of the new generation.
    pub(super) fn execute_restart_live(&mut self, uid: u64, session: &std::path::Path) {
        // The question was asked a while ago: the tab must still be the active, idle, read-only
        // session it was about.
        let unchanged = self.restart_offer() == Restart::Ready
            && matches!(
                self.tabs.get(self.active),
                Some(t) if t.uid == uid && t.session_path() == Some(session)
            );
        if !unchanged {
            self.toasts.error(
                &self.ctx,
                "Restart cancelled: the session changed or is busy again",
            );
            return;
        }
        self.restart_tab_in_place(uid, None);
    }

    /// `Mutation::Restart` of the tab's session, in `view` (`None`: the view it runs in);
    /// its view is replaced by one on the result. Restarting into the other view is how a tab
    /// switches between the terminal and the chat view.
    pub(super) fn restart_tab_in_place(&mut self, uid: u64, view: Option<SessionView>) {
        let Some(target) = self
            .tabs
            .iter()
            .find(|t| t.uid == uid)
            .map(|t| t.target().clone())
        else {
            return;
        };
        self.toasts.info(&self.ctx, "Restarting session…");
        self.gw
            .mutate(Mutation::Restart { target, view }, move |app, result| {
                if matches!(&result, Ok(Done::Session(_, SessionView::Terminal)))
                    && view == Some(SessionView::Chat)
                {
                    app.toasts.info(
                        &app.ctx,
                        "Opened in terminal view because the OmpDeck session host is unavailable",
                    );
                }
                app.on_tab_restarted(uid, result);
            });
    }

    fn on_tab_restarted(&mut self, uid: u64, result: Result<Done, String>) {
        let (target, view) = match result {
            Ok(Done::Session(target, view)) => (target, view),
            Ok(other) => {
                self.toasts.error(
                    &self.ctx,
                    format!("Could not restart: unexpected answer {other:?}"),
                );
                return;
            }
            Err(e) => {
                // The old process still runs (or is gone): the tab shows whichever it is.
                self.toasts
                    .error(&self.ctx, format!("Could not restart: {e}"));
                return;
            }
        };
        let Some(i) = self.tabs.iter().position(|t| t.uid == uid) else {
            return; // the tab was closed meanwhile; the session lives on in the gateway
        };
        let term = self.open_view(&target);
        self.tabs[i].reattach(target, term);
        self.tabs[i].set_view(view);
        self.dirty = true;
        self.focus_pending = true;
    }

    pub(super) fn request_switch_view(&mut self, uid: u64, view: SessionView) {
        let Some(tab) = self.tabs.iter().find(|t| t.uid == uid) else {
            return;
        };
        if let Some(why) = crate::sidebar::view_switch_reason(tab, view, self.host_available()) {
            self.toasts.info(&self.ctx, why);
            return;
        }
        let Some(session) = tab.session_path().map(Path::to_path_buf) else {
            return;
        };
        let label = if view == SessionView::Chat {
            "chat view"
        } else {
            "terminal view"
        };
        self.confirm = Some((
            Confirm {
                title: format!("Switch to {label}?"),
                body: format!(
                    "Stops the omp process behind “{}” and resumes the same conversation in {label}. The tab keeps its place. Switching while omp is working would interrupt its turn.",
                    tab.label(&self.labels)
                ),
                confirm_label: "Restart and switch".to_owned(),
                danger: false,
            },
            PendingConfirm::SwitchView { uid, session, view },
        ));
    }

    pub(super) fn execute_switch_view(&mut self, uid: u64, session: &Path, view: SessionView) {
        let Some(tab) = self.tabs.iter().find(|t| t.uid == uid) else {
            return;
        };
        if tab.session_path() != Some(session) {
            self.toasts
                .info(&self.ctx, "Switch cancelled: the session changed");
            return;
        }
        if let Some(why) = crate::sidebar::view_switch_reason(tab, view, self.host_available()) {
            self.toasts.info(&self.ctx, why);
            return;
        }
        self.restart_tab_in_place(uid, Some(view));
    }

    /// Sidebar "Restart session": ask first (an open tab may be mid-turn).
    pub(super) fn request_restart_session(&mut self, entry: &SessionEntry) {
        if !self.backends_available() {
            return;
        }
        let tab = self.tabs.iter().find_map(|t| {
            if t.role == TabRole::Session
                && !t.terminal().is_exited()
                && t.session_path() == Some(entry.path.as_path())
            {
                t.mux_name.clone().map(|n| {
                    let busy = Activity::of_terminal(t.terminal()) != Activity::Idle;
                    (t.uid, n, busy)
                })
            } else {
                None
            }
        });
        let (uid, mux_name, busy) = match tab {
            Some((uid, name, busy)) => (Some(uid), name, busy),
            None => match self.running_session_name(entry) {
                Some(name) => (None, name, false),
                None => {
                    self.toasts
                        .info(&self.ctx, "That session is not running anymore.");
                    return;
                }
            },
        };
        let label = entry.label().to_owned();
        let warn = if busy {
            " omp is working; restarting interrupts the current turn."
        } else {
            ""
        };
        self.confirm = Some((
            Confirm {
                title: "Restart session?".to_owned(),
                body: format!(
                    "Stops the omp process behind “{label}” and resumes the same session with \
                     the control extension. The conversation is kept.{warn}"
                ),
                confirm_label: "Restart".to_owned(),
                danger: busy,
            },
            PendingConfirm::RestartSession(RestartTarget {
                uid,
                mux_name,
                session: entry.path.clone(),
                cwd: entry.cwd.clone(),
                title: label,
            }),
        ));
    }

    /// The user confirmed a sidebar restart: in place when the session has a tab, else open it
    /// in a new tab once the gateway restarted it.
    pub(super) fn execute_restart_session(&mut self, target: RestartTarget) {
        if !self.backends_available() {
            return;
        }
        let at = self.tabs.iter().position(|t| {
            t.session_path() == Some(target.session.as_path()) && !t.terminal().is_exited()
        });
        match (target.uid, at) {
            (Some(uid), Some(at)) if self.tabs.get(at).is_some_and(|t| t.uid == uid) => {
                self.restart_tab_in_place(uid, None);
            }
            (None, None) => {
                self.restart_detached(ClosedTab {
                    mux_name: Some(target.mux_name),
                    cwd: target.cwd,
                    session_path: Some(target.session),
                    title: target.title,
                    view: self.cfg.session_view,
                });
            }
            _ => self
                .toasts
                .error(&self.ctx, "Restart cancelled: the session changed"),
        }
    }

    /// Restarts a running session no tab shows: the gateway attaches it, restarts it, and the
    /// result opens as a new tab. The session is reserved meanwhile so opening it from the
    /// sidebar does not start a second view of it.
    fn restart_detached(&mut self, tab: ClosedTab) {
        let slot = (tab.session_path_owned(), tab.mux_name.clone());
        self.spawn.reserve(slot.clone());
        self.toasts.info(&self.ctx, "Restarting session…");
        let attach = Mutation::Launch(LaunchSpec {
            mux_name: tab.mux_name.clone(),
            cwd: tab.cwd.clone(),
            session_path: tab.session_path.clone(),
            label: tab.title.clone(),
            argv: crate::tabs::omp_argv(tab.session_path()),
            role: SessionRole::Session,
            copy_of: None,
            view: self.cfg.session_view,
        });
        self.gw.mutate(attach, move |app, attached| {
            let restarted = match attached {
                Ok(Done::Session(target, _)) => {
                    app.gw.mutate(
                        Mutation::Restart { target, view: None },
                        move |app, result| {
                            app.finish_restart_detached(slot, tab, result);
                        },
                    );
                    return;
                }
                Ok(other) => Err(format!("unexpected answer {other:?}")),
                Err(e) => Err(e),
            };
            app.finish_restart_detached(slot, tab, restarted);
        });
    }

    fn finish_restart_detached(
        &mut self,
        slot: (Option<PathBuf>, Option<String>),
        tab: ClosedTab,
        result: Result<Done, String>,
    ) {
        self.spawn.release(&slot);
        match result {
            Ok(Done::Session(..)) => {
                // Opens (or focuses) the restarted session through the normal path.
                self.open_resolved(tab);
            }
            Ok(other) => self.toasts.error(
                &self.ctx,
                format!("Could not restart: unexpected answer {other:?}"),
            ),
            Err(e) => self
                .toasts
                .error(&self.ctx, format!("Could not restart: {e}")),
        }
    }
}
