//! The session backends this machine offers and which one new tabs use.
//!
//! Every available backend is polled and can stop sessions, whatever the user chose: a session
//! lives in exactly one backend, so switching the setting only decides where *new* sessions are
//! created. Sessions that are still alive in the other backend stay listed as running, can be
//! re-attached (the launch goes to the backend that holds the name) and can be stopped.

use std::time::{Duration, Instant};

use deck_core::config::SessionView;
use deck_core::mux::{BackendChoice, Mux, MuxKind};

/// How long a stop waits for the backend to drop the session.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// The backend new sessions use, given what is available. `Auto` prefers tmux (the Linux
/// default); an explicit choice that is not available falls back to the same preference so the
/// app still works (the caller shows a note, see [`Backends::unavailable_choice`]).
pub fn pick_kind(choice: BackendChoice, available: &[MuxKind]) -> Option<MuxKind> {
    let has = |k: MuxKind| available.contains(&k);
    let wanted = match choice {
        BackendChoice::Tmux => Some(MuxKind::Tmux),
        BackendChoice::Host => Some(MuxKind::Host),
        BackendChoice::Auto => None,
    };
    wanted
        .filter(|k| has(*k))
        .or_else(|| [MuxKind::Tmux, MuxKind::Host].into_iter().find(|k| has(*k)))
}

/// Available backends (tmux first) plus the user's choice.
#[derive(Clone, Debug)]
pub struct Backends {
    muxes: Vec<Mux>,
    choice: BackendChoice,
}

impl Backends {
    /// Probes both backends. `Mux::detect` honours the test-namespace env vars and forces the
    /// host on Windows (both probes then find the same backend, which is kept once).
    pub fn detect(choice: BackendChoice) -> Backends {
        let mut muxes: Vec<Mux> = Vec::with_capacity(2);
        for probe in [BackendChoice::Tmux, BackendChoice::Host] {
            if let Some(mux) = Mux::detect(probe)
                && !muxes.iter().any(|m| m.kind() == mux.kind())
            {
                muxes.push(mux);
            }
        }
        Backends::new(muxes, choice)
    }

    pub fn new(muxes: Vec<Mux>, choice: BackendChoice) -> Backends {
        Backends { muxes, choice }
    }

    /// Every available backend, tmux first.
    pub fn all(&self) -> &[Mux] {
        &self.muxes
    }

    pub fn is_empty(&self) -> bool {
        self.muxes.is_empty()
    }

    pub fn set_choice(&mut self, choice: BackendChoice) {
        self.choice = choice;
    }

    pub fn of_kind(&self, kind: MuxKind) -> Option<&Mux> {
        self.muxes.iter().find(|m| m.kind() == kind)
    }

    pub fn has(&self, kind: MuxKind) -> bool {
        self.of_kind(kind).is_some()
    }

    /// Where a new session is created.
    pub fn primary(&self) -> Option<&Mux> {
        let kinds: Vec<MuxKind> = self.muxes.iter().map(Mux::kind).collect();
        pick_kind(self.choice, &kinds).and_then(|k| self.of_kind(k))
    }

    /// The backend a launch runs on. A session that is alive keeps its `owner`. A new session
    /// opened in the chat view needs the host (tmux cannot carry TSP), so it uses it whenever it
    /// is available, whatever the choice; without a host it takes the primary backend and runs
    /// as a terminal. Everything else goes to the primary backend.
    pub fn for_launch(&self, owner: Option<MuxKind>, view: SessionView) -> Option<&Mux> {
        owner
            .and_then(|k| self.of_kind(k))
            .or_else(|| {
                (view == SessionView::Chat)
                    .then(|| self.of_kind(MuxKind::Host))
                    .flatten()
            })
            .or_else(|| self.primary())
    }

    /// The explicitly chosen backend when it is not available and another one stands in.
    pub fn unavailable_choice(&self) -> Option<MuxKind> {
        let wanted = match self.choice {
            BackendChoice::Auto => return None,
            BackendChoice::Tmux => MuxKind::Tmux,
            BackendChoice::Host => MuxKind::Host,
        };
        (!self.has(wanted) && self.primary().is_some()).then_some(wanted)
    }

