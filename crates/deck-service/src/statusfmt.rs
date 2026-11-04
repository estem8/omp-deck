//! Pure formatting/decision helpers for the status bar, mirroring omp's own status line
//! (docs/notes/omp-status.md §1.3, §2c): token formatter, context thresholds, title glyph.

use deck_term::{RunPhase, RunState, Terminal, TspFacts};

/// omp's token formatter: `999`, `1.2K`, `23K`, `1M`, `1.5M`, `120M`, `2.5B`.
pub fn fmt_tokens(n: u64) -> String {
    fn scaled(n: u64, div: f64, suffix: &str, decimals: bool) -> String {
        let v = n as f64 / div;
        if decimals {
            let s = format!("{v:.1}");
            let s = s.strip_suffix(".0").unwrap_or(&s);
            format!("{s}{suffix}")
        } else {
            format!("{}{suffix}", v.round() as u64)
        }
    }
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => scaled(n, 1e3, "K", true),
        10_000..=999_999 => scaled(n, 1e3, "K", false),
        1_000_000..=9_999_999 => scaled(n, 1e6, "M", true),
        10_000_000..=999_999_999 => scaled(n, 1e6, "M", false),
        1_000_000_000..=9_999_999_999 => scaled(n, 1e9, "B", true),
        _ => scaled(n, 1e9, "B", false),
    }
}

/// `$7.00` (omp: `"$" + cost.toFixed(2)`).
pub fn fmt_cost(cost: f64) -> String {
    format!("${:.2}", if cost.is_finite() { cost.max(0.0) } else { 0.0 })
}

/// Escalation of the context meter (omp's `chrome/context-thresholds.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextLevel {
    Normal,
    Warn,
    High,
    Critical,
}

/// Usage as a percentage of the window; `None` without both numbers or with a zero window.
pub fn context_percent(tokens: Option<u64>, window: Option<u64>) -> Option<f64> {
    let (t, w) = (tokens?, window?);
    (w > 0).then(|| t as f64 / w as f64 * 100.0)
}

/// Thresholds are `min(50 %, 150K tok)`, `min(70 %, 270K)`, `min(90 %, 500K)` of the window.
pub fn context_level(pct: f64, window: u64) -> ContextLevel {
    let cap = |pct_limit: f64, tok_limit: f64| -> f64 {
        if window == 0 {
            pct_limit
        } else {
            pct_limit.min(tok_limit / window as f64 * 100.0)
        }
    };
    if pct >= cap(90.0, 500_000.0) {
        ContextLevel::Critical
    } else if pct >= cap(70.0, 270_000.0) {
        ContextLevel::High
    } else if pct >= cap(50.0, 150_000.0) {
        ContextLevel::Warn
    } else {
        ContextLevel::Normal
    }
}

/// `17.3%/1M`, `?%/1M`, `23K/?` — the text of omp's `context_pct` segment.
pub fn context_label(tokens: Option<u64>, window: Option<u64>) -> String {
    match (context_percent(tokens, window), window) {
        (Some(pct), Some(w)) => format!("{pct:.1}%/{}", fmt_tokens(w)),
        (None, Some(w)) => format!("?%/{}", fmt_tokens(w)),
        (_, None) => match tokens {
            Some(t) => format!("{}/?", fmt_tokens(t)),
            None => "?".to_owned(),
        },
    }
}

/// What omp is doing, read from the strongest signal a terminal gives (see [`Activity::derive`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Activity {
    /// Nothing of omp's state seen (yet): nothing is claimed.
    Unknown,
    Idle,
    Busy,
    /// omp wants the user's input (`!` title, `blocked` report, or an ask/approval in the dock).
    Attention,
}

