//! Opening terminal tabs. The gateway launches (or attaches) the session and reports its target;
//! this window then attaches its own view of it (a frontend terminal) and places the tab.
//! Launches are processed one at a time in request order, so restored tabs keep their order.

use std::collections::VecDeque;
use std::path::PathBuf;

use deck_core::config::SessionView;
use deck_core::omp::UpdateKind;
use deck_service::controller::SessionTarget;
use deck_service::gateway::protocol::{Done, LaunchSpec, Mutation, SessionRole, Tool, ViewRole};
use deck_term::{TermOptions, Terminal};

use super::{App, CopyPending, Tab, TabRole};
use crate::panes::Split;
use crate::tabs::{Anchor, TabLaunch};

/// What to do with the tab once it exists.
pub(super) enum Post {
    None,
    /// `omp --fork`: the tab waits for the gateway to report the copy's session file.
    Copy {
        label: String,
        dest: PathBuf,
    },
    /// A restored tab; `active` = it was the focused one.
    Restore {
        active: bool,
        /// Index in the persisted tab list (what a persisted split refers to).
        index: usize,
    },
    /// The tab takes the strip position next to its former neighbours (a restart, a moved
    /// session).
    Anchor(Anchor),
}

/// Whether the finished tab becomes the active one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Focus {
    /// Always (restored tabs, restarts — they handle focus themselves or replace the active tab).
    Always,
    /// Only while the user has not activated or closed another tab since the launch was
    /// requested (the latest request wins).
    Claim,
    /// Never; the tab opens in the background.
    Never,
}

pub(super) struct SpawnReq {
    pub mux_name: Option<String>,
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,
    pub title: String,
    pub argv: Vec<String>,
    /// An omp update or install run (a session of its own in the gateway) instead of `argv`.
    pub tool: Option<Tool>,
    /// `omp --fork` copy of the session with this id.
    pub copy_of: Option<String>,
    /// The view the session opens in (what the gateway uses when it has to create it).
    pub view: SessionView,
    pub post: Post,
    pub focus: Focus,
}

impl SpawnReq {
    /// An omp session tab running `argv`.
    pub fn session(
        mux_name: Option<String>,
        cwd: PathBuf,
        session_path: Option<PathBuf>,
        title: String,
        view: SessionView,
        post: Post,
        focus: Focus,
    ) -> SpawnReq {
        let argv = crate::tabs::omp_argv(session_path.as_deref());
        SpawnReq {
            mux_name,
            cwd,
            session_path,
            title,
            argv,
            tool: None,
            copy_of: None,
            view,
            post,
            focus,
        }
    }

    /// A tool tab (omp update / installer), outside the persisted tabs.
    pub fn tool(tool: Tool, cwd: PathBuf) -> SpawnReq {
        SpawnReq {
            mux_name: None,
            cwd,
            session_path: None,
            title: String::new(),
            argv: Vec::new(),
            tool: Some(tool),
            copy_of: None,
            view: SessionView::Terminal,
            post: Post::None,
            focus: Focus::Claim,
        }
    }

    fn role(&self) -> TabRole {
        match &self.tool {
            None => TabRole::Session,
            Some(Tool::Update(kind)) => TabRole::Update(*kind),
            Some(Tool::Install) => TabRole::InstallOmp,
        }
    }

    fn mutation(&self) -> Mutation {
        match &self.tool {
            Some(tool) => Mutation::RunTool(tool.clone()),
            None => Mutation::Launch(LaunchSpec {
                mux_name: self.mux_name.clone(),
                cwd: self.cwd.clone(),
                session_path: self.session_path.clone(),
                label: self.title.clone(),
                argv: self.argv.clone(),
                role: SessionRole::Session,
                copy_of: self.copy_of.clone(),
                view: self.view,
            }),
        }
    }
}

enum Job {
    Spawn {
        req: Box<SpawnReq>,
        /// The activation counter when the request was made (`Focus::Claim` only): the launch
        /// may take focus only while the counter still equals it. Per request, so a newer launch
        /// cannot revalidate an older one the user already overtook.
        claim: Option<u64>,
    },
    /// All restored tabs are queued before this marker: focus the one that was active.
    RestoreDone,
}

/// Session file and backend name of a launch.
type Slot = (Option<PathBuf>, Option<String>);

/// The launch queue and its one in-flight launch.
#[derive(Default)]
pub(super) struct Pump {
    queue: VecDeque<Job>,
    busy: bool,
    /// The launch the gateway is working on (it is in neither the queue nor the tab strip yet).
    inflight: Option<Slot>,
    /// Sessions whose tab was closed for a relaunch that has not been queued yet (a restart
    /// waiting for the gateway): they must not be opened again meanwhile.
    reserved: Vec<Slot>,
}