    /// Whether any backend lists session `name`. Blocks.
    pub fn has_session(&self, name: &str) -> bool {
        self.muxes.iter().any(|m| m.has_session(name))
    }

    /// The warning banner for the current backend situation, if any.
    pub fn banner(&self) -> Option<String> {
        banner_text(self, cfg!(windows))
    }

    /// Namespace that scopes the control endpoints of every tab, fixed at startup: the one of
    /// the backend new sessions use (tmux's socket or the host namespace; both are `omp-deck`
    /// in production, so existing tabs stay reachable), `direct` without any backend.
    pub fn ctl_namespace(&self) -> String {
        self.primary().map_or("direct", Mux::namespace).to_owned()
    }

    /// Writes tmux's generated config; the host needs none.
    pub fn prepare(&self) {
        for mux in &self.muxes {
            if let Mux::Tmux(t) = mux
                && let Err(e) = t.ensure_config()
            {
                log::error!("writing tmux config failed: {e}");
            }
        }
    }

    /// Kills session `name` in whichever backend holds it (none: it is already gone). Blocks.
    pub fn kill(&self, name: &str) -> Result<(), String> {
        let mut first_error = None;
        for mux in &self.muxes {
            if mux.has_session(name)
                && let Err(e) = mux.kill(name)
            {
                log::warn!("kill {name}: {e}");
                first_error.get_or_insert(e.to_string());
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// [`kill`](Self::kill), then waits until no backend lists the session anymore. Blocks.
    pub fn stop(&self, name: &str) -> Result<(), String> {
        self.kill(name)?;
        let deadline = Instant::now() + STOP_TIMEOUT;
        while self.muxes.iter().any(|m| m.has_session(name)) {
            if Instant::now() >= deadline {
                return Err(format!("session {name} did not stop"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }
}

/// UI name of a backend.
pub fn kind_label(kind: MuxKind) -> &'static str {
    match kind {
        MuxKind::Tmux => "tmux",
        MuxKind::Host => "OmpDeck host",
    }
}

/// [`Backends::banner`] for a given OS: no backend at all, or an explicit choice that is not
/// available and has been replaced.
fn banner_text(backends: &Backends, windows: bool) -> Option<String> {
    let Some(primary) = backends.primary() else {
        return Some(
            if windows {
                "The OmpDeck session host (omp-deck-host.exe) was not found next to OmpDeck — \
             sessions cannot be started. Reinstall OmpDeck."
            } else {
                "Neither tmux nor the OmpDeck session host (omp-deck-host) was found — sessions \
             are not persistent; closing a tab ends omp"
            }
            .to_owned(),
        );
    };
    backends.unavailable_choice().map(|wanted| {
        format!(
            "{} is selected but not available — new sessions use {}",
            kind_label(wanted),
            kind_label(primary.kind())
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use MuxKind::{Host, Tmux};

    fn host_only(choice: BackendChoice) -> Backends {
        let host = deck_core::mux::HostMux::new("omp-deck-test-backends").expect("namespace");
        Backends::new(vec![Mux::Host(host)], choice)
    }

    #[test]
    fn auto_prefers_tmux_then_host() {
        assert_eq!(pick_kind(BackendChoice::Auto, &[Tmux, Host]), Some(Tmux));
        assert_eq!(pick_kind(BackendChoice::Auto, &[Host]), Some(Host));
        assert_eq!(pick_kind(BackendChoice::Auto, &[Tmux]), Some(Tmux));
        assert_eq!(pick_kind(BackendChoice::Auto, &[]), None);
    }

    #[test]
    fn an_explicit_choice_wins_when_available() {
        assert_eq!(pick_kind(BackendChoice::Host, &[Tmux, Host]), Some(Host));
        assert_eq!(pick_kind(BackendChoice::Tmux, &[Host, Tmux]), Some(Tmux));
    }

    #[test]
    fn an_unavailable_choice_falls_back_instead_of_losing_persistence() {
        assert_eq!(pick_kind(BackendChoice::Tmux, &[Host]), Some(Host));
        assert_eq!(pick_kind(BackendChoice::Host, &[Tmux]), Some(Tmux));
        assert_eq!(pick_kind(BackendChoice::Host, &[]), None);
    }

    #[test]
    fn no_backend_has_no_primary_and_a_direct_namespace() {
        let b = Backends::new(Vec::new(), BackendChoice::Auto);
        assert!(b.primary().is_none() && b.is_empty());
        assert_eq!(b.ctl_namespace(), "direct");
        assert!(b.unavailable_choice().is_none());
        assert_eq!(b.kill("omp-x"), Ok(()), "nothing to kill is not an error");
    }

    #[test]
    fn banners_say_what_is_wrong_and_stay_quiet_otherwise() {
        let none = Backends::new(Vec::new(), BackendChoice::Auto);
        let unix = banner_text(&none, false).expect("banner");
        assert!(unix.contains("not persistent"), "{unix}");
        let win = banner_text(&none, true).expect("banner");
        assert!(
            win.contains("omp-deck-host.exe") && !win.contains("tmux"),
            "{win}"
        );
        assert_eq!(banner_text(&host_only(BackendChoice::Auto), false), None);
        assert_eq!(banner_text(&host_only(BackendChoice::Host), false), None);
        let fell_back = banner_text(&host_only(BackendChoice::Tmux), false).expect("banner");
        assert_eq!(
            fell_back,
            "tmux is selected but not available — new sessions use OmpDeck host"
        );
    }

    #[test]
    fn the_primary_follows_the_choice_while_every_backend_stays_listed() {
        let mut b = host_only(BackendChoice::Auto);
        assert_eq!(b.primary().map(Mux::kind), Some(Host));
        b.set_choice(BackendChoice::Host);
        assert_eq!(b.primary().map(Mux::kind), Some(Host));
        assert_eq!(b.all().len(), 1);
    }

    fn tmux_and_host(choice: BackendChoice) -> Backends {
        let tmux = deck_core::tmux::Tmux::with_socket(
            "tmux",
            "omp-deck-test-backends",
            "/nonexistent/tmux.conf",
        );
        let host = deck_core::mux::HostMux::new("omp-deck-test-backends").expect("namespace");
        Backends::new(vec![Mux::Tmux(tmux), Mux::Host(host)], choice)
    }

    #[test]
    fn a_chat_launch_takes_the_host_even_when_tmux_is_primary_or_chosen() {
        for choice in [
            BackendChoice::Auto,
            BackendChoice::Tmux,
            BackendChoice::Host,
        ] {
            let b = tmux_and_host(choice);
            assert_eq!(
                b.for_launch(None, SessionView::Chat).map(Mux::kind),
                Some(Host),
                "{choice:?}"
            );
        }
        let b = tmux_and_host(BackendChoice::Tmux);
        assert_eq!(b.primary().map(Mux::kind), Some(Tmux));
        assert_eq!(
            b.for_launch(None, SessionView::Terminal).map(Mux::kind),
            Some(Tmux),
            "a terminal launch still follows the choice"
        );
    }

    #[test]
    fn a_live_session_keeps_its_backend_whatever_the_view() {
        let b = tmux_and_host(BackendChoice::Auto);
        assert_eq!(
            b.for_launch(Some(Tmux), SessionView::Chat).map(Mux::kind),
            Some(Tmux)
        );
        assert_eq!(
            b.for_launch(Some(Host), SessionView::Terminal)
                .map(Mux::kind),
            Some(Host)
        );
    }

    #[test]
    fn a_chat_launch_without_a_host_falls_back_to_the_primary_backend() {
        let tmux = deck_core::tmux::Tmux::with_socket(
            "tmux",
            "omp-deck-test-backends",
            "/nonexistent/tmux.conf",
        );
        let b = Backends::new(vec![Mux::Tmux(tmux)], BackendChoice::Auto);
        assert_eq!(
            b.for_launch(None, SessionView::Chat).map(Mux::kind),
            Some(Tmux)
        );
        let none = Backends::new(Vec::new(), BackendChoice::Auto);
        assert!(none.for_launch(None, SessionView::Chat).is_none());
    }
}