impl Activity {
    /// omp's state from its classic terminal title `π <glyph> <label>`. Under TSP omp sets a
    /// plain title instead, so callers that hold more than a title use [`Activity::derive`] or
    /// [`Activity::of_terminal`].
    pub fn from_title(title: Option<&str>) -> Activity {
        let Some(rest) = title.and_then(|t| t.trim_start().strip_prefix('π')) else {
            return Activity::Unknown;
        };
        match rest.split_whitespace().next() {
            Some(">") => Activity::Idle,
            Some("!") => Activity::Attention,
            Some(_) => Activity::Busy,
            None => Activity::Unknown,
        }
    }

    /// omp's state from its OSC 7501 run status: working is busy, blocked wants the user, and
    /// idle, done and error all mean the turn is over (an error is still a finished turn; the
    /// failure itself is read from the session file).
    pub fn from_run_state(state: &RunState) -> Activity {
        match state.state {
            RunPhase::Working => Activity::Busy,
            RunPhase::Blocked { .. } => Activity::Attention,
            RunPhase::Idle | RunPhase::Done | RunPhase::Error => Activity::Idle,
        }
    }

    /// The single precedence for omp's state: the OSC 7501 report when one is held (omp
    /// sends one on every transition, so a held `done` or `error` cannot outlive the next
    /// turn: its `working` replaces it), else the classic `π` title, else what the TSP
    /// document shows, else [`Activity::Unknown`]. Under TSP omp sets no `π` title, and sends
    /// no report before its first transition or with `terminal.programStatus` off.
    pub fn derive(
        run_state: Option<&RunState>,
        title: Option<&str>,
        tsp: Option<&TspFacts>,
    ) -> Activity {
        if let Some(state) = run_state {
            return Activity::from_run_state(state);
        }
        match Activity::from_title(title) {
            Activity::Unknown => tsp.map_or(Activity::Unknown, Activity::from_tsp_facts),
            known => known,
        }
    }

    /// [`Activity::derive`] for a terminal and its TSP mirror.
    pub fn of_terminal(terminal: &Terminal) -> Activity {
        // Neither the final title nor a retained TSP working row describes a live process.
        if terminal.is_exited() {
            return Activity::Unknown;
        }
        let tsp = terminal.tsp();
        let facts = if tsp.live() {
            tsp.with(TspFacts::of)
        } else {
            None
        };
        Activity::derive(
            terminal.run_state().as_ref(),
            terminal.title(),
            facts.as_ref(),
        )
    }

    /// omp's state from what its TSP document shows: an `ask` or approval in the dock (which
    /// stands in the composer's place) wants the user; a working row is a running turn; a
    /// sendable composer is idle. Anything else (a full-screen page, no composer yet)
    /// claims nothing.
    pub fn from_tsp_facts(facts: &TspFacts) -> Activity {
        if facts.asking {
            Activity::Attention
        } else if facts.working {
            Activity::Busy
        } else if facts.composer {
            Activity::Idle
        } else {
            Activity::Unknown
        }
    }

    /// True while a turn streams (gating for idle-only actions).
    pub fn is_busy(self) -> bool {
        self == Activity::Busy
    }
}

/// Model name as omp's status line shows it (`Claude Opus 5.5` → `Opus 5.5`).
pub fn short_model_name(name: &str) -> &str {
    name.strip_prefix("Claude ").unwrap_or(name)
}

/// Thinking levels in ascending order with the fill of omp's glyph (`○ ◔ ◑ ◒ ◕ ◉`).
pub fn thinking_fill(level: &str) -> Option<f32> {
    Some(match level {
        "off" => return None,
        "minimal" => 0.0,
        "low" => 0.25,
        "medium" => 0.5,
        "high" => 0.75,
        "xhigh" => 0.9,
        "max" => 1.0,
        _ => 0.5,
    })
}

/// omp's short level names (`med`, `xhigh`).
pub fn thinking_short(level: &str) -> &str {
    match level {
        "minimal" => "min",
        "medium" => "med",
        other => other,
    }
}

pub fn ellipsize_left(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars || max_chars < 2 {
        return s.to_owned();
    }
    let tail: String = s.chars().skip(n - (max_chars - 1)).collect();
    format!("…{tail}")
}

