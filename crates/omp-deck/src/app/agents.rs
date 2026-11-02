//! App-side glue for the agents dashboard: the gateway's subagent snapshot of the active tab
//! (status-bar chip and live overlay), the fleet snapshot behind the dashboard cards, the
//! dashboard view and the subagent popup (which follows a transcript topic).

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use deck_core::agents::AgentsTarget;
use deck_core::fleet::{FleetSession, FleetSnapshot, FleetTarget};
use deck_core::mux::session_name_for;
use deck_service::gateway::protocol::{Topic, TopicValue};
use deck_service::gateway::running::RunningSnapshot;
use egui::{Key, Modifiers};

use super::App;
use crate::agent_ctl::{CtlView, Link, Restart};
use crate::agent_model::{self, AgentsView};
use crate::agent_popup::{self, PopupAction, PopupInput, PopupState, PopupTab};
use crate::dashboard::{self, DashAction, DashInput, DashState};
use crate::dashboard_model::{Card, CardKey, CardSrc, Extra, Status};
use crate::tabs::{ClosedTab, Tab, TabRole};
use crate::theme::{self, Palette};
use crate::toast::ToastKind;
use deck_service::statusfmt::short_model_name;

/// What the dashboard knows about a session file from the sidebar's scan.
pub(super) struct IndexEntry {
    id: String,
    title: String,
    cwd: PathBuf,
    modified: SystemTime,
}

/// Session file → scan facts; rebuilt when the sessions scan changed.
#[derive(Default)]
pub(super) struct DashIndex {
    generation: Option<u64>,
    entries: HashMap<PathBuf, IndexEntry>,
    /// Canonical backend name of each session id → its file (a fresh omp has not opened it yet).
    by_name: HashMap<String, PathBuf>,
}

/// Cards and ages are rebuilt at this granularity.
const NOW_BUCKET_SECS: u64 = 15;

/// One card's inputs, borrowed from the app state.
struct Src<'a> {
    card: CardSrc<'a>,
    /// The session's omp process is running (decides how the fleet reports unfinished agents).
    owner_alive: bool,
}

/// Open session tabs plus live backend sessions that no tab shows, as card inputs.
fn collect_sources<'a>(
    tabs: &'a [Tab],
    labels: &'a HashMap<PathBuf, String>,
    index: &'a DashIndex,
    running: Option<&'a RunningSnapshot>,
    active: usize,
    now: SystemTime,
) -> Vec<Src<'a>> {
    let now_bucket = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / NOW_BUCKET_SECS * NOW_BUCKET_SECS);
    let bucket_time = SystemTime::UNIX_EPOCH + Duration::from_secs(now_bucket);
    let mut out = Vec::new();
    for (i, t) in tabs.iter().enumerate() {
        if t.role != TabRole::Session {
            continue;
        }
        let entry = t.session_path().and_then(|p| index.entries.get(p));
        let title = t.label(labels);
        out.push(Src {
            card: CardSrc {
                key: CardKey::Tab(t.uid),
                title: if title.is_empty() {
                    "New session"
                } else {
                    title
                },
                cwd: &t.cwd,
                session: t.session_path(),
                id: entry.map_or_else(|| t.session_path().map_or("", file_stem), |e| e.id.as_str()),
                status: Status::of_tab(t.status(false)),
                // A session without a file yet was just started: it is the most recent thing.
                activity_at: entry.map(|e| e.modified).or(Some(bucket_time)),
                active: i == active,
            },
            owner_alive: !t.terminal().is_exited(),
        });
    }
    let Some(running) = running else { return out };
    let backs_a_tab = |name: &str, file: Option<&Path>| {
        tabs.iter().any(|t| {
            t.role == TabRole::Session
                && (t.mux_name.as_deref() == Some(name)
                    || file.is_some_and(|f| t.session_path() == Some(f)))
        })
    };
    for name in &running.alive {
        let file = running
            .files_of(name)
            .first()
            .or_else(|| index.by_name.get(name))
            .map(PathBuf::as_path);
        if backs_a_tab(name, file) {
            continue;
        }
        let entry = file.and_then(|f| index.entries.get(f));
        out.push(Src {
            card: CardSrc {
                key: CardKey::Backend(name.clone()),
                title: entry.map_or(name.as_str(), |e| e.title.as_str()),
                cwd: entry.map_or(Path::new(""), |e| e.cwd.as_path()),
                session: file,
                id: entry.map_or(name.as_str(), |e| e.id.as_str()),
                status: Status::of_backend(running.activity_of(name)),
                activity_at: entry.map(|e| e.modified),
                active: false,
            },
            owner_alive: true,
        });
    }
    out
}

