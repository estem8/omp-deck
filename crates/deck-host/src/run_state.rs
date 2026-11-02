//! omp's run status as the Program Status Protocol (OSC 7501) reports it.
//!
//! omp writes `ESC ] 7501 ; state=<state>[:kind=<kind>]:app=omp[:msg=<base64>] ESC \` on every
//! transition of its run status (in classic and native/TSP mode alike; setting
//! `terminal.programStatus`, default on) and `state=clear` when it removes its record. Unlike
//! the `π > label` title, which omp drops under TSP, this channel exists in both modes.
//!
//! alacritty ignores the sequence, so every terminal that wants the state scans it beside its
//! parser: [`RunStateScanner`] for raw byte streams (split across reads), [`parse_osc`] for an
//! already tokenised OSC (the host's VT side scanner). [`encode_report`] writes the report back
//! for snapshot replays, so a late joiner learns the state the session is in.

use base64::Engine as _;
use base64::alphabet::STANDARD as STANDARD_ALPHABET;
use base64::engine::general_purpose::STANDARD;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use deck_tsp::document::Document;
use serde::{Deserialize, Serialize};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
/// Bytes of an OSC string after `7501;`: the encoded `msg` limit (2732) plus the other fields,
/// with room to spare. A longer string is dropped whole.
const MAX_BODY: usize = 4096;
/// The protocol's limit of the base64 text of `msg`.
const MAX_MSG_ENCODED: usize = 2732;
/// What follows `ESC ]` of a run-status report.
const PREFIX: &[u8] = b"7501;";

/// What omp's run status says it is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    /// At the prompt waiting for an instruction (also after the user interrupted a run).
    Idle,
    /// Running a turn.
    Working,
    /// Finished a turn whose result the user has not looked at yet.
    Done,
    /// The turn failed; the report's `msg` says why.
    Error,
    /// Waiting for the user.
    Blocked {
        #[serde(default)]
        kind: Option<BlockedKind>,
    },
}

/// What a blocked run waits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedKind {
    /// An approval prompt.
    Permission,
    /// An `ask` question.
    Question,
}

/// The run status a terminal holds: the last report that was not cleared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    pub state: RunPhase,
    /// Why the run is blocked or failed (`blocked` and `error` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
}

impl RunState {
    pub fn new(state: RunPhase) -> RunState {
        RunState { state, msg: None }
    }
}

/// One complete OSC 7501 report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// The terminal's record is now this.
    Set(RunState),
    /// `state=clear`: no record any more.
    Clear,
}

impl Report {
    /// Applies the report to the held state.
    pub fn apply(self, held: &mut Option<RunState>) {
        *held = match self {
            Report::Set(state) => Some(state),
            Report::Clear => None,
        };
    }
}

