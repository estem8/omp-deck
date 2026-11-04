//! Model and effort changes over live control.
//!
//! omp's `/switch` and `/effort` typed into the composer would displace a draft, so a change on
//! a session whose live control can take it goes through the control extension instead: nothing
//! is typed and the composer is not touched. The typed command stays the fallback for what the
//! extension cannot do (`needs_compact`: omp compacts the transcript first, a TUI-only step) and
//! for a session that has no live control. A change is never sent both ways: once the command may
//! have reached omp, an unknown outcome is an error, not a retry (the journal never replays
//! uncertain work).

use crate::controller::{Command, SessionKey, SessionTarget};
use crate::gateway::protocol::{Done, GatewayError};

use super::Engine;
use deck_core::ctl::CtlReply;

/// The control extension's refusal for a switch only omp's compact-first step can do.
const NEEDS_COMPACT: &str = "needs_compact";
/// The wait for the control socket ended ([`Engine::queue_ctl`]): the command never left.
pub(super) const NO_SOCKET: &str = "could not reach the session's control socket";
/// Control-client failures before anything was written (`deck_core::ctl`).
const NEVER_SENT: [&str; 2] = ["not connected", "control client stopped"];
/// The extension cannot do it here, and changed nothing: omp's internals drifted from what it
/// was written for (`unsupported`), or it is an older extension of a session that still runs
/// (`model switching unavailable`, `effort unavailable`, `unknown cmd`).
const UNSUPPORTED: [&str; 4] = [
    "unsupported",
    "model switching unavailable",
    "effort unavailable",
    "unknown cmd",
];

/// How a model change is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// Live control: the composer is untouched.
    Ctl,
    /// Typed `/switch` / `/effort` (with its draft confirmation).
    Typed,
}

/// What the session offers: `ctl_key` it runs with the control extension, `connected` the socket
/// is open now, `expected` the launch created it with the extension (the socket is on its way),
/// `quiet` nothing is being typed and no earlier model change waits (a typed one still queued
/// would apply after this one and win).
#[derive(Clone, Copy, Debug)]
pub(super) struct Facts {
    pub(super) ctl_key: bool,
    pub(super) connected: bool,
    pub(super) expected: bool,
    pub(super) quiet: bool,
}

pub(super) fn route(f: Facts) -> Route {
    if f.ctl_key && f.quiet && (f.connected || f.expected) {
        Route::Ctl
    } else {
        Route::Typed
    }
}

/// What the control reply (or its failure) means for the change.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Applied; omp's own status text.
    Done(String),
    /// Nothing was changed and nothing was sent: type the command instead.
    Typed,
    /// omp refused, or the outcome is unknown: reported, never repeated.
    Failed(String),
}

pub(super) fn verdict(result: Result<Done, GatewayError>) -> Verdict {
    match result {
        Ok(Done::Ctl(CtlReply {
            ok: true, message, ..
        })) => Verdict::Done(message.unwrap_or_default()),
        Ok(Done::Ctl(reply)) => {
            let error = reply.error.unwrap_or_default();
            if error == NEEDS_COMPACT
                || NEVER_SENT.contains(&error.as_str())
                || UNSUPPORTED.contains(&error.as_str())
            {
                Verdict::Typed
            } else if error.is_empty() {
                Verdict::Failed("omp did not change the model".to_owned())
            } else if error == "connection closed" || error == "write failed" {
                Verdict::Failed(format!(
                    "omp may not have changed the model ({error}): check the status bar"
                ))
            } else {
                Verdict::Failed(error)
            }
        }
        Ok(_) => Verdict::Failed("unexpected answer from the control socket".to_owned()),
        Err(GatewayError::Failed(text)) if text == NO_SOCKET => Verdict::Typed,
        Err(e) => Verdict::Failed(e.to_string()),
    }
}

/// A model change sent over live control and what it takes to type it instead.
pub(super) struct ModelChange {
    pub(super) session: SessionKey,
    pub(super) target: SessionTarget,
    pub(super) command: Command,
    /// The client that asked: it answers the draft confirmation of the typed fallback.
    pub(super) owner: String,
}

