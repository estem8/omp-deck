//! Session actions offered by the status bar: which are available when, the injection jobs they
//! become, and the HTML export (docs/notes/omp-actions.md §7).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use deck_term::input::{Key, Mods};

use crate::inject::{Job, Requires, sanitize_line};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    Handoff,
    Compact,
    Shake,
    Fork,
    Tree,
    Rename,
    AutoName,
    PlanMode,
    Retry,
    Stop,
    CopyLast,
    Export,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum CompactMode {
    Default,
    Soft,
    Snapcompact,
    Focus(String),
}

/// omp's `/shake` modes (`builtin-lifecycle.ts` `parseShakeMode`; a bare `/shake` is `elide`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ShakeMode {
    /// Tool results + large blocks.
    Elide,
    Images,
    Thinking,
}

/// A fully specified action ready to run.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Action {
    Handoff(Option<String>),
    Compact(CompactMode),
    Shake(ShakeMode),
    Fork,
    Rename(String),
    AutoName,
    PlanMode,
    Retry,
    Stop,
    CopyLast,
    ExportHtml,
}

/// What the availability rules look at.
#[derive(Clone, Copy, Debug, Default)]
pub struct ActionCtx {
    pub busy: bool,
    /// Activity is exactly `Idle` (not Attention/Unknown): idle-only actions need this.
    pub idle: bool,
    /// The session file exists on disk.
    pub has_session_file: bool,
    pub last_turn_failed: bool,
    pub has_last_reply: bool,
    /// The omp process behind the tab ended.
    pub exited: bool,
    /// The omp executable is known (export / fork need it).
    pub omp_found: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Enabled,
    Disabled(&'static str),
}

impl Availability {
    pub fn is_enabled(self) -> bool {
        self == Availability::Enabled
    }
}

/// Retry shows only after a failed turn, Stop only while one runs; everything else is always shown.
pub fn visible(kind: Kind, c: &ActionCtx) -> bool {
    match kind {
        Kind::Retry => c.last_turn_failed && !c.busy,
        Kind::Stop => c.busy,
        _ => true,
    }
}

pub fn availability(kind: Kind, c: &ActionCtx) -> Availability {
    use Availability::{Disabled, Enabled};
    const BUSY: &str =
        "Available when omp is idle (wait for the response to finish, or answer its prompt)";
    const NO_FILE: &str = "Available once the session has its first reply";
    // The tree reads the session file: it works for an ended tab too.
    if kind == Kind::Tree {
        return if c.has_session_file {
            Enabled
        } else {
            Disabled(NO_FILE)
        };
    }
    if c.exited {
        return Disabled("omp has exited in this tab");
    }
    match kind {
        Kind::Handoff | Kind::Compact | Kind::Shake => {
            if !c.idle {
                Disabled(BUSY)
            } else if !c.has_session_file {
                Disabled(NO_FILE)
            } else {
                Enabled
            }
        }
        Kind::Fork => {
            if !c.omp_found {
                Disabled("omp was not found")
            } else if !c.has_session_file {
                Disabled(NO_FILE)
            } else {
                Enabled
            }
        }
        Kind::Export => {
            if !c.omp_found {
                Disabled("omp was not found")
            } else if !c.has_session_file {
                Disabled(NO_FILE)
            } else {
                Enabled
            }
        }
        Kind::CopyLast => {
            if c.has_last_reply {
                Enabled
            } else {
                Disabled("No assistant reply yet")
            }
        }
        Kind::Retry => {
            if !c.idle {
                Disabled(BUSY)
            } else {
                Enabled
            }
        }
        Kind::Stop | Kind::Tree | Kind::Rename | Kind::AutoName | Kind::PlanMode => Enabled,
    }
}

/// One line per input, no control characters (see [`sanitize_line`]).
fn one_line(s: &str) -> String {
    sanitize_line(s)
}

/// What the user clicked, for messages ("Compact (soft)").
pub fn compact_label(mode: &CompactMode) -> &'static str {
    match mode {
        CompactMode::Default => "Compact",
        CompactMode::Soft => "Compact (soft)",
        CompactMode::Snapcompact => "Compact (snapcompact)",
        CompactMode::Focus(_) => "Compact (with focus)",
    }
}

/// What the user clicked, for messages ("Shake (images)").
pub fn shake_label(mode: ShakeMode) -> &'static str {
    match mode {
        ShakeMode::Elide => "Shake",
        ShakeMode::Images => "Shake (images)",
        ShakeMode::Thinking => "Shake (thinking)",
    }
}