/// Standard alphabet; omp pads, other writers may not.
const MSG_CODEC: GeneralPurpose = GeneralPurpose::new(
    &STANDARD_ALPHABET,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Parses the string after `OSC 7501 ;` (`state=working:app=omp`). `None` when the report is
/// malformed (no or unknown state, a `msg` that is not base64 of control-free UTF-8 within the
/// size limit — the protocol discards the whole report then) or is not about the root record
/// omp writes (it carries an `id`).
pub fn parse_report(body: &[u8]) -> Option<Report> {
    let mut state = None;
    let mut kind = None;
    let mut msg = None;
    for field in body.split(|&b| b == b':') {
        let Some(eq) = field.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (key, value) = (&field[..eq], &field[eq + 1..]);
        match key {
            b"state" => state = Some(value),
            b"kind" => kind = Some(value),
            b"msg" => msg = Some(value),
            // A record with an id is not the root record.
            b"id" => return None,
            _ => {}
        }
    }
    let msg = match msg {
        None | Some([]) => None,
        Some(encoded) if encoded.len() > MAX_MSG_ENCODED => return None,
        Some(encoded) => {
            let text = String::from_utf8(MSG_CODEC.decode(encoded).ok()?).ok()?;
            if text.chars().any(char::is_control) {
                return None;
            }
            Some(text).filter(|t| !t.is_empty())
        }
    };
    let phase = match state? {
        b"clear" => return Some(Report::Clear),
        b"idle" => RunPhase::Idle,
        b"working" => RunPhase::Working,
        b"done" => RunPhase::Done,
        b"error" => RunPhase::Error,
        b"blocked" => RunPhase::Blocked {
            kind: match kind {
                Some(b"permission") => Some(BlockedKind::Permission),
                Some(b"question") => Some(BlockedKind::Question),
                _ => None,
            },
        },
        _ => return None,
    };
    let msg = match phase {
        RunPhase::Blocked { .. } | RunPhase::Error => msg,
        _ => None,
    };
    Some(Report::Set(RunState { state: phase, msg }))
}

/// A tokenised OSC (`params` as vte hands them over, split at `;`) that is a run-status report.
pub fn parse_osc(params: &[&[u8]]) -> Option<Report> {
    match params {
        [b"7501", body] => parse_report(body),
        _ => None,
    }
}

/// Appends `state` as the report omp itself would write (replayed to late joiners).
pub fn encode_report(state: &RunState, out: &mut Vec<u8>) {
    out.extend_from_slice(&[ESC, b']']);
    out.extend_from_slice(PREFIX);
    out.extend_from_slice(b"state=");
    let (name, kind) = match state.state {
        RunPhase::Idle => ("idle", None),
        RunPhase::Working => ("working", None),
        RunPhase::Done => ("done", None),
        RunPhase::Error => ("error", None),
        RunPhase::Blocked { kind } => (
            "blocked",
            kind.map(|k| match k {
                BlockedKind::Permission => "permission",
                BlockedKind::Question => "question",
            }),
        ),
    };
    out.extend_from_slice(name.as_bytes());
    if let Some(kind) = kind {
        out.extend_from_slice(b":kind=");
        out.extend_from_slice(kind.as_bytes());
    }
    out.extend_from_slice(b":app=omp");
    if let Some(msg) = &state.msg {
        out.extend_from_slice(b":msg=");
        out.extend_from_slice(STANDARD.encode(msg).as_bytes());
    }
    out.extend_from_slice(&[ESC, b'\\']);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Ground,
    /// After `ESC`.
    Esc,
    /// After `ESC ]` and this many bytes of [`PREFIX`].
    Prefix(u8),
    /// Collecting the report.
    Body,
    /// After an `ESC` inside the report.
    BodyEsc,
}

/// Recogniser of OSC 7501 in a raw byte stream. Keeps its state between reads, so a report
/// split across reads (or terminated by BEL or ST) is found. RIS clears the held status.
#[derive(Debug, Default)]
pub struct RunStateScanner {
    mode: Mode,
    body: Vec<u8>,
    /// The report outgrew [`MAX_BODY`]: dropped at its end.
    overflow: bool,
}

impl RunStateScanner {
    /// Feeds child output; returns the last report completed in it, if any.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<Report> {
        let mut last = None;
        for &b in bytes {
            if let Some(report) = self.step(b) {
                last = Some(report);
            }
        }
        last
    }

    /// Forgets a half-read report.
    pub fn reset(&mut self) {
        *self = RunStateScanner::default();
    }

    fn step(&mut self, b: u8) -> Option<Report> {
        match self.mode {
            Mode::Ground => {
                if b == ESC {
                    self.mode = Mode::Esc;
                }
            }
            Mode::Esc => {
                self.mode = match b {
                    b']' => Mode::Prefix(0),
                    ESC => Mode::Esc,
                    _ => Mode::Ground,
                };
                if b == b'c' {
                    return Some(Report::Clear);
                }
            }
            Mode::Prefix(n) => {
                self.mode = match PREFIX.get(usize::from(n)) {
                    Some(&want) if b == want => {
                        if usize::from(n) + 1 == PREFIX.len() {
                            self.body.clear();
                            self.overflow = false;
                            Mode::Body
                        } else {
                            Mode::Prefix(n + 1)
                        }
                    }
                    _ if b == ESC => Mode::Esc,
                    _ => Mode::Ground,
                };
            }
            Mode::Body => match b {
                BEL => return self.finish(),
                ESC => self.mode = Mode::BodyEsc,
                // CAN and SUB abort the string.
                0x18 | 0x1a => self.mode = Mode::Ground,
                _ if self.body.len() < MAX_BODY => self.body.push(b),
                _ => self.overflow = true,
            },
            Mode::BodyEsc => match b {
                b'\\' => return self.finish(),
                // A new OSC aborts this one.
                b']' => self.mode = Mode::Prefix(0),
                ESC => {}
                _ => self.mode = Mode::Ground,
            },
        }
        None
    }

    fn finish(&mut self) -> Option<Report> {
        self.mode = Mode::Ground;
        if std::mem::take(&mut self.overflow) {
            return None;
        }
        parse_report(&self.body)
    }
}