/// `$ / M tokens` price for the model dropdown: `3`, `0.25`, `15`.
pub fn fmt_price(p: f64) -> String {
    if !p.is_finite() || p <= 0.0 {
        return "0".to_owned();
    }
    let s = if p >= 10.0 {
        format!("{p:.0}")
    } else {
        format!("{p:.2}")
    };
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn exited_terminal_activity_is_unknown_even_with_a_busy_title() {
        let mut terminal = Terminal::spawn_headless(
            deck_term::SpawnSpec {
                program: "sh".into(),
                args: vec![
                    "-c".into(),
                    r"printf '\033]0;π ⠦ project\007\033]7501;state=working\007'; exit 0".into(),
                ],
                cwd: std::env::temp_dir(),
                env: Vec::new(),
                tmux_client: false,
            },
            deck_term::TermOptions::default(),
            std::sync::Arc::new(|_| {}),
        )
        .expect("spawn child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !terminal.is_exited() {
            terminal.poll();
            assert!(std::time::Instant::now() < deadline, "child did not exit");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(Activity::from_title(terminal.title()), Activity::Busy);
        assert_eq!(terminal.run_state(), None);
        assert_eq!(Activity::of_terminal(&terminal), Activity::Unknown);
        assert_eq!(
            crate::status::TabStatus::of(
                terminal.exit_code(),
                Activity::of_terminal(&terminal),
                false
            ),
            crate::status::TabStatus::Exited { clean: true }
        );
    }

    #[test]
    fn token_formatter_matches_omp() {
        let cases = [
            (0, "0"),
            (999, "999"),
            (1_000, "1K"),
            (1_234, "1.2K"),
            (9_999, "10K"),
            (23_281, "23K"),
            (500_000, "500K"),
            (1_000_000, "1M"),
            (1_500_000, "1.5M"),
            (120_000_000, "120M"),
            (2_500_000_000, "2.5B"),
        ];
        for (n, want) in cases {
            assert_eq!(fmt_tokens(n), want, "{n}");
        }
    }

    #[test]
    fn cost_has_two_decimals() {
        assert_eq!(fmt_cost(7.0), "$7.00");
        assert_eq!(fmt_cost(0.255), "$0.26");
        assert_eq!(fmt_cost(f64::NAN), "$0.00");
    }

    #[test]
    fn context_label_variants() {
        assert_eq!(context_label(Some(173_000), Some(1_000_000)), "17.3%/1M");
        assert_eq!(context_label(None, Some(1_000_000)), "?%/1M");
        assert_eq!(context_label(Some(23_281), None), "23K/?");
        assert_eq!(context_label(None, None), "?");
        assert_eq!(context_percent(Some(5), Some(0)), None);
    }

    #[test]
    fn context_thresholds_use_the_smaller_of_percent_and_tokens() {
        // 1M window: 15 % / 27 % / 50 %.
        assert_eq!(context_level(14.9, 1_000_000), ContextLevel::Normal);
        assert_eq!(context_level(15.0, 1_000_000), ContextLevel::Warn);
        assert_eq!(context_level(27.0, 1_000_000), ContextLevel::High);
        assert_eq!(context_level(50.0, 1_000_000), ContextLevel::Critical);
        // 100K window: percentages win (50 / 70 / 90).
        assert_eq!(context_level(49.0, 100_000), ContextLevel::Normal);
        assert_eq!(context_level(50.0, 100_000), ContextLevel::Warn);
        assert_eq!(context_level(70.0, 100_000), ContextLevel::High);
        assert_eq!(context_level(95.0, 100_000), ContextLevel::Critical);
    }

    #[test]
    fn activity_from_omp_title() {
        use Activity::*;
        assert_eq!(Activity::from_title(Some("π > proj")), Idle);
        assert_eq!(Activity::from_title(Some("π ⠦ proj")), Busy);
        assert_eq!(Activity::from_title(Some("π ! proj")), Attention);
        assert_eq!(Activity::from_title(Some("π > ")), Idle);
        assert_eq!(Activity::from_title(Some("host")), Unknown);
        assert_eq!(Activity::from_title(None), Unknown);
        assert_eq!(Activity::from_title(Some("π")), Unknown);
        assert!(Busy.is_busy() && !Idle.is_busy() && !Unknown.is_busy());
    }

    fn state(phase: RunPhase) -> RunState {
        RunState::new(phase)
    }

    #[test]
    fn run_state_maps_onto_activity() {
        use Activity::*;
        assert_eq!(Activity::from_run_state(&state(RunPhase::Working)), Busy);
        assert_eq!(
            Activity::from_run_state(&state(RunPhase::Blocked { kind: None })),
            Attention
        );
        for finished in [RunPhase::Idle, RunPhase::Done, RunPhase::Error] {
            assert_eq!(
                Activity::from_run_state(&state(finished)),
                Idle,
                "{finished:?}"
            );
        }
    }

    #[test]
    fn a_held_run_state_beats_the_title_which_beats_nothing() {
        use Activity::*;
        let working = state(RunPhase::Working);
        let done = state(RunPhase::Done);
        // The report wins over a contradicting classic title either way.
        assert_eq!(
            Activity::derive(Some(&working), Some("π > proj"), None),
            Busy
        );
        assert_eq!(Activity::derive(Some(&done), Some("π ⠋ proj"), None), Idle);
        // A plain title (what omp sets under TSP) does not matter when a report is held ...
        assert_eq!(Activity::derive(Some(&working), Some("omp"), None), Busy);
        assert_eq!(Activity::derive(Some(&working), None, None), Busy);
        // ... and without a report the classic title speaks.
        assert_eq!(Activity::derive(None, Some("π ⠋ proj"), None), Busy);
        assert_eq!(Activity::derive(None, Some("π ! proj"), None), Attention);
        assert_eq!(Activity::derive(None, Some("omp"), None), Unknown);
        assert_eq!(Activity::derive(None, None, None), Unknown);
    }

    #[test]
    fn the_tsp_document_speaks_last_and_asks_win() {
        use Activity::*;
        let facts = |asking, working, composer| TspFacts {
            asking,
            working,
            composer,
        };
        let from = |f: TspFacts| Activity::derive(None, Some("omp"), Some(&f));
        assert_eq!(from(facts(false, false, true)), Idle);
        // A running turn still has its sendable composer (steering): working wins.
        assert_eq!(from(facts(false, true, true)), Busy);
        // An ask or approval takes the composer's place and wins over everything.
        assert_eq!(from(facts(true, true, true)), Attention);
        // An empty dock claims nothing.
        assert_eq!(from(facts(false, false, false)), Unknown);
        // A report or a classic title beats the document.
        let working = state(RunPhase::Working);
        assert_eq!(
            Activity::derive(Some(&working), None, Some(&facts(false, false, true))),
            Busy
        );
        assert_eq!(
            Activity::derive(None, Some("π ⠋ proj"), Some(&facts(false, false, true))),
            Busy
        );
    }

    #[test]
    fn names_and_paths() {
        assert_eq!(short_model_name("Claude Opus 5.5"), "Opus 5.5");
        assert_eq!(short_model_name("Fake One"), "Fake One");
        assert_eq!(ellipsize_left("/a/b/c/d", 20), "/a/b/c/d");
        assert_eq!(ellipsize_left("/aaaa/bbbb/cccc", 8), "…bb/cccc");
        assert_eq!(thinking_short("medium"), "med");
        assert_eq!(thinking_fill("off"), None);
        assert_eq!(thinking_fill("max"), Some(1.0));
    }

    #[test]
    fn prices() {
        assert_eq!(fmt_price(3.0), "3");
        assert_eq!(fmt_price(0.25), "0.25");
        assert_eq!(fmt_price(15.0), "15");
        assert_eq!(fmt_price(0.0), "0");
        assert_eq!(fmt_price(1.5), "1.5");
    }
}