/// Where the popup reads its agents from: the active tab's own view when it has the agent (with
/// the live overlay), else the dashboard's view of that session. `bool` = the session is the
/// active tab's.
fn popup_source<'a>(
    target: Option<&AgentsTarget>,
    agents_view: &'a AgentsView,
    dash: &'a DashState,
    session: &Path,
    agent: &str,
) -> Option<(&'a AgentsView, bool)> {
    let active = target.is_some_and(|t| t.session == session);
    if active && agents_view.agents().iter().any(|a| a.id == agent) {
        return Some((agents_view, true));
    }
    dash.sessions.get(session).map(|s| (&s.view, active))
}

fn file_stem(p: &Path) -> &str {
    p.file_stem().and_then(|s| s.to_str()).unwrap_or("")
}

impl App {
    /// The active tab is an omp session (the status bar's agents chip follows it).
    pub(super) fn active_session_tab(&self) -> bool {
        matches!(
            self.tabs.get(self.active),
            Some(t) if t.role == TabRole::Session
        )
    }

    /// Per frame, before drawing: follow the active tab's session file and refresh the cached
    /// view (the status bar's counts, the live overlay, the popup of the active session).
    pub(super) fn drive_agents(&mut self) {
        self.drive_ctl();
        let want = match self.tabs.get(self.active) {
            Some(t) if t.role == TabRole::Session => {
                t.session_path_owned().map(|session| AgentsTarget {
                    session,
                    owner_alive: !t.terminal().is_exited(),
                })
            }
            _ => None,
        };
        if want != self.agents_target {
            self.gw.set_agents_target(want.clone());
            self.agents_target = want;
        }
        // A snapshot of another session (right after a tab or session switch) is not ours.
        let snap = self.gw.agents().filter(|s| {
            self.agents_target
                .as_ref()
                .is_some_and(|t| t.session == s.session)
        });
        let live = self.live_snapshot();
        self.agents_view
            .update(snap.as_ref(), live.as_ref(), SystemTime::now());
        // The popup needs a session it can read: the active one, or the dashboard's.
        if let Some(p) = &self.popup {
            let active = self
                .agents_target
                .as_ref()
                .is_some_and(|t| t.session == p.session);
            if !active && !self.dash.open {
                self.close_popup_topic();
            }
        }
    }

    /// Per frame while the dashboard is open: rebuild the cards when their inputs changed, follow
    /// the sessions with the fleet watcher, refresh the subagent views. Closed: the watcher idles.
    pub(super) fn drive_dashboard(&mut self) {
        if !self.dash.open {
            if self.dash.sig.is_some() {
                self.dash.close();
                self.gw.set_fleet_targets(Vec::new());
            }
            return;
        }
        self.refresh_dash_index();
        let now = SystemTime::now();
        let running = self.gw.running();
        let fleet = self.gw.fleet();
        let live = self.live_snapshot();
        let live_ctx = live
            .as_deref()
            .and_then(|l| l.context.as_ref())
            .map(|c| (c.used_tokens, c.context_window));
        let sig = self.dash_signature(running.as_ref(), fleet.as_ref(), live_ctx, now);
        if self.dash.sig != Some(sig) {
            let srcs = collect_sources(
                &self.tabs,
                &self.labels,
                &self.dash_index,
                running.as_deref(),
                self.active,
                now,
            );
            let cards = build_cards(&srcs, fleet.as_deref(), &self.models, live_ctx, now);
            let targets: Vec<FleetTarget> = srcs
                .iter()
                .filter_map(|s| {
                    Some(FleetTarget {
                        session: s.card.session?.to_path_buf(),
                        cwd: s.card.cwd.to_path_buf(),
                        owner_alive: s.owner_alive,
                    })
                })
                .collect();
            drop(srcs);
            self.gw.set_fleet_targets(targets);
            self.dash.set_cards(cards, fleet, sig);
        }
        let root = self.ctl_root().map(Path::to_path_buf);
        self.dash
            .update_views(root.as_deref().map(|p| (p, live.as_ref())), now);
    }

