//! When a session's status change deserves a notification: a pure state machine over
//! [`TabStatus`] transitions, the window's focus, which sessions are in view and the user's
//! toggles. Keyed by the session's identity `K` (the service uses
//! [`SessionKey`](crate::controller::SessionKey)).

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_core::config::NotifyConfig;
use deck_core::status::TurnOutcome;

use super::inbox;
use crate::status::TabStatus;

/// A turn must have run at least this long before its end is worth a notification.
pub const MIN_BUSY: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// omp finished a turn (busy → idle).
    TurnFinished,
    /// omp waits for the user (`π !`).
    Attention,
    /// The process ended.
    Exited { code: Option<i32> },
}

/// The notification-relevant state of a tab (the bell is not one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Idle,
    Busy,
    Attention,
    Exited,
}

impl Kind {
    fn of(status: TabStatus) -> Kind {
        match status {
            TabStatus::Exited { .. } => Kind::Exited,
            TabStatus::Attention => Kind::Attention,
            TabStatus::Busy => Kind::Busy,
            TabStatus::Bell | TabStatus::Idle => Kind::Idle,
        }
    }
}

/// One tab as seen this frame.
#[derive(Clone, Copy, Debug)]
pub struct Sighting {
    pub status: TabStatus,
    pub exit_code: Option<i32>,
    /// The tab is shown in a pane (the active tab, or any pane of a split).
    pub visible: bool,
    /// omp's state is known: it reported a run state (OSC 7501), set its `π …` title, or (TSP)
    /// shows a document that says what it does. Without it a live status says nothing about
    /// turns: omp has not said anything yet, or tmux restored the title from before it because
    /// the process is ending (`CSI 23 t`, one tick before the exit is seen). That must not
    /// read as "idle".
    pub omp_title: bool,
}

/// What an asynchronous turn classification was asked about; see [`Tracker::is_current`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket<K> {
    pub uid: K,
    generation: u64,
    session: Option<PathBuf>,
}

struct Memory {
    kind: Kind,
    /// When the current busy stretch was first seen.
    busy_since: Option<Instant>,
    /// Length of the busy stretch that ended last.
    last_busy: Duration,
    /// Counts this tab's tracked status transitions; an asynchronous verdict about an earlier
    /// state must not outlive the next one.
    generation: u64,
}

pub struct Tracker<K> {
    tabs: HashMap<K, Memory>,
    /// Tabs with a notification still on screen.
    shown: HashSet<K>,
    /// Of those, the ones sent while the user looked at the tab: [`Tracker::take_seen`] leaves
    /// them alone, their inbox entry's removal withdraws them (see `Inbox::sync_viewing`).
    held: HashSet<K>,
}

impl<K> Default for Tracker<K> {
    fn default() -> Self {
        Tracker {
            tabs: HashMap::new(),
            shown: HashSet::new(),
            held: HashSet::new(),
        }
    }
}

/// Whether `event` belongs in the inbox: its toggle is on and either the user may be told while
/// looking at the tab (`while_viewing`) or is not looking (window focused, tab shown in a pane).
pub fn wanted(cfg: &NotifyConfig, event: Event, window_focused: bool, visible: bool) -> bool {
    let toggle = match event {
        Event::TurnFinished => cfg.turn_finished,
        Event::Attention => cfg.attention,
        Event::Exited { .. } => cfg.exited,
    };
    toggle && (cfg.while_viewing || !(window_focused && visible))
}

/// Whether a recorded event also gets a desktop pop-up.
pub fn desktop_wanted(cfg: &NotifyConfig, window_focused: bool) -> bool {
    cfg.desktop && (!cfg.only_when_unfocused || !window_focused)
}

