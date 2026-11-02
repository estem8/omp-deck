//! The one notification pipeline: session observations in, notification effects out.
//!
//! [`NotificationCenter`] owns the transition tracker, the inbox and the asynchronous
//! finished-turn classification. Clients feed it what they observe of each live session
//! ([`Observation`]: identity, status, session file, label, whether a view shows it) plus
//! whether their window has focus, and apply the returned [`Effect`]s with their own platform
//! channel (desktop pop-ups, the session's bell marker). No client runs its own transition
//! detection. OmpDeck is the sole notifier: launched omp instances get `PI_NOTIFICATIONS=off`.
//!
//! Policy kept from the desktop: the inbox records events for a session in view when
//! `while_viewing` is on (the default), desktop pop-ups follow `only_when_unfocused`, a turn the
//! user stopped is not reported, a failed turn is reported as an error with omp's message, and a
//! late verdict is dropped once the session changed status or session file since it was asked.

pub mod classify;
pub mod inbox;
pub mod tracker;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

use deck_core::config::NotifyConfig;

use crate::controller::SessionTarget;
use crate::status::TabStatus;
use inbox::{Entry, Inbox, Kind};
use tracker::{Event, Sighting, Ticket, Tracker};

/// While a session is busy the client ticks at least this often, so the end of its turn is seen
/// even when nothing is painted.
pub const BUSY_WATCH: Duration = Duration::from_secs(1);

/// Called from worker threads when a verdict is ready; the client ticks the center soon.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// One live session as a client sees it now.
#[derive(Clone, Copy, Debug)]
pub struct Observation<'a> {
    /// The session and its generation: a restarted session (or another conversation in it) is
    /// a new target and starts with no notification state.
    pub target: &'a SessionTarget,
    pub status: TabStatus,
    pub exit_code: Option<i32>,
    pub session: Option<&'a Path>,
    /// The session's label.
    pub title: &'a str,
    /// Basename of the session's folder.
    pub project: &'a str,
    /// A view shows the session (the active tab, or any pane of a split).
    pub visible: bool,
    /// omp's state is known ([`tracker::Sighting::omp_title`]).
    pub omp_title: bool,
}

/// Which sound a desktop pop-up asks for: a name of the freedesktop sound theme, so the user's
/// theme and volume decide how it plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    Finished,
    Attention,
    Error,
    Ended,
    /// The settings' sample notification.
    Test,
}

impl Sound {
    pub fn of(kind: &Kind) -> Sound {
        match kind {
            Kind::Finished => Sound::Finished,
            Kind::Attention => Sound::Attention,
            Kind::Error(_) => Sound::Error,
            Kind::Exited { .. } => Sound::Ended,
        }
    }

    /// The freedesktop sound-theme name (`sound-name` hint).
    pub fn theme_name(self) -> &'static str {
        match self {
            Sound::Finished => "complete",
            Sound::Attention => "dialog-warning",
            Sound::Error => "dialog-error",
            Sound::Ended | Sound::Test => "message-new-instant",
        }
    }
}

/// What the client does with its platform channels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Show a desktop pop-up for the session (replacing its previous one).
    Show {
        target: SessionTarget,
        title: String,
        body: String,
        /// `None`: silent (the user turned sounds off).
        sound: Option<Sound>,
    },
    /// Withdraw the session's pop-up (best effort).
    Withdraw(SessionTarget),
    /// Withdraw every session's pop-up.
    WithdrawAll,
    /// Mark the session as having an unseen finished turn (the tab's bell).
    Bell(SessionTarget),
    /// A canonical event for the paired phones' Web Push (every event, whatever the desktop
    /// policy; each device's prefs and presence decide on the gateway).
    Mobile {
        target: SessionTarget,
        kind: MobileKind,
        /// The session's label.
        title: String,
    },
}

/// Which push preference a canonical event falls under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MobileKind {
    /// A turn ended (also one that failed).
    TurnFinished,
    Attention,
    /// The session's process ended.
    Ended,
}

