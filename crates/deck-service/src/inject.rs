//! Per-tab command injection into omp's composer (docs/notes/omp-actions.md §3.7).
//!
//! A pure state machine driven from the frame loop: [`Injector::poll`] takes the current time and
//! the tab's observable state and returns the bytes to write now plus when to look again. It never
//! sleeps and never touches the PTY itself, so the whole protocol is testable with a fake clock.
//!
//! Slash command sequence: (the cursor must be in omp's composer; a draft there → confirm →
//! Ctrl+C, ≥ 650 ms after the last Ctrl+C, then ≥ 650 ms until the composer reads empty) →
//! bracketed paste of the command → the composer shows it and the rows below it hold still →
//! (idle only, and only while omp's autocomplete popup is open) `Esc` → the popup is seen closing →
//! Enter. Direct hotkeys are a single key. Stop is `Esc`, then — only if omp still works after
//! 650 ms — one more `Esc`. Jobs of one tab are strictly serialized with a pause between them.
//!
//! After the paste every step waits for omp's screen to show the previous one took effect, never a
//! fixed delay: omp reads `ESC` + `\r` arriving within its 75 ms escape timeout as Alt+Enter, and a
//! stall anywhere on the way (OmpDeck's PTY thread, the tmux server, ConPTY) delivered the old timed
//! `Esc` … 150 ms … Enter back to back — a newline under a still-open popup, the command unsent.
//! The popup is detected as a change of [`composer::rows_below`] against the empty composer before
//! the paste, omp's status line aside (it redraws on input; [`composer::rows_below_changed`]);
//! without one no `Esc` is sent at all. A step omp never shows in time ([`SHOW_TIMEOUT`],
//! [`ESC_TIMEOUT`]) abandons the job without Enter ([`End::Stalled`]): the command stays in the
//! composer rather than risk the merge.
//!
//! Text jobs (review comments, prompts, steering): the paste bytes come from [`Env::paste`] (the
//! terminal's own mode-aware encoding, evaluated at write time so the pure state machine never
//! guesses the modes); `None` → the job is refused ([`End::NoPaste`]). [`ComposeMode::Insert`]
//! pastes at omp's cursor and ends there; the submitting modes follow the slash sequence with the
//! text as the paste and end with Enter ([`ComposeMode::Send`] idle, [`ComposeMode::Steer`] busy)
//! or Ctrl+Q ([`ComposeMode::FollowUp`], busy). Steering never sends `Esc` (it would interrupt the
//! turn) and never a submit key once omp stopped working (an Enter would start a new prompt, an
//! empty one would abort). Attachments are pasted first as their own paste, like a file drop;
//! the text follows once omp shows them.
//! [`Poll::ended`] reports every job that left the queue, by [`JobId`]; [`End::Completed`] means
//! every byte of it was handed out — not that omp ran it successfully.
//!
//! Keys (Ctrl+C, Esc, Enter, hotkeys) are [`Write::Key`] strokes that the terminal encodes like
//! the user's own key press (`deck_term::Terminal::send_key`), never raw bytes: in
//! win32-input-mode ConPTY keeps a trailing raw `ESC` as the start of a sequence and merges it
//! with the next write, so a raw `ESC` … `\r` reached omp as Alt+Enter and left the command
//! unsent in the composer.
//!
//! Safety rules: a job that started while omp was idle is abandoned (no further bytes) as soon as
//! omp is no longer idle, so a turn that begins mid-sequence is never interrupted by our `Esc` or
//! submitted by our `\r`; the caller drops the user's own keystrokes while a job is in flight
//! ([`Poll::hold_for`]); no byte follows a lone `Esc` within [`AFTER_ESC`].
//!
//! TSP sessions ([`Env::tsp`] set, chat view): omp's composer is not on the grid, so every step
//! observes the TSP document instead ([`TspComposer`]: the dock's sendable `editor` in focus, an
//! `omp.editor.ask*` dialog or a lost focus reading as [`Composer::Missing`], attachments as chips,
//! the autocomplete overlay as the popup). A slash command and a plain [`ComposeMode::Send`] go as
//! one [`Write::Send`] (the `send` event, when the program's hello lists it) after the same
//! confirm → Ctrl+C → composer-reads-empty steps as on the grid (so a displaced draft never lands
//! in omp's recall history); there is no paste, popup `Esc` or Enter. Inserts, steering,
//! follow-ups, attachments, Stop and hotkeys
//! keep the PTY sequence above (omp edits its own editor), and a program without `send` falls back
//! to it for every job. As on the grid, [`End::Completed`] means every byte was handed out.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use deck_term::ScreenText;
use deck_term::input::{Key, Mods};

use crate::composer::{self, Composer, TspComposer};
use crate::statusfmt::Activity;

/// omp exits on two Ctrl+C within 500 ms; keep clear of that.
pub const CTRL_C_GAP: Duration = Duration::from_millis(650);
/// Time omp needs to clear the composer after Ctrl+C.
const AFTER_CLEAR: Duration = Duration::from_millis(650);
/// Re-check interval while a step waits for omp's screen.
const SCREEN_POLL: Duration = Duration::from_millis(30);
/// omp must show the cleared composer and then the pasted text within this time.
const SHOW_TIMEOUT: Duration = Duration::from_secs(3);
/// The rows below the composer must hold still this long before the popup check: omp opens its
/// autocomplete popup asynchronously, a frame or two after it draws the text.
const SETTLE: Duration = Duration::from_millis(150);
/// Minimum gap between an `Esc` and the next byte.
const AFTER_ESC: Duration = Duration::from_millis(150);
/// The popup must be seen closing within this time after its `Esc`.
const ESC_TIMEOUT: Duration = Duration::from_secs(2);
/// Stop: how long the first `Esc` gets to end the turn before a second one is sent.
const STOP_RETRY: Duration = Duration::from_millis(650);
/// Pause between two jobs of the same tab.
const BETWEEN_JOBS: Duration = Duration::from_millis(300);
/// User input stays dropped this long after the last injected byte.
pub const HOLD_TAIL: Duration = Duration::from_millis(150);

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// What omp must be doing for a job to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Requires {
    Any,
    /// `Activity::Idle` exactly (not attention, not unknown, not busy): `/compact` would abort a
    /// turn, `/handoff` refuses, and omp's title must prove it is waiting for input.
    Idle,
    /// Only meaningful while a turn runs (stop).
    Busy,
}

/// How a [`Payload::Text`] ends once its text is in omp's composer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ComposeMode {
    /// Submit with Enter (idle only; the draft is confirmed and cleared first).
    Send,
    /// Enter while omp works: omp steers the running turn with the text.
    Steer,
    /// Ctrl+Q (0x11) while omp works: omp queues the text as a follow-up.
    FollowUp,
    /// Pasted at omp's cursor, never submitted (any activity; a draft stays).
    Insert,
}

impl ComposeMode {
    /// What omp must be doing for the job to start.
    pub fn requires(self) -> Requires {
        match self {
            ComposeMode::Send => Requires::Idle,
            ComposeMode::Steer | ComposeMode::FollowUp => Requires::Busy,
            ComposeMode::Insert => Requires::Any,
        }
    }

    fn submits(self) -> bool {
        self != ComposeMode::Insert
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Payload {
    /// A slash command typed through the composer (single line, no control characters).
    Slash(String),
    /// One key stroke (hotkeys such as Alt+Shift+P); no composer handling needed.
    Key(Key, Mods),
    /// `Esc`, and a second `Esc` after [`STOP_RETRY`] if the turn still runs.
    Stop,
    /// Review comments, typed prompts and other multi-line text, written with
    /// `Terminal::paste_bytes`. `attach`: quoted file paths (`deck_term::input::encode_dropped_paths`)
    /// pasted on their own first, as a file drop, so omp attaches the images among them; the text
    /// follows once omp shows them. Empty `text`: only the attachments.
    Text {
        attach: Option<String>,
        text: String,
        mode: ComposeMode,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// Human label for messages ("Compact", "Switch model").
    pub label: String,
    pub payload: Payload,
    pub requires: Requires,
}

/// One line without control characters: whitespace controls (newline, tab, NEL …) become a single
/// blank, every other C0/C1 control (ESC, DEL, …) is dropped. An `ESC[201~` inside a pasted title
/// would otherwise end the bracketed paste early and run the rest as keystrokes.
pub fn sanitize_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out.trim().to_owned()
}

/// Multi-line text for a bracketed paste: newlines and tabs stay, every other C0/C1 control
/// (ESC, CR, DEL, NEL …) is dropped so nothing in it can end the paste early or act as a key.
pub fn sanitize_text(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect()
}

impl Job {
    /// The text is sanitized with [`sanitize_line`], whatever the caller built it from.
    pub fn slash(label: impl Into<String>, text: &str, requires: Requires) -> Job {
        Job {
            label: label.into(),
            payload: Payload::Slash(sanitize_line(text)),
            requires,
        }
    }

    /// A multi-line text pasted into omp's composer (`submit=false`: **Insert**, any activity,
    /// keeps a draft, never sends; `submit=true`: **Send**, idle only, the draft is confirmed and
    /// cleared first, then Esc + Enter). The text is sanitized with [`sanitize_text`].
    pub fn text(label: impl Into<String>, text: &str, submit: bool) -> Job {
        let mode = if submit {
            ComposeMode::Send
        } else {
            ComposeMode::Insert
        };
        Job::compose(label, None, text, mode)
    }

    /// Attachments (already quoted paths, pasted first as their own paste), then `text`, then
    /// the submit of `mode`. The text and the paths are sanitized with [`sanitize_text`].
    pub fn compose(
        label: impl Into<String>,
        attach: Option<&str>,
        text: &str,
        mode: ComposeMode,
    ) -> Job {
        Job {
            label: label.into(),
            payload: Payload::Text {
                attach: attach.map(sanitize_text).filter(|a| !a.is_empty()),
                text: sanitize_text(text),
                mode,
            },
            requires: mode.requires(),
        }
    }

    pub fn key(label: impl Into<String>, key: Key, mods: Mods, requires: Requires) -> Job {
        Job {
            label: label.into(),
            payload: Payload::Key(key, mods),
            requires,
        }
    }

    pub fn stop() -> Job {
        Job {
            label: "Stop".to_owned(),
            payload: Payload::Stop,
            requires: Requires::Busy,
        }
    }
}

/// Observable tab state handed to [`Injector::poll`].
pub struct Env<'a> {
    pub now: Instant,
    /// From the terminal title; idle-only checks need `Activity::Idle` exactly.
    pub activity: Activity,
    /// Last 0x03 written to this tab's PTY (by the user or by us).
    pub last_ctrl_c: Option<Instant>,
    /// The visible grid; only inspected when a slash job starts.
    pub screen: &'a dyn Fn() -> ScreenText,
    /// The exact bytes the terminal's own paste sends for a text under its current modes
    /// (`deck_term::Terminal::paste_bytes`); `None` when omp has not enabled bracketed paste.
    /// Only [`Payload::Text`] jobs use it.
    pub paste: &'a dyn Fn(&str) -> Option<Vec<u8>>,
    /// The composer as omp's TSP document shows it (`None` = classic grid session). A session
    /// whose omp speaks TSP has no composer on its grid: the document is the only place that says
    /// what the composer holds, whether a dialog took it, and where `send` goes.
    pub tsp: Option<&'a dyn Fn() -> TspComposer>,
}

/// One piece of input for the tab's terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// Raw bytes (the bracketed paste of a slash command).
    Bytes(Vec<u8>),
    /// A key stroke, to be encoded like the user's own key (`deck_term::Terminal::send_key`).
    Key(Key, Mods),
    /// A TSP `send` event: submits `text` as its own prompt through the composer's ordinary
    /// submission path (`deck_term::Terminal::inject_tsp`), on a composer already cleared.
    Send {
        surface: String,
        editor: String,
        text: String,
    },
}