impl<K: Clone + Eq + Hash> Tracker<K> {
    /// Feed one tab's current state. Returns the event to notify about, once per transition
    /// (a transition that is gated out is consumed all the same).
    pub fn observe(
        &mut self,
        uid: K,
        now: Instant,
        seen: Sighting,
        window_focused: bool,
        cfg: &NotifyConfig,
    ) -> Option<Event> {
        let kind = Kind::of(seen.status);
        let Some(mem) = self.tabs.get_mut(&uid) else {
            // First sight (a restored or just-opened tab): nothing happened yet.
            self.tabs.insert(
                uid,
                Memory {
                    kind,
                    busy_since: (kind == Kind::Busy).then_some(now),
                    last_busy: Duration::ZERO,
                    generation: 0,
                },
            );
            return None;
        };
        let prev = mem.kind;
        // Only the exit (or omp's own title) changes what is known about a live session.
        if prev == kind || (!seen.omp_title && kind != Kind::Exited) {
            return None;
        }
        let busy_for = mem
            .busy_since
            .map_or(Duration::ZERO, |since| now.duration_since(since));
        mem.kind = kind;
        mem.busy_since = (kind == Kind::Busy).then_some(now);
        mem.last_busy = busy_for;
        mem.generation += 1;
        let event = match (prev, kind) {
            (Kind::Busy, Kind::Idle) if busy_for >= MIN_BUSY => Some(Event::TurnFinished),
            (_, Kind::Attention) => Some(Event::Attention),
            (_, Kind::Exited) => Some(Event::Exited {
                code: seen.exit_code,
            }),
            _ => None,
        };
        event.filter(|e| wanted(cfg, *e, window_focused, seen.visible))
    }

    /// Whether the tab was last seen busy (a turn is running).
    pub fn is_busy(&self, uid: &K) -> bool {
        self.tabs.get(uid).is_some_and(|m| m.kind == Kind::Busy)
    }

    /// A notification for `uid` was sent; `while_looking` when the user was looking at the tab,
    /// which keeps it up until it is withdrawn with the tab's inbox entry.
    pub fn shown(&mut self, uid: K, while_looking: bool) {
        if while_looking {
            self.held.insert(uid.clone());
        } else {
            self.held.remove(&uid);
        }
        self.shown.insert(uid);
    }

    /// The notification for `uid` is gone (dismissed, expired, clicked).
    pub fn gone(&mut self, uid: &K) {
        self.shown.remove(uid);
        self.held.remove(uid);
    }

    /// How long the busy stretch lasted that `uid`'s latest busy → other transition ended.
    pub fn last_busy(&self, uid: &K) -> Duration {
        self.tabs.get(uid).map_or(Duration::ZERO, |m| m.last_busy)
    }

    /// The transition count of `uid`, `None` for an unknown tab.
    pub fn generation(&self, uid: &K) -> Option<u64> {
        self.tabs.get(uid).map(|m| m.generation)
    }

    /// Whether a verdict requested under `ticket` still describes the tab: it exists, has not
    /// changed status since, and still shows the same session file.
    pub fn is_current(&self, ticket: &Ticket<K>, session: Option<&Path>) -> bool {
        self.generation(&ticket.uid) == Some(ticket.generation)
            && ticket.session.as_deref() == session
    }

    /// Stamp for an asynchronous verdict about `uid`'s present state.
    pub fn ticket(&self, uid: K, session: Option<PathBuf>) -> Option<Ticket<K>> {
        Some(Ticket {
            generation: self.generation(&uid)?,
            uid,
            session,
        })
    }

    /// The tabs whose notification should be withdrawn now: the user looks at them (window
    /// focused, tabs shown in a pane). Notifications sent while looking are not taken.
    pub fn take_seen(&mut self, visible: &[K], window_focused: bool) -> Vec<K> {
        if !window_focused {
            return Vec::new();
        }
        visible
            .iter()
            .filter(|uid| !self.held.contains(*uid) && self.shown.remove(*uid))
            .cloned()
            .collect()
    }

    /// Forget tabs that no longer exist; returns those that still had a notification up.
    pub fn retain(&mut self, live: &HashSet<K>) -> Vec<K> {
        self.tabs.retain(|uid, _| live.contains(uid));
        let stale: Vec<K> = self
            .shown
            .iter()
            .filter(|u| !live.contains(*u))
            .cloned()
            .collect();
        for u in &stale {
            self.shown.remove(u);
            self.held.remove(u);
        }
        stale
    }

    /// Some tab is busy right now (the caller keeps repainting to see it end).
    pub fn any_busy(&self) -> bool {
        self.tabs.values().any(|m| m.kind == Kind::Busy)
    }
}

/// Title and body of the desktop notification for `kind` on a tab named `tab` in project `project`
/// (an error's message goes on a line of its own).
pub fn describe(kind: &inbox::Kind, tab: &str, project: &str) -> (String, String) {
    let what = kind.what();
    let mut body = if project.is_empty() {
        what
    } else {
        format!("{what} · {project}")
    };
    if let inbox::Kind::Error(Some(msg)) = kind
        && !msg.is_empty()
    {
        body.push('\n');
        body.push_str(msg);
    }
    (tab.to_owned(), body)
}