impl MobileKind {
    pub fn of(kind: &Kind) -> MobileKind {
        match kind {
            Kind::Finished | Kind::Error(_) => MobileKind::TurnFinished,
            Kind::Attention => MobileKind::Attention,
            Kind::Exited { .. } => MobileKind::Ended,
        }
    }
}

/// Result of one [`NotificationCenter::tick`].
#[derive(Debug, Default)]
pub struct Ticked {
    pub effects: Vec<Effect>,
    /// Tick again within this time even without input (a session is busy).
    pub watch_in: Option<Duration>,
}

/// Latest label and session file of a live session.
struct Known {
    session: Option<PathBuf>,
    title: String,
    project: String,
}

/// A finished turn's verdict from the worker; `kind` is `None` for a turn the user stopped.
struct Classified {
    ticket: Ticket<SessionTarget>,
    kind: Option<Kind>,
}

pub struct NotificationCenter {
    tracker: Tracker<SessionTarget>,
    inbox: Inbox<SessionTarget>,
    /// The live targets.
    known: HashMap<SessionTarget, Known>,
    classified: Receiver<Classified>,
    classified_tx: Sender<Classified>,
    wake: Wake,
}

impl NotificationCenter {
    pub fn new(wake: Wake) -> NotificationCenter {
        let (classified_tx, classified) = mpsc::channel();
        NotificationCenter {
            tracker: Tracker::default(),
            inbox: Inbox::default(),
            known: HashMap::new(),
            classified,
            classified_tx,
            wake,
        }
    }

    /// What happened in sessions the user was not looking at.
    pub fn inbox(&self) -> &Inbox<SessionTarget> {
        &self.inbox
    }