impl Pump {
    /// The next job, unless a launch is in flight.
    fn next(&mut self) -> Option<Job> {
        if self.busy {
            return None;
        }
        self.queue.pop_front()
    }

    fn begin(&mut self, slot: Slot) {
        self.busy = true;
        self.inflight = Some(slot);
    }

    /// The in-flight launch is done (opened, refused or failed).
    fn finish(&mut self) {
        self.busy = false;
        self.inflight = None;
    }

    pub(super) fn reserve(&mut self, slot: Slot) {
        self.reserved.push(slot);
    }

    pub(super) fn release(&mut self, slot: &Slot) {
        if let Some(i) = self.reserved.iter().position(|s| s == slot) {
            self.reserved.swap_remove(i);
        }
    }

    fn queued_for(&self, path: Option<&std::path::Path>, name: Option<&str>) -> bool {
        let matches = |(p, n): &Slot| {
            (path.is_some() && p.as_deref() == path) || (name.is_some() && n.as_deref() == name)
        };
        self.inflight.iter().any(matches)
            || self.reserved.iter().any(matches)
            || self.queue.iter().any(|j| match j {
                Job::Spawn { req, .. } => {
                    matches(&(req.session_path.clone(), req.mux_name.clone()))
                }
                Job::RestoreDone => false,
            })
    }

    /// A launch took focus by bumping the activation counter from `before` to `after`: queued
    /// claims that were still valid follow it; revoked ones stay revoked.
    fn rebase_claims(&mut self, before: u64, after: u64) {
        for j in &mut self.queue {
            if let Job::Spawn { claim, .. } = j
                && *claim == Some(before)
            {
                *claim = Some(after);
            }
        }
    }
}

impl Focus {
    /// Whether a launch may take focus, given the latest claim and the activation counter.
    fn grants(self, claim: Option<u64>, activation_seq: u64) -> bool {
        match self {
            Focus::Always => true,
            Focus::Claim => claim == Some(activation_seq),
            Focus::Never => false,
        }
    }
}

impl App {
    /// This window's own terminal view of the gateway session `target`.
    pub(super) fn open_view(&self, target: &SessionTarget) -> Terminal {
        let opts = TermOptions {
            scrollback: 10_000,
            theme: deck_service::gateway::terminal_theme(self.cfg.theme, self.cfg.style),
            clipboard: self.clipboard.clone(),
            // ConPTY does not implement the kitty keyboard protocol; the owner (not this side)
            // answers `CSI ? u`, so this only selects the key encoder.
            kitty_keyboard: !cfg!(windows),
        };
        let frontend = self.gw.frontend(target.clone(), ViewRole::Controlling);
        Terminal::frontend(frontend, opts, self.ctx.clone())
    }

    pub(super) fn spawn_tab_post(&mut self, req: SpawnReq) {
        // The claim belongs to this request: only the user's later tab switches revoke it.
        let claim = (req.focus == Focus::Claim).then_some(self.activation_seq);
        self.spawn.queue.push_back(Job::Spawn {
            req: Box::new(req),
            claim,
        });
        self.pump_spawns();
    }

    pub(super) fn queue_restore_done(&mut self) {
        self.spawn.queue.push_back(Job::RestoreDone);
        self.pump_spawns();
    }

    /// A launch for this session file or backend name is queued, in flight, or reserved.
    pub(super) fn spawn_queued_for(
        &self,
        path: Option<&std::path::Path>,
        name: Option<&str>,
    ) -> bool {
        self.spawn.queued_for(path, name)
    }

    fn pump_spawns(&mut self) {
        while let Some(job) = self.spawn.next() {
            let (req, claim) = match job {
                Job::RestoreDone => {
                    self.finish_restore();
                    continue;
                }
                Job::Spawn { req, claim } => (req, claim),
            };
            self.spawn
                .begin((req.session_path.clone(), req.mux_name.clone()));
            let op = req.mutation();
            self.gw
                .mutate(op, move |app, result| app.on_spawned(*req, claim, result));
        }
    }

    /// The gateway answered a launch: attach a view and place the tab, or say why not.
    fn on_spawned(&mut self, req: SpawnReq, claim: Option<u64>, result: Result<Done, String>) {
        self.spawn.finish();
        match result {
            Ok(Done::Session(target, view)) => self.finish_spawn(req, claim, target, view),
            Ok(other) => {
                log::error!("launch answered with {other:?}");
                self.toasts.error(
                    &self.ctx,
                    format!("Could not open “{}”: unexpected answer", req.title),
                );
            }
            Err(e) => {
                self.toasts
                    .error(&self.ctx, format!("Could not open “{}”: {e}", req.title));
            }
        }
        self.pump_spawns();
    }