/// What a finished turn is reported as, from how its last assistant message ended; `None` = the
/// user stopped it (omp stays quiet then too).
pub fn classify(outcome: TurnOutcome) -> Option<inbox::Kind> {
    match outcome {
        TurnOutcome::Error(msg) => Some(inbox::Kind::Error(msg)),
        TurnOutcome::Aborted => None,
        TurnOutcome::Completed | TurnOutcome::Unknown => Some(inbox::Kind::Finished),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(status: TabStatus, active: bool) -> Sighting {
        Sighting {
            status,
            exit_code: match status {
                TabStatus::Exited { clean } => Some(if clean { 0 } else { 2 }),
                _ => None,
            },
            visible: active,
            omp_title: true,
        }
    }

    const BUSY: TabStatus = TabStatus::Busy;
    const IDLE: TabStatus = TabStatus::Idle;

    /// Busy for `secs`, then `end`; returns the event at the end. Window state as given.
    fn run(
        end: TabStatus,
        secs: u64,
        focused: bool,
        active: bool,
        cfg: &NotifyConfig,
    ) -> Option<Event> {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe(1, t0, seen(IDLE, active), focused, cfg);
        t.observe(1, t0, seen(BUSY, active), focused, cfg);
        t.observe(
            1,
            t0 + Duration::from_secs(secs),
            seen(end, active),
            focused,
            cfg,
        )
    }

    #[test]
    fn turn_finished_needs_three_busy_seconds() {
        let cfg = NotifyConfig::default();
        assert_eq!(run(IDLE, 2, false, false, &cfg), None);
        assert_eq!(run(IDLE, 3, false, false, &cfg), Some(Event::TurnFinished));
        assert_eq!(run(IDLE, 90, false, false, &cfg), Some(Event::TurnFinished));
    }

    #[test]
    fn attention_and_exit_fire_regardless_of_busy_time() {
        let cfg = NotifyConfig::default();
        assert_eq!(
            run(TabStatus::Attention, 0, false, false, &cfg),
            Some(Event::Attention)
        );
        assert_eq!(
            run(TabStatus::Exited { clean: false }, 0, false, false, &cfg),
            Some(Event::Exited { code: Some(2) })
        );
        assert_eq!(
            run(TabStatus::Exited { clean: true }, 0, false, false, &cfg),
            Some(Event::Exited { code: Some(0) })
        );
    }

    #[test]
    fn idle_to_attention_fires_too() {
        let cfg = NotifyConfig::default();
        let mut t = Tracker::default();
        let now = Instant::now();
        t.observe(1, now, seen(IDLE, false), false, &cfg);
        assert_eq!(
            t.observe(1, now, seen(TabStatus::Attention, false), false, &cfg),
            Some(Event::Attention)
        );
    }

    #[test]
    fn answering_attention_then_finishing_notifies_once_for_each_transition() {
        let cfg = NotifyConfig::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        t.observe(1, at(0), seen(BUSY, false), false, &cfg);
        assert_eq!(
            t.observe(1, at(5), seen(TabStatus::Attention, false), false, &cfg),
            Some(Event::Attention)
        );
        assert_eq!(
            t.observe(1, at(6), seen(TabStatus::Attention, false), false, &cfg),
            None
        );
        // busy time restarts when the user answers
        assert_eq!(t.observe(1, at(7), seen(BUSY, false), false, &cfg), None);
        assert_eq!(t.observe(1, at(8), seen(IDLE, false), false, &cfg), None);
        t.observe(1, at(9), seen(BUSY, false), false, &cfg);
        assert_eq!(
            t.observe(1, at(13), seen(IDLE, false), false, &cfg),
            Some(Event::TurnFinished)
        );
    }

    #[test]
    fn a_transition_is_reported_only_once() {
        let cfg = NotifyConfig::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe(1, t0, seen(BUSY, false), false, &cfg);
        let end = t0 + Duration::from_secs(10);
        assert_eq!(
            t.observe(1, end, seen(IDLE, false), false, &cfg),
            Some(Event::TurnFinished)
        );
        assert_eq!(t.observe(1, end, seen(IDLE, false), false, &cfg), None);
        assert_eq!(
            t.observe(
                1,
                end + Duration::from_secs(1),
                seen(IDLE, false),
                false,
                &cfg
            ),
            None
        );
    }

    #[test]
    fn first_sight_of_a_tab_never_notifies() {
        let cfg = NotifyConfig::default();
        for s in [
            IDLE,
            BUSY,
            TabStatus::Attention,
            TabStatus::Exited { clean: false },
        ] {
            let mut t = Tracker::default();
            assert_eq!(
                t.observe(7, Instant::now(), seen(s, false), false, &cfg),
                None
            );
        }
    }

    #[test]
    fn bell_counts_as_idle() {
        let cfg = NotifyConfig::default();
        assert_eq!(
            run(TabStatus::Bell, 10, false, false, &cfg),
            Some(Event::TurnFinished)
        );
    }

    #[test]
    fn toggles_gate_each_event() {
        let mut cfg = NotifyConfig {
            turn_finished: false,
            ..NotifyConfig::default()
        };
        assert_eq!(run(IDLE, 10, false, false, &cfg), None);
        assert_eq!(
            run(TabStatus::Attention, 0, false, false, &cfg),
            Some(Event::Attention)
        );
        cfg.attention = false;
        assert_eq!(run(TabStatus::Attention, 0, false, false, &cfg), None);
        cfg.exited = false;
        assert_eq!(
            run(TabStatus::Exited { clean: false }, 0, false, false, &cfg),
            None
        );
    }

    /// tmux restores the pre-omp title when the process ends, a tick before the exit is seen:
    /// a busy session that ends is "Session ended" (its own toggle), never a finished turn.
    #[test]
    fn a_busy_session_that_ends_is_an_exit_not_a_finished_turn() {
        let ending = |cfg: &NotifyConfig| {
            let mut t = Tracker::default();
            let t0 = Instant::now();
            t.observe(1, t0, seen(IDLE, false), false, cfg);
            t.observe(1, t0, seen(BUSY, false), false, cfg);
            let later = t0 + Duration::from_secs(30);
            let popped = Sighting {
                omp_title: false,
                ..seen(IDLE, false)
            };
            let at_pop = t.observe(1, later, popped, false, cfg);
            let at_exit = t.observe(
                1,
                later,
                Sighting {
                    omp_title: false,
                    ..seen(TabStatus::Exited { clean: true }, false)
                },
                false,
                cfg,
            );
            (at_pop, at_exit)
        };
        let on = NotifyConfig::default();
        assert_eq!(ending(&on), (None, Some(Event::Exited { code: Some(0) })));
        let off = NotifyConfig {
            exited: false,
            ..NotifyConfig::default()
        };
        assert_eq!(ending(&off), (None, None));
        // omp's own idle title after the busy stretch still is a finished turn.
        assert_eq!(run(IDLE, 30, false, false, &off), Some(Event::TurnFinished));
    }

    #[test]
    fn the_desktop_switch_does_not_gate_the_inbox() {
        let cfg = NotifyConfig {
            desktop: false,
            ..NotifyConfig::default()
        };
        assert_eq!(run(IDLE, 10, false, false, &cfg), Some(Event::TurnFinished));
        assert_eq!(
            run(TabStatus::Attention, 0, true, false, &cfg),
            Some(Event::Attention)
        );
    }

    #[test]
    fn the_inbox_records_everything_the_user_is_not_looking_at() {
        let cfg = NotifyConfig {
            while_viewing: false,
            ..NotifyConfig::default()
        };
        assert!(cfg.only_when_unfocused);
        assert_eq!(
            run(IDLE, 10, true, false, &cfg),
            Some(Event::TurnFinished),
            "focused window, background tab"
        );
        assert_eq!(
            run(IDLE, 10, true, true, &cfg),
            None,
            "focused window, shown tab"
        );
        assert_eq!(
            run(IDLE, 10, false, true, &cfg),
            Some(Event::TurnFinished),
            "window away, shown tab"
        );
    }

    #[test]
    fn while_viewing_also_records_the_tab_in_view_but_toggles_still_gate() {
        let mut cfg = NotifyConfig::default();
        assert!(cfg.while_viewing);
        for (focused, visible) in [(true, true), (true, false), (false, true), (false, false)] {
            assert!(wanted(&cfg, Event::TurnFinished, focused, visible));
        }
        cfg.turn_finished = false;
        assert!(!wanted(&cfg, Event::TurnFinished, true, true));
        cfg.turn_finished = true;
        cfg.while_viewing = false;
        assert!(!wanted(&cfg, Event::TurnFinished, true, true));
        for (focused, visible) in [(true, false), (false, true), (false, false)] {
            assert!(wanted(&cfg, Event::TurnFinished, focused, visible));
        }
    }

    #[test]
    fn desktop_pop_ups_follow_the_switch_and_the_focus_rule() {
        let mut cfg = NotifyConfig::default();
        assert!(desktop_wanted(&cfg, false));
        assert!(
            !desktop_wanted(&cfg, true),
            "only when unfocused: a background tab of a focused window stays in the inbox"
        );
        cfg.only_when_unfocused = false;
        assert!(desktop_wanted(&cfg, true));
        cfg.desktop = false;
        assert!(!desktop_wanted(&cfg, false));
        assert!(!desktop_wanted(&cfg, true));
    }

    #[test]
    fn a_finished_turn_is_classified_by_how_it_ended() {
        assert_eq!(
            classify(TurnOutcome::Completed),
            Some(inbox::Kind::Finished)
        );
        assert_eq!(classify(TurnOutcome::Unknown), Some(inbox::Kind::Finished));
        assert_eq!(
            classify(TurnOutcome::Error(Some("boom".into()))),
            Some(inbox::Kind::Error(Some("boom".into())))
        );
        assert_eq!(classify(TurnOutcome::Aborted), None);
    }

    #[test]
    fn the_busy_time_of_a_finished_turn_is_remembered() {
        let cfg = NotifyConfig::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe(1, t0, seen(IDLE, false), false, &cfg);
        t.observe(1, t0, seen(BUSY, false), false, &cfg);
        t.observe(
            1,
            t0 + Duration::from_secs(7),
            seen(IDLE, false),
            false,
            &cfg,
        );
        assert_eq!(t.last_busy(&1), Duration::from_secs(7));
    }

    #[test]
    fn a_transition_while_the_user_looks_is_consumed() {
        let cfg = NotifyConfig {
            while_viewing: false,
            ..NotifyConfig::default()
        };
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe(1, t0, seen(BUSY, true), true, &cfg);
        let end = t0 + Duration::from_secs(10);
        assert_eq!(t.observe(1, end, seen(IDLE, true), true, &cfg), None);
        // the user looks away later: the old transition must not fire now
        assert_eq!(
            t.observe(
                1,
                end + Duration::from_secs(1),
                seen(IDLE, false),
                false,
                &cfg
            ),
            None
        );
    }

    #[test]
    fn tabs_are_tracked_independently() {
        let cfg = NotifyConfig::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe(1, t0, seen(BUSY, false), false, &cfg);
        t.observe(2, t0, seen(IDLE, false), false, &cfg);
        let end = t0 + Duration::from_secs(5);
        assert_eq!(t.observe(2, end, seen(IDLE, false), false, &cfg), None);
        assert_eq!(
            t.observe(1, end, seen(IDLE, false), false, &cfg),
            Some(Event::TurnFinished)
        );
    }

    #[test]
    fn activating_the_tab_withdraws_its_notification_only_when_the_window_is_focused() {
        let mut t = Tracker::default();
        t.shown(4, false);
        assert!(t.take_seen(&[4], false).is_empty(), "window away");
        assert!(t.take_seen(&[5], true).is_empty(), "other tab");
        assert!(t.take_seen(&[], true).is_empty());
        assert_eq!(t.take_seen(&[4], true), vec![4]);
        assert!(t.take_seen(&[4], true).is_empty(), "withdrawn once");
    }

    #[test]
    fn every_visible_pane_counts_as_looked_at() {
        let mut t = Tracker::default();
        t.shown(4, false);
        t.shown(6, false);
        t.shown(9, false);
        let mut seen = t.take_seen(&[4, 9], true);
        seen.sort_unstable();
        assert_eq!(seen, vec![4, 9]);
        assert_eq!(t.take_seen(&[6], true), vec![6]);
    }

    #[test]
    fn a_dismissed_notification_is_not_withdrawn_again() {
        let mut t = Tracker::default();
        t.shown(4, false);
        t.gone(&4);
        assert!(t.take_seen(&[4], true).is_empty());
    }

    #[test]
    fn a_notification_sent_while_looking_is_not_withdrawn_by_looking() {
        let mut t = Tracker::default();
        t.shown(4, true);
        assert!(t.take_seen(&[4], true).is_empty());
        assert!(t.take_seen(&[4], true).is_empty(), "still held");
        // a later one for a tab not in view is an ordinary notification again
        t.shown(4, false);
        assert_eq!(t.take_seen(&[4], true), vec![4]);
        // gone releases the hold
        t.shown(4, true);
        t.gone(&4);
        t.shown(4, false);
        assert_eq!(t.take_seen(&[4], true), vec![4]);
    }

    #[test]
    fn closed_tabs_are_forgotten_and_their_notifications_reported() {
        let cfg = NotifyConfig::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe(1, t0, seen(BUSY, false), false, &cfg);
        t.observe(2, t0, seen(IDLE, false), false, &cfg);
        t.shown(1, false);
        let live: HashSet<u64> = [2].into_iter().collect();
        assert_eq!(t.retain(&live), vec![1]);
        assert!(!t.any_busy());
        // a reused uid starts from scratch: first sight, no event
        assert_eq!(t.observe(1, t0, seen(IDLE, false), false, &cfg), None);
    }

    #[test]
    fn describe_names_the_event_and_project() {
        use inbox::Kind;
        assert_eq!(
            describe(&Kind::Finished, "fix bug", "omp-deck"),
            ("fix bug".into(), "Finished its turn · omp-deck".into())
        );
        assert_eq!(
            describe(&Kind::Attention, "t", "").1,
            "Needs your attention"
        );
        assert_eq!(
            describe(&Kind::Exited { code: Some(137) }, "t", "p").1,
            "Process exited with code 137 · p"
        );
        assert_eq!(
            describe(&Kind::Exited { code: Some(0) }, "t", "p").1,
            "Session ended · p"
        );
        assert_eq!(
            describe(&Kind::Exited { code: None }, "t", "p").1,
            "Process exited · p"
        );
        assert_eq!(
            describe(&Kind::Error(Some("rate limited".into())), "t", "p").1,
            "Stopped with error · p\nrate limited"
        );
    }

    fn observe_default(t: &mut Tracker<u64>, at: Instant, status: TabStatus) {
        t.observe(1, at, seen(status, false), false, &NotifyConfig::default());
    }

    fn finished_turn() -> (Tracker<u64>, Ticket<u64>, Instant) {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        observe_default(&mut t, t0, IDLE);
        observe_default(&mut t, t0, BUSY);
        let t1 = t0 + Duration::from_secs(5);
        observe_default(&mut t, t1, IDLE);
        let ticket = t
            .ticket(1, Some(std::path::PathBuf::from("/s/a.jsonl")))
            .expect("known tab");
        (t, ticket, t1)
    }

    #[test]
    fn a_verdict_is_current_until_the_tab_changes_status() {
        let (mut t, ticket, t1) = finished_turn();
        let path = std::path::Path::new("/s/a.jsonl");
        assert!(t.is_current(&ticket, Some(path)));
        observe_default(&mut t, t1, TabStatus::Attention);
        assert!(!t.is_current(&ticket, Some(path)));
    }

    #[test]
    fn the_next_turn_or_an_exit_outdates_the_verdict() {
        let path = std::path::Path::new("/s/a.jsonl");
        let (mut t, ticket, t1) = finished_turn();
        observe_default(&mut t, t1, BUSY);
        assert!(!t.is_current(&ticket, Some(path)));
        let (mut t, ticket, t1) = finished_turn();
        observe_default(&mut t, t1, TabStatus::Exited { clean: true });
        assert!(!t.is_current(&ticket, Some(path)));
    }

    #[test]
    fn a_different_session_file_or_a_closed_tab_outdates_the_verdict() {
        let (mut t, ticket, _) = finished_turn();
        assert!(!t.is_current(&ticket, Some(std::path::Path::new("/s/b.jsonl"))));
        assert!(!t.is_current(&ticket, None));
        t.retain(&HashSet::new());
        assert!(!t.is_current(&ticket, Some(std::path::Path::new("/s/a.jsonl"))));
    }
}