    /// Per tick (also while the window is hidden): watch the sessions, record and announce due
    /// events, withdraw the ones the user caught up on. `sessions` lists every live session.
    pub fn tick(
        &mut self,
        now: Instant,
        sessions: &[Observation<'_>],
        window_focused: bool,
        cfg: &NotifyConfig,
    ) -> Ticked {
        let mut out = Ticked::default();
        let visible: Vec<SessionTarget> = sessions
            .iter()
            .filter(|o| o.visible)
            .map(|o| o.target.clone())
            .collect();
        let mut live = HashSet::with_capacity(sessions.len());
        let mut fired = Vec::new();
        for o in sessions {
            live.insert(o.target.clone());
            self.remember(o);
            let seen = Sighting {
                status: o.status,
                exit_code: o.exit_code,
                visible: o.visible,
                omp_title: o.omp_title,
            };
            let was_busy = self.tracker.is_busy(o.target);
            if let Some(event) =
                self.tracker
                    .observe(o.target.clone(), now, seen, window_focused, cfg)
            {
                fired.push((o.target.clone(), event));
            }
            if !was_busy && self.tracker.is_busy(o.target) && self.inbox.turn_started(o.target) {
                out.effects.push(Effect::Withdraw(o.target.clone()));
                self.tracker.gone(o.target);
            }
        }
        for key in self.tracker.retain(&live) {
            out.effects.push(Effect::Withdraw(key));
        }
        self.inbox.retain_live(&live);
        self.known.retain(|k, _| live.contains(k));
        for (key, event) in fired {
            let looking = window_focused && visible.contains(&key);
            match event {
                Event::TurnFinished => {
                    self.classify_finished_turn(key, looking, window_focused, cfg, &mut out)
                }
                Event::Attention => {
                    self.record(key, Kind::Attention, looking, window_focused, cfg, &mut out)
                }
                Event::Exited { code } => self.record(
                    key,
                    Kind::Exited { code },
                    looking,
                    window_focused,
                    cfg,
                    &mut out,
                ),
            }
        }
        while let Ok(done) = self.classified.try_recv() {
            let key = done.ticket.uid.clone();
            // Only for the very state it was asked about: same session, same session file, no
            // status change since (a newer attention/exit/next turn wins over a late verdict).
            let current = self
                .known
                .get(&key)
                .is_some_and(|k| self.tracker.is_current(&done.ticket, k.session.as_deref()));
            // And only if the user still is not looking (unless the inbox also wants the
            // session in view).
            let looking = window_focused && visible.contains(&key);
            if let Some(kind) = done.kind
                && current
                && !(looking && !cfg.while_viewing)
            {
                self.record(key, kind, looking, window_focused, cfg, &mut out);
            }
        }
        for key in self.inbox.sync_viewing(&visible, window_focused) {
            self.tracker.gone(&key);
            out.effects.push(Effect::Withdraw(key));
        }
        for key in self.tracker.take_seen(&visible, window_focused) {
            out.effects.push(Effect::Withdraw(key));
        }
        if self.tracker.any_busy() {
            out.watch_in = Some(BUSY_WATCH);
        }
        out
    }

    fn remember(&mut self, o: &Observation<'_>) {
        match self.known.get_mut(o.target) {
            Some(k) => {
                if k.session.as_deref() != o.session {
                    k.session = o.session.map(Path::to_path_buf);
                }
                if k.title != o.title {
                    o.title.clone_into(&mut k.title);
                }
                if k.project != o.project {
                    o.project.clone_into(&mut k.project);
                }
            }
            None => {
                self.known.insert(
                    o.target.clone(),
                    Known {
                        session: o.session.map(Path::to_path_buf),
                        title: o.title.to_owned(),
                        project: o.project.to_owned(),
                    },
                );
            }
        }
    }

    /// A turn just ended: find out on a worker how, then record it (see the verdict drain in
    /// [`tick`](Self::tick)).
    fn classify_finished_turn(
        &mut self,
        key: SessionTarget,
        looking: bool,
        window_focused: bool,
        cfg: &NotifyConfig,
        out: &mut Ticked,
    ) {
        let Some(session) = self.known.get(&key).map(|k| k.session.clone()) else {
            return;
        };
        let busy_start = SystemTime::now()
            .checked_sub(self.tracker.last_busy(&key))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let Some(ticket) = self.tracker.ticket(key.clone(), session.clone()) else {
            return;
        };
        let (tx, wake) = (self.classified_tx.clone(), self.wake.clone());
        let spawned = std::thread::Builder::new()
            .name("omp-deck-turn-outcome".to_owned())
            .spawn(move || {
                let kind = classify::finished_turn(session.as_deref(), busy_start);
                if tx.send(Classified { ticket, kind }).is_ok() {
                    wake();
                }
            });
        if let Err(e) = spawned {
            log::warn!("cannot start the turn classifier: {e}");
            self.record(key, Kind::Finished, looking, window_focused, cfg, out);
        }
    }

    /// Puts the event in the inbox and marks the bell for turn ends of a session the user is not
    /// looking at (`looking`: window focused and the session shown). A desktop pop-up follows
    /// [`tracker::desktop_wanted`], also for a session in view; that one stays with its inbox
    /// entry. Whether the inbox records an event for a session in view is decided by the caller
    /// (`while_viewing`). Every recorded event is also offered to the phones
    /// ([`Effect::Mobile`]), once, whatever the desktop policy.
    fn record(
        &mut self,
        key: SessionTarget,
        kind: Kind,
        looking: bool,
        window_focused: bool,
        cfg: &NotifyConfig,
        out: &mut Ticked,
    ) {
        let Some(known) = self.known.get(&key) else {
            return;
        };
        if !looking && matches!(kind, Kind::Finished | Kind::Error(_)) {
            out.effects.push(Effect::Bell(key.clone()));
        }
        let (title, body) = tracker::describe(&kind, &known.title, &known.project);
        let sound = cfg.sound.then(|| Sound::of(&kind));
        out.effects.push(Effect::Mobile {
            target: key.clone(),
            kind: MobileKind::of(&kind),
            title: known.title.clone(),
        });
        let entry = Entry::new(
            key.clone(),
            kind,
            &known.title,
            &known.project,
            SystemTime::now(),
        );
        self.inbox.record(if looking {
            entry.while_looking()
        } else {
            entry
        });
        if tracker::desktop_wanted(cfg, window_focused) {
            self.tracker.shown(key.clone(), looking);
            out.effects.push(Effect::Show {
                target: key,
                title,
                body,
                sound,
            });
        }
    }

    /// The user opened the session from its inbox entry (the client then shows the session, if
    /// it is still this target). A target no longer live is ignored.
    pub fn open(&mut self, target: &SessionTarget) -> Vec<Effect> {
        self.dismiss(target)
    }

    /// The user dismissed the session's inbox entry. A target no longer live is ignored.
    pub fn dismiss(&mut self, target: &SessionTarget) -> Vec<Effect> {
        if !self.known.contains_key(target) {
            return Vec::new();
        }
        self.inbox.remove(target);
        self.tracker.gone(target);
        vec![Effect::Withdraw(target.clone())]
    }

    /// "Clear all" in the inbox.
    pub fn clear_all(&mut self) -> Vec<Effect> {
        for e in self.inbox.entries() {
            self.tracker.gone(&e.tab);
        }
        self.inbox.clear();
        vec![Effect::WithdrawAll]
    }

    /// The platform reports a click on the session's pop-up (the client then shows the session,
    /// if it is still this target). A target no longer live is ignored.
    pub fn clicked(&mut self, target: &SessionTarget) {
        if !self.known.contains_key(target) {
            return;
        }
        self.tracker.gone(target);
        self.inbox.remove(target);
    }

    /// The platform reports the session's pop-up went away without a click.
    pub fn gone(&mut self, target: &SessionTarget) {
        self.tracker.gone(target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::SessionKey;
    use std::io::Write as _;

    fn center() -> NotificationCenter {
        NotificationCenter::new(Arc::new(|| {}))
    }

    fn target(key: SessionKey, generation: u64) -> SessionTarget {
        SessionTarget {
            epoch: crate::controller::ServiceEpoch::current(),
            key,
            generation,
        }
    }

    fn obs<'a>(
        target: &'a SessionTarget,
        status: TabStatus,
        session: Option<&'a Path>,
        visible: bool,
    ) -> Observation<'a> {
        Observation {
            target,
            status,
            exit_code: match status {
                TabStatus::Exited { clean } => Some(if clean { 0 } else { 1 }),
                _ => None,
            },
            session,
            title: "fix bug",
            project: "proj",
            visible,
            omp_title: true,
        }
    }

    /// Ticks until the classifier's verdict arrived (or a few seconds passed).
    fn settle(
        c: &mut NotificationCenter,
        at: Instant,
        o: &[Observation<'_>],
        focused: bool,
        cfg: &NotifyConfig,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        for _ in 0..200 {
            let t = c.tick(at, o, focused, cfg);
            let done = t.effects.iter().any(|e| matches!(e, Effect::Show { .. }));
            effects.extend(t.effects);
            if done {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        effects
    }

    fn session_file(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("temp file");
        for l in lines {
            writeln!(f, "{l}").expect("write");
        }
        f
    }

    #[test]
    fn a_failed_turn_of_the_focused_session_is_an_error_in_the_inbox_and_a_popup_policy_decision() {
        // The session in view, window focused: while_viewing records it (no bell), the pop-up
        // follows only_when_unfocused (default: none while focused).
        let file = session_file(&[
            r#"{"type":"message","timestamp":"2099-01-01T00:00:00Z","message":{"role":"assistant","stopReason":"error","errorMessage":"rate limited","content":[]}}"#,
        ]);
        let key = target(SessionKey::host("omp-deck", "s1"), 1);
        let cfg = NotifyConfig::default();
        let mut c = center();
        let t0 = Instant::now();
        let path = Some(file.path());
        c.tick(t0, &[obs(&key, TabStatus::Busy, path, true)], true, &cfg);
        let end = t0 + Duration::from_secs(10);
        let mut effects = Vec::new();
        for _ in 0..200 {
            effects.extend(
                c.tick(end, &[obs(&key, TabStatus::Idle, path, true)], true, &cfg)
                    .effects,
            );
            if !c.inbox().entries().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let entries = c.inbox().entries();
        assert_eq!(entries.len(), 1, "{effects:?}");
        assert!(matches!(&entries[0].kind, Kind::Error(Some(m)) if m == "rate limited"));
        assert!(!entries[0].armed, "recorded while looking");
        // The phones are still offered the event (their own presence decides).
        assert_eq!(
            effects,
            [Effect::Mobile {
                target: key.clone(),
                kind: MobileKind::TurnFinished,
                title: "fix bug".into(),
            }],
            "no bell and no pop-up for a focused session in view"
        );
    }

    #[test]
    fn an_aborted_turn_is_never_reported() {
        let file = session_file(&[
            r#"{"type":"message","timestamp":"2099-01-01T00:00:00Z","message":{"role":"assistant","stopReason":"aborted","content":[]}}"#,
        ]);
        let key = target(SessionKey::host("omp-deck", "s2"), 1);
        let cfg = NotifyConfig::default();
        let mut c = center();
        let t0 = Instant::now();
        let path = Some(file.path());
        c.tick(t0, &[obs(&key, TabStatus::Busy, path, false)], false, &cfg);
        let end = t0 + Duration::from_secs(10);
        let effects = settle(
            &mut c,
            end,
            &[obs(&key, TabStatus::Idle, path, false)],
            false,
            &cfg,
        );
        assert!(effects.is_empty(), "{effects:?}");
        assert!(c.inbox().entries().is_empty());
    }

    #[test]
    fn a_verdict_is_dropped_when_the_session_file_changed_while_status_stayed_idle() {
        // /resume swaps the file without a status change: the earlier turn's verdict must not be
        // recorded against the new conversation.
        let old = session_file(&[]);
        let new = session_file(&[]);
        let key = target(SessionKey::host("omp-deck", "s3"), 1);
        let cfg = NotifyConfig::default();
        let mut c = center();
        let t0 = Instant::now();
        c.tick(
            t0,
            &[obs(&key, TabStatus::Busy, Some(old.path()), false)],
            false,
            &cfg,
        );
        let end = t0 + Duration::from_secs(10);
        // The turn ends; the verdict is requested for the old file.
        c.tick(
            end,
            &[obs(&key, TabStatus::Idle, Some(old.path()), false)],
            false,
            &cfg,
        );
        let effects = settle(
            &mut c,
            end,
            &[obs(&key, TabStatus::Idle, Some(new.path()), false)],
            false,
            &cfg,
        );
        assert!(effects.is_empty(), "{effects:?}");
        assert!(c.inbox().entries().is_empty());
    }

    #[test]
    fn a_background_finished_turn_rings_the_bell_and_pops_up_when_unfocused() {
        let key = target(SessionKey::tmux("omp-deck", "s4"), 1);
        let cfg = NotifyConfig::default();
        let mut c = center();
        let t0 = Instant::now();
        c.tick(t0, &[obs(&key, TabStatus::Busy, None, false)], false, &cfg);
        let effects = settle(
            &mut c,
            t0 + Duration::from_secs(10),
            &[obs(&key, TabStatus::Idle, None, false)],
            false,
            &cfg,
        );
        assert!(effects.contains(&Effect::Bell(key.clone())), "{effects:?}");
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Show { target: k, .. } if *k == key)),
            "{effects:?}"
        );
        // Looking at it (window focused, session shown) withdraws pop-up and entry.
        let t = c.tick(
            t0 + Duration::from_secs(11),
            &[obs(&key, TabStatus::Idle, None, true)],
            true,
            &cfg,
        );
        assert!(t.effects.contains(&Effect::Withdraw(key.clone())));
        assert!(c.inbox().entries().is_empty());
    }

    #[test]
    fn a_popup_asks_for_its_events_sound_unless_sounds_are_off() {
        let shown_sound = |cfg: &NotifyConfig| -> Option<Sound> {
            let key = target(SessionKey::tmux("omp-deck", "snd"), 1);
            let mut c = center();
            let t0 = Instant::now();
            c.tick(t0, &[obs(&key, TabStatus::Busy, None, false)], false, cfg);
            let effects = settle(
                &mut c,
                t0 + Duration::from_secs(10),
                &[obs(&key, TabStatus::Idle, None, false)],
                false,
                cfg,
            );
            effects
                .into_iter()
                .find_map(|e| match e {
                    Effect::Show { sound, .. } => Some(sound),
                    _ => None,
                })
                .expect("a pop-up")
        };
        assert_eq!(shown_sound(&NotifyConfig::default()), Some(Sound::Finished));
        let quiet = NotifyConfig {
            sound: false,
            ..NotifyConfig::default()
        };
        assert_eq!(shown_sound(&quiet), None);
    }

    #[test]
    fn each_kind_has_its_own_theme_sound() {
        let name = |k: Kind| Sound::of(&k).theme_name();
        assert_eq!(name(Kind::Finished), "complete");
        assert_eq!(name(Kind::Attention), "dialog-warning");
        assert_eq!(name(Kind::Error(None)), "dialog-error");
        assert_eq!(name(Kind::Exited { code: Some(0) }), "message-new-instant");
    }

    #[test]
    fn same_named_tmux_and_host_sessions_are_notified_separately() {
        let tmux = target(SessionKey::tmux("omp-deck", "s"), 1);
        let host = target(SessionKey::host("omp-deck", "s"), 1);
        let cfg = NotifyConfig::default();
        let mut c = center();
        let t0 = Instant::now();
        c.tick(
            t0,
            &[
                obs(&tmux, TabStatus::Busy, None, false),
                obs(&host, TabStatus::Idle, None, false),
            ],
            false,
            &cfg,
        );
        let later = t0 + Duration::from_secs(10);
        let effects = settle(
            &mut c,
            later,
            &[
                obs(&tmux, TabStatus::Idle, None, false),
                obs(&host, TabStatus::Idle, None, false),
            ],
            false,
            &cfg,
        );
        assert!(effects.contains(&Effect::Bell(tmux.clone())), "{effects:?}");
        assert!(
            !effects.contains(&Effect::Bell(host.clone())),
            "{effects:?}"
        );
        // Looking at the host session leaves the tmux session's entry alone.
        let t = c.tick(
            later + Duration::from_secs(1),
            &[
                obs(&tmux, TabStatus::Idle, None, false),
                obs(&host, TabStatus::Idle, None, true),
            ],
            true,
            &cfg,
        );
        assert!(
            !t.effects.contains(&Effect::Withdraw(tmux.clone())),
            "{:?}",
            t.effects
        );
        assert_eq!(c.inbox().entries().len(), 1);
        assert_eq!(c.inbox().entries()[0].tab, tmux);
    }

    #[test]
    fn a_restarted_session_starts_without_the_old_runs_state() {
        // Same backend session, same (absent) file: the restart is a new generation.
        let old = target(SessionKey::tmux("omp-deck", "r"), 1);
        let new = target(SessionKey::tmux("omp-deck", "r"), 2);
        let cfg = NotifyConfig::default();
        let mut c = center();
        let t0 = Instant::now();
        c.tick(t0, &[obs(&old, TabStatus::Busy, None, false)], false, &cfg);
        // The old run's busy stretch is not the new run's: its first idle sighting is no turn end.
        let t = c.tick(
            t0 + Duration::from_secs(10),
            &[obs(&new, TabStatus::Idle, None, false)],
            false,
            &cfg,
        );
        assert!(t.effects.is_empty(), "{:?}", t.effects);
        std::thread::sleep(Duration::from_millis(100));
        let t = c.tick(
            t0 + Duration::from_secs(11),
            &[obs(&new, TabStatus::Idle, None, false)],
            false,
            &cfg,
        );
        assert!(t.effects.is_empty(), "{:?}", t.effects);
        assert!(c.inbox().entries().is_empty());
    }

    #[test]
    fn the_old_runs_entry_goes_and_its_late_click_is_ignored() {
        let old = target(SessionKey::host("omp-deck", "r"), 1);
        let new = target(SessionKey::host("omp-deck", "r"), 2);
        let cfg = NotifyConfig::default();
        let mut c = center();
        let t0 = Instant::now();
        c.tick(t0, &[obs(&old, TabStatus::Busy, None, false)], false, &cfg);
        let effects = settle(
            &mut c,
            t0 + Duration::from_secs(10),
            &[obs(&old, TabStatus::Idle, None, false)],
            false,
            &cfg,
        );
        assert!(effects.contains(&Effect::Bell(old.clone())), "{effects:?}");
        let t = c.tick(
            t0 + Duration::from_secs(11),
            &[obs(&new, TabStatus::Idle, None, false)],
            false,
            &cfg,
        );
        assert!(
            t.effects.contains(&Effect::Withdraw(old.clone())),
            "{:?}",
            t.effects
        );
        assert!(c.inbox().entries().is_empty());
        // The old run's pop-up is clicked late: nothing happens to the new run.
        assert!(c.dismiss(&old).is_empty());
        c.clicked(&old);
        assert!(c.inbox().entries().is_empty());
    }

    fn mobile(effects: &[Effect]) -> Vec<(SessionTarget, MobileKind)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Mobile { target, kind, .. } => Some((target.clone(), *kind)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn each_canonical_event_is_offered_to_the_phones_exactly_once_whatever_the_desktop_policy() {
        // Pop-ups off entirely: the desktop shows nothing, the phones still get each event.
        let cfg = NotifyConfig {
            desktop: false,
            ..NotifyConfig::default()
        };
        let key = target(SessionKey::tmux("omp-deck", "m1"), 1);
        let mut c = center();
        let t0 = Instant::now();
        c.tick(t0, &[obs(&key, TabStatus::Busy, None, false)], false, &cfg);
        let end = t0 + Duration::from_secs(10);
        let mut effects = Vec::new();
        for _ in 0..200 {
            effects.extend(
                c.tick(end, &[obs(&key, TabStatus::Idle, None, false)], false, &cfg)
                    .effects,
            );
            if !c.inbox().entries().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // More ticks of the same state add nothing.
        for i in 1..5 {
            effects.extend(
                c.tick(
                    end + Duration::from_secs(i),
                    &[obs(&key, TabStatus::Idle, None, false)],
                    false,
                    &cfg,
                )
                .effects,
            );
        }
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Show { .. })),
            "{effects:?}"
        );
        assert_eq!(
            mobile(&effects),
            [(key.clone(), MobileKind::TurnFinished)],
            "{effects:?}"
        );
        let effects = c
            .tick(
                end + Duration::from_secs(20),
                &[obs(&key, TabStatus::Exited { clean: true }, None, false)],
                false,
                &cfg,
            )
            .effects;
        assert_eq!(mobile(&effects), [(key, MobileKind::Ended)], "{effects:?}");
    }

    #[test]
    fn a_late_verdict_the_tracker_dropped_offers_nothing_to_the_phones() {
        let old = session_file(&[]);
        let new = session_file(&[]);
        let key = target(SessionKey::host("omp-deck", "m2"), 1);
        let cfg = NotifyConfig::default();
        let mut c = center();
        let t0 = Instant::now();
        c.tick(
            t0,
            &[obs(&key, TabStatus::Busy, Some(old.path()), false)],
            false,
            &cfg,
        );
        let end = t0 + Duration::from_secs(10);
        let mut effects = c
            .tick(
                end,
                &[obs(&key, TabStatus::Idle, Some(old.path()), false)],
                false,
                &cfg,
            )
            .effects;
        for _ in 0..25 {
            std::thread::sleep(Duration::from_millis(20));
            effects.extend(
                c.tick(
                    end,
                    &[obs(&key, TabStatus::Idle, Some(new.path()), false)],
                    false,
                    &cfg,
                )
                .effects,
            );
        }
        assert!(mobile(&effects).is_empty(), "{effects:?}");
    }

    #[test]
    fn mobile_kinds_follow_the_push_preferences() {
        assert_eq!(MobileKind::of(&Kind::Finished), MobileKind::TurnFinished);
        assert_eq!(MobileKind::of(&Kind::Error(None)), MobileKind::TurnFinished);
        assert_eq!(MobileKind::of(&Kind::Attention), MobileKind::Attention);
        assert_eq!(
            MobileKind::of(&Kind::Exited { code: Some(1) }),
            MobileKind::Ended
        );
    }
}