    fn finish_spawn(
        &mut self,
        req: SpawnReq,
        claim: Option<u64>,
        target: SessionTarget,
        view: SessionView,
    ) {
        if req.view == SessionView::Chat && view == SessionView::Terminal {
            self.toasts.info(&self.ctx, "Opened in terminal view: this session is already running in a terminal, or the OmpDeck session host is unavailable");
        }
        let role = req.role();
        let SpawnReq {
            mux_name,
            cwd,
            session_path,
            title,
            post,
            focus,
            ..
        } = req;
        // The gateway returns the controller that already runs a session as is: a tab that shows
        // it just takes focus.
        let shown = self.tabs.iter().position(|t| t.target().key == target.key);
        if let Some(i) = shown {
            if focus.grants(claim, self.activation_seq) {
                self.activate(i);
            }
            return;
        }
        let term = self.open_view(&target);
        self.next_tab_uid += 1;
        let uid = self.next_tab_uid;
        let mut tab = Tab::new(
            term,
            target.clone(),
            uid,
            TabLaunch {
                mux_name,
                cwd,
                session_path,
                title,
                role,
                view,
            },
        );
        if let Some(info) = self.gw.controller(&target) {
            tab.set_info(&info);
        }
        self.tabs.push(tab);
        let mut at = self.tabs.len() - 1;
        if let Post::Anchor(anchor) = &post {
            // The tab was appended; put it next to the neighbour it sat beside.
            let uids = self.tab_uids();
            let want = anchor.index(&uids[..at]);
            if want != at {
                let t = self.tabs.remove(at);
                self.tabs.insert(want, t);
                at = want;
            }
        }
        // A pane left empty (a restart, a pane opened without a tab) takes the new session tab.
        if let Some(uid) = self.tabs[at].pane_uid() {
            let (path, name) = (
                self.tabs[at]
                    .session_path()
                    .map(std::path::Path::to_path_buf),
                self.tabs[at].mux_name().map(str::to_owned),
            );
            // The launch's own pane when one was reserved for it (and still exists), else the
            // first empty pane; with none empty the tab is shown like any opened tab.
            match self.take_pane_claim(path.as_deref(), name.as_deref()) {
                Some(ticket) => {
                    self.split.fill_reserved(ticket, uid);
                }
                None => {
                    self.split.fill_vacated(uid);
                }
            }
        }
        if focus.grants(claim, self.activation_seq) {
            let before = self.activation_seq;
            self.activate(at);
            // Our own activation must not void a later request still waiting to focus.
            self.spawn.rebase_claims(before, self.activation_seq);
        } else if at <= self.active && self.tabs.len() > 1 {
            // Inserted in front of the active tab: it keeps its identity, not its index.
            self.active += 1;
        }
        self.dirty = true;
        match post {
            Post::None | Post::Anchor(_) => {}
            Post::Restore { active, index } => {
                self.restore_uids.insert(index, uid);
                if active {
                    self.restore_focus = Some(uid);
                }
            }
            Post::Copy { label, dest } => {
                self.tabs[at].copy = Some(CopyPending { dest: dest.clone() });
                self.toasts.info(
                    &self.ctx,
                    format!(
                        "Copying “{label}” to {}…",
                        deck_core::sessions::abbreviate_home(&dest)
                    ),
                );
            }
        }
    }

    /// The gateway found the new session file of a tab's copy, or gave up.
    pub(super) fn on_copy_result(
        &mut self,
        target: &SessionTarget,
        result: Result<PathBuf, String>,
    ) {
        let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|t| t.target().key == target.key && t.copy.is_some())
        else {
            return; // the tab is gone
        };
        let Some(copy) = tab.copy.take() else {
            return;
        };
        match result {
            Ok(_) => {
                let dest = deck_core::sessions::abbreviate_home(&copy.dest);
                self.dirty = true;
                self.toasts.success(
                    &self.ctx,
                    format!("Copied to {dest} — the copy is a new session"),
                );
                self.refresh_sessions();
            }
            Err(e) => self.toasts.error(&self.ctx, format!("Copy failed: {e}")),
        }
    }

    fn finish_restore(&mut self) {
        let uids = std::mem::take(&mut self.restore_uids);
        let state = self.restore_split.take();
        if self.tabs.is_empty() {
            return;
        }
        let mut at = self
            .restore_focus
            .take()
            .and_then(|uid| self.tab_index(uid))
            .unwrap_or(self.tabs.len() - 1);
        if let Some(state) = state {
            let split = Split::from_state(&state, |i| uids.get(&i).copied());
            if split.is_split() {
                // The focused pane's tab is the active one.
                if let Some(i) = split.focused_uid().and_then(|u| self.tab_index(u)) {
                    at = i;
                }
                self.split = split;
            }
        }
        self.activate(at);
    }

    /// Opens an omp update run as a tab of its own (outside the persisted tabs).
    pub(super) fn open_update_tab(&mut self, kind: UpdateKind) {
        let cwd = self
            .home
            .clone()
            .unwrap_or_else(deck_core::platform::home_or_root);
        self.spawn_tab_post(SpawnReq::tool(Tool::Update(kind), cwd));
    }
}