impl Engine {
    /// Sends a model or effort `command` of controller `i` over live control when that is
    /// possible; `false` leaves it to the typed path (which also reports its refusals).
    pub(super) fn try_model_ctl(
        &mut self,
        conn: u64,
        id: u64,
        key: &crate::controller::RequestKey,
        i: usize,
        target: &SessionTarget,
        command: &Command,
    ) -> bool {
        let m = &self.sess.managed[i];
        let Some(Ok(change)) = m.ctl.model_change(command) else {
            return false;
        };
        let facts = Facts {
            ctl_key: m.ctl_key.is_some(),
            connected: m.live.as_ref().is_some_and(|l| l.client.connected()),
            expected: m.ctl_expect(),
            quiet: !m.ctl.is_busy() && m.ctl.pending_switch().is_none(),
        };
        if m.ctl.target() != target || m.exited() || route(facts) != Route::Ctl {
            return false;
        }
        self.track(key, "model-change", super::mutate::Post::None);
        self.accepted(conn, id, key);
        self.defer.model_changes.insert(
            key.clone(),
            ModelChange {
                session: target.key.clone(),
                target: target.clone(),
                command: command.clone(),
                owner: key.client.clone(),
            },
        );
        self.queue_ctl(i, key.clone(), change.ctl);
        true
    }

    /// The control socket answered (or could not): finish the change, or type it.
    pub(super) fn model_change_finished(
        &mut self,
        key: crate::controller::RequestKey,
        change: ModelChange,
        result: Result<Done, GatewayError>,
    ) {
        match verdict(result) {
            Verdict::Done(message) => self.job_finished(key, Ok(Done::Message(message))),
            Verdict::Failed(text) => self.job_failed(key, GatewayError::Failed(text)),
            Verdict::Typed => {
                let Some(i) = self.sess.find(&change.session) else {
                    return self.job_failed(
                        key,
                        GatewayError::Failed("the session was closed".to_owned()),
                    );
                };
                if let Err(r) =
                    self.hand_over(i, key.clone(), &change.owner, change.target, change.command)
                {
                    self.job_failed(key, GatewayError::Rejected(r));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(message: &str) -> Result<Done, GatewayError> {
        Ok(Done::Ctl(CtlReply {
            ok: true,
            message: Some(message.to_owned()),
            ..CtlReply::default()
        }))
    }

    fn refused(error: &str) -> Result<Done, GatewayError> {
        Ok(Done::Ctl(CtlReply {
            ok: false,
            error: Some(error.to_owned()),
            ..CtlReply::default()
        }))
    }

    #[test]
    fn live_control_is_used_only_when_the_session_has_it_and_nothing_is_typing() {
        let base = Facts {
            ctl_key: true,
            connected: true,
            expected: false,
            quiet: true,
        };
        assert_eq!(route(base), Route::Ctl);
        // The socket is on its way for a session launched with the extension.
        assert_eq!(
            route(Facts {
                connected: false,
                expected: true,
                ..base
            }),
            Route::Ctl
        );
        // No extension, no socket and none expected, or an earlier command still typing.
        assert_eq!(
            route(Facts {
                ctl_key: false,
                ..base
            }),
            Route::Typed
        );
        assert_eq!(
            route(Facts {
                connected: false,
                ..base
            }),
            Route::Typed
        );
        assert_eq!(
            route(Facts {
                quiet: false,
                ..base
            }),
            Route::Typed
        );
    }

    #[test]
    fn a_reply_decides_between_done_typed_and_error() {
        assert_eq!(
            verdict(ok("Session-only model: a/b")),
            Verdict::Done("Session-only model: a/b".to_owned())
        );
        // Only omp's compact-first switch and a command that never left fall back to typing.
        for text in [
            "needs_compact",
            "not connected",
            "control client stopped",
            // Nothing changed: the extension cannot do it in this omp.
            "unsupported",
            "model switching unavailable",
            "effort unavailable",
            "unknown cmd",
        ] {
            assert_eq!(verdict(refused(text)), Verdict::Typed, "{text}");
        }
        assert_eq!(
            verdict(Err(GatewayError::Failed(NO_SOCKET.to_owned()))),
            Verdict::Typed
        );
        // omp's own refusals are the error, never retried as typed text.
        assert_eq!(
            verdict(refused("Unknown model: nope")),
            Verdict::Failed("Unknown model: nope".to_owned())
        );
        // The command may have been delivered: reported, not replayed.
        for text in ["connection closed", "write failed"] {
            assert!(
                matches!(verdict(refused(text)), Verdict::Failed(t) if t.contains("may not have")),
                "{text}"
            );
        }
        assert!(matches!(
            verdict(Err(GatewayError::Failed(
                "the session was closed".to_owned()
            ))),
            Verdict::Failed(_)
        ));
    }
}
