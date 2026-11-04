//! Model / thinking-effort switches: the `/switch` and `/effort` commands and the optimistic
//! bookkeeping that waits for omp to confirm them. Pure; the picker UI lives in the desktop.

use std::time::{Duration, Instant};

use deck_core::ctl::CtlCommand;

/// How long an optimistic switch waits for the session file to show it.
pub const SWITCH_TIMEOUT: Duration = Duration::from_secs(8);
/// Sessions that record no model yet (fresh, nothing persisted) can never confirm; give up sooner.
pub const FRESH_TIMEOUT: Duration = Duration::from_millis(1500);

/// `/switch provider/id[:level]`.
pub fn switch_command(selector: &str, level: Option<&str>) -> String {
    match level {
        Some(l) => format!("/switch {selector}:{l}"),
        None => format!("/switch {selector}"),
    }
}

/// `/switch`'s live-control twin: the selector carries the `:level` suffix like the slash
/// command's argument (omp's resolver reads it).
fn ctl_switch(selector: &str, level: Option<&str>) -> CtlCommand {
    CtlCommand::SwitchModel {
        selector: match level {
            Some(l) => format!("{selector}:{l}"),
            None => selector.to_owned(),
        },
    }
}

pub fn effort_command(level: &str) -> String {
    format!("/effort {level}")
}

/// What picking something in the dropdown sends to omp.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub command: String,
    /// The same change as a live-control command (omp applies it without touching the composer).
    pub ctl: CtlCommand,
    pub selector: Option<String>,
    pub level: Option<String>,
}

/// Picking the session's own model with a level is just an `/effort`; picking it without one
/// changes nothing (omp records no event, so it could never be confirmed).
///
/// A role alias (`@smol`, `@default`, a custom role: `/switch @role[:level]`) is resolved by omp
/// itself: the deck cannot know which model it names, so the change expects no particular model
/// (`selector` is `None`, only a requested level is confirmed) and is never "the same model".
pub fn plan_switch(current: Option<&str>, selector: &str, level: Option<&str>) -> Option<Change> {
    if selector.starts_with('@') {
        return Some(Change {
            command: switch_command(selector, level),
            ctl: ctl_switch(selector, level),
            selector: None,
            level: level.map(str::to_owned),
        });
    }
    if current == Some(selector) {
        return level.map(|l| Change {
            command: effort_command(l),
            ctl: CtlCommand::SetEffort {
                level: l.to_owned(),
            },
            selector: None,
            level: Some(l.to_owned()),
        });
    }
    Some(Change {
        command: switch_command(selector, level),
        ctl: ctl_switch(selector, level),
        selector: Some(selector.to_owned()),
        level: level.map(str::to_owned),
    })
}

/// A model/thinking change sent to omp and not yet seen in the session file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub selector: Option<String>,
    pub level: Option<String>,
    pub since: Instant,
}

impl Pending {
    /// Seen in the session file: the model matches (when one was requested) and the level is the
    /// effective or configured one (when one was requested).
    pub fn confirmed_by(
        &self,
        model: Option<&str>,
        thinking: Option<&str>,
        configured: Option<&str>,
    ) -> bool {
        let model_ok = self.selector.as_deref().is_none_or(|s| model == Some(s));
        let level_ok = self
            .level
            .as_deref()
            .is_none_or(|l| thinking == Some(l) || configured == Some(l));
        model_ok && level_ok
    }

    /// `next` sent while `self` is still unconfirmed: an `/effort` keeps the queued `/switch`'s
    /// model (omp applies both, in order), so the chip and the confirmation expect that model
    /// at the new level; a `/switch` replaces everything.
    pub fn then(self, next: Pending) -> Pending {
        match next.selector {
            Some(_) => next,
            None => Pending {
                selector: self.selector,
                level: next.level.or(self.level),
                since: next.since,
            },
        }
    }

    pub fn timed_out(&self, now: Instant, limit: Duration) -> bool {
        now.duration_since(self.since) >= limit
    }