#[cfg(test)]
mod tests {
    use super::{Focus, Job, Mutation, PathBuf, Post, Pump, SessionView, SpawnReq, Tool};

    #[test]
    fn a_queued_launch_focuses_only_while_the_active_tab_is_unchanged() {
        assert!(Focus::Claim.grants(Some(4), 4));
        assert!(!Focus::Claim.grants(Some(4), 5), "the user switched tabs");
        assert!(!Focus::Claim.grants(None, 0));
    }

    #[test]
    fn restarts_always_focus_and_background_opens_never_do() {
        assert!(Focus::Always.grants(None, 9));
        assert!(!Focus::Never.grants(Some(1), 1));
    }

    fn job(path: &str, claim: Option<u64>) -> Job {
        Job::Spawn {
            req: Box::new(SpawnReq::session(
                None,
                PathBuf::from("/"),
                Some(PathBuf::from(path)),
                String::new(),
                SessionView::Terminal,
                Post::None,
                Focus::Claim,
            )),
            claim,
        }
    }

    fn claim_of(pump: &Pump, i: usize) -> Option<u64> {
        match &pump.queue[i] {
            Job::Spawn { claim, .. } => *claim,
            Job::RestoreDone => None,
        }
    }

    #[test]
    fn one_launch_is_in_flight_at_a_time_in_request_order() {
        let mut pump = Pump::default();
        for p in ["/a", "/b", "/c"] {
            pump.queue.push_back(job(p, None));
        }
        let Some(Job::Spawn { req, .. }) = pump.next() else {
            panic!("a job was queued");
        };
        pump.begin((req.session_path.clone(), None));
        assert!(pump.next().is_none(), "the next waits for the answer");
        assert!(pump.queued_for(Some(std::path::Path::new("/a")), None));
        assert!(pump.queued_for(Some(std::path::Path::new("/c")), None));
        pump.finish();
        assert!(!pump.queued_for(Some(std::path::Path::new("/a")), None));
        let Some(Job::Spawn { req, .. }) = pump.next() else {
            panic!("the second job follows");
        };
        assert_eq!(req.session_path, Some(PathBuf::from("/b")));
    }

    #[test]
    fn focus_claims_are_per_request() {
        let mut pump = Pump::default();
        // Older request claimed at 3 (the user has since moved on to 4); a newer one at 4.
        pump.queue.push_back(job("/old", Some(3)));
        pump.queue.push_back(job("/new", Some(4)));
        let seq = 4;
        assert!(
            !Focus::Claim.grants(claim_of(&pump, 0), seq),
            "the older claim stays void"
        );
        assert!(Focus::Claim.grants(claim_of(&pump, 1), seq));
        // The newer launch activates (4 -> 5): the valid claim follows, the void one does not.
        pump.rebase_claims(4, 5);
        assert_eq!(claim_of(&pump, 0), Some(3));
        assert_eq!(claim_of(&pump, 1), Some(5));
    }

    #[test]
    fn reserved_restarts_count_as_queued_until_released() {
        let mut pump = Pump::default();
        let slot = (Some(PathBuf::from("/s")), Some("omp-s".to_owned()));
        pump.reserve(slot.clone());
        assert!(pump.queued_for(Some(std::path::Path::new("/s")), None));
        assert!(pump.queued_for(None, Some("omp-s")));
        pump.release(&slot);
        assert!(!pump.queued_for(Some(std::path::Path::new("/s")), None));
    }

    #[test]
    fn a_launch_asks_the_gateway_for_the_view_the_request_names() {
        for view in [SessionView::Terminal, SessionView::Chat] {
            let req = SpawnReq::session(
                Some("omp-v".into()),
                PathBuf::from("/p"),
                None,
                String::new(),
                view,
                Post::None,
                Focus::Never,
            );
            let Mutation::Launch(spec) = req.mutation() else {
                panic!("a session tab launches a session");
            };
            assert_eq!(spec.view, view);
        }
        let tool = SpawnReq::tool(Tool::Install, PathBuf::from("/p"));
        assert!(matches!(tool.mutation(), Mutation::RunTool(_)));
    }
}