const ESC_KEY: Write = Write::Key(Key::Escape, Mods::NONE);
const ENTER_KEY: Write = Write::Key(Key::Enter, Mods::NONE);
const CTRL_C_KEY: Write = Write::Key(Key::Char('c'), Mods::CTRL);
/// omp's follow-up hotkey (`app.message.followUp`, Ctrl+Q: 0x11 in legacy encoding).
const FOLLOW_UP_KEY: Write = Write::Key(Key::Char('q'), Mods::CTRL);

/// Identity of one pushed job, unique per [`Injector`] (see [`Injector::push`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

/// How a job left the injector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// It ran to its last step (every byte of it was handed out).
    Completed,
    /// Dropped before it started because its precondition failed.
    Refused(Requires),
    /// A slash/text job dropped because the cursor is not in omp's composer (an overlay is open).
    DialogOpen,
    /// Abandoned after it started because omp stopped being idle.
    Aborted,
    /// Abandoned because omp did not show a step in time ([`SHOW_TIMEOUT`]); the command may sit
    /// unsent in the composer.
    Stalled,
    /// A text job dropped because omp has not enabled bracketed paste (`Env::paste` was `None`).
    NoPaste,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ended {
    pub id: JobId,
    pub label: String,
    pub end: End,
}

/// What the caller must do after a poll.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Poll {
    /// Input for the terminal, in order.
    pub writes: Vec<Write>,
    /// Poll again no later than this from now (`None` = nothing pending).
    pub wake_in: Option<Duration>,
    /// The composer holds an unsent draft: ask the user, then call [`Injector::answer`].
    pub confirm: Option<Confirm>,
    /// The jobs that left the injector during this poll, in order.
    pub ended: Vec<Ended>,
    /// Drop the user's keyboard/paste input to the terminal for this long from now: bytes were
    /// written, or a job with written bytes still waits for its next step.
    pub hold_for: Option<Duration>,
}