/// omp's plan-mode hotkey is Alt+Shift+P.
const ALT_SHIFT: Mods = Mods::new(true, true, false);

/// The composer/hotkey injection behind an action; `None` for actions that run elsewhere.
pub fn job(action: &Action) -> Option<Job> {
    Some(match action {
        Action::Handoff(focus) => {
            let focus = focus.as_deref().map(one_line).filter(|f| !f.is_empty());
            let text = match focus {
                Some(f) => format!("/handoff {f}"),
                None => "/handoff".to_owned(),
            };
            Job::slash("Handoff", &text, Requires::Idle)
        }
        Action::Compact(mode) => {
            let text = match mode {
                CompactMode::Default => "/compact".to_owned(),
                CompactMode::Soft => "/compact soft".to_owned(),
                CompactMode::Snapcompact => "/compact snapcompact".to_owned(),
                CompactMode::Focus(f) => {
                    let f = one_line(f);
                    if f.is_empty() {
                        "/compact".to_owned()
                    } else {
                        format!("/compact {f}")
                    }
                }
            };
            Job::slash(compact_label(mode), &text, Requires::Idle)
        }
        Action::Shake(mode) => {
            let text = match mode {
                ShakeMode::Elide => "/shake",
                ShakeMode::Images => "/shake images",
                ShakeMode::Thinking => "/shake thinking",
            };
            Job::slash(shake_label(*mode), text, Requires::Idle)
        }
        Action::Rename(title) => {
            let t = one_line(title);
            if t.is_empty() {
                return None;
            }
            Job::slash("Rename", &format!("/rename {t}"), Requires::Any)
        }
        Action::AutoName => Job::slash("Auto-name", "/rename", Requires::Any),
        Action::PlanMode => Job::key("Plan mode", Key::Char('p'), ALT_SHIFT, Requires::Any),
        Action::Retry => Job::key("Retry", Key::F(5), Mods::NONE, Requires::Idle),
        Action::Stop => Job::stop(),
        Action::Fork | Action::CopyLast | Action::ExportHtml => return None,
    })
}

// ------------------------------------------------------------------ export

/// `$XDG_CACHE_HOME/omp-deck/exports`.
pub fn exports_dir() -> PathBuf {
    dirs::cache_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("omp-deck")
        .join("exports")
}

/// The file name from omp's `Exported to: <name>` line.
pub fn parse_exported(stdout: &str) -> Option<&str> {
    stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("Exported to:"))
        .map(str::trim)
        .filter(|n| !n.is_empty())
}