    /// Short text for the chip while waiting; `name_of` maps a selector to the model's display
    /// name (from the client's catalog).
    pub fn describe(&self, name_of: &dyn Fn(&str) -> Option<String>) -> String {
        match (&self.selector, &self.level) {
            (Some(s), _) => name_of(s).unwrap_or_else(|| s.clone()),
            (None, Some(l)) => format!("thinking {l}"),
            (None, None) => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(switch_command("a/b", None), "/switch a/b");
        assert_eq!(switch_command("a/b", Some("high")), "/switch a/b:high");
        assert_eq!(effort_command("auto"), "/effort auto");
    }

    #[test]
    fn switching_to_the_current_model_degrades_to_effort_or_nothing() {
        assert_eq!(plan_switch(Some("a/b"), "a/b", None), None);
        let c = plan_switch(Some("a/b"), "a/b", Some("high")).expect("effort");
        assert_eq!(c.command, "/effort high");
        assert_eq!(c.selector, None);
        let c = plan_switch(Some("a/b"), "a/c", Some("low")).expect("switch");
        assert_eq!(c.command, "/switch a/c:low");
        assert_eq!(c.selector.as_deref(), Some("a/c"));
        let c = plan_switch(None, "a/c", None).expect("switch");
        assert_eq!(c.command, "/switch a/c");
    }

    #[test]
    fn every_change_has_a_live_control_twin() {
        let ctl =
            |current, selector, level| plan_switch(current, selector, level).expect("a change").ctl;
        assert_eq!(
            ctl(Some("a/b"), "c/d", None),
            CtlCommand::SwitchModel {
                selector: "c/d".to_owned()
            }
        );
        // The level rides on the selector, like `/switch c/d:high`.
        assert_eq!(
            ctl(Some("a/b"), "c/d", Some("high")),
            CtlCommand::SwitchModel {
                selector: "c/d:high".to_owned()
            }
        );
        assert_eq!(
            ctl(None, "@smol", Some("low")),
            CtlCommand::SwitchModel {
                selector: "@smol:low".to_owned()
            }
        );
        // The session's own model with a level is an effort change.
        assert_eq!(
            ctl(Some("a/b"), "a/b", Some("low")),
            CtlCommand::SetEffort {
                level: "low".to_owned()
            }
        );
    }

    #[test]
    fn a_role_alias_is_switched_by_omp_and_expects_no_model() {
        let c = plan_switch(Some("a/b"), "@smol", None).expect("a role is never a no-op");
        assert_eq!(c.command, "/switch @smol");
        assert_eq!((c.selector, c.level), (None, None));
        // The same text as the current model's selector is still a role, never "no change".
        let c = plan_switch(Some("@smol"), "@smol", Some("low")).expect("switch");
        assert_eq!(c.command, "/switch @smol:low");
        assert_eq!((c.selector, c.level.as_deref()), (None, Some("low")));
        // Nothing named a model, so nothing waits for one: only the level is confirmed.
        let p = Pending {
            selector: None,
            level: Some("low".to_owned()),
            since: Instant::now(),
        };
        assert!(p.confirmed_by(Some("x/y"), Some("low"), None));
        assert!(!p.confirmed_by(Some("x/y"), Some("high"), None));
    }

    #[test]
    fn pending_is_confirmed_by_model_and_level() {
        let p = Pending {
            selector: Some("a/b".to_owned()),
            level: Some("high".to_owned()),
            since: Instant::now(),
        };
        assert!(p.confirmed_by(Some("a/b"), Some("high"), None));
        assert!(p.confirmed_by(Some("a/b"), Some("medium"), Some("high")));
        assert!(!p.confirmed_by(Some("a/c"), Some("high"), None));
        assert!(!p.confirmed_by(Some("a/b"), Some("low"), Some("low")));
        let only_level = Pending {
            selector: None,
            level: Some("auto".to_owned()),
            since: p.since,
        };
        assert!(only_level.confirmed_by(Some("whatever"), Some("high"), Some("auto")));
        let only_model = Pending {
            selector: Some("a/b".to_owned()),
            level: None,
            since: p.since,
        };
        assert!(only_model.confirmed_by(Some("a/b"), None, None));
    }

    #[test]
    fn pending_times_out() {
        let now = Instant::now();
        let p = Pending {
            selector: None,
            level: Some("low".to_owned()),
            since: now,
        };
        assert!(!p.timed_out(now + Duration::from_secs(7), SWITCH_TIMEOUT));
        assert!(p.timed_out(now + SWITCH_TIMEOUT, SWITCH_TIMEOUT));
        assert!(p.timed_out(now + FRESH_TIMEOUT, FRESH_TIMEOUT));
    }

    #[test]
    fn effort_queued_behind_an_unconfirmed_switch_keeps_its_model() {
        let p = |selector: Option<&str>, level: Option<&str>| Pending {
            selector: selector.map(str::to_owned),
            level: level.map(str::to_owned),
            since: Instant::now(),
        };
        // `/switch opus` then `/effort medium`: omp ends on opus at medium.
        let merged = p(Some("a/opus"), None).then(p(None, Some("medium")));
        assert_eq!(
            (merged.selector.as_deref(), merged.level.as_deref()),
            (Some("a/opus"), Some("medium"))
        );
        assert!(merged.confirmed_by(Some("a/opus"), Some("medium"), None));
        assert!(!merged.confirmed_by(Some("a/sonnet"), Some("medium"), None));
        // A later `/switch` replaces the earlier one, level included.
        let replaced = p(Some("a/opus"), Some("high")).then(p(Some("a/sonnet"), None));
        assert_eq!(
            (replaced.selector.as_deref(), replaced.level),
            (Some("a/sonnet"), None)
        );
    }
}