#[cfg(test)]
impl Poll {
    fn labels(&self, end: End) -> Vec<String> {
        self.ended
            .iter()
            .filter(|e| e.end == end)
            .map(|e| e.label.clone())
            .collect()
    }
    fn completed(&self) -> Vec<String> {
        self.labels(End::Completed)
    }
    fn dialog_open(&self) -> Vec<String> {
        self.labels(End::DialogOpen)
    }
    fn aborted(&self) -> Vec<String> {
        self.labels(End::Aborted)
    }
    fn stalled(&self) -> Vec<String> {
        self.labels(End::Stalled)
    }
    fn no_paste(&self) -> Vec<String> {
        self.labels(End::NoPaste)
    }
    fn refused(&self) -> Vec<(String, Requires)> {
        self.ended
            .iter()
            .filter_map(|e| match e.end {
                End::Refused(r) => Some((e.label.clone(), r)),
                _ => None,
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    pub id: JobId,
    pub label: String,
    pub draft: String,
}

#[derive(Debug)]
enum Stage {
    /// Look at the composer first.
    Start,
    /// Waiting for the user to decide about the draft.
    AwaitConfirm,
    /// Waiting until a Ctrl+C is safe.
    ClearPending,
    /// The composer must read empty from this instant on (Ctrl+C sent, or nothing to clear).
    Cleared(Instant),
    /// The attachment paste was written at `at` onto a composer that read `before`; the text
    /// follows once omp shows a change. `baseline`: the rows below the composer before it.
    Attached {
        at: Instant,
        before: String,
        baseline: Vec<String>,
    },
    /// Paste written at `at`; `baseline` = the rows below the empty composer before it.
    /// `settle` = the rows below the composer since it shows the text, and since when.
    Pasted {
        at: Instant,
        baseline: Vec<String>,
        settle: Option<(Vec<String>, Instant)>,
    },
    /// Popup `Esc` sent at `at` while the rows below the composer were `popup`.
    Escaped { at: Instant, popup: Vec<String> },
    /// Stop: the second-`Esc` check is due at this instant.
    Stopping(Instant),
}

struct Active {
    id: JobId,
    job: Job,
    stage: Stage,
    /// A byte of this job already reached the PTY.
    wrote: bool,
    /// omp was idle when the job started: it must stay idle until the final Enter.
    started_idle: bool,
}

#[derive(Default)]
pub struct Injector {
    queue: VecDeque<(JobId, Job)>,
    active: Option<Active>,
    /// Id of the next pushed job.
    next_id: u64,
    /// Earliest start of the next job.
    ready_at: Option<Instant>,
    /// Own record of the last Ctrl+C (also covers PTY writes not yet seen by the terminal).
    ctrl_c: Option<Instant>,
    /// When the last byte of any job was handed to the PTY.
    last_write: Option<Instant>,
}

impl Injector {
    pub fn push(&mut self, job: Job) -> JobId {
        let id = JobId(self.next_id);
        self.next_id += 1;
        self.queue.push_back((id, job));
        id
    }

    /// The jobs still held (running first, then queued in order). A pushed job that is no longer
    /// here and was not reported in a [`Poll::ended`] was dropped by [`Self::answer`],
    /// [`Self::clear`] or [`Self::drop_unstarted`].
    pub fn live_ids(&self) -> impl Iterator<Item = JobId> + '_ {
        self.active
            .as_ref()
            .map(|a| a.id)
            .into_iter()
            .chain(self.queue.iter().map(|(id, _)| *id))
    }

    /// Whether a byte of the running job `id` already reached the PTY.
    pub fn has_written(&self, id: JobId) -> bool {
        self.active.as_ref().is_some_and(|a| a.id == id && a.wrote)
    }

    /// True while a job runs or is queued.
    pub fn is_busy(&self) -> bool {
        self.active.is_some() || !self.queue.is_empty()
    }

    /// True while a confirmation from the user is outstanding.
    pub fn awaiting_answer(&self) -> bool {
        matches!(
            self.active,
            Some(Active {
                stage: Stage::AwaitConfirm,
                ..
            })
        )
    }

    /// The user's answer to [`Poll::confirm`]: `true` = clear the draft and go on, `false` = drop the job.
    pub fn answer(&mut self, clear: bool) {
        if !self.awaiting_answer() {
            return;
        }
        if clear {
            if let Some(a) = &mut self.active {
                a.stage = Stage::ClearPending;
            }
        } else {
            self.active = None;
        }
    }

    /// Drop everything (omp exited).
    pub fn clear(&mut self) {
        self.queue.clear();
        self.active = None;
    }

    /// Tab closing: drop every job that has not put a byte into the PTY yet. True when nothing is
    /// left, i.e. the injector is at a job boundary and the tab can go now.
    pub fn drop_unstarted(&mut self) -> bool {
        self.queue.clear();
        if self.active.as_ref().is_some_and(|a| !a.wrote) {
            self.active = None;
        }
        self.active.is_none()
    }

    /// Drops the jobs `ids` that have not put a byte into the PTY yet (queued, or running and
    /// unwritten, such as one waiting for a draft confirmation). True when any was dropped; a
    /// running job that already wrote is left to finish.
    pub fn drop_unwritten(&mut self, ids: &[JobId]) -> bool {
        let before = self.queue.len();
        self.queue.retain(|(id, _)| !ids.contains(id));
        let mut dropped = self.queue.len() != before;
        if self
            .active
            .as_ref()
            .is_some_and(|a| !a.wrote && ids.contains(&a.id))
        {
            self.active = None;
            dropped = true;
        }
        dropped
    }

    /// Tab closing: true once nothing is left to send, no job is mid-way and the last written byte
    /// is at least [`HOLD_TAIL`] old (the terminal needs that long to deliver it before detaching).
    pub fn can_close(&mut self, now: Instant) -> bool {
        self.drop_unstarted()
            && self
                .last_write
                .is_none_or(|t| now.saturating_duration_since(t) >= HOLD_TAIL)
    }

    pub fn poll(&mut self, env: &Env<'_>) -> Poll {
        let mut out = Poll::default();
        // Several steps can complete in one poll (e.g. a zero wait), so loop until blocked.
        loop {
            if self.active.is_none() && !self.start_next(env, &mut out) {
                break;
            }
            let Some(active) = self.active.as_mut() else {
                break;
            };
            match step(active, env, &mut self.ctrl_c, &mut out) {
                Step::Continue => {}
                Step::Wait(d) => {
                    out.wake_in = Some(d);
                    break;
                }
                Step::Blocked => break,
                Step::Done => {
                    if let Some(a) = self.active.take() {
                        out.ended.push(Ended {
                            id: a.id,
                            label: a.job.label,
                            end: End::Completed,
                        });
                    }
                    self.ready_at = Some(env.now + BETWEEN_JOBS);
                }
                Step::Dropped(end) => {
                    // Nothing was sent, so no pause is needed before the next job.
                    if let Some(a) = self.active.take() {
                        out.ended.push(Ended {
                            id: a.id,
                            label: a.job.label,
                            end,
                        });
                    }
                }
                ended @ (Step::Aborted | Step::Stalled) => {
                    if let Some(a) = self.active.take() {
                        out.ended.push(Ended {
                            id: a.id,
                            label: a.job.label,
                            end: if matches!(ended, Step::Aborted) {
                                End::Aborted
                            } else {
                                End::Stalled
                            },
                        });
                    }
                    self.ready_at = Some(env.now + BETWEEN_JOBS);
                }
            }
        }
        if !out.writes.is_empty() {
            self.last_write = Some(env.now);
        }
        out.hold_for = match &self.active {
            Some(a) if a.wrote => Some(out.wake_in.unwrap_or_default() + HOLD_TAIL),
            None if !out.writes.is_empty() => Some(HOLD_TAIL),
            _ => None,
        };
        out
    }

    /// Pops the next runnable job into `active`; false when nothing can start now.
    fn start_next(&mut self, env: &Env<'_>, out: &mut Poll) -> bool {
        if let Some(at) = self.ready_at {
            if env.now < at {
                if !self.queue.is_empty() {
                    out.wake_in = Some(at - env.now);
                }
                return false;
            }
            self.ready_at = None;
        }
        while let Some((id, job)) = self.queue.pop_front() {
            let blocked = match job.requires {
                Requires::Any => false,
                Requires::Idle => env.activity != Activity::Idle,
                Requires::Busy => !env.activity.is_busy(),
            };
            if blocked {
                out.ended.push(Ended {
                    id,
                    end: End::Refused(job.requires),
                    label: job.label,
                });
                continue;
            }
            self.active = Some(Active {
                id,
                job,
                stage: Stage::Start,
                wrote: false,
                started_idle: env.activity == Activity::Idle,
            });
            return true;
        }
        false
    }
}

enum Step {
    Continue,
    Wait(Duration),
    /// Needs outside input (confirmation).
    Blocked,
    Done,
    /// Ended before any byte was written, for this reason.
    Dropped(End),
    /// Ended without its final Enter because omp left the idle state.
    Aborted,
    /// Ended without its final Enter because omp did not show a step in time.
    Stalled,
}

fn write(a: &mut Active, out: &mut Poll, w: Write) {
    a.wrote = true;
    out.writes.push(w);
}

/// What the job's step sees of omp's composer: the grid's, or the TSP document's.
struct Obs {
    composer: Composer,
    /// What sits below/over the composer: the rows under it (`None` = no composer found), or
    /// (TSP) one marker row while the autocomplete popup is open. Compared with
    /// [`composer::rows_below_changed`].
    below: Option<Vec<String>>,
    /// Changes whenever the composer shows something new (its draft, or the attachments).
    shape: String,
    /// The composer shows something the job's paste can have produced.
    visible: bool,
    tsp: Option<TspComposer>,
}

fn observe(env: &Env<'_>) -> Obs {
    if let Some(tsp) = env.tsp {
        let t = tsp();
        return Obs {
            composer: t.state.clone(),
            below: Some(if t.popup {
                vec!["autocomplete".to_owned()]
            } else {
                Vec::new()
            }),
            shape: t.fingerprint(),
            visible: t.state != Composer::Missing,
            tsp: Some(t),
        };
    }
    let screen = (env.screen)();
    let composer = composer::inspect(&screen);
    Obs {
        below: composer::rows_below(&screen),
        shape: match &composer {
            Composer::Draft(d) => d.clone(),
            _ => String::new(),
        },
        visible: matches!(composer, Composer::Draft(_)),
        composer,
        tsp: None,
    }
}

/// The text a job submits as one TSP `send` event, when the program accepts it: a slash command
/// or a plain Send (steering, follow-ups and attachments need the composer's own keys).
fn atomic_text(payload: &Payload, tsp: Option<&TspComposer>) -> Option<String> {
    if !tsp.is_some_and(|t| t.can_send) {
        return None;
    }
    match payload {
        Payload::Slash(text) => Some(text.clone()),
        Payload::Text {
            attach: None,
            text,
            mode: ComposeMode::Send,
        } => Some(text.clone()),
        _ => None,
    }
}

fn step(a: &mut Active, env: &Env<'_>, ctrl_c: &mut Option<Instant>, out: &mut Poll) -> Step {
    let now = env.now;
    let wait_until = |t: Instant| Step::Wait(t.saturating_duration_since(now));
    if a.started_idle && env.activity != Activity::Idle && !matches!(a.stage, Stage::Start) {
        return Step::Aborted;
    }
    match (&a.job.payload, &a.stage) {
        (Payload::Key(key, mods), Stage::Start) => {
            let w = Write::Key(*key, *mods);
            write(a, out, w);
            Step::Done
        }
        (Payload::Key(..), _) => Step::Done,
        (Payload::Stop, Stage::Start) => {
            write(a, out, ESC_KEY);
            a.stage = Stage::Stopping(now + STOP_RETRY);
            wait_until(now + STOP_RETRY)
        }
        (Payload::Stop, Stage::Stopping(until)) => {
            if now < *until {
                return wait_until(*until);
            }
            // Never two Esc within 500 ms; by now the first one was ≥ 650 ms ago.
            if env.activity.is_busy() {
                write(a, out, ESC_KEY);
            }
            Step::Done
        }
        (Payload::Stop, _) => Step::Done,
        (
            Payload::Text {
                attach,
                text,
                mode: ComposeMode::Insert,
            },
            Stage::Start,
        ) => {
            // Insert: pasted at omp's cursor; a draft stays, nothing is cleared, nothing submitted.
            let first = attach.as_deref().unwrap_or(text);
            let Some(bytes) = (env.paste)(first) else {
                return Step::Dropped(End::NoPaste);
            };
            let obs = observe(env);
            if obs.composer == Composer::Missing {
                return Step::Dropped(End::DialogOpen);
            }
            let single = attach.is_none() || text.is_empty();
            write(a, out, Write::Bytes(bytes));
            if single {
                return Step::Done;
            }
            a.stage = Stage::Attached {
                at: now,
                before: obs.shape,
                baseline: obs.below.unwrap_or_default(),
            };
            Step::Wait(SCREEN_POLL)
        }
        (Payload::Slash(_) | Payload::Text { .. }, Stage::Start) => {
            let obs = observe(env);
            let atomic = atomic_text(&a.job.payload, obs.tsp.as_ref()).is_some();
            if !atomic
                && let Payload::Text { attach, text, .. } = &a.job.payload
                && (env.paste)(attach.as_deref().unwrap_or(text)).is_none()
            {
                return Step::Dropped(End::NoPaste);
            }
            match obs.composer {
                Composer::Missing => Step::Dropped(End::DialogOpen),
                Composer::Draft(draft) => {
                    out.confirm = Some(Confirm {
                        id: a.id,
                        label: a.job.label.clone(),
                        draft,
                    });
                    a.stage = Stage::AwaitConfirm;
                    Step::Blocked
                }
                Composer::Empty => {
                    a.stage = Stage::Cleared(now);
                    Step::Continue
                }
            }
        }
        (
            Payload::Text {
                mode: ComposeMode::Insert,
                ..
            },
            Stage::Attached { .. },
        ) => after_attach(a, env, out),
        (
            Payload::Text {
                mode: ComposeMode::Insert,
                ..
            },
            _,
        ) => Step::Done,
        (Payload::Slash(_) | Payload::Text { .. }, Stage::AwaitConfirm) => Step::Blocked,
        (Payload::Slash(_) | Payload::Text { .. }, Stage::ClearPending) => {
            // A draft is cleared with Ctrl+C like on the grid, never displaced into omp's recall
            // history by the `send` itself. Not into an ask or dialog that took the composer
            // while the confirmation was open: Ctrl+C there would answer it.
            if env.tsp.is_some_and(|f| {
                let t = f();
                t.state == Composer::Missing && atomic_text(&a.job.payload, Some(&t)).is_some()
            }) {
                return Step::Dropped(End::DialogOpen);
            }
            let last = [*ctrl_c, env.last_ctrl_c].into_iter().flatten().max();
            match last {
                Some(t) if now < t + CTRL_C_GAP => wait_until(t + CTRL_C_GAP),
                _ => {
                    write(a, out, CTRL_C_KEY);
                    *ctrl_c = Some(now);
                    a.stage = Stage::Cleared(now + AFTER_CLEAR);
                    wait_until(now + AFTER_CLEAR)
                }
            }
        }
        (Payload::Slash(_) | Payload::Text { .. }, Stage::Cleared(until)) => {
            let until = *until;
            if now < until {
                return wait_until(until);
            }
            let obs = observe(env);
            if let Some(text) = atomic_text(&a.job.payload, obs.tsp.as_ref()) {
                if text.trim().is_empty() {
                    return Step::Dropped(End::Aborted);
                }
                // No composer to address (an ask or dialog took it): wait a moment for it to come
                // back, else the job is refused as with any open dialog.
                let Some((surface, editor)) = obs.tsp.and_then(|t| t.target) else {
                    return if now >= until + SHOW_TIMEOUT {
                        Step::Dropped(End::DialogOpen)
                    } else {
                        Step::Wait(SCREEN_POLL)
                    };
                };
                // The editor's text must be seen empty first: a `send` over a draft would
                // displace it into recall.
                if obs.composer != Composer::Empty {
                    return if now >= until + SHOW_TIMEOUT {
                        Step::Stalled
                    } else {
                        Step::Wait(SCREEN_POLL)
                    };
                }
                write(
                    a,
                    out,
                    Write::Send {
                        surface,
                        editor,
                        text,
                    },
                );
                return Step::Done;
            }
            // Pasting onto a draft omp has not cleared yet would submit the two of them as one.
            if obs.composer != Composer::Empty {
                return if now >= until + SHOW_TIMEOUT {
                    Step::Stalled
                } else {
                    Step::Wait(SCREEN_POLL)
                };
            }
            let baseline = obs.below.unwrap_or_default();
            let (bytes, attaching) = match &a.job.payload {
                Payload::Slash(text) => ([PASTE_START, text.as_bytes(), PASTE_END].concat(), false),
                Payload::Text { attach, text, .. } => {
                    match (env.paste)(attach.as_deref().unwrap_or(text)) {
                        Some(bytes) => (bytes, attach.is_some()),
                        // The terminal's modes changed since the start: nothing was sent for this
                        // paste.
                        None => return Step::Aborted,
                    }
                }
                Payload::Key(..) | Payload::Stop => return Step::Done,
            };
            write(a, out, Write::Bytes(bytes));
            a.stage = if attaching {
                Stage::Attached {
                    at: now,
                    before: String::new(),
                    baseline,
                }
            } else {
                Stage::Pasted {
                    at: now,
                    baseline,
                    settle: None,
                }
            };
            Step::Wait(SCREEN_POLL)
        }
        (Payload::Slash(_) | Payload::Text { .. }, Stage::Attached { .. }) => {
            after_attach(a, env, out)
        }
        (Payload::Slash(_) | Payload::Text { .. }, Stage::Pasted { .. }) => {
            after_paste(a, env, out)
        }
        (Payload::Slash(_) | Payload::Text { .. }, Stage::Escaped { .. }) => {
            after_escape(a, env, out)
        }
        (Payload::Slash(_) | Payload::Text { .. }, Stage::Stopping(_)) => Step::Done,
    }
}

/// The composer shows the job's paste: the command itself (omp may draw an argument hint after
/// it, and wraps long lines), or any text for a review (omp collapses long pastes into a marker;
/// under TSP an attachment-only paste shows as chips with no text).
fn shows_paste(payload: &Payload, obs: &Obs) -> bool {
    match payload {
        Payload::Slash(text) => {
            let Composer::Draft(draft) = &obs.composer else {
                return false;
            };
            let squash = |s: &str| -> String {
                s.chars()
                    .filter(|c| !c.is_whitespace() && *c != '↵')
                    .collect()
            };
            squash(draft).starts_with(&squash(text))
        }
        _ => {
            obs.visible
                && (matches!(obs.composer, Composer::Draft(_))
                    || obs.tsp.as_ref().is_some_and(|t| t.chips > 0))
        }
    }
}

/// Attachment paste written: once omp shows it (the composer changed), the text is pasted (an
/// Insert ends there); without a text the submit check follows at once.
fn after_attach(a: &mut Active, env: &Env<'_>, out: &mut Poll) -> Step {
    let now = env.now;
    let Stage::Attached {
        at,
        before,
        baseline,
    } = &mut a.stage
    else {
        return Step::Done;
    };
    let obs = observe(env);
    let shown = obs.visible && obs.shape != *before;
    if !shown {
        return if now >= *at + SHOW_TIMEOUT {
            Step::Stalled
        } else {
            Step::Wait(SCREEN_POLL)
        };
    }
    let baseline = std::mem::take(baseline);
    let Payload::Text { text, mode, .. } = &a.job.payload else {
        return Step::Done;
    };
    let mode = *mode;
    if !text.is_empty() {
        let Some(bytes) = (env.paste)(text) else {
            return Step::Aborted;
        };
        write(a, out, Write::Bytes(bytes));
    }
    if !mode.submits() {
        return Step::Done;
    }
    a.stage = Stage::Pasted {
        at: now,
        baseline,
        settle: None,
    };
    Step::Wait(SCREEN_POLL)
}

/// Paste written: once omp shows it, the submit key right away (no popup, or omp busy: an `Esc`
/// would interrupt the turn), else `Esc` to close the popup first. Steer and follow-up need omp
/// still working at that moment: an Enter on an idle omp would send the text as a new prompt.
fn after_paste(a: &mut Active, env: &Env<'_>, out: &mut Poll) -> Step {
    let now = env.now;
    let mode = match &a.job.payload {
        Payload::Text { mode, .. } => *mode,
        Payload::Slash(_) | Payload::Key(..) | Payload::Stop => ComposeMode::Send,
    };
    let Stage::Pasted {
        at,
        baseline,
        settle,
    } = &mut a.stage
    else {
        return Step::Done;
    };
    let obs = observe(env);
    if !shows_paste(&a.job.payload, &obs) {
        return if now >= *at + SHOW_TIMEOUT {
            Step::Stalled
        } else {
            Step::Wait(SCREEN_POLL)
        };
    }
    let submit = match mode {
        ComposeMode::Steer | ComposeMode::FollowUp if !env.activity.is_busy() => {
            return Step::Aborted;
        }
        ComposeMode::FollowUp => FOLLOW_UP_KEY,
        ComposeMode::Send | ComposeMode::Steer | ComposeMode::Insert => ENTER_KEY,
    };
    let popup = if env.activity == Activity::Idle {
        let below = obs.below.unwrap_or_default();
        match settle {
            Some((rows, since)) if !composer::rows_below_changed(rows, &below) => {
                let due = *since + SETTLE;
                if now < due {
                    return Step::Wait(SCREEN_POLL.min(due - now));
                }
            }
            _ => {
                *settle = Some((below, now));
                return Step::Wait(SCREEN_POLL);
            }
        }
        composer::rows_below_changed(baseline, &below).then_some(below)
    } else {
        None
    };
    match popup {
        Some(popup) => {
            write(a, out, ESC_KEY);
            a.stage = Stage::Escaped { at: now, popup };
            Step::Wait(AFTER_ESC)
        }
        None => {
            write(a, out, submit);
            Step::Done
        }
    }
}

/// Popup `Esc` written: Enter once the rows below the composer changed, i.e. omp read the `Esc` on
/// its own and closed the popup. Without that proof an Enter could still reach omp together with
/// the `Esc` (Alt+Enter), so the job gives up after [`ESC_TIMEOUT`].
fn after_escape(a: &mut Active, env: &Env<'_>, out: &mut Poll) -> Step {
    let now = env.now;
    let Stage::Escaped { at, popup } = &a.stage else {
        return Step::Done;
    };
    let due = *at + AFTER_ESC;
    if now < due {
        return Step::Wait(due - now);
    }
    if observe(env)
        .below
        .is_some_and(|rows| composer::rows_below_changed(popup, &rows))
    {
        write(a, out, ENTER_KEY);
        Step::Done
    } else if now >= *at + ESC_TIMEOUT {
        Step::Stalled
    } else {
        Step::Wait(SCREEN_POLL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALT_SHIFT: Mods = Mods::new(true, true, false);
    const STATUS: &str = " π · ◒ Fake One · 🗑 w3f/proj · ◫ 7.4%/100K ⟲";
    /// [`STATUS`] after omp refreshed its git segment on input.
    const STATUS_REDRAWN: &str = " π · ◒ Fake One · 🗑 w3f/proj · @ main ?7 · ◫ 7.4%/100K ⟲";
    /// omp's autocomplete popup under the composer.
    const POPUP: [&str; 2] = ["> a/b     Model B", "  a/bc    Model BC"];

    fn paste(text: &str) -> Write {
        Write::Bytes([PASTE_START, text.as_bytes(), PASTE_END].concat())
    }

    fn plan_job(label: &str) -> Job {
        Job::key(label, Key::Char('p'), ALT_SHIFT, Requires::Any)
    }

    /// Rail composer (omp 18.12.5) holding `draft`, cursor at its end, `below` under it.
    fn rail_screen(draft: &str, below: &[&str]) -> ScreenText {
        let mut lines: Vec<String> = vec!["".into(), "  v18.12.5".into()];
        for row in draft.split('\n') {
            lines.push(format!("▎ {row}").trim_end().to_owned());
        }
        let cursor_row = lines.len() - 1;
        let cursor_col = lines[cursor_row].chars().count().max(2);
        lines.extend(below.iter().map(|r| (*r).to_owned()));
        ScreenText {
            lines,
            cursor_row,
            cursor_col,
        }
    }

    fn overlay() -> ScreenText {
        ScreenText {
            lines: vec![
                "".into(),
                "▎".into(),
                "╭─ Switch Model ───────╮".into(),
                "│  🔍 >                │".into(),
                "╰──────────────────────╯".into(),
            ],
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    /// A scripted omp: what it draws follows the injector's writes (unless `react` is off, as when
    /// the bytes are stuck on the way).
    struct Fixture {
        base: Instant,
        activity: Activity,
        paste_ok: bool,
        last_ctrl_c: Option<Instant>,
        /// Shown instead of the simulated composer.
        fixed: Option<ScreenText>,
        draft: String,
        popup: bool,
        /// A pasted slash command opens the autocomplete popup.
        paste_opens_popup: bool,
        react: bool,
        /// Bytes from the first `Esc` on are stuck on their way to omp.
        stuck_from_esc: bool,
        /// An Enter reached omp while its popup was open (it would apply a completion).
        enter_into_popup: bool,
        /// omp's status line, the last row; omp draws the popup above it.
        status: String,
        /// omp redraws the status line on the paste (a git count refreshed on input).
        redraw_status_on_paste: bool,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture {
                base: Instant::now(),
                activity: Activity::Idle,
                paste_ok: true,
                last_ctrl_c: None,
                fixed: None,
                draft: String::new(),
                popup: false,
                paste_opens_popup: true,
                react: true,
                stuck_from_esc: false,
                enter_into_popup: false,
                status: STATUS.to_owned(),
                redraw_status_on_paste: false,
            }
        }

        fn screen(&self) -> ScreenText {
            if let Some(s) = &self.fixed {
                return s.clone();
            }
            let mut below = vec![""];
            if self.popup {
                below.extend(POPUP);
            }
            below.push(&self.status);
            rail_screen(&self.draft, &below)
        }

        fn poll(&mut self, inj: &mut Injector, ms: u64) -> Poll {
            let screen = self.screen();
            let f = move || screen.clone();
            let paste_ok = self.paste_ok;
            let p = inj.poll(&Env {
                now: self.base + Duration::from_millis(ms),
                activity: self.activity,
                last_ctrl_c: self.last_ctrl_c,
                screen: &f,
                paste: &|t| paste_ok.then(|| [PASTE_START, t.as_bytes(), PASTE_END].concat()),
                tsp: None,
            });
            for w in &p.writes {
                self.apply(w);
            }
            p
        }

        fn apply(&mut self, w: &Write) {
            if self.stuck_from_esc && *w == ESC_KEY {
                self.react = false;
            }
            if !self.react {
                return;
            }
            match w {
                Write::Bytes(b) => {
                    let inner = &b[PASTE_START.len()..b.len() - PASTE_END.len()];
                    self.draft.push_str(&String::from_utf8_lossy(inner));
                    self.popup = self.paste_opens_popup && self.draft.starts_with('/');
                    if self.redraw_status_on_paste {
                        self.status = STATUS_REDRAWN.to_owned();
                    }
                }
                Write::Key(Key::Escape, _) => self.popup = false,
                Write::Key(Key::Enter, _) => {
                    self.enter_into_popup |= self.popup;
                    self.draft.clear();
                    self.popup = false;
                }
                // Like omp: Ctrl+C empties the composer but leaves a stale popup on screen;
                // Ctrl+Q queues the draft as a follow-up.
                Write::Key(Key::Char('c' | 'q'), m) if m.ctrl => self.draft.clear(),
                Write::Key(..) | Write::Send { .. } => {}
            }
        }
    }

    fn drain(fx: &mut Fixture, inj: &mut Injector) -> Vec<(u64, Write)> {
        let mut writes = Vec::new();
        let mut t = 0;
        while inj.is_busy() && t < 10_000 {
            let p = fx.poll(inj, t);
            for w in p.writes {
                writes.push((t, w));
            }
            t += 10;
        }
        writes
    }

    fn keys(w: &[(u64, Write)]) -> Vec<Write> {
        w.iter().map(|(_, k)| k.clone()).collect()
    }

    fn at(w: &[(u64, Write)], key: &Write) -> u64 {
        w.iter()
            .find(|(_, k)| k == key)
            .map(|(t, _)| *t)
            .unwrap_or_else(|| panic!("{key:?} never written: {w:?}"))
    }

    #[test]
    fn popup_is_closed_with_escape_before_the_enter() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch model", "/switch a/b", Requires::Any));
        let w = drain(&mut fx, &mut inj);
        assert_eq!(keys(&w), vec![paste("/switch a/b"), ESC_KEY, ENTER_KEY]);
        assert!(!fx.enter_into_popup && fx.draft.is_empty());
        // The popup opens a frame after the text: it gets time to show before the decision.
        assert!(at(&w, &ESC_KEY) >= SETTLE.as_millis() as u64);
        assert!(at(&w, &ENTER_KEY) - at(&w, &ESC_KEY) >= AFTER_ESC.as_millis() as u64);
    }

    #[test]
    fn without_a_popup_no_escape_is_sent() {
        let mut fx = Fixture::new();
        fx.paste_opens_popup = false;
        let mut inj = Injector::default();
        inj.push(Job::slash("Rename", "/rename new title", Requires::Any));
        let w = drain(&mut fx, &mut inj);
        assert_eq!(keys(&w), vec![paste("/rename new title"), ENTER_KEY]);
        assert!(fx.draft.is_empty());
    }

    /// The bug: omp refreshes its git segment on input, so in a repo whose files were changing the
    /// status line redrew right after the paste; read as a popup, the prompt got an `Esc`, waited
    /// for a popup to close and stalled in the composer without its Enter.
    #[test]
    fn a_status_line_redraw_after_the_paste_is_not_a_popup() {
        let mut fx = Fixture::new();
        fx.redraw_status_on_paste = true;
        let mut inj = Injector::default();
        inj.push(Job::text("Send prompt", "Reply with ok.", true));
        let w = drain(&mut fx, &mut inj);
        assert_eq!(keys(&w), vec![paste("Reply with ok."), ENTER_KEY]);
        assert!(fx.draft.is_empty());
    }

    #[test]
    fn a_popup_is_still_closed_first_when_the_status_line_redraws_too() {
        let mut fx = Fixture::new();
        fx.redraw_status_on_paste = true;
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch model", "/switch a/b", Requires::Any));
        let w = drain(&mut fx, &mut inj);
        assert_eq!(keys(&w), vec![paste("/switch a/b"), ESC_KEY, ENTER_KEY]);
        assert!(!fx.enter_into_popup && fx.draft.is_empty());
    }

    /// A status redraw while the popup is still open is not the popup closing: no Enter yet.
    #[test]
    fn a_status_line_redraw_under_an_open_popup_is_not_its_close() {
        let mut fx = Fixture::new();
        fx.stuck_from_esc = true;
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch model", "/switch a/b", Requires::Any));
        let mut writes = Vec::new();
        let mut stalled = Vec::new();
        let mut t = 0;
        while inj.is_busy() && t < 10_000 {
            if writes.contains(&ESC_KEY) {
                fx.status = STATUS_REDRAWN.to_owned();
            }
            let p = fx.poll(&mut inj, t);
            stalled.extend(p.stalled());
            writes.extend(p.writes);
            t += 10;
        }
        assert_eq!(writes, vec![paste("/switch a/b"), ESC_KEY]);
        assert_eq!(stalled, vec!["Switch model".to_owned()]);
    }

    /// The bug: the Esc and the Enter reached omp back to back (Alt+Enter, a newline under a still
    /// open popup, the command unsent). An Enter only follows once omp is seen reading the Esc.
    #[test]
    fn enter_never_follows_an_escape_omp_has_not_shown() {
        let mut fx = Fixture::new();
        fx.stuck_from_esc = true;
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch model", "/switch a/b", Requires::Any));
        let mut writes = Vec::new();
        let mut stalled = Vec::new();
        let mut t = 0;
        while inj.is_busy() && t < 10_000 {
            let p = fx.poll(&mut inj, t);
            stalled.extend(p.stalled());
            writes.extend(p.writes);
            t += 10;
        }
        assert_eq!(writes, vec![paste("/switch a/b"), ESC_KEY]);
        assert_eq!(stalled, vec!["Switch model".to_owned()]);
        // The Esc arrives late after all: the Enter would have merged with it, none was sent.
        assert!(fx.popup);
    }

    #[test]
    fn a_late_popup_close_still_gets_its_enter() {
        let mut fx = Fixture::new();
        fx.stuck_from_esc = true;
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch model", "/switch a/b", Requires::Any));
        let mut writes = Vec::new();
        let mut esc_at = None;
        for t in (0..5_000).step_by(10) {
            if esc_at.is_some_and(|e| t >= e + 1_200) {
                fx.popup = false; // omp finally read the Esc
                fx.react = true;
            }
            let p = fx.poll(&mut inj, t);
            if p.writes.contains(&ESC_KEY) {
                esc_at = Some(t);
            }
            writes.extend(p.writes.into_iter().map(|w| (t, w)));
        }
        let esc = esc_at.expect("an Esc");
        assert_eq!(
            keys(&writes),
            vec![paste("/switch a/b"), ESC_KEY, ENTER_KEY]
        );
        assert!(at(&writes, &ENTER_KEY) >= esc + 1_200);
    }

    #[test]
    fn a_paste_omp_never_shows_stalls_without_escape_or_enter() {
        let mut fx = Fixture::new();
        fx.react = false;
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        let mut stalled_at = None;
        let mut writes = Vec::new();
        for t in (0..5_000).step_by(10) {
            let p = fx.poll(&mut inj, t);
            if !p.stalled().is_empty() && stalled_at.is_none() {
                stalled_at = Some(t);
            }
            writes.extend(p.writes);
        }
        assert_eq!(writes, vec![paste("/compact")]);
        assert_eq!(stalled_at, Some(SHOW_TIMEOUT.as_millis() as u64));
    }

    #[test]
    fn a_draft_left_by_the_clear_is_never_pasted_onto() {
        let mut fx = Fixture::new();
        fx.draft = "half typed".into();
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        fx.poll(&mut inj, 0);
        inj.answer(true);
        fx.react = false; // the Ctrl+C does not clear it
        let mut writes = Vec::new();
        let mut stalled = Vec::new();
        for t in (0..6_000).step_by(10) {
            let p = fx.poll(&mut inj, t);
            stalled.extend(p.stalled());
            writes.extend(p.writes);
        }
        assert_eq!(writes, vec![CTRL_C_KEY]);
        assert_eq!(stalled, vec!["Compact".to_owned()]);
    }

    #[test]
    fn busy_slash_skips_escape_even_with_a_popup() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch", "/effort low", Requires::Any));
        let w = drain(&mut fx, &mut inj);
        assert_eq!(keys(&w), vec![paste("/effort low"), ENTER_KEY]);
    }

    #[test]
    fn attention_and_unknown_slash_skip_escape_too() {
        for activity in [Activity::Attention, Activity::Unknown] {
            let mut fx = Fixture::new();
            fx.activity = activity;
            let mut inj = Injector::default();
            inj.push(Job::slash("Context", "/context", Requires::Any));
            let w = drain(&mut fx, &mut inj);
            assert_eq!(keys(&w), vec![paste("/context"), ENTER_KEY], "{activity:?}");
        }
    }

    #[test]
    fn draft_asks_then_clears_with_ctrl_c_and_waits() {
        let mut fx = Fixture::new();
        fx.draft = "half typed".into();
        let mut inj = Injector::default();
        let id = inj.push(Job::slash(
            "Compact (soft)",
            "/compact soft",
            Requires::Idle,
        ));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(
            p.confirm,
            Some(Confirm {
                id,
                label: "Compact (soft)".to_owned(),
                draft: "half typed".to_owned()
            })
        );
        assert!(p.writes.is_empty() && inj.awaiting_answer());
        // Nothing happens until the user answers.
        assert!(fx.poll(&mut inj, 5_000).writes.is_empty());
        inj.answer(true);
        let p = fx.poll(&mut inj, 5_000);
        assert_eq!(p.writes, vec![CTRL_C_KEY]);
        assert_eq!(p.wake_in, Some(Duration::from_millis(650)));
        assert!(fx.poll(&mut inj, 5_649).writes.is_empty());
        let p = fx.poll(&mut inj, 5_650);
        assert_eq!(p.writes, vec![paste("/compact soft")]);
    }

    /// A retry: Ctrl+C left the earlier popup on screen and the same command opens it again.
    #[test]
    fn retry_over_a_stale_popup_submits() {
        let mut fx = Fixture::new();
        fx.draft = "/switch a/b".into();
        fx.popup = true;
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch model", "/switch a/b", Requires::Any));
        fx.poll(&mut inj, 0);
        inj.answer(true);
        let mut writes = Vec::new();
        let mut stalled = Vec::new();
        for t in (0..5_000).step_by(10) {
            let p = fx.poll(&mut inj, t);
            stalled.extend(p.stalled());
            writes.extend(p.writes);
        }
        assert!(stalled.is_empty());
        assert_eq!(writes.first(), Some(&CTRL_C_KEY));
        assert_eq!(writes.last(), Some(&ENTER_KEY));
        assert!(fx.draft.is_empty());
    }

    #[test]
    fn multiline_draft_with_cursor_on_empty_last_line_asks() {
        let mut fx = Fixture::new();
        fx.fixed = Some(ScreenText {
            lines: vec!["".into(), "▎ one".into(), "▎ two".into(), "▎".into()],
            cursor_row: 3,
            cursor_col: 2,
        });
        let mut inj = Injector::default();
        inj.push(Job::slash("Handoff", "/handoff", Requires::Idle));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.confirm.map(|c| c.draft), Some("one ↵ two".to_owned()));
    }

    #[test]
    fn declined_draft_drops_the_job() {
        let mut fx = Fixture::new();
        fx.draft = "hello".into();
        let mut inj = Injector::default();
        inj.push(Job::slash("Handoff", "/handoff", Requires::Idle));
        fx.poll(&mut inj, 0);
        inj.answer(false);
        assert!(!inj.is_busy());
        assert!(fx.poll(&mut inj, 10).writes.is_empty());
    }

    #[test]
    fn overlay_refuses_slash_but_not_hotkeys() {
        let mut fx = Fixture::new();
        fx.fixed = Some(overlay());
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        inj.push(plan_job("Plan mode"));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.dialog_open(), vec!["Compact".to_owned()]);
        // The refusal costs no pause: the hotkey runs in the same poll.
        assert_eq!(p.writes, vec![Write::Key(Key::Char('p'), ALT_SHIFT)]);
    }

    #[test]
    fn ctrl_c_respects_users_recent_ctrl_c() {
        let mut fx = Fixture::new();
        fx.draft = "draft".into();
        fx.last_ctrl_c = Some(fx.base + Duration::from_millis(100));
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Any));
        fx.poll(&mut inj, 200);
        inj.answer(true);
        let p = fx.poll(&mut inj, 200);
        assert!(p.writes.is_empty());
        assert_eq!(p.wake_in, Some(Duration::from_millis(550)));
        let p = fx.poll(&mut inj, 750);
        assert_eq!(p.writes, vec![CTRL_C_KEY]);
    }

    #[test]
    fn idle_only_needs_activity_idle_exactly() {
        for activity in [Activity::Busy, Activity::Attention, Activity::Unknown] {
            let mut fx = Fixture::new();
            fx.activity = activity;
            let mut inj = Injector::default();
            inj.push(Job::slash("Compact", "/compact", Requires::Idle));
            let p = fx.poll(&mut inj, 0);
            assert_eq!(
                p.refused(),
                vec![("Compact".to_owned(), Requires::Idle)],
                "{activity:?}"
            );
            assert!(p.writes.is_empty());
        }
    }

    #[test]
    fn busy_job_refused_while_not_busy() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::stop());
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.refused(), vec![("Stop".to_owned(), Requires::Busy)]);
        fx.activity = Activity::Busy;
        inj.push(Job::stop());
        assert_eq!(fx.poll(&mut inj, 1_000).writes, vec![ESC_KEY]);
    }

    #[test]
    fn leaving_idle_before_the_enter_aborts_without_more_bytes() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.writes.len(), 1, "paste goes out");
        // A queued message starts a turn while the paste waits: no Esc (would interrupt) and no Enter.
        fx.activity = Activity::Busy;
        let p = fx.poll(&mut inj, 150);
        assert!(p.writes.is_empty());
        assert_eq!(p.aborted(), vec!["Compact".to_owned()]);
        assert!(!inj.is_busy());
        assert!(drain(&mut fx, &mut inj).is_empty());
    }

    #[test]
    fn leaving_idle_between_escape_and_enter_aborts() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Context", "/context", Requires::Any));
        let mut t = 0;
        while !fx.poll(&mut inj, t).writes.contains(&ESC_KEY) {
            t += 10;
            assert!(t < 5_000, "no Esc");
        }
        fx.activity = Activity::Attention;
        let p = fx.poll(&mut inj, t + 300);
        assert!(p.writes.is_empty());
        assert_eq!(p.aborted(), vec!["Context".to_owned()]);
    }

    #[test]
    fn a_job_that_started_busy_runs_to_its_enter() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::slash("Switch", "/switch a/b", Requires::Any));
        fx.poll(&mut inj, 0);
        fx.activity = Activity::Idle; // the turn ended meanwhile: not an abort
        let mut writes = Vec::new();
        for t in (10..3_000).step_by(10) {
            let p = fx.poll(&mut inj, t);
            assert!(p.aborted().is_empty());
            writes.extend(p.writes);
        }
        // Idle now, so the popup Esc is safe.
        assert_eq!(writes, vec![ESC_KEY, ENTER_KEY]);
    }

    #[test]
    fn jobs_are_serialized_with_a_pause() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(plan_job("Plan"));
        inj.push(Job::key("Hub", Key::Char('a'), Mods::ALT, Requires::Any));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.writes, vec![Write::Key(Key::Char('p'), ALT_SHIFT)]);
        assert_eq!(p.wake_in, Some(Duration::from_millis(300)));
        assert!(fx.poll(&mut inj, 299).writes.is_empty());
        assert_eq!(
            fx.poll(&mut inj, 300).writes,
            vec![Write::Key(Key::Char('a'), Mods::ALT)]
        );
    }

    #[test]
    fn stop_sends_one_escape_then_a_second_only_if_still_busy() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::stop());
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.writes, vec![ESC_KEY]);
        assert_eq!(p.wake_in, Some(Duration::from_millis(650)));
        assert!(fx.poll(&mut inj, 649).writes.is_empty());
        assert_eq!(fx.poll(&mut inj, 650).writes, vec![ESC_KEY]);

        // The first Esc worked: no second one.
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::stop());
        fx.poll(&mut inj, 0);
        fx.activity = Activity::Idle;
        let p = fx.poll(&mut inj, 650);
        assert!(p.writes.is_empty() && p.aborted().is_empty());
        assert!(!inj.is_busy());
    }

    #[test]
    fn no_byte_follows_a_lone_escape_within_100ms() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::stop());
        inj.push(plan_job("Plan"));
        let w = drain(&mut fx, &mut inj);
        assert_eq!(w.len(), 3);
        let gap = |a: usize, b: usize| w[b].0 - w[a].0;
        assert!(gap(0, 1) >= 650 && gap(1, 2) >= 300);
    }

    #[test]
    fn user_input_is_held_while_a_started_job_waits() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Context", "/context", Requires::Any));
        let mut t = 0;
        let mut hold_until = 0;
        while inj.is_busy() {
            let p = fx.poll(&mut inj, t);
            let next = t + p.wake_in.map_or(0, |d| d.as_millis() as u64);
            if inj.is_busy() {
                // The hold always reaches past the next step.
                let h = p.hold_for.expect("hold while the job runs");
                assert!(t + h.as_millis() as u64 > next, "t={t}");
            }
            if let Some(h) = p.hold_for {
                hold_until = t + h.as_millis() as u64;
            }
            t = next.max(t + 10);
            assert!(t < 5_000);
        }
        let last = t - 10;
        assert!(hold_until >= last + HOLD_TAIL.as_millis() as u64 - 10);
        assert_eq!(fx.poll(&mut inj, t + 500).hold_for, None);
    }

    #[test]
    fn no_hold_while_waiting_for_the_users_answer() {
        let mut fx = Fixture::new();
        fx.draft = "draft".into();
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Any));
        assert_eq!(fx.poll(&mut inj, 0).hold_for, None);
        inj.answer(true);
        // Ctrl+C goes out now: held through the 650 ms wait plus the tail.
        let p = fx.poll(&mut inj, 10);
        assert_eq!(p.hold_for, Some(Duration::from_millis(800)));
    }

    #[test]
    fn closing_drops_unstarted_work_and_waits_for_a_started_job() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Context", "/context", Requires::Any));
        inj.push(plan_job("Plan"));
        assert!(inj.drop_unstarted(), "nothing written yet");
        assert!(!inj.is_busy());

        inj.push(Job::slash("Context", "/context", Requires::Any));
        inj.push(plan_job("Plan"));
        fx.poll(&mut inj, 0); // paste written
        assert!(!inj.drop_unstarted(), "mid-job: wait for the boundary");
        // The queued hotkey is gone; the running job finishes, then the injector is idle.
        let w = drain(&mut fx, &mut inj);
        assert_eq!(w.last().map(|(_, b)| b.clone()), Some(ENTER_KEY));
        assert!(inj.drop_unstarted());
    }

    #[test]
    fn closing_waits_for_the_tail_after_the_last_byte() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        let w = drain(&mut fx, &mut inj);
        let Some((at, last)) = w.last().cloned() else {
            panic!("no writes");
        };
        assert_eq!(last, ENTER_KEY);
        let t = |ms: u64| fx.base + Duration::from_millis(ms);
        assert!(!inj.can_close(t(at)), "the Enter was just written");
        assert!(!inj.can_close(t(at + 149)));
        assert!(inj.can_close(t(at + 150)));
    }

    #[test]
    fn titles_cannot_break_out_of_the_bracketed_paste() {
        let job = Job::slash(
            "Rename",
            "/rename a\x1b[201~\r/quit\u{7f}b\u{9b}c\nd\te",
            Requires::Any,
        );
        assert_eq!(
            job.payload,
            Payload::Slash("/rename a[201~ /quitbc d e".to_owned())
        );
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(job);
        let w = drain(&mut fx, &mut inj);
        let Write::Bytes(pasted) = &w[0].1 else {
            panic!("the paste comes first: {w:?}");
        };
        assert!(pasted.starts_with(PASTE_START) && pasted.ends_with(PASTE_END));
        let inner = &pasted[PASTE_START.len()..pasted.len() - PASTE_END.len()];
        assert!(inner.iter().all(|b| *b >= 0x20 && *b != 0x7f), "{inner:?}");
    }

    #[test]
    fn sanitize_collapses_whitespace_and_drops_controls() {
        assert_eq!(sanitize_line("  a \n\n b\u{85}c\u{1b}d "), "a b cd");
        assert_eq!(sanitize_line("\u{1b}\u{7f}"), "");
        assert_eq!(sanitize_line("naïve → ok"), "naïve → ok");
    }

    // ---- Payload::Text (review comments)

    const REVIEW: &str = "Review:\n1. a.rs:3\n> x\nfix\tthis";

    #[test]
    fn text_is_sanitized_keeping_newlines_and_tabs() {
        let job = Job::text("Send review", "a\nb\tc\u{1b}[201~\r\n\u{7f}d\u{85}e", true);
        assert_eq!(
            job.payload,
            Payload::Text {
                attach: None,
                text: "a\nb\tc[201~\nde".to_owned(),
                mode: ComposeMode::Send,
            }
        );
        assert_eq!(job.requires, Requires::Idle);
        assert_eq!(Job::text("Insert", "x", false).requires, Requires::Any);
    }

    #[test]
    fn insert_pastes_once_without_clearing_confirming_or_submitting() {
        let mut fx = Fixture::new();
        fx.draft = "half-written draft".into();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::text("Insert review", REVIEW, false));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.confirm, None);
        assert_eq!(p.writes, vec![paste(REVIEW)]);
        assert_eq!(p.completed(), vec!["Insert review".to_owned()]);
        assert!(!inj.is_busy());
    }

    #[test]
    fn insert_is_refused_without_a_composer_and_costs_no_bytes() {
        let mut fx = Fixture::new();
        fx.fixed = Some(overlay());
        let mut inj = Injector::default();
        inj.push(Job::text("Insert review", REVIEW, false));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.dialog_open(), vec!["Insert review".to_owned()]);
        assert!(p.writes.is_empty() && p.completed().is_empty());
    }

    #[test]
    fn text_without_bracketed_paste_is_refused_with_a_note() {
        for submit in [false, true] {
            let mut fx = Fixture::new();
            fx.paste_ok = false;
            let mut inj = Injector::default();
            inj.push(Job::text("Review", REVIEW, submit));
            let p = fx.poll(&mut inj, 0);
            assert_eq!(p.no_paste(), vec!["Review".to_owned()], "submit={submit}");
            assert!(p.writes.is_empty() && !inj.is_busy());
        }
    }

    #[test]
    fn send_pastes_then_enters_and_reports_completion() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::text("Send review", REVIEW, true));
        let mut writes = Vec::new();
        let mut done = Vec::new();
        for t in (0..3_000).step_by(10) {
            let p = fx.poll(&mut inj, t);
            done.extend(p.completed());
            writes.extend(p.writes);
        }
        assert_eq!(writes, vec![paste(REVIEW), ENTER_KEY]);
        assert_eq!(done, vec!["Send review".to_owned()]);
        assert!(fx.draft.is_empty());
    }

    #[test]
    fn send_needs_idle() {
        for activity in [Activity::Busy, Activity::Attention, Activity::Unknown] {
            let mut fx = Fixture::new();
            fx.activity = activity;
            let mut inj = Injector::default();
            inj.push(Job::text("Send review", REVIEW, true));
            let p = fx.poll(&mut inj, 0);
            assert_eq!(
                p.refused(),
                vec![("Send review".to_owned(), Requires::Idle)],
                "{activity:?}"
            );
            assert!(p.writes.is_empty());
        }
    }

    #[test]
    fn send_confirms_a_draft_then_clears_it_with_ctrl_c_before_pasting() {
        let mut fx = Fixture::new();
        fx.draft = "unsent words".into();
        let mut inj = Injector::default();
        inj.push(Job::text("Send review", REVIEW, true));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.confirm.map(|c| c.draft), Some("unsent words".to_owned()));
        assert!(p.writes.is_empty());
        inj.answer(true);
        let mut writes = Vec::new();
        for t in (0..3_000).step_by(10) {
            writes.extend(fx.poll(&mut inj, t).writes);
        }
        assert_eq!(writes, vec![CTRL_C_KEY, paste(REVIEW), ENTER_KEY]);
    }

    #[test]
    fn declining_the_draft_confirmation_drops_the_send() {
        let mut fx = Fixture::new();
        fx.draft = "unsent words".into();
        let mut inj = Injector::default();
        inj.push(Job::text("Send review", REVIEW, true));
        fx.poll(&mut inj, 0);
        inj.answer(false);
        assert!(!inj.is_busy());
        let p = fx.poll(&mut inj, 100);
        assert!(p.writes.is_empty() && p.completed().is_empty());
    }

    #[test]
    fn send_is_abandoned_when_omp_starts_working_mid_sequence() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::text("Send review", REVIEW, true));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.writes, vec![paste(REVIEW)]);
        fx.activity = Activity::Busy;
        let p = fx.poll(&mut inj, 200);
        assert_eq!(p.aborted(), vec!["Send review".to_owned()]);
        assert!(p.writes.is_empty() && p.completed().is_empty());
    }

    // ---- Payload::Text with ComposeMode (steer, follow-up, attachments)

    const ATTACH: &str = "/data/attachments/dev/0123456789abcdef-shot.png";

    fn drain_ended(fx: &mut Fixture, inj: &mut Injector) -> (Vec<Write>, Vec<(String, End)>) {
        let mut writes = Vec::new();
        let mut ended = Vec::new();
        for t in (0..5_000).step_by(10) {
            let p = fx.poll(inj, t);
            writes.extend(p.writes);
            ended.extend(p.ended.into_iter().map(|e| (e.label, e.end)));
        }
        (writes, ended)
    }

    #[test]
    fn compose_modes_require_the_right_activity() {
        assert_eq!(
            Job::compose("s", None, "x", ComposeMode::Steer).requires,
            Requires::Busy
        );
        assert_eq!(
            Job::compose("f", None, "x", ComposeMode::FollowUp).requires,
            Requires::Busy
        );
        assert_eq!(
            Job::compose("p", None, "x", ComposeMode::Send).requires,
            Requires::Idle
        );
        assert_eq!(
            Job::compose("i", None, "x", ComposeMode::Insert).requires,
            Requires::Any
        );
        // Control characters in the paths cannot end the paste early either.
        let job = Job::compose("s", Some("/a\u{1b}[201~b"), "x", ComposeMode::Send);
        assert!(matches!(job.payload, Payload::Text { attach: Some(a), .. } if a == "/a[201~b"));
    }

    #[test]
    fn steer_pastes_then_enters_while_omp_works_and_never_escapes() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::compose("Steer", None, "go left", ComposeMode::Steer));
        let (writes, ended) = drain_ended(&mut fx, &mut inj);
        assert_eq!(writes, vec![paste("go left"), ENTER_KEY]);
        assert_eq!(ended, vec![("Steer".to_owned(), End::Completed)]);
    }

    #[test]
    fn follow_up_pastes_then_sends_ctrl_q() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::compose(
            "Follow-up",
            None,
            "then tests",
            ComposeMode::FollowUp,
        ));
        let (writes, _) = drain_ended(&mut fx, &mut inj);
        assert_eq!(writes, vec![paste("then tests"), FOLLOW_UP_KEY]);
        assert!(!writes.contains(&ENTER_KEY) && !writes.contains(&ESC_KEY));
    }

    #[test]
    fn steer_is_refused_while_idle() {
        let mut fx = Fixture::new();
        let mut inj = Injector::default();
        inj.push(Job::compose("Steer", None, "x", ComposeMode::Steer));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.refused(), vec![("Steer".to_owned(), Requires::Busy)]);
        assert!(p.writes.is_empty());
    }

    /// The turn ended between the paste and the submit: an Enter now would send the text as a
    /// new prompt instead of steering, so nothing follows the paste.
    #[test]
    fn steer_is_abandoned_when_the_turn_ends_before_the_submit() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::compose("Steer", None, "go left", ComposeMode::Steer));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.writes, vec![paste("go left")]);
        fx.activity = Activity::Idle;
        let p = fx.poll(&mut inj, 100);
        assert_eq!(p.aborted(), vec!["Steer".to_owned()]);
        assert!(p.writes.is_empty());
    }

    /// A draft is confirmed and cleared with Ctrl+C (safe mid-turn), never with Esc.
    #[test]
    fn steer_clears_a_confirmed_draft_with_ctrl_c_only() {
        let mut fx = Fixture::new();
        fx.activity = Activity::Busy;
        fx.draft = "unsent".into();
        let mut inj = Injector::default();
        inj.push(Job::compose("Steer", None, "go left", ComposeMode::Steer));
        let p = fx.poll(&mut inj, 0);
        assert_eq!(p.confirm.map(|c| c.draft), Some("unsent".to_owned()));
        inj.answer(true);
        let (writes, _) = drain_ended(&mut fx, &mut inj);
        assert_eq!(writes, vec![CTRL_C_KEY, paste("go left"), ENTER_KEY]);
    }

    #[test]
    fn attachments_are_their_own_paste_before_the_text() {
        let mut fx = Fixture::new();
        fx.paste_opens_popup = false;
        let mut inj = Injector::default();
        inj.push(Job::compose(
            "Send prompt",
            Some(ATTACH),
            "what is this?",
            ComposeMode::Send,
        ));
        let (writes, ended) = drain_ended(&mut fx, &mut inj);
        assert_eq!(
            writes,
            vec![paste(ATTACH), paste("what is this?"), ENTER_KEY]
        );
        assert_eq!(ended, vec![("Send prompt".to_owned(), End::Completed)]);
    }

    #[test]
    fn attachments_alone_are_submitted_too() {
        let mut fx = Fixture::new();
        fx.paste_opens_popup = false;
        let mut inj = Injector::default();
        inj.push(Job::compose(
            "Send prompt",
            Some(ATTACH),
            "",
            ComposeMode::Send,
        ));
        let (writes, _) = drain_ended(&mut fx, &mut inj);
        assert_eq!(writes, vec![paste(ATTACH), ENTER_KEY]);
    }

    #[test]
    fn insert_with_attachments_keeps_the_draft_and_never_submits() {
        let mut fx = Fixture::new();
        fx.paste_opens_popup = false;
        fx.activity = Activity::Busy;
        fx.draft = "draft ".into();
        let mut inj = Injector::default();
        inj.push(Job::compose(
            "Insert",
            Some(ATTACH),
            "look",
            ComposeMode::Insert,
        ));
        let (writes, ended) = drain_ended(&mut fx, &mut inj);
        assert_eq!(writes, vec![paste(ATTACH), paste("look")]);
        assert_eq!(ended, vec![("Insert".to_owned(), End::Completed)]);
        assert_eq!(fx.draft, format!("draft {ATTACH}look"));
    }

    #[test]
    fn an_attachment_omp_never_shows_stalls_before_the_text() {
        let mut fx = Fixture::new();
        fx.paste_opens_popup = false;
        fx.react = false;
        let mut inj = Injector::default();
        inj.push(Job::compose(
            "Send prompt",
            Some(ATTACH),
            "x",
            ComposeMode::Send,
        ));
        let (writes, ended) = drain_ended(&mut fx, &mut inj);
        assert_eq!(writes, vec![paste(ATTACH)]);
        assert_eq!(ended, vec![("Send prompt".to_owned(), End::Stalled)]);
    }

    /// A scripted omp that speaks TSP: its composer is only in the document (the grid is blank).
    struct TspFixture {
        base: Instant,
        activity: Activity,
        /// `None` = no composer to address (an ask or dialog took it).
        text: Option<String>,
        can_send: bool,
        chips: usize,
        popup: bool,
        /// The next paste is an attachment drop: it adds a chip, not text.
        paste_is_chip: bool,
        sent: Vec<String>,
        submitted: Vec<String>,
        ctrl_c: bool,
        /// omp's recall history: a `send` over a draft displaces it here; Ctrl+C never does.
        recall: Vec<String>,
        /// Ctrl+C does not clear the editor.
        stuck: bool,
    }

    impl TspFixture {
        fn new() -> TspFixture {
            TspFixture {
                base: Instant::now(),
                activity: Activity::Idle,
                text: Some(String::new()),
                can_send: true,
                chips: 0,
                popup: false,
                paste_is_chip: false,
                sent: Vec::new(),
                submitted: Vec::new(),
                ctrl_c: false,
                recall: Vec::new(),
                stuck: false,
            }
        }

        fn composer(&self) -> TspComposer {
            let state = match &self.text {
                None => Composer::Missing,
                Some(t) if t.trim().is_empty() && self.chips > 0 => {
                    Composer::Draft(format!("[{} attachment(s)]", self.chips))
                }
                Some(t) if t.trim().is_empty() => Composer::Empty,
                Some(t) => Composer::Draft(t.clone()),
            };
            TspComposer {
                target: self
                    .text
                    .as_ref()
                    .map(|_| ("s:1".to_owned(), "3.line/input".to_owned())),
                state,
                can_send: self.can_send,
                chips: self.chips,
                popup: self.popup,
            }
        }

        fn poll(&mut self, inj: &mut Injector, ms: u64) -> Poll {
            let c = self.composer();
            let tsp = move || c.clone();
            let p = inj.poll(&Env {
                now: self.base + Duration::from_millis(ms),
                activity: self.activity,
                last_ctrl_c: None,
                screen: &|| ScreenText::default(),
                paste: &|t| Some([PASTE_START, t.as_bytes(), PASTE_END].concat()),
                tsp: Some(&tsp),
            });
            for w in &p.writes {
                self.apply(w);
            }
            p
        }

        fn apply(&mut self, w: &Write) {
            let text = self.text.get_or_insert_with(String::new);
            match w {
                Write::Bytes(b) => {
                    let inner = &b[PASTE_START.len()..b.len() - PASTE_END.len()];
                    if self.paste_is_chip {
                        self.chips += 1;
                        self.paste_is_chip = false;
                    } else {
                        text.push_str(&String::from_utf8_lossy(inner));
                        self.popup = text.starts_with('/');
                    }
                }
                Write::Send { text: t, .. } => {
                    // Like omp: a draft the send finds is displaced into recall, the text is its
                    // own prompt.
                    if !text.trim().is_empty() {
                        self.recall.push(text.clone());
                    }
                    self.sent.push(t.clone());
                    text.clear();
                    self.chips = 0;
                }
                Write::Key(Key::Enter, _) => {
                    self.submitted.push(text.clone());
                    text.clear();
                    self.chips = 0;
                    self.popup = false;
                }
                Write::Key(Key::Escape, _) => self.popup = false,
                Write::Key(Key::Char('c'), m) if m.ctrl => {
                    self.ctrl_c = true;
                    if !self.stuck {
                        text.clear();
                        self.chips = 0;
                    }
                }
                Write::Key(..) => {}
            }
        }

        /// Polls every 10 ms until the injector is idle; every write and every ended job.
        fn drain(&mut self, inj: &mut Injector) -> (Vec<Write>, Vec<(String, End)>) {
            let (mut writes, mut ended) = (Vec::new(), Vec::new());
            let mut t = 0;
            while inj.is_busy() && t < 10_000 {
                let p = self.poll(inj, t);
                writes.extend(p.writes);
                ended.extend(p.ended.into_iter().map(|e| (e.label, e.end)));
                t += 10;
            }
            (writes, ended)
        }
    }

    fn send_event(text: &str) -> Write {
        Write::Send {
            surface: "s:1".to_owned(),
            editor: "3.line/input".to_owned(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn tsp_send_is_one_event_into_the_editor_not_a_paste() {
        let mut fx = TspFixture::new();
        let mut inj = Injector::default();
        inj.push(Job::slash("Rename", "/rename new title", Requires::Idle));
        inj.push(Job::text("Prompt", "Reply with ok.", true));
        let (writes, ended) = fx.drain(&mut inj);
        assert_eq!(
            writes,
            vec![
                send_event("/rename new title"),
                send_event("Reply with ok.")
            ]
        );
        assert_eq!(
            ended,
            vec![
                ("Rename".to_owned(), End::Completed),
                ("Prompt".to_owned(), End::Completed)
            ]
        );
        assert!(!fx.ctrl_c);
    }

    #[test]
    fn tsp_draft_asks_first_and_a_clear_answer_clears_with_ctrl_c_then_sends() {
        let mut fx = TspFixture::new();
        fx.text = Some("half typed".to_owned());
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        let first = fx.poll(&mut inj, 0);
        assert!(first.writes.is_empty());
        assert_eq!(
            first.confirm.map(|c| c.draft),
            Some("half typed".to_owned())
        );
        inj.answer(true);
        let (writes, ended) = fx.drain(&mut inj);
        assert_eq!(writes, vec![CTRL_C_KEY, send_event("/compact")]);
        assert_eq!(ended, vec![("Compact".to_owned(), End::Completed)]);
        assert!(
            fx.recall.is_empty(),
            "the draft was cleared, not displaced into recall: {:?}",
            fx.recall
        );
        // Declining drops the job and leaves the draft alone.
        let mut fx = TspFixture::new();
        fx.text = Some("half typed".to_owned());
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        fx.poll(&mut inj, 0);
        inj.answer(false);
        let (writes, _) = fx.drain(&mut inj);
        assert!(writes.is_empty());
        assert_eq!(fx.text.as_deref(), Some("half typed"));
    }

    #[test]
    fn tsp_send_never_goes_over_a_draft_ctrl_c_did_not_clear() {
        let mut fx = TspFixture::new();
        fx.text = Some("half typed".to_owned());
        fx.stuck = true;
        let mut inj = Injector::default();
        inj.push(Job::text("Prompt", "Reply with ok.", true));
        fx.poll(&mut inj, 0);
        inj.answer(true);
        let (writes, ended) = fx.drain(&mut inj);
        assert_eq!(writes, vec![CTRL_C_KEY], "no send over the draft");
        assert_eq!(ended, vec![("Prompt".to_owned(), End::Stalled)]);
        assert!(fx.recall.is_empty() && fx.sent.is_empty());
    }

    #[test]
    fn tsp_job_is_refused_while_an_ask_holds_the_composer() {
        let mut fx = TspFixture::new();
        fx.text = None;
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        inj.push(Job::text("Note", "text", false));
        let (writes, ended) = fx.drain(&mut inj);
        assert!(writes.is_empty());
        assert_eq!(
            ended,
            vec![
                ("Compact".to_owned(), End::DialogOpen),
                ("Note".to_owned(), End::DialogOpen)
            ]
        );
    }

    #[test]
    fn tsp_ask_opening_after_the_confirmation_drops_the_send_unwritten() {
        let mut fx = TspFixture::new();
        fx.text = Some("draft".to_owned());
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        fx.poll(&mut inj, 0);
        fx.text = None;
        inj.answer(true);
        let (writes, ended) = fx.drain(&mut inj);
        assert!(writes.is_empty());
        assert_eq!(ended, vec![("Compact".to_owned(), End::DialogOpen)]);
    }

    #[test]
    fn tsp_busy_refuses_idle_jobs_and_idle_refuses_steering() {
        let mut fx = TspFixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::text("Prompt", "hi", true));
        let (writes, ended) = fx.drain(&mut inj);
        assert!(writes.is_empty());
        assert_eq!(
            ended,
            vec![("Prompt".to_owned(), End::Refused(Requires::Idle))]
        );
        fx.activity = Activity::Idle;
        inj.push(Job::compose("Steer", None, "now", ComposeMode::Steer));
        let (writes, ended) = fx.drain(&mut inj);
        assert!(writes.is_empty());
        assert_eq!(
            ended,
            vec![("Steer".to_owned(), End::Refused(Requires::Busy))]
        );
    }

    #[test]
    fn tsp_insert_pastes_at_the_cursor_and_keeps_the_draft() {
        let mut fx = TspFixture::new();
        fx.text = Some("mine ".to_owned());
        let mut inj = Injector::default();
        inj.push(Job::text("Insert", "added", false));
        let (writes, ended) = fx.drain(&mut inj);
        assert_eq!(writes, vec![paste("added")]);
        assert_eq!(ended, vec![("Insert".to_owned(), End::Completed)]);
        assert_eq!(fx.text.as_deref(), Some("mine added"));
        assert!(fx.sent.is_empty() && fx.submitted.is_empty());
    }

    #[test]
    fn tsp_steering_pastes_waits_for_the_document_then_presses_the_key() {
        let mut fx = TspFixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::compose(
            "Steer",
            None,
            "use sqlite",
            ComposeMode::Steer,
        ));
        inj.push(Job::compose(
            "Later",
            None,
            "and tests",
            ComposeMode::FollowUp,
        ));
        let (writes, ended) = fx.drain(&mut inj);
        assert_eq!(
            writes,
            vec![
                paste("use sqlite"),
                ENTER_KEY,
                paste("and tests"),
                FOLLOW_UP_KEY
            ]
        );
        assert_eq!(ended.len(), 2);
        assert!(ended.iter().all(|(_, e)| *e == End::Completed));
    }

    #[test]
    fn tsp_without_send_in_the_hello_falls_back_to_paste_and_enter() {
        let mut fx = TspFixture::new();
        fx.can_send = false;
        let mut inj = Injector::default();
        inj.push(Job::slash("Rename", "/rename x", Requires::Idle));
        let (writes, _) = fx.drain(&mut inj);
        // The document's autocomplete popup is closed with Esc before the Enter, as on the grid.
        assert_eq!(writes, vec![paste("/rename x"), ESC_KEY, ENTER_KEY]);
        assert_eq!(fx.submitted, vec!["/rename x".to_owned()]);
    }

    #[test]
    fn tsp_attachments_are_pasted_first_and_shown_as_chips_before_the_text() {
        let mut fx = TspFixture::new();
        fx.paste_is_chip = true;
        let mut inj = Injector::default();
        inj.push(Job::compose(
            "Prompt",
            Some("'/tmp/a.png'"),
            "look at this",
            ComposeMode::Send,
        ));
        let (writes, ended) = fx.drain(&mut inj);
        assert_eq!(
            writes,
            vec![paste("'/tmp/a.png'"), paste("look at this"), ENTER_KEY]
        );
        assert_eq!(ended, vec![("Prompt".to_owned(), End::Completed)]);
        assert_eq!(fx.submitted, vec!["look at this".to_owned()]);
        assert!(fx.sent.is_empty(), "a send would drop the attachments");
    }

    #[test]
    fn tsp_a_send_asks_before_it_goes_over_attachments_alone() {
        let mut fx = TspFixture::new();
        fx.chips = 2;
        let mut inj = Injector::default();
        inj.push(Job::slash("Compact", "/compact", Requires::Idle));
        let first = fx.poll(&mut inj, 0);
        assert!(
            first.writes.is_empty(),
            "nothing is written before the answer"
        );
        assert_eq!(
            first.confirm.map(|c| c.draft),
            Some("[2 attachment(s)]".to_owned())
        );
        inj.answer(true);
        let (writes, ended) = fx.drain(&mut inj);
        assert_eq!(writes, vec![CTRL_C_KEY, send_event("/compact")]);
        assert_eq!(ended, vec![("Compact".to_owned(), End::Completed)]);
        assert_eq!(fx.chips, 0);
    }

    #[test]
    fn tsp_stop_still_uses_escape_and_keys_pass_through() {
        let mut fx = TspFixture::new();
        fx.activity = Activity::Busy;
        let mut inj = Injector::default();
        inj.push(Job::stop());
        let (writes, _) = fx.drain(&mut inj);
        assert_eq!(writes, vec![ESC_KEY, ESC_KEY]);
        let mut inj = Injector::default();
        inj.push(plan_job("Plan"));
        let (writes, _) = fx.drain(&mut inj);
        assert_eq!(writes, vec![Write::Key(Key::Char('p'), ALT_SHIFT)]);
    }
}