/// Runs `omp --export <jsonl>` inside `dest` (omp writes the HTML into its cwd) and returns the file.
pub fn export_html(program: &Path, jsonl: &Path, dest: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    let out = Command::new(program)
        .arg("--export")
        .arg(jsonl)
        .current_dir(dest)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run omp: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = err.trim();
        let msg = if msg.is_empty() { stdout.trim() } else { msg };
        return Err(format!("omp --export failed: {msg}"));
    }
    let name = parse_exported(&stdout)
        .ok_or_else(|| format!("unexpected omp output: {}", stdout.trim()))?;
    let path = dest.join(name);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("export file missing: {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::Payload;

    fn ctx() -> ActionCtx {
        ActionCtx {
            busy: false,
            idle: true,
            has_session_file: true,
            last_turn_failed: false,
            has_last_reply: true,
            exited: false,
            omp_found: true,
        }
    }

    #[test]
    fn idle_only_actions_are_disabled_while_busy() {
        let busy = ActionCtx {
            busy: true,
            idle: false,
            ..ctx()
        };
        assert!(!availability(Kind::Handoff, &busy).is_enabled());
        assert!(!availability(Kind::Compact, &busy).is_enabled());
        assert!(!availability(Kind::Shake, &busy).is_enabled());
        assert!(availability(Kind::PlanMode, &busy).is_enabled());
        assert!(availability(Kind::Fork, &busy).is_enabled());
        assert!(availability(Kind::Handoff, &ctx()).is_enabled());
    }

    #[test]
    fn idle_only_actions_need_exactly_idle_not_just_not_busy() {
        let attention = ActionCtx {
            busy: false,
            idle: false,
            ..ctx()
        };
        assert!(!availability(Kind::Compact, &attention).is_enabled());
        assert!(!availability(Kind::Shake, &attention).is_enabled());
        assert!(!availability(Kind::Handoff, &attention).is_enabled());
    }

    #[test]
    fn fresh_tab_disables_file_dependent_actions() {
        let fresh = ActionCtx {
            has_session_file: false,
            has_last_reply: false,
            ..ctx()
        };
        for k in [
            Kind::Handoff,
            Kind::Compact,
            Kind::Shake,
            Kind::Fork,
            Kind::Export,
            Kind::CopyLast,
        ] {
            assert!(!availability(k, &fresh).is_enabled(), "{k:?}");
        }
        for k in [Kind::Rename, Kind::PlanMode] {
            assert!(availability(k, &fresh).is_enabled(), "{k:?}");
        }
    }

    #[test]
    fn exited_tab_disables_everything() {
        let dead = ActionCtx {
            exited: true,
            ..ctx()
        };
        assert!(!availability(Kind::PlanMode, &dead).is_enabled());
    }

    #[test]
    fn retry_and_stop_visibility() {
        let failed = ActionCtx {
            last_turn_failed: true,
            ..ctx()
        };
        assert!(visible(Kind::Retry, &failed));
        assert!(!visible(Kind::Retry, &ctx()));
        assert!(!visible(Kind::Stop, &ctx()));
        let busy = ActionCtx {
            busy: true,
            idle: false,
            last_turn_failed: true,
            ..ctx()
        };
        assert!(visible(Kind::Stop, &busy));
        assert!(!visible(Kind::Retry, &busy));
    }

    fn slash_text(a: &Action) -> String {
        match job(a).map(|j| j.payload) {
            Some(Payload::Slash(t)) => t,
            other => panic!("not a slash job: {other:?}"),
        }
    }

    #[test]
    fn commands_are_single_line() {
        assert_eq!(slash_text(&Action::Handoff(None)), "/handoff");
        assert_eq!(
            slash_text(&Action::Handoff(Some("  keep the\nAPI notes ".into()))),
            "/handoff keep the API notes"
        );
        assert_eq!(slash_text(&Action::Handoff(Some("  ".into()))), "/handoff");
        assert_eq!(
            slash_text(&Action::Compact(CompactMode::Soft)),
            "/compact soft"
        );
        assert_eq!(
            slash_text(&Action::Compact(CompactMode::Snapcompact)),
            "/compact snapcompact"
        );
        assert_eq!(
            slash_text(&Action::Compact(CompactMode::Focus("auth flow".into()))),
            "/compact auth flow"
        );
        assert_eq!(slash_text(&Action::Shake(ShakeMode::Elide)), "/shake");
        assert_eq!(
            slash_text(&Action::Shake(ShakeMode::Images)),
            "/shake images"
        );
        assert_eq!(
            slash_text(&Action::Shake(ShakeMode::Thinking)),
            "/shake thinking"
        );
        assert_eq!(
            slash_text(&Action::Rename("New  title".into())),
            "/rename New title"
        );
        assert_eq!(slash_text(&Action::AutoName), "/rename");
        assert_eq!(job(&Action::Rename("   ".into())), None);
    }

    #[test]
    fn hotkey_jobs_carry_the_documented_keys() {
        let keys = |a: Action| match job(&a).map(|j| (j.payload, j.requires)) {
            Some((Payload::Key(k, m), r)) => (k, m, r),
            other => panic!("not a key: {other:?}"),
        };
        // omp-actions.md §7: Alt+Shift+P (`ESC P`), F5 (`ESC[15~`).
        assert_eq!(
            keys(Action::PlanMode),
            (Key::Char('p'), ALT_SHIFT, Requires::Any)
        );
        assert_eq!(keys(Action::Retry), (Key::F(5), Mods::NONE, Requires::Idle));
        assert_eq!(
            job(&Action::Stop),
            Some(Job::stop()),
            "Stop is its own two-step job (Esc, then Esc again only if still busy)"
        );
        assert!(job(&Action::Fork).is_none());
        assert!(job(&Action::ExportHtml).is_none());
    }

    #[test]
    fn export_line_is_parsed() {
        assert_eq!(
            parse_exported("Exported to: omp-session-2026_abc.html\n"),
            Some("omp-session-2026_abc.html")
        );
        assert_eq!(
            parse_exported("noise\n  Exported to:  a b.html \n"),
            Some("a b.html")
        );
        assert_eq!(parse_exported("Exported to:\n"), None);
        assert_eq!(parse_exported("nothing"), None);
    }
}