/// What omp's TSP document shows of its run: the signal for a TSP session that holds neither a
/// run-state report (omp sends none before its first transition) nor a `π` title (dropped under
/// TSP).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TspFacts {
    /// An `ask` or approval stands in the composer's place (dock role `omp.editor.ask*`).
    pub asking: bool,
    /// The dock shows the working row (role `omp.working`): a turn runs.
    pub working: bool,
    /// The dock's composer takes a prompt (an `editor` with `sendable` true).
    pub composer: bool,
}

impl TspFacts {
    /// Reads the open inline surface's dock; `None` without one (no omp session surface yet,
    /// or a full-screen page only). Allocates nothing: the gateway calls it every tick.
    pub fn of(document: &Document) -> Option<TspFacts> {
        let dock = document.inline()?.node("dock")?;
        let mut facts = TspFacts::default();
        facts.visit(dock);
        Some(facts)
    }

    fn visit(&mut self, node: deck_tsp::document::NodeRef<'_>) {
        let role = node.str_prop("role").unwrap_or_default();
        self.asking |= role.starts_with("omp.editor.ask");
        self.working |= role == "omp.working";
        self.composer |= node.kind() == "editor"
            && node.prop("sendable") == Some(&serde_json::Value::Bool(true));
        for child in node.children() {
            self.visit(child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(phase: RunPhase) -> Option<Report> {
        Some(Report::Set(RunState::new(phase)))
    }

    fn scan(chunks: &[&[u8]]) -> Vec<Option<Report>> {
        let mut s = RunStateScanner::default();
        chunks.iter().map(|c| s.feed(c)).collect()
    }

    #[test]
    fn every_state_parses() {
        let cases: [(&[u8], RunPhase); 5] = [
            (b"state=idle:app=omp", RunPhase::Idle),
            (b"state=working:app=omp", RunPhase::Working),
            (b"state=done:app=omp", RunPhase::Done),
            (b"state=error:app=omp", RunPhase::Error),
            (b"state=blocked:app=omp", RunPhase::Blocked { kind: None }),
        ];
        for (body, phase) in cases {
            assert_eq!(parse_report(body), set(phase), "{:?}", body);
        }
        assert_eq!(parse_report(b"state=clear"), Some(Report::Clear));
        assert_eq!(parse_report(b"state=clear:app=omp"), Some(Report::Clear));
    }

    #[test]
    fn blocked_kinds() {
        for (body, kind) in [
            (
                &b"state=blocked:kind=permission:app=omp"[..],
                Some(BlockedKind::Permission),
            ),
            (
                &b"state=blocked:kind=question:app=omp"[..],
                Some(BlockedKind::Question),
            ),
            (&b"state=blocked:kind=other:app=omp"[..], None),
        ] {
            assert_eq!(
                parse_report(body),
                set(RunPhase::Blocked { kind }),
                "{:?}",
                body
            );
        }
        // Field order does not matter.
        assert_eq!(
            parse_report(b"app=omp:kind=question:state=blocked"),
            set(RunPhase::Blocked {
                kind: Some(BlockedKind::Question)
            })
        );
    }

    #[test]
    fn msg_is_base64_text_of_blocked_and_error_reports() {
        let msg = STANDARD.encode("Approve write to /tmp/ü?");
        let blocked = format!("state=blocked:kind=permission:app=omp:msg={msg}");
        assert_eq!(
            parse_report(blocked.as_bytes()),
            Some(Report::Set(RunState {
                state: RunPhase::Blocked {
                    kind: Some(BlockedKind::Permission)
                },
                msg: Some("Approve write to /tmp/ü?".into()),
            }))
        );
        let error = format!("state=error:app=omp:msg={}", STANDARD.encode("boom"));
        assert_eq!(
            parse_report(error.as_bytes()),
            Some(Report::Set(RunState {
                state: RunPhase::Error,
                msg: Some("boom".into()),
            }))
        );
        // A msg on a state that has none is not kept; an unpadded msg decodes.
        let idle = format!("state=idle:app=omp:msg={}", STANDARD.encode("x"));
        assert_eq!(parse_report(idle.as_bytes()), set(RunPhase::Idle));
        assert_eq!(
            parse_report(b"state=error:msg=Ym9vbQ"),
            Some(Report::Set(RunState {
                state: RunPhase::Error,
                msg: Some("boom".into()),
            }))
        );
    }

    #[test]
    fn malformed_reports_are_discarded() {
        let control = STANDARD.encode("a\u{1b}b");
        for body in [
            &b""[..],
            b"app=omp",
            b"state=sleeping:app=omp",
            b"state=working:id=sub:app=omp",
            b"state=error:msg=!!not-base64!!",
            format!("state=error:msg={control}").as_bytes(),
            format!("state=error:msg={}", "A".repeat(MAX_MSG_ENCODED + 4)).as_bytes(),
            // Valid base64 of invalid UTF-8.
            format!("state=error:msg={}", STANDARD.encode([0xff, 0xfe])).as_bytes(),
        ] {
            assert_eq!(
                parse_report(body),
                None,
                "{:?}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn scanner_reads_bel_and_st_terminated_reports() {
        assert_eq!(
            scan(&[b"\x1b]7501;state=working:app=omp\x07"]),
            [set(RunPhase::Working)]
        );
        assert_eq!(
            scan(&[b"noise\x1b]7501;state=done:app=omp\x1b\\more"]),
            [set(RunPhase::Done)]
        );
        assert_eq!(
            scan(&[b"\x1b]7501;state=clear\x1b\\"]),
            [Some(Report::Clear)]
        );
    }

    #[test]
    fn scanner_takes_the_last_report_of_a_chunk() {
        assert_eq!(
            scan(&[b"\x1b]7501;state=working:app=omp\x1b\\\x1b]7501;state=idle:app=omp\x1b\\"]),
            [set(RunPhase::Idle)]
        );
    }

    #[test]
    fn scanner_reports_split_at_every_byte() {
        let wire = b"x\x1b]7501;state=blocked:kind=question:app=omp:msg=aGk=\x1b\\y";
        let want = Report::Set(RunState {
            state: RunPhase::Blocked {
                kind: Some(BlockedKind::Question),
            },
            msg: Some("hi".into()),
        });
        for cut in 1..wire.len() {
            let got = scan(&[&wire[..cut], &wire[cut..]]);
            let found: Vec<_> = got.into_iter().flatten().collect();
            assert_eq!(found, std::slice::from_ref(&want), "cut at {cut}");
        }
        // One byte at a time.
        let mut s = RunStateScanner::default();
        let found: Vec<_> = wire.iter().filter_map(|&b| s.feed(&[b])).collect();
        assert_eq!(found, [want]);
    }

    #[test]
    fn scanner_ignores_other_sequences() {
        assert_eq!(
            scan(&[
                b"\x1b]0;\xcf\x80 > x\x07\x1b]75010;state=idle\x07\x1b]7502;state=idle\x07\x1b[7501;m",
                b"\x1b]7501;state=idle:app=omp"
            ]),
            [None, None],
            "the last report has no terminator yet"
        );
    }

    #[test]
    fn scanner_drops_aborted_overlong_and_nested_reports() {
        // CAN aborts; a new OSC aborts the old one and is read itself.
        assert_eq!(scan(&[b"\x1b]7501;state=idle\x18\x1b\\"]), [None]);
        assert_eq!(
            scan(&[b"\x1b]7501;state=idle\x1b]7501;state=working\x07"]),
            [set(RunPhase::Working)]
        );
        // An over-long report is dropped and the scanner recovers.
        let mut long = b"\x1b]7501;state=error:msg=".to_vec();
        long.extend(std::iter::repeat_n(b'A', MAX_BODY * 2));
        long.extend_from_slice(b"\x07\x1b]7501;state=idle\x07");
        assert_eq!(scan(&[&long]), [set(RunPhase::Idle)]);
    }

    #[test]
    fn scanner_ris_clears_status_across_read_boundaries() {
        assert_eq!(
            scan(&[b"\x1b]7501;state=working\x07\x1b", b"c"]),
            [set(RunPhase::Working), Some(Report::Clear)]
        );
        assert_eq!(
            scan(&[b"\x1bc\x1b]7501;state=idle\x07"]),
            [set(RunPhase::Idle)]
        );
    }
    #[test]
    fn tokenised_osc_matches_the_byte_scanner() {
        assert_eq!(
            parse_osc(&[b"7501", b"state=working:app=omp"]),
            set(RunPhase::Working)
        );
        // `;` is no field separator: a tokenizer's split string is malformed.
        assert_eq!(parse_osc(&[b"7501", b"state=working", b"app=omp"]), None);
        assert_eq!(parse_osc(&[b"7501"]), None);
        assert_eq!(parse_osc(&[b"0", b"state=working"]), None);
    }

    #[test]
    fn encoded_reports_scan_back() {
        for state in [
            RunState::new(RunPhase::Idle),
            RunState::new(RunPhase::Working),
            RunState::new(RunPhase::Done),
            RunState {
                state: RunPhase::Error,
                msg: Some("é boom".into()),
            },
            RunState {
                state: RunPhase::Blocked {
                    kind: Some(BlockedKind::Permission),
                },
                msg: Some("Approve?".into()),
            },
            RunState::new(RunPhase::Blocked { kind: None }),
        ] {
            let mut wire = Vec::new();
            encode_report(&state, &mut wire);
            assert_eq!(
                RunStateScanner::default().feed(&wire),
                Some(Report::Set(state.clone())),
                "{state:?}"
            );
        }
    }

    #[test]
    fn apply_sets_and_clears() {
        let mut held = None;
        Report::Set(RunState::new(RunPhase::Working)).apply(&mut held);
        assert_eq!(held, Some(RunState::new(RunPhase::Working)));
        Report::Clear.apply(&mut held);
        assert_eq!(held, None);
    }

    #[test]
    fn run_state_serde_round_trip() {
        for state in [
            RunState::new(RunPhase::Working),
            RunState {
                state: RunPhase::Blocked {
                    kind: Some(BlockedKind::Question),
                },
                msg: Some("q".into()),
            },
        ] {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<RunState>(&json).unwrap(), state);
        }
    }

    /// An open inline surface whose dock holds `dock` (a JSON array of node objects).
    fn document_with_dock(dock: serde_json::Value) -> Document {
        use deck_tsp::stream::{Message, Verb};
        let msg = |verb, body: serde_json::Value| Message {
            verb,
            params: Vec::new(),
            body: serde_json::to_vec(&body).expect("json"),
        };
        let mut doc = Document::new();
        let now = std::time::Instant::now();
        let open = serde_json::json!({"id": "s", "mode": "inline", "role": "omp.session"});
        doc.apply(&msg(Verb::Open, open), now);
        let mut ops = vec![
            serde_json::json!(["add", "main", "s", null, {"id": "main", "k": "col"}]),
            serde_json::json!(["add", "dock", "s", null, {"id": "dock", "k": "col"}]),
        ];
        for node in dock.as_array().expect("array") {
            ops.push(serde_json::json!(["add", node["id"], "dock", null, node]));
        }
        let frame = serde_json::json!({"sf": "s", "s": 1, "ops": ops});
        let applied = doc.apply(&msg(Verb::Frame, frame), now);
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        doc
    }

    #[test]
    fn tsp_facts_read_the_docks_composer_working_row_and_asks() {
        let facts = |dock: serde_json::Value| TspFacts::of(&document_with_dock(dock));
        let (composer, working, asking) = (
            TspFacts {
                composer: true,
                ..TspFacts::default()
            },
            TspFacts {
                working: true,
                ..TspFacts::default()
            },
            TspFacts {
                asking: true,
                ..TspFacts::default()
            },
        );
        let editor = serde_json::json!({"id": "ed", "k": "editor", "p": {"sendable": true}});
        let unsendable = serde_json::json!({"id": "ed", "k": "editor", "p": {"sendable": false}});
        // omp nests the working row in the HUD.
        let hud = serde_json::json!({"id": "hud", "k": "row", "p": {"role": "omp.hud.activity"},
            "c": [{"id": "w", "k": "row", "p": {"role": "omp.working"}}]});
        let ask = |role: &str| serde_json::json!({"id": "ask", "k": "col", "p": {"role": role}});

        assert_eq!(facts(serde_json::json!([editor])), Some(composer));
        assert_eq!(
            facts(serde_json::json!([editor, hud])),
            Some(TspFacts {
                composer: true,
                working: true,
                asking: false
            })
        );
        assert_eq!(facts(serde_json::json!([hud])), Some(working));
        for role in [
            "omp.editor.ask",
            "omp.editor.ask.approval",
            "omp.editor.ask.hidden",
        ] {
            assert_eq!(
                facts(serde_json::json!([ask(role)])),
                Some(asking),
                "{role}"
            );
        }
        // The composer's own `omp.editor` role is not an ask; an unsendable editor is no
        // composer; an empty dock is an omp that shows nothing yet.
        assert_eq!(
            facts(serde_json::json!([ask("omp.editor"), unsendable])),
            Some(TspFacts::default())
        );
        assert_eq!(facts(serde_json::json!([])), Some(TspFacts::default()));
        // No inline surface: no facts.
        assert_eq!(TspFacts::of(&Document::new()), None);
    }
}