    /// Everything the cards are built from, hashed in place: no allocation and no scan of the
    /// backend sessions per frame. The sources themselves are only collected when this moved.
    fn dash_signature(
        &self,
        running: Option<&Arc<RunningSnapshot>>,
        fleet: Option<&Arc<FleetSnapshot>>,
        live_ctx: Option<(u64, u64)>,
        now: SystemTime,
    ) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // Ages ("3m") move with the clock: re-render once per bucket.
        now.duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() / NOW_BUCKET_SECS)
            .hash(&mut h);
        self.dash_index.generation.hash(&mut h);
        running.map(Arc::as_ptr).hash(&mut h);
        fleet.map(Arc::as_ptr).hash(&mut h);
        self.models.generation.hash(&mut h);
        live_ctx.hash(&mut h);
        self.active.hash(&mut h);
        for (i, t) in self.tabs.iter().enumerate() {
            if t.role != TabRole::Session {
                continue;
            }
            i.hash(&mut h);
            t.uid.hash(&mut h);
            t.label(&self.labels).hash(&mut h);
            t.cwd.hash(&mut h);
            t.session_path().hash(&mut h);
            t.mux_name.hash(&mut h);
            Status::of_tab(t.status(false)).hash(&mut h);
            t.terminal().is_exited().hash(&mut h);
        }
        h.finish()
    }

    /// Session file → scan facts, rebuilt when the sidebar's scan changed.
    fn refresh_dash_index(&mut self) {
        let generation = self.gw.sessions_generation();
        if self.dash_index.generation == Some(generation) {
            return;
        }
        self.dash_index.generation = Some(generation);
        self.dash_index.entries.clear();
        self.dash_index.by_name.clear();
        if let Some(groups) = self.groups() {
            for g in groups.iter() {
                for e in &g.sessions {
                    self.dash_index
                        .by_name
                        .insert(session_name_for(&e.id), e.path.clone());
                    self.dash_index.entries.insert(
                        e.path.clone(),
                        IndexEntry {
                            id: e.id.clone(),
                            title: e.label().to_owned(),
                            cwd: e.cwd.clone(),
                            modified: e.modified,
                        },
                    );
                }
            }
        }
    }

    /// Ctrl+Shift+A, the palette, the app menu (`focus_active` = false) and the status bar's agents
    /// chip (`focus_active` = true: the active session's card opens expanded, scrolled into view).
    pub(super) fn toggle_dashboard(&mut self, focus_active: bool) {
        if self.dash.open {
            self.close_dashboard();
            return;
        }
        let focus = focus_active
            .then(|| match self.tabs.get(self.active) {
                Some(t) if t.role == TabRole::Session => Some(CardKey::Tab(t.uid)),
                _ => None,
            })
            .flatten();
        self.dash.show_with(focus);
        // The terminals leave the screen: tell omp its focus is gone.
        for t in self.tabs.iter_mut() {
            t.terminal_mut().set_focused(false);
        }
        self.ctx.request_repaint();
    }

    pub(super) fn close_dashboard(&mut self) {
        if !self.dash.open {
            return;
        }
        self.dash.close();
        self.gw.set_fleet_targets(Vec::new());
        // A popup that only the dashboard can feed goes with it.
        if self.popup.as_ref().is_some_and(|p| {
            !self
                .agents_target
                .as_ref()
                .is_some_and(|t| t.session == p.session)
        }) {
            self.close_popup_topic();
        }
        // The active terminal is on screen again: it has been seen.
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.bell = false;
        }
        self.focus_pending = true;
        self.ctx.request_repaint();
    }

    /// Esc closes the dashboard when nothing in front of it wants the key. True when consumed.
    pub(super) fn consume_dashboard_escape(&mut self, ctx: &egui::Context) -> bool {
        self.dash.open
            && self.popup.is_none()
            && self.tree.is_none()
            && !self.usage_open
            && !self.sessions.open
            && !self.modal_open()
            && !egui::Popup::is_any_open(ctx)
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
    }

    /// Esc closes the popup, unless a menu is open (it takes the key first). True when consumed.
    pub(super) fn consume_popup_escape(&mut self, ctx: &egui::Context) -> bool {
        self.popup.is_some()
            && !egui::Popup::is_any_open(ctx)
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
    }

    /// Drops the popup and stops following its transcript.
    fn close_popup_topic(&mut self) {
        if let Some(p) = self.popup.take() {
            self.gw.unsubscribe(&Topic::Transcript(p.transcript_path));
        }
    }

    pub(super) fn close_agent_popup(&mut self) {
        self.close_popup_topic();
        self.focus_pending = true;
    }

    fn open_agent(&mut self, session: &Path, id: &str, tab: PopupTab) {
        let state = {
            let Some((view, _)) = popup_source(
                self.agents_target.as_ref(),
                &self.agents_view,
                &self.dash,
                session,
                id,
            ) else {
                return;
            };
            let Some(agent) = view.agents().iter().find(|a| a.id == id) else {
                return;
            };
            PopupState::new(session.to_path_buf(), agent, tab)
        };
        self.close_popup_topic();
        self.gw
            .subscribe(Topic::Transcript(state.transcript_path.clone()));
        self.popup = Some(state);
        self.ctx.request_repaint();
    }

    /// The dashboard, in place of the terminal area.
    pub(super) fn show_dashboard(&mut self, ui: &mut egui::Ui) {
        self.terminal_focused = false;
        let ctl_active = CtlView {
            link: self.ctl_link(),
            restart: self.restart_offer(),
            reqs: &self.ctl_reqs,
            errors: &self.ctl_errors,
        };
        let no_reqs = HashMap::new();
        let no_errors = HashMap::new();
        let ctl_idle = CtlView {
            link: Link::ReadOnly,
            restart: Restart::Hidden,
            reqs: &no_reqs,
            errors: &no_errors,
        };
        let input = DashInput {
            ctl_active: &ctl_active,
            ctl_idle: &ctl_idle,
            popup: self
                .popup
                .as_ref()
                .map(|p| (p.session.as_path(), p.agent_id.as_str())),
        };
        let actions = theme::skin::central(ui, Palette::of_ui(ui), false, |ui| {
            dashboard::show(ui, &mut self.dash, &input)
        })
        .inner;
        for a in actions {
            match a {
                DashAction::Close => self.close_dashboard(),
                DashAction::OpenCard(key) => self.open_dash_card(&key),
                DashAction::Agent { session, id } => self.open_dash_agent(&session, &id),
                DashAction::Kill(id) => self.request_kill_agent(id),
                DashAction::Revive(id) => self.send_ctl_revive(id),
                DashAction::Restart => self.request_restart_live(),
            }
        }
    }

    /// Focus the card's tab, or open its session in a new one; the dashboard closes.
    fn open_dash_card(&mut self, key: &CardKey) {
        match key {
            CardKey::Tab(uid) => {
                if let Some(i) = self.tab_index(*uid) {
                    self.activate(i);
                }
            }
            CardKey::Backend(name) => {
                let Some(card) = self.dash.cards.iter().find(|c| &c.key == key) else {
                    return;
                };
                let req = ClosedTab {
                    mux_name: Some(name.clone()),
                    cwd: card.cwd.clone(),
                    session_path: card.session.clone(),
                    title: card.title.clone(),
                    view: self.cfg.session_view,
                };
                self.open_resolved(req);
                self.close_dashboard();
            }
        }
    }

    /// Subagent clicked in the dashboard: activate the session's tab when there is one (live
    /// control follows the active tab), keep the dashboard, show the popup.
    fn open_dash_agent(&mut self, session: &Path, id: &str) {
        let tab = self
            .tabs
            .iter()
            .position(|t| t.role == TabRole::Session && t.session_path() == Some(session));
        if let Some(i) = tab
            && i != self.active
        {
            self.activate_keeping_dashboard(i);
        }
        let mode = self.popup.as_ref().map_or(PopupTab::Transcript, |p| p.tab);
        self.open_agent(session, id, mode);
    }

    /// "Open session for live control" in a read-only popup.
    fn open_popup_session(&mut self) {
        let Some(session) = self.popup.as_ref().map(|p| p.session.clone()) else {
            return;
        };
        let key = self
            .dash
            .cards
            .iter()
            .find(|c| c.session.as_deref() == Some(session.as_path()))
            .map(|c| c.key.clone());
        self.close_popup_topic();
        if let Some(key) = key {
            self.open_dash_card(&key);
        }
        self.close_dashboard();
    }

    pub(super) fn show_agent_popup(&mut self, ctx: &egui::Context) {
        if self.popup.is_none() {
            return;
        }
        let link_here = self.ctl_link();
        let Some(st) = self.popup.as_mut() else {
            return;
        };
        // The gateway's transcript, when it moved.
        let topic = Topic::Transcript(st.transcript_path.clone());
        let rev = self.gw.revision(&topic);
        if rev != st.fed
            && let Some(TopicValue::Transcript { revision, items }) = self.gw.topic(&topic)
        {
            st.feed(rev, revision, items);
        }
        let session = st.session.clone();
        let tab = st.tab;
        let source = popup_source(
            self.agents_target.as_ref(),
            &self.agents_view,
            &self.dash,
            &session,
            &st.agent_id,
        );
        let Some((view, active)) = source else {
            // Neither the active tab nor the dashboard has this session any more.
            self.close_popup_topic();
            return;
        };
        let Some(index) = view.agents().iter().position(|a| a.id == st.agent_id) else {
            // The agent vanished (session switched or its files were removed).
            self.close_popup_topic();
            return;
        };
        let order = agent_model::visible(view.agents(), true, &HashSet::new());
        let no_reqs = HashMap::new();
        let no_errors = HashMap::new();
        let ctl = CtlView {
            link: if active { link_here } else { Link::ReadOnly },
            restart: Restart::Hidden,
            reqs: if active { &self.ctl_reqs } else { &no_reqs },
            errors: if active { &self.ctl_errors } else { &no_errors },
        };
        let actions = agent_popup::show(
            ctx,
            st,
            &PopupInput {
                view,
                index,
                order: &order,
                needs_open: !active,
                ctl: &ctl,
            },
        );
        // Links in rendered markdown go through the same allow-listed opener as terminal links.
        let urls = ctx.output_mut(|o| {
            let mut urls = Vec::new();
            o.commands.retain(|c| match c {
                egui::OutputCommand::OpenUrl(u) => {
                    urls.push(u.url.clone());
                    false
                }
                _ => true,
            });
            urls
        });
        for url in urls {
            self.open_external(url);
        }
        for a in actions {
            match a {
                PopupAction::Close => self.close_agent_popup(),
                PopupAction::Open(id) => self.open_agent(&session, &id, tab),
                PopupAction::Copy { text, what } => self.copy_text(&text, what),
                PopupAction::OpenFile(path) => self.open_path(path),
                PopupAction::Error(message) => self.toasts.error(&self.ctx, message),
                PopupAction::Kill(id) => self.request_kill_agent(id),
                PopupAction::Revive(id) => self.send_ctl_revive(id),
                PopupAction::Chat { agent, text } => self.send_ctl_chat(agent, text),
                PopupAction::OpenSession => self.open_popup_session(),
            }
        }
    }

    /// Open a file with the desktop's default handler off the UI thread.
    pub(super) fn open_path(&mut self, path: PathBuf) {
        self.gw.native(
            move || {
                if !path.is_absolute() || !path.is_file() {
                    Err(format!("File not found: {}", path.display()))
                } else {
                    deck_core::platform::open_path(path.as_os_str())
                        .map_err(|e| format!("Could not open the file: {e}"))
                }
            },
            |app, outcome: Result<(), String>| {
                if let Err(msg) = outcome {
                    app.toasts.notify(&app.ctx, ToastKind::Error, msg);
                }
            },
        );
    }
}

/// One card per source, with the metrics of the fleet's session where it has one.
fn build_cards(
    srcs: &[Src<'_>],
    fleet: Option<&FleetSnapshot>,
    models: &crate::model_picker::ModelsCache,
    live_ctx: Option<(u64, u64)>,
    now: SystemTime,
) -> Vec<Card> {
    let by_path: HashMap<&Path, &FleetSession> = fleet
        .map(|f| {
            f.sessions
                .iter()
                .map(|s| (s.session.as_path(), s.as_ref()))
                .collect()
        })
        .unwrap_or_default();
    srcs.iter()
        .map(|s| {
            let session = s.card.session.and_then(|p| by_path.get(p).copied());
            let info = session
                .and_then(|f| f.model.as_deref())
                .and_then(|m| models.get(m));
            let extra = Extra {
                model_name: info.map(|i| short_model_name(&i.name).to_owned()),
                window: info.and_then(|i| i.context_window),
                live_ctx: live_ctx.filter(|_| s.card.active),
            };
            Card::build(&s.card, session, &extra, now)
        })
        .collect()
}
