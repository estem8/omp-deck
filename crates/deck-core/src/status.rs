//! Live status of the omp session behind the active tab: what omp's own status line shows.
//!
//! Sources (docs/notes/omp-status.md): the session `.jsonl` (incremental reader), the terminal
//! breadcrumb that tells which jsonl a pane's omp is on, `git status`, and `gh pr view`. The
//! UI only reads [`StatusWatcher::latest`]; every blocking call happens on worker threads.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use memchr::{memchr, memmem};
use serde::Deserialize;

use crate::agents::tail;
use crate::mux::Mux;
use crate::platform::{self, FileId};

// ---------------------------------------------------------------------------------------------
// Breadcrumb

/// `<agent_dir>/terminal-sessions/<key>`: omp writes it on every session start/switch (`/new`,
/// `/fork`, `/resume`, `/move`), so it names the pane's current file. The key is omp's terminal
/// id: the tty minus `/dev/` with `/`→`-` (`pts-6`), or `apple-<TERM_SESSION_ID>` where there is
/// no tty (the OmpDeck host on Windows).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Breadcrumb {
    pub cwd: PathBuf,
    pub session: PathBuf,
    /// Session announced but not yet persisted (the file appears after the first reply).
    pub fresh: bool,
}

/// The breadcrumb key omp derives from a tty path: `/dev/pts/6` → `pts-6`.
pub fn breadcrumb_key_for_tty(tty: &str) -> String {
    tty.strip_prefix("/dev/").unwrap_or(tty).replace('/', "-")
}

/// The breadcrumb key omp derives from `TERM_SESSION_ID` when stdin is no tty: `apple-<id>`.
pub fn breadcrumb_key_for_term_session(id: &str) -> String {
    format!("apple-{id}")
}

/// Path of the breadcrumb file for `key`. A raw tty path is accepted too and normalised
/// (`/dev/pts/6` → `terminal-sessions/pts-6`); an already derived key passes through unchanged.
pub fn breadcrumb_path(agent_dir: &Path, key: &str) -> PathBuf {
    agent_dir
        .join("terminal-sessions")
        .join(breadcrumb_key_for_tty(key))
}

/// Reads and parses the breadcrumb of `key`; `None` if missing or malformed.
pub fn read_breadcrumb(agent_dir: &Path, key: &str) -> Option<Breadcrumb> {
    if key.trim().is_empty() {
        return None;
    }
    let text = fs::read_to_string(breadcrumb_path(agent_dir, key)).ok()?;
    parse_breadcrumb(&text)
}

/// `<cwd>\n<abs session .jsonl>\n[fresh\n]cwdstat <dev> <ino>`.
pub fn parse_breadcrumb(text: &str) -> Option<Breadcrumb> {
    let mut lines = text.lines();
    let cwd = lines.next()?.trim_end_matches('\r');
    let session = lines.next()?.trim_end_matches('\r');
    if cwd.is_empty() || !Path::new(session).is_absolute() {
        return None;
    }
    Some(Breadcrumb {
        cwd: PathBuf::from(cwd),
        session: PathBuf::from(session),
        fresh: lines.any(|l| l.trim() == "fresh"),
    })
}

/// The breadcrumb for `key`, unless it predates the pane's process (a stale file left by a
/// previous owner of the same pty). omp does not rewrite an unchanged breadcrumb when a tab
/// restarts with `--resume` on the same session, so an older file is still trusted when it names
/// the session the tab already knows (`known`). A little slack covers clock-tick rounding of the
/// process start time.
pub(crate) fn trusted_breadcrumb(
    agent_dir: &Path,
    key: &str,
    pane_started: Option<SystemTime>,
    known: Option<&Path>,
) -> Option<Breadcrumb> {
    let crumb = read_breadcrumb(agent_dir, key)?;
    let modified = pane_started.and_then(|_| {
        fs::metadata(breadcrumb_path(agent_dir, key))
            .ok()?
            .modified()
            .ok()
    });
    breadcrumb_trusted(&crumb.session, modified, pane_started, known).then_some(crumb)
}

/// Trust rule: a breadcrumb at least as new as the pane's process (within slack) is trusted;
/// an older one only when it names the session the tab already knows. With an unknown start
/// time (pane process gone) only a breadcrumb naming the known session is trusted.
fn breadcrumb_trusted(
    session: &Path,
    modified: Option<SystemTime>,
    pane_started: Option<SystemTime>,
    known: Option<&Path>,
) -> bool {
    let same = known == Some(session);
    match (modified, pane_started) {
        (Some(m), Some(s)) if m + Duration::from_secs(2) >= s => true,
        _ => same,
    }
}

// ---------------------------------------------------------------------------------------------
// Session jsonl reader

/// Bytes read per `read(2)` during a (first) pass.
const CHUNK: usize = 4 * 1024 * 1024;
/// Title slot (line 0) is 256 bytes including the newline; look at a little more.
const SLOT_PEEK: usize = 1024;
/// Only the beginning of a `message` line is searched for the assistant role marker.
const ROLE_PEEK: usize = 1024;
/// Cap of [`SessionStats::last_assistant_text`].
const MAX_ASSISTANT_TEXT: usize = 256 * 1024;

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionStats {
    pub title: Option<String>,
    /// Last `model_change` / assistant `provider/id`, whichever came later in the file.
    pub model: Option<String>,
    /// Effective level of the last `thinking_level_change` (`null` → "off").
    pub thinking: Option<String>,
    /// What the user picked (`auto`, a level, or None = default).
    pub thinking_configured: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Σ assistant `usage.cost.total` + `model_usage` entries, over the whole file (omp's rule;
    /// compaction does not reset it). A missing, non-numeric, negative or non-finite total adds
    /// nothing (counted in [`CostSource::uncosted`]).
    pub cost: f64,
    /// [`Self::cost`] split by source and by recorded `usage.cost` component.
    pub cost_detail: CostDetail,
    /// Prompt tokens of the last completed assistant message after the last compaction
    /// (`contextSnapshot.promptTokens`, else `contextTokens`, else input+cacheRead+cacheWrite).
    /// Right after a compaction: its `tokensAfter` estimate, or None when absent.
    pub context_tokens: Option<u64>,
    /// Last assistant message ended with `stopReason: "error"` (Retry).
    pub last_turn_failed: bool,
    /// Text parts of the most recent assistant message that has any (≤ 256 KiB).
    pub last_assistant_text: Option<String>,
}

/// Recorded session cost per source; `assistant.total + side.total == SessionStats::cost`.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CostDetail {
    /// Assistant `message` entries.
    pub assistant: CostSource,
    /// `model_usage` entries (side-model calls: titles, judges, `find`, …).
    pub side: CostSource,
}

/// Σ of one source's `usage` records as omp wrote them; nothing is priced from the model catalog.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CostSource {
    /// Records carrying a `usage` object.
    pub records: u64,
    /// Σ usable `usage.cost.total`.
    pub total: f64,
    /// Records with a usable total and all four usable components (explicit zeros count).
    pub itemized: u64,
    /// Components (and their token counts) of the [`Self::itemized`] records only.
    pub input: CostPart,
    pub output: CostPart,
    pub cache_read: CostPart,
    pub cache_write: CostPart,
    /// Records with a usable total but a missing / unusable component.
    pub unitemized_records: u64,
    /// Their Σ total and Σ input+output+cacheRead+cacheWrite tokens.
    pub unitemized: CostPart,
    /// Records without a usable total (they add nothing to [`Self::total`]).
    pub uncosted: u64,
    /// Their Σ input+output+cacheRead+cacheWrite tokens.
    pub uncosted_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CostPart {
    pub usd: f64,
    pub tokens: u64,
}

impl CostPart {
    fn add(&mut self, usd: f64, tokens: u64) {
        self.usd += usd;
        self.tokens += tokens;
    }
}

impl CostSource {
    /// Σ of the four components of the itemized records.
    pub fn components(&self) -> f64 {
        self.input.usd + self.output.usd + self.cache_read.usd + self.cache_write.usd
    }

    /// Part of the itemized records' totals their four components do not account for (omp may
    /// bill something else, or round); negative when the components exceed the totals.
    pub fn unreconciled(&self) -> f64 {
        self.total - self.unitemized.usd - self.components()
    }

    fn add(&mut self, u: &Usage, tokens: [u64; 4]) {
        self.records += 1;
        let all: u64 = tokens.iter().sum();
        let cost = u.cost.as_ref();
        let Some(total) = usd(cost.and_then(|c| c.total)) else {
            self.uncosted += 1;
            self.uncosted_tokens += all;
            return;
        };
        self.total += total;
        let parts = cost.map(|c| [c.input, c.output, c.cache_read, c.cache_write].map(usd));
        match parts {
            Some([Some(i), Some(o), Some(cr), Some(cw)]) => {
                self.itemized += 1;
                self.input.add(i, tokens[0]);
                self.output.add(o, tokens[1]);
                self.cache_read.add(cr, tokens[2]);
                self.cache_write.add(cw, tokens[3]);
            }
            _ => {
                self.unitemized_records += 1;
                self.unitemized.add(total, all);
            }
        }
    }
}

/// A recorded dollar amount omp could have written: finite and not negative.
fn usd(v: Option<f64>) -> Option<f64> {
    v.filter(|c| c.is_finite() && *c >= 0.0)
}

#[derive(Default)]
struct Titles {
    /// Line 0 slot: `Some(title)` when line 0 is a title slot ("" = untitled).
    slot: Option<String>,
    /// Header `title`, overridden by later `title_change` entries (files without a slot).
    entry: Option<String>,
}

pub struct SessionStatsReader {
    path: PathBuf,
    file_id: Option<FileId>,
    parsed_to: u64,
    stats: SessionStats,
    /// A `model_change` was applied: from then on only further changes set `stats.model`.
    model_from_change: bool,
    titles: Titles,
}

impl SessionStatsReader {
    pub fn new(path: PathBuf) -> Self {
        SessionStatsReader {
            path,
            file_id: None,
            parsed_to: 0,
            stats: SessionStats::default(),
            model_from_change: false,
            titles: Titles::default(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stats(&self) -> &SessionStats {
        &self.stats
    }

    /// Reads what changed since the last call. `Ok(true)` if the stats changed.
    ///
    /// Keyed on the file's identity: a different file or a file shorter than what was parsed starts over.
    /// Complete lines appended since last time are applied; a half-written last line waits for
    /// its newline. Line 0 (the in-place rewritten title slot) is re-read every call.
    pub fn refresh(&mut self) -> io::Result<bool> {
        let mut file = File::open(&self.path)?;
        let md = file.metadata()?;
        let id = platform::file_identity(&file)?;
        let len = md.len();
        let mut changed = false;
        if self.file_id != Some(id) || len < self.parsed_to {
            changed = self.stats != SessionStats::default();
            self.stats = SessionStats::default();
            self.model_from_change = false;
            self.titles = Titles::default();
            self.parsed_to = 0;
            self.file_id = Some(id);
        }
        let result = if len > self.parsed_to {
            self.read_new(&mut file, len)
        } else {
            Ok(false)
        };
        changed |= *result.as_ref().unwrap_or(&true);
        // Title slot even when the body read failed halfway.
        changed |= self.refresh_slot(&mut file);
        result?;
        Ok(changed)
    }

    fn read_new(&mut self, file: &mut File, len: u64) -> io::Result<bool> {
        file.seek(SeekFrom::Start(self.parsed_to))?;
        let mut reader = file.take(len - self.parsed_to);
        let mut remaining = len - self.parsed_to;
        let mut buf: Vec<u8> = Vec::new();
        let mut changed = false;
        while remaining > 0 {
            let want = usize::try_from(remaining).map_or(CHUNK, |r| r.min(CHUNK));
            let old = buf.len();
            buf.resize(old + want, 0);
            let n = reader.read(&mut buf[old..])?;
            buf.truncate(old + n);
            if n == 0 {
                break;
            }
            remaining -= n as u64;
            let mut off = 0;
            while let Some(i) = memchr(b'\n', &buf[off..]) {
                changed |= self.apply_line(&buf[off..off + i]);
                off += i + 1;
            }
            self.parsed_to += off as u64;
            buf.drain(..off);
        }
        Ok(changed)
    }

    /// Re-reads line 0; a torn or unparsable read keeps the previous title.
    fn refresh_slot(&mut self, file: &mut File) -> bool {
        let mut head = vec![0u8; SLOT_PEEK];
        let Ok(n) = file
            .seek(SeekFrom::Start(0))
            .and_then(|_| read_full(file, &mut head))
        else {
            return false;
        };
        let head = &head[..n];
        let Some(end) = memchr(b'\n', head) else {
            return false;
        };
        let Ok(slot) = serde_json::from_slice::<SlotLine>(&head[..end]) else {
            return false;
        };
        if slot.kind.as_deref() == Some("title") {
            self.titles.slot = Some(slot.title.unwrap_or_default());
        }
        self.sync_title()
    }

    fn sync_title(&mut self) -> bool {
        let title = self
            .titles
            .slot
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| self.titles.entry.clone().filter(|t| !t.is_empty()));
        set(&mut self.stats.title, title)
    }

    /// Applies one complete line (without its `\n`); true if a stat changed. Lines of no
    /// interest are rejected on a byte prefix before any JSON parsing; malformed ones are skipped.
    fn apply_line(&mut self, line: &[u8]) -> bool {
        let Some(rest) = line.strip_prefix(b"{\"type\":\"") else {
            return false;
        };
        let Some(end) = rest.iter().take(32).position(|&b| b == b'"') else {
            return false;
        };
        let applied = match &rest[..end] {
            b"message" => {
                let head = &line[..line.len().min(ROLE_PEEK)];
                if memmem::find(head, br#""role":"assistant""#).is_none() {
                    return false;
                }
                Some(self.apply_assistant(line))
            }
            b"model_usage" => {
                parse::<UsageEntry>(line).map(|e| self.add_usage(e.usage.as_ref(), true))
            }
            b"model_change" => parse::<ModelChange>(line).map(|e| {
                let model = e.model.filter(|m| !m.is_empty());
                // The newest change is the session's model, whatever a later assistant record says (a switch mid-turn
                // is followed by the old model's reply).
                self.model_from_change |= model.is_some();
                set(&mut self.stats.model, model)
            }),
            b"thinking_level_change" => parse::<ThinkingChange>(line).map(|e| {
                let level = Some(e.level.unwrap_or_else(|| "off".to_owned()));
                set(&mut self.stats.thinking, level)
                    | set(&mut self.stats.thinking_configured, e.configured)
            }),
            b"compaction" => parse::<Compaction>(line).map(|e| {
                // Context is unknown until the next assistant reply; omp estimates tokensAfter.
                set(
                    &mut self.stats.context_tokens,
                    e.tokens_after.map(|t| t as u64),
                )
            }),
            b"title_change" => parse::<TitleChange>(line).map(|e| {
                self.titles.entry = e.title;
                self.sync_title()
            }),
            b"session" => parse::<TitleChange>(line).map(|e| {
                if e.title.is_some() {
                    self.titles.entry = e.title;
                }
                self.sync_title()
            }),
            _ => return false,
        };
        applied.unwrap_or_else(|| {
            log::debug!(
                "status: skipping malformed jsonl line ({} bytes)",
                line.len()
            );
            false
        })
    }

    /// Adds one `usage` record; `side` = a `model_usage` entry rather than an assistant reply.
    fn add_usage(&mut self, usage: Option<&Usage>, side: bool) -> bool {
        let Some(u) = usage else { return false };
        let s = &mut self.stats;
        let (i, o, cr, cw) = (u.input(), u.output(), u.cache_read(), u.cache_write());
        let detail = &mut s.cost_detail;
        let source = if side {
            &mut detail.side
        } else {
            &mut detail.assistant
        };
        source.add(u, [i, o, cr, cw]);
        s.cost = detail.assistant.total + detail.side.total;
        s.input_tokens += i;
        s.output_tokens += o;
        s.cache_read += cr;
        s.cache_write += cw;
        // A record always changes the counts in `cost_detail`.
        true
    }

    fn apply_assistant(&mut self, line: &[u8]) -> bool {
        let Some(entry) = parse::<MessageEntry>(line) else {
            log::debug!(
                "status: skipping malformed assistant line ({} bytes)",
                line.len()
            );
            return false;
        };
        let msg = entry.message;
        let mut changed = self.add_usage(msg.usage.as_ref(), false);
        if let (Some(p), Some(m)) = (&msg.provider, &msg.model)
            && !p.is_empty()
            && !m.is_empty()
            && !self.model_from_change
        {
            changed |= set(&mut self.stats.model, Some(format!("{p}/{m}")));
        }
        let reason = msg.stop_reason.as_deref();
        changed |= set(&mut self.stats.last_turn_failed, reason == Some("error"));
        if !matches!(reason, Some("error" | "aborted"))
            && let Some(u) = &msg.usage
        {
            changed |= set(
                &mut self.stats.context_tokens,
                Some(context_tokens(&msg, u)),
            );
        }
        if let Some(text) = assistant_text(&msg) {
            changed |= set(&mut self.stats.last_assistant_text, Some(text));
        }
        changed
    }
}

/// omp's prompt-token rule (omp-status.md §2a): snapshot, else reported context tokens, else
/// input + cacheRead + cacheWrite (falling back to totalTokens − output when those are zero).
fn context_tokens(msg: &Message, u: &Usage) -> u64 {
    if let Some(p) = msg.context_snapshot.as_ref().and_then(|s| s.prompt_tokens) {
        return p as u64;
    }
    if let Some(c) = u.context_tokens {
        return c as u64;
    }
    let sum = u.input() + u.cache_read() + u.cache_write();
    if sum == 0 {
        (u.total_tokens.unwrap_or(0.0) as u64).saturating_sub(u.output())
    } else {
        sum
    }
}

/// Text parts joined by newlines, capped at [`MAX_ASSISTANT_TEXT`]; None when there are none.
fn assistant_text(msg: &Message) -> Option<String> {
    let mut out = String::new();
    for part in msg.content.iter().flatten() {
        if part.kind.as_deref() != Some("text") {
            continue;
        }
        let Some(t) = part.text.as_deref().filter(|t| !t.is_empty()) else {
            continue;
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(t);
        if out.len() >= MAX_ASSISTANT_TEXT {
            break;
        }
    }
    if out.is_empty() {
        return None;
    }
    if out.len() > MAX_ASSISTANT_TEXT {
        let mut cut = MAX_ASSISTANT_TEXT;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    Some(out)
}

/// Assigns and reports whether the value changed.
fn set<T: PartialEq>(slot: &mut T, value: T) -> bool {
    if *slot == value {
        false
    } else {
        *slot = value;
        true
    }
}

fn parse<'a, T: Deserialize<'a>>(line: &'a [u8]) -> Option<T> {
    serde_json::from_slice(line).ok()
}

/// `Read::read_exact` that tolerates EOF: returns the bytes obtained.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

#[derive(Deserialize)]
struct SlotLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize)]
struct TitleChange {
    title: Option<String>,
}

#[derive(Deserialize)]
struct ModelChange {
    model: Option<String>,
}

#[derive(Deserialize)]
struct ThinkingChange {
    #[serde(rename = "thinkingLevel")]
    level: Option<String>,
    configured: Option<String>,
}

#[derive(Deserialize)]
struct Compaction {
    #[serde(rename = "tokensAfter")]
    tokens_after: Option<f64>,
}

#[derive(Deserialize)]
struct UsageEntry {
    usage: Option<Usage>,
}

/// Numbers are read as `f64` so `4.0`-style values never invalidate a line.
#[derive(Deserialize, Default)]
struct Usage {
    input: Option<f64>,
    output: Option<f64>,
    #[serde(rename = "cacheRead")]
    cache_read: Option<f64>,
    #[serde(rename = "cacheWrite")]
    cache_write: Option<f64>,
    #[serde(rename = "totalTokens")]
    total_tokens: Option<f64>,
    #[serde(rename = "contextTokens")]
    context_tokens: Option<f64>,
    /// Lenient: a non-object `cost` loses only the cost, not the line's tokens.
    #[serde(default, deserialize_with = "lenient_cost")]
    cost: Option<UsageCost>,
}

impl Usage {
    fn input(&self) -> u64 {
        self.input.unwrap_or(0.0) as u64
    }
    fn output(&self) -> u64 {
        self.output.unwrap_or(0.0) as u64
    }
    fn cache_read(&self) -> u64 {
        self.cache_read.unwrap_or(0.0) as u64
    }
    fn cache_write(&self) -> u64 {
        self.cache_write.unwrap_or(0.0) as u64
    }
}

/// omp's per-component dollars (`usage.cost`); each field lenient so one odd value never drops
/// the record.
#[derive(Deserialize)]
struct UsageCost {
    #[serde(default, deserialize_with = "lenient_f64")]
    total: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    input: Option<f64>,
    #[serde(default, deserialize_with = "lenient_f64")]
    output: Option<f64>,
    #[serde(default, rename = "cacheRead", deserialize_with = "lenient_f64")]
    cache_read: Option<f64>,
    #[serde(default, rename = "cacheWrite", deserialize_with = "lenient_f64")]
    cache_write: Option<f64>,
}

fn lenient_f64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Ok(serde_json::Value::deserialize(d)?.as_f64())
}

fn lenient_cost<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<UsageCost>, D::Error> {
    Ok(serde_json::from_value(serde_json::Value::deserialize(d)?).ok())
}

#[derive(Deserialize)]
struct MessageEntry<'a> {
    /// ISO-8601 time the entry was written.
    timestamp: Option<String>,
    #[serde(borrow)]
    message: Message<'a>,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow, default)]
    content: Option<Vec<Part<'a>>>,
    provider: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
    #[serde(rename = "stopReason")]
    stop_reason: Option<String>,
    #[serde(rename = "contextSnapshot")]
    context_snapshot: Option<ContextSnapshot>,
    role: Option<String>,
    #[serde(rename = "errorMessage")]
    error_message: Option<String>,
    /// Epoch milliseconds when the request started.
    timestamp: Option<f64>,
    /// Epoch milliseconds when the session wrote the finished message.
    #[serde(rename = "completedAt")]
    completed_at: Option<f64>,
}

#[derive(Deserialize)]
struct Part<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    text: Option<std::borrow::Cow<'a, str>>,
}

#[derive(Deserialize)]
struct ContextSnapshot {
    #[serde(rename = "promptTokens")]
    prompt_tokens: Option<f64>,
}

// ---------------------------------------------------------------------------------------------
// Last turn outcome

/// Tail window read by [`last_turn`].
const TAIL_WINDOW: u64 = 256 * 1024;
/// Cap of the `Error` message, in chars.
const MAX_ERROR_CHARS: usize = 200;

/// How the session's most recent assistant message ended.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TurnOutcome {
    /// Any `stopReason` other than `error` / `aborted`.
    Completed,
    /// `stopReason: "error"` with `errorMessage` (≤ 200 chars) if present.
    Error(Option<String>),
    Aborted,
    /// No assistant message in the tail window (or the file is unreadable).
    Unknown,
}

/// [`TurnOutcome`] plus when the deciding assistant message was written.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LastTurn {
    pub outcome: TurnOutcome,
    /// When the deciding assistant reply finished: `message.completedAt`, else the entry's own
    /// ISO `timestamp` (both stamped when the message is written), else `message.timestamp` (which
    /// omp stamps at request start). `None` when `outcome` is `Unknown`, no timestamp is present,
    /// or the value is not a representable time.
    pub assistant_at: Option<SystemTime>,
}

/// Outcome of the last assistant message in the session file at `path` (see [`last_turn`]).
pub fn last_turn_outcome(path: &Path) -> TurnOutcome {
    last_turn(path).outcome
}

/// Reads at most [`TAIL_WINDOW`] bytes from EOF (dropping the leading partial line and ignoring an
/// unparsable truncated last line) and returns the last `message` entry with role `assistant`.
pub fn last_turn(path: &Path) -> LastTurn {
    let unknown = LastTurn {
        outcome: TurnOutcome::Unknown,
        assistant_at: None,
    };
    let Ok(tail) = read_tail(path) else {
        return unknown;
    };
    for line in tail.rsplit(|&b| b == b'\n') {
        if !line.starts_with(b"{\"type\":\"message\"") {
            continue;
        }
        let head = &line[..line.len().min(ROLE_PEEK)];
        if memmem::find(head, br#""role":"assistant""#).is_none() {
            continue;
        }
        let Some(entry) = parse::<MessageEntry>(line) else {
            continue; // truncated or malformed
        };
        let msg = entry.message;
        if msg.role.as_deref() != Some("assistant") {
            continue;
        }
        let outcome = match msg.stop_reason.as_deref() {
            Some("error") => TurnOutcome::Error(
                msg.error_message
                    .as_deref()
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(|m| m.chars().take(MAX_ERROR_CHARS).collect()),
            ),
            Some("aborted") => TurnOutcome::Aborted,
            _ => TurnOutcome::Completed,
        };
        // The reply's finish time: `message.timestamp` is when the request started.
        let assistant_at = msg
            .completed_at
            .and_then(tail::from_epoch_ms)
            .or_else(|| {
                entry
                    .timestamp
                    .as_deref()
                    .and_then(|t| tail::parse_iso(t.as_bytes()))
            })
            .or_else(|| msg.timestamp.and_then(tail::from_epoch_ms));
        return LastTurn {
            outcome,
            assistant_at,
        };
    }
    unknown
}

/// The last [`TAIL_WINDOW`] bytes of `path`, starting at a line boundary: when the window does not
/// begin right after a `\n` its first (partial) line is dropped.
fn read_tail(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_WINDOW);
    // One extra leading byte tells whether `start` is already a line start.
    let from = start.saturating_sub(1);
    file.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::with_capacity((len - from) as usize);
    file.take(len - from).read_to_end(&mut buf)?;
    if start > 0 {
        if buf.first() == Some(&b'\n') {
            buf.remove(0);
        } else {
            match memchr(b'\n', &buf) {
                Some(i) => {
                    buf.drain(..=i);
                }
                None => buf.clear(),
            }
        }
    }
    Ok(buf)
}

// ---------------------------------------------------------------------------------------------
// Subprocess helper

/// Runs `cmd` (stdin/stderr null) and returns stdout when it exits successfully. The child is
/// killed on timeout or when `cancel` becomes true; stdout is drained on its own thread so a
/// large output can never block the child.
fn run_capture(
    mut cmd: Command,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Option<Vec<u8>> {
    let mut child = platform::no_window(&mut cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut data = Vec::new();
        let _ = tx.send(stdout.read_to_end(&mut data).map(|_| data));
    });
    let deadline = Instant::now() + timeout;
    let data = loop {
        let slice = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        match rx.recv_timeout(slice) {
            Ok(r) => break r.ok(),
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline || cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
                    // The reader thread ends when the pipe closes; it is not joined here.
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break None,
        }
    };
    let status = child.wait().ok()?;
    status.success().then_some(data?)
}

// ---------------------------------------------------------------------------------------------
// Git / PR

const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const GH_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitInfo {
    /// Branch name, or "detached".
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
    /// Filled by [`StatusWatcher`] only.
    pub pr: Option<PrInfo>,
    /// Primary checkout root when the cwd is a linked git worktree.
    pub worktree_of: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrInfo {
    pub number: u64,
    pub url: String,
}

/// `git -C <cwd> status --porcelain=v2 --branch -unormal` with a 5 s timeout; `None` when `cwd` is
/// not a repository, git is missing, or the command times out. `pr` is left empty.
pub fn git_info(cwd: &Path) -> Option<GitInfo> {
    git_info_cancellable(cwd, None)
}

pub(crate) fn git_info_cancellable(cwd: &Path, cancel: Option<&AtomicBool>) -> Option<GitInfo> {
    let mut cmd = Command::new("git");
    // --no-optional-locks: never contend with omp's own git tools for the index lock;
    // core.fsmonitor=false: never run a program the repository's config names.
    cmd.arg("--no-optional-locks")
        .args(crate::git::NO_FSMONITOR)
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain=v2", "--branch", "-unormal"])
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0");
    let out = run_capture(cmd, GIT_TIMEOUT, cancel)?;
    let mut info = parse_git_status_v2(&String::from_utf8_lossy(&out))?;
    info.worktree_of = crate::worktree::resolve(cwd).map(|w| w.primary);
    Some(info)
}

/// Parses `git status --porcelain=v2 --branch`. Counts follow omp's rule (omp-status.md §2b):
/// `*` = entries with a worktree change (`Y != '.'`), `+` = entries with an index change
/// (`X` ∈ A M D R C, plus T), `?` = untracked entries (directories collapsed by `-unormal`); an
/// entry both staged and modified counts in both. `None` without a `# branch.head` header.
pub fn parse_git_status_v2(out: &str) -> Option<GitInfo> {
    let mut info = GitInfo::default();
    let mut have_head = false;
    for line in out.lines() {
        if let Some(h) = line.strip_prefix("# branch.head ") {
            have_head = true;
            info.branch = (if h == "(detached)" {
                "detached"
            } else {
                h.trim()
            })
            .to_owned();
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for tok in ab.split_whitespace() {
                if let Some(n) = tok.strip_prefix('+') {
                    info.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = tok.strip_prefix('-') {
                    info.behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("? ") {
            info.untracked += 1;
        } else if matches!(line.as_bytes().first(), Some(b'1' | b'2' | b'u'))
            && let [_, b' ', x, y, ..] = line.as_bytes()
        {
            if matches!(*x, b'A' | b'M' | b'D' | b'R' | b'C' | b'T') {
                info.staged += 1;
            }
            if *y != b'.' {
                info.unstaged += 1;
            }
        }
    }
    have_head.then_some(info)
}

/// Default branch of the repo at `cwd` from `refs/remotes/origin/HEAD`, if known.
fn default_branch(cwd: &Path, cancel: &AtomicBool) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("--no-optional-locks")
        .args(crate::git::NO_FSMONITOR)
        .arg("-C")
        .arg(cwd)
        .args(["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"]);
    let out = run_capture(cmd, GIT_TIMEOUT, Some(cancel))?;
    let name = String::from_utf8_lossy(&out).trim().to_owned();
    let name = name.strip_prefix("origin/").unwrap_or(&name);
    (!name.is_empty()).then(|| name.to_owned())
}

/// `gh pr view --json number,url` in `cwd`; `None` on any failure (no PR, not logged in, …).
fn gh_pr_view(cwd: &Path, cancel: &AtomicBool) -> Option<PrInfo> {
    let mut cmd = Command::new("gh");
    cmd.args(["pr", "view", "--json", "number,url"])
        .current_dir(cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1");
    parse_pr_view(&run_capture(cmd, GH_TIMEOUT, Some(cancel))?)
}

fn parse_pr_view(stdout: &[u8]) -> Option<PrInfo> {
    #[derive(Deserialize)]
    struct Pr {
        number: u64,
        url: String,
    }
    let pr: Pr = serde_json::from_slice(stdout).ok()?;
    Some(PrInfo {
        number: pr.number,
        url: pr.url,
    })
}

// ---------------------------------------------------------------------------------------------
// Watcher

const JSONL_INTERVAL: Duration = Duration::from_secs(1);
const GIT_INTERVAL: Duration = Duration::from_secs(3);

/// What the watcher follows: the ACTIVE tab only.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusTarget {
    /// The tab's multiplexer backend + session name (None in direct mode).
    pub mux: Option<(Mux, String)>,
    /// The tab's known session file; used when no trustworthy breadcrumb exists.
    pub session: Option<PathBuf>,
    pub cwd: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionStatus {
    /// Current file (breadcrumb wins; follows /new, /fork, /move).
    pub session: Option<PathBuf>,
    /// The breadcrumb says the session is not persisted yet.
    pub fresh: bool,
    pub cwd: PathBuf,
    pub stats: SessionStats,
    pub git: Option<GitInfo>,
}

type PrKey = (PathBuf, String);

struct Control {
    target: Option<StatusTarget>,
    /// Bumped on every target change; ticks started under an older value publish nothing.
    generation: u64,
    force: bool,
    stop: bool,
    pr_done: Vec<(PrKey, Option<PrInfo>)>,
}

struct Shared {
    control: Mutex<Control>,
    wake: Condvar,
    latest: Mutex<Option<Arc<SessionStatus>>>,
    /// Kills in-flight git/gh children on drop.
    cancel: AtomicBool,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One background thread tracking the active tab.
pub struct StatusWatcher {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl StatusWatcher {
    /// jsonl/breadcrumb every 1 s, git every 3 s, `gh pr view --json number,url` once per (cwd,
    /// branch) when `gh` exists and the branch is neither detached nor the default branch (failures
    /// cached as no PR); `on_update` runs after each changed snapshot.
    pub fn spawn(agent_dir: PathBuf, on_update: impl Fn() + Send + 'static) -> StatusWatcher {
        let shared = Arc::new(Shared {
            control: Mutex::new(Control {
                target: None,
                generation: 0,
                force: false,
                stop: false,
                pr_done: Vec::new(),
            }),
            wake: Condvar::new(),
            latest: Mutex::new(None),
            cancel: AtomicBool::new(false),
        });
        let mut worker = Worker::new(Arc::clone(&shared), agent_dir, Box::new(on_update));
        let handle = std::thread::Builder::new()
            .name("deck-status".into())
            .spawn(move || worker.run());
        let handle = match handle {
            Ok(h) => Some(h),
            Err(e) => {
                log::error!("cannot spawn status watcher thread: {e}");
                None
            }
        };
        StatusWatcher { shared, handle }
    }

    /// Switches the tracked tab; the previous snapshot is dropped at once and the new target is
    /// refreshed immediately. Setting an equal target is a no-op.
    pub fn set_target(&self, target: Option<StatusTarget>) {
        let mut ctl = lock(&self.shared.control);
        if ctl.target == target {
            return;
        }
        ctl.target = target;
        ctl.generation += 1;
        ctl.force = true;
        *lock(&self.shared.latest) = None;
        drop(ctl);
        self.shared.wake.notify_all();
    }

    /// Refreshes jsonl, breadcrumb and git immediately.
    pub fn refresh_now(&self) {
        lock(&self.shared.control).force = true;
        self.shared.wake.notify_all();
    }

    /// Snapshot for the current target only; None until its first refresh finished.
    pub fn latest(&self) -> Option<Arc<SessionStatus>> {
        lock(&self.shared.latest).clone()
    }
}

impl Drop for StatusWatcher {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Release);
        lock(&self.shared.control).stop = true;
        self.shared.wake.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

struct Worker {
    shared: Arc<Shared>,
    agent_dir: PathBuf,
    on_update: Box<dyn Fn() + Send>,
    gh: bool,
    generation: u64,
    target: Option<StatusTarget>,
    reader: Option<SessionStatsReader>,
    stats: SessionStats,
    session: Option<PathBuf>,
    fresh: bool,
    cwd: PathBuf,
    git: Option<GitInfo>,
    git_cwd: Option<PathBuf>,
    last_git: Option<Instant>,
    pr_cache: HashMap<PrKey, Option<PrInfo>>,
    pr_pending: HashSet<PrKey>,
    published: Option<Arc<SessionStatus>>,
}

impl Worker {
    fn new(shared: Arc<Shared>, agent_dir: PathBuf, on_update: Box<dyn Fn() + Send>) -> Worker {
        Worker {
            shared,
            agent_dir,
            on_update,
            gh: platform::find_on_path("gh").is_some(),
            generation: 0,
            target: None,
            reader: None,
            stats: SessionStats::default(),
            session: None,
            fresh: false,
            cwd: PathBuf::new(),
            git: None,
            git_cwd: None,
            last_git: None,
            pr_cache: HashMap::new(),
            pr_pending: HashSet::new(),
            published: None,
        }
    }

    fn run(&mut self) {
        let mut next_tick = Instant::now();
        loop {
            let force;
            {
                let shared = Arc::clone(&self.shared);
                let mut ctl = lock(&shared.control);
                loop {
                    if ctl.stop {
                        return;
                    }
                    let due = ctl.target.is_some() && Instant::now() >= next_tick;
                    if ctl.force || !ctl.pr_done.is_empty() || due {
                        break;
                    }
                    ctl = if ctl.target.is_some() {
                        let wait = next_tick.saturating_duration_since(Instant::now());
                        shared
                            .wake
                            .wait_timeout(ctl, wait)
                            .unwrap_or_else(PoisonError::into_inner)
                            .0
                    } else {
                        shared
                            .wake
                            .wait(ctl)
                            .unwrap_or_else(PoisonError::into_inner)
                    };
                }
                force = std::mem::take(&mut ctl.force);
                for (key, pr) in ctl.pr_done.drain(..) {
                    self.pr_pending.remove(&key);
                    self.pr_cache.insert(key, pr);
                }
                if ctl.generation != self.generation {
                    self.generation = ctl.generation;
                    self.target = ctl.target.clone();
                    self.reset();
                }
            }
            if self.target.is_some() {
                self.tick(force);
            }
            next_tick = Instant::now() + JSONL_INTERVAL;
        }
    }

    /// Forgets everything tied to the previous target (the PR cache is global per cwd+branch).
    fn reset(&mut self) {
        self.reader = None;
        self.stats = SessionStats::default();
        self.session = None;
        self.fresh = false;
        self.git = None;
        self.git_cwd = None;
        self.last_git = None;
        self.published = None;
    }

    fn tick(&mut self, force: bool) {
        self.refresh_session();
        let due = force
            || self.last_git.is_none_or(|t| t.elapsed() >= GIT_INTERVAL)
            || self.git_cwd.as_deref() != Some(self.cwd.as_path());
        if due {
            if self.git.is_some() {
                // Show fresh jsonl data without waiting for a slow git.
                self.publish();
            }
            self.git = git_info_cancellable(&self.cwd, Some(&self.shared.cancel));
            self.git_cwd = Some(self.cwd.clone());
            self.last_git = Some(Instant::now());
        }
        self.publish();
    }

    /// Resolves session file + cwd (breadcrumb wins) and refreshes the jsonl stats.
    fn refresh_session(&mut self) {
        let Some(target) = self.target.as_ref() else {
            return;
        };
        let mut session = target.session.clone();
        let mut cwd = target.cwd.clone();
        let mut fresh = false;
        if let Some((mux, name)) = &target.mux
            && let Some(pane) = mux.pane(name)
            && let Some(b) = trusted_breadcrumb(
                &self.agent_dir,
                &pane.breadcrumb_key,
                Some(pane.started),
                target.session.as_deref(),
            )
        {
            session = Some(b.session);
            cwd = b.cwd;
            fresh = b.fresh;
        }
        self.fresh = fresh;
        self.cwd = cwd;
        let Some(path) = session else {
            self.session = None;
            self.reader = None;
            self.stats = SessionStats::default();
            return;
        };
        if self.reader.as_ref().is_none_or(|r| r.path() != path) {
            self.reader = Some(SessionStatsReader::new(path.clone()));
            self.stats = SessionStats::default();
        }
        self.session = Some(path);
        if let Some(reader) = self.reader.as_mut() {
            match reader.refresh() {
                Ok(true) => self.stats = reader.stats().clone(),
                Ok(false) => {}
                Err(e) => {
                    if e.kind() != io::ErrorKind::NotFound {
                        log::debug!("status: reading {}: {e}", reader.path().display());
                    }
                    // Not there (yet, or any more): show an empty session, retry next tick.
                    *reader = SessionStatsReader::new(reader.path().to_path_buf());
                    self.stats = SessionStats::default();
                }
            }
        }
    }

    /// Builds the snapshot (attaching the cached PR; starting a lookup when missing) and publishes
    /// it if it differs from the last one.
    fn publish(&mut self) {
        let mut git = self.git.clone();
        if let Some(g) = &mut git
            && g.branch != "detached"
            && self.gh
        {
            let key = (self.cwd.clone(), g.branch.clone());
            match self.pr_cache.get(&key) {
                Some(pr) => g.pr = pr.clone(),
                None => {
                    if self.pr_pending.insert(key.clone()) {
                        self.spawn_pr_lookup(key);
                    }
                }
            }
        }
        let status = SessionStatus {
            session: self.session.clone(),
            fresh: self.fresh,
            cwd: self.cwd.clone(),
            stats: self.stats.clone(),
            git,
        };
        if self.published.as_deref() == Some(&status) {
            return;
        }
        let status = Arc::new(status);
        {
            let ctl = lock(&self.shared.control);
            if ctl.stop || ctl.generation != self.generation {
                return;
            }
            *lock(&self.shared.latest) = Some(Arc::clone(&status));
        }
        self.published = Some(status);
        (self.on_update)();
    }

    fn spawn_pr_lookup(&self, key: PrKey) {
        let shared = Arc::clone(&self.shared);
        let spawned = std::thread::Builder::new()
            .name("deck-status-pr".into())
            .spawn({
                let shared = Arc::clone(&shared);
                let key = key.clone();
                move || {
                    let (cwd, branch) = &key;
                    let is_default = match default_branch(cwd, &shared.cancel) {
                        Some(d) => d == *branch,
                        None => branch == "main" || branch == "master",
                    };
                    let pr = if is_default {
                        None
                    } else {
                        gh_pr_view(cwd, &shared.cancel)
                    };
                    lock(&shared.control).pr_done.push((key, pr));
                    shared.wake.notify_all();
                }
            });
        if let Err(e) = spawned {
            log::error!("cannot spawn PR lookup thread: {e}");
            // Cache the failure so it is not retried every tick.
            lock(&shared.control).pr_done.push((key, None));
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn breadcrumb_keys_for_both_terminal_forms() {
        assert_eq!(breadcrumb_key_for_tty("/dev/pts/6"), "pts-6");
        assert_eq!(breadcrumb_key_for_tty("/dev/ttys003"), "ttys003");
        assert_eq!(
            breadcrumb_key_for_tty("pts-6"),
            "pts-6",
            "a derived key passes through"
        );
        assert_eq!(
            breadcrumb_key_for_term_session("0123456789abcdef"),
            "apple-0123456789abcdef"
        );
        let agent = Path::new("/a");
        assert_eq!(
            breadcrumb_path(agent, "/dev/pts/6"),
            agent.join("terminal-sessions").join("pts-6")
        );
        assert_eq!(
            breadcrumb_path(agent, "apple-0123456789abcdef"),
            agent
                .join("terminal-sessions")
                .join("apple-0123456789abcdef")
        );
    }

    use super::*;
    use std::io::Write;

    const SESSION: &str = include_str!("../tests/fixtures/status/session.jsonl");
    const ABORTED: &str = include_str!("../tests/fixtures/status/aborted_line.jsonl");

    fn fixture_lines() -> Vec<&'static str> {
        SESSION.lines().collect()
    }

    fn write_lines(path: &Path, lines: &[&str]) {
        let mut s = lines.join("\n");
        s.push('\n');
        fs::write(path, s).unwrap();
    }

    fn append(path: &Path, bytes: &[u8]) {
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    fn assistant_line(
        model: &str,
        input: u64,
        out: u64,
        cost: f64,
        stop: &str,
        text: &str,
    ) -> String {
        format!(
            r#"{{"type":"message","id":"a1","parentId":null,"timestamp":"2026-10-21T13:08:35.545Z","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}],"provider":"p","model":"{model}","usage":{{"input":{input},"output":{out},"cacheRead":0,"cacheWrite":0,"totalTokens":{},"cost":{{"total":{cost}}}}},"stopReason":"{stop}"}}}}"#,
            input + out
        )
    }

    #[test]
    fn fixture_totals_and_latest_values_match_omp_rules() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        fs::write(&p, SESSION).unwrap();
        let mut r = SessionStatsReader::new(p);
        assert!(r.refresh().unwrap());
        let s = r.stats();
        // Σ over 3 assistant messages + 1 model_usage entry, compaction does not reset.
        assert_eq!(s.input_tokens, 17268);
        assert_eq!(s.output_tokens, 3985);
        assert_eq!(s.cache_read, 201_728);
        assert_eq!(s.cache_write, 0);
        assert!((s.cost - 0.005_586_384).abs() < 1e-12, "{}", s.cost);
        assert_eq!(
            s.model.as_deref(),
            Some("commandcode/deepseek/deepseek-v4.1-flash")
        );
        assert_eq!(s.thinking.as_deref(), Some("max"));
        assert_eq!(s.thinking_configured.as_deref(), Some("max"));
        assert_eq!(
            s.title.as_deref(),
            Some("Investigate Hibernate Resume Failure Crash")
        );
        // Last good assistant message carries contextSnapshot.promptTokens.
        assert_eq!(s.context_tokens, Some(203_954));
        assert!(!s.last_turn_failed);
        assert!(
            s.last_assistant_text
                .as_deref()
                .unwrap()
                .starts_with("Cleanup done.")
        );
        // Nothing new: unchanged.
        assert!(!r.refresh().unwrap());
    }

    #[test]
    fn error_and_aborted_turns() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        fs::write(&p, SESSION).unwrap();
        let mut r = SessionStatsReader::new(p.clone());
        r.refresh().unwrap();
        let ctx = r.stats().context_tokens;

        append(&p, assistant_line("m", 0, 0, 0.0, "error", "").as_bytes());
        append(&p, b"\n");
        assert!(r.refresh().unwrap());
        assert!(r.stats().last_turn_failed);
        assert_eq!(
            r.stats().context_tokens,
            ctx,
            "errors never move the context"
        );
        assert!(
            r.stats()
                .last_assistant_text
                .as_deref()
                .unwrap()
                .starts_with("Cleanup")
        );

        // Aborted: zero usage, not a failure, context untouched.
        append(&p, ABORTED.as_bytes());
        assert!(r.refresh().unwrap(), "model switch of the aborted entry");
        assert!(!r.stats().last_turn_failed);
        assert_eq!(r.stats().context_tokens, ctx);
        // The fixture has a model_change: an assistant record of another model does not move the selected model.
        assert_eq!(
            r.stats().model.as_deref(),
            Some("commandcode/deepseek/deepseek-v4.1-flash")
        );
    }

    #[test]
    fn compaction_resets_context_estimate_but_not_totals() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let lines = fixture_lines();
        // Everything up to and including the compaction entry (3rd from last line).
        write_lines(&p, &lines[..lines.len() - 2]);
        let mut r = SessionStatsReader::new(p);
        r.refresh().unwrap();
        assert_eq!(r.stats().context_tokens, Some(78_541));
        assert_eq!(r.stats().input_tokens, 17268 - 2226);
    }

    #[test]
    fn model_and_thinking_follow_later_entries() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        write_lines(&p, &fixture_lines()[..4]);
        let mut r = SessionStatsReader::new(p.clone());
        r.refresh().unwrap();
        assert_eq!(
            r.stats().model.as_deref(),
            Some("anthropic/claude-fable-5-1")
        );
        assert_eq!(r.stats().thinking.as_deref(), Some("high"));
        assert_eq!(r.stats().thinking_configured, None);

        append(
            &p,
            br#"{"type":"model_change","id":"x","parentId":null,"timestamp":"t","model":"openai/gpt-x","role":"default"}
{"type":"thinking_level_change","id":"y","parentId":"x","timestamp":"t","thinkingLevel":null,"configured":"auto"}
{"type":"thinking_level_change","id":"z","parentId":"y","timestamp":"t","thinkingLevel":"low","configured":"auto"}
"#,
        );
        assert!(r.refresh().unwrap());
        assert_eq!(r.stats().model.as_deref(), Some("openai/gpt-x"));
        assert_eq!(r.stats().thinking.as_deref(), Some("low"));
        assert_eq!(r.stats().thinking_configured.as_deref(), Some("auto"));

        // thinkingLevel: null alone means "off".
        append(
            &p,
            br#"{"type":"thinking_level_change","id":"w","parentId":"z","timestamp":"t","thinkingLevel":null,"configured":"off"}
"#,
        );
        r.refresh().unwrap();
        assert_eq!(r.stats().thinking.as_deref(), Some("off"));
    }

    #[test]
    fn the_newest_model_change_wins_over_an_assistant_of_the_old_model() {
        let change = |m: &str| {
            format!(
                r#"{{"type":"model_change","id":"c","parentId":null,"timestamp":"t","model":"{m}","role":"temporary"}}"#
            )
        };
        let reply = |m: &str| assistant_line(m, 10, 2, 0.1, "stop", "ok");
        let model_of = |lines: &[String]| {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("s.jsonl");
            write_lines(&p, &lines.iter().map(String::as_str).collect::<Vec<_>>());
            let mut r = SessionStatsReader::new(p);
            r.refresh().unwrap();
            r.stats().model.clone()
        };
        // A change after the last reply.
        assert_eq!(
            model_of(&[reply("a"), change("p/b")]).as_deref(),
            Some("p/b")
        );
        // A switch mid-turn: the turn's reply is still the old model's, written after the change.
        assert_eq!(
            model_of(&[change("p/b"), reply("a")]).as_deref(),
            Some("p/b")
        );
        // Older files without any model_change: the assistant tells.
        assert_eq!(model_of(&[reply("a")]).as_deref(), Some("p/a"));
    }

    #[test]
    fn a_rewind_forgets_that_a_model_change_was_seen() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let change = r#"{"type":"model_change","id":"c","parentId":null,"timestamp":"t","model":"p/b","role":"temporary"}"#;
        write_lines(
            &p,
            &[change, &assistant_line("a", 10, 2, 0.1, "stop", "ok")],
        );
        let mut r = SessionStatsReader::new(p.clone());
        r.refresh().unwrap();
        assert_eq!(r.stats().model.as_deref(), Some("p/b"));
        // Same inode, shorter: only a reply without any change remains.
        write_lines(&p, &[&assistant_line("a", 1, 1, 0.1, "stop", "x")]);
        r.refresh().unwrap();
        assert_eq!(r.stats().model.as_deref(), Some("p/a"));
    }

    #[test]
    fn title_slot_is_reread_in_place_and_beats_entries() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        fs::write(&p, SESSION).unwrap();
        let mut r = SessionStatsReader::new(p.clone());
        r.refresh().unwrap();
        let before = r.stats().clone();

        // omp rewrites line 0 in place, keeping its 256-byte width.
        let mut slot = r#"{"type":"title","v":1,"title":"🖥️ DECK: Neuer Titel","source":"user","updatedAt":"2026-10-21T12:59:09.501Z","pad":""#.as_bytes().to_vec();
        slot.resize(255 - 2, b' ');
        slot.extend_from_slice(b"\"}\n");
        assert_eq!(slot.len(), 256);
        {
            use std::io::Write;
            let mut f = fs::OpenOptions::new().write(true).open(&p).unwrap();
            f.write_all(&slot).unwrap();
        }
        assert!(r.refresh().unwrap());
        assert_eq!(r.stats().title.as_deref(), Some("🖥️ DECK: Neuer Titel"));
        let mut expected = before;
        expected.title = r.stats().title.clone();
        assert_eq!(r.stats(), &expected, "only the title changed");
        assert!(!r.refresh().unwrap());
    }

    #[test]
    fn untitled_slot_falls_back_to_header_and_title_change() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let slot = format!(
            r#"{{"type":"title","v":1,"title":"","pad":"{}"}}"#,
            " ".repeat(200)
        );
        write_lines(
            &p,
            &[
                &slot,
                r#"{"type":"session","version":3,"id":"i","title":"Header title","timestamp":"t","cwd":"/x"}"#,
            ],
        );
        let mut r = SessionStatsReader::new(p.clone());
        r.refresh().unwrap();
        assert_eq!(r.stats().title.as_deref(), Some("Header title"));
        append(
            &p,
            br#"{"type":"title_change","id":"a","parentId":null,"timestamp":"t","title":"Renamed","source":"user"}
"#,
        );
        r.refresh().unwrap();
        assert_eq!(r.stats().title.as_deref(), Some("Renamed"));
    }

    #[test]
    fn partial_last_line_waits_for_its_newline_even_inside_a_multibyte_char() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        fs::write(&p, SESSION).unwrap();
        let mut r = SessionStatsReader::new(p.clone());
        r.refresh().unwrap();
        let base = r.stats().clone();

        let line = assistant_line("p/new", 100, 7, 0.5, "stop", "héllo → wörld ✓");
        let bytes = line.as_bytes();
        // Split inside the 2-byte 'é'.
        let cut = line.find('é').unwrap() + 1;
        append(&p, &bytes[..cut]);
        assert!(!r.refresh().unwrap(), "no newline yet");
        assert_eq!(r.stats(), &base);
        append(&p, &bytes[cut..bytes.len() - 5]);
        assert!(!r.refresh().unwrap());
        append(&p, &bytes[bytes.len() - 5..]);
        assert!(!r.refresh().unwrap(), "still no newline");
        append(&p, b"\n");
        assert!(r.refresh().unwrap());
        let s = r.stats();
        assert_eq!(s.input_tokens, base.input_tokens + 100);
        assert_eq!(s.output_tokens, base.output_tokens + 7);
        assert_eq!(s.last_assistant_text.as_deref(), Some("héllo → wörld ✓"));
        assert_eq!(s.context_tokens, Some(100));
        assert_eq!(
            s.model.as_deref(),
            Some("commandcode/deepseek/deepseek-v4.1-flash"),
            "the fixture's model_change stays the session's model"
        );
    }

    #[test]
    fn malformed_and_unknown_lines_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let ok = assistant_line("m", 10, 2, 0.25, "stop", "ok");
        write_lines(
            &p,
            &[
                r#"{"type":"message","message":{"role":"assistant","usage":{"input":"#,
                r#"{"type":"future_thing","x":1}"#,
                "not json at all",
                &ok,
            ],
        );
        let mut r = SessionStatsReader::new(p);
        assert!(r.refresh().unwrap());
        assert_eq!(r.stats().input_tokens, 10);
        assert!((r.stats().cost - 0.25).abs() < 1e-12);
    }

    #[test]
    fn inode_change_resets_even_when_the_new_file_is_longer() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        write_lines(&p, &fixture_lines()[..3]);
        let mut r = SessionStatsReader::new(p.clone());
        r.refresh().unwrap();
        assert_eq!(r.stats().input_tokens, 0);

        // omp's rewrite path: temp file + rename.
        let tmp = d.path().join(".s.tmp");
        fs::write(&tmp, SESSION).unwrap();
        fs::rename(&tmp, &p).unwrap();
        assert!(r.refresh().unwrap());
        assert_eq!(
            r.stats().input_tokens,
            17268,
            "re-parsed from byte 0, nothing double counted"
        );
        assert!(!r.refresh().unwrap());
    }

    #[test]
    fn shrink_in_place_resets() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        fs::write(&p, SESSION).unwrap();
        let mut r = SessionStatsReader::new(p.clone());
        r.refresh().unwrap();
        // Same inode, fewer bytes.
        let lines = fixture_lines();
        write_lines(&p, &lines[..lines.len() - 4]);
        assert!(r.refresh().unwrap());
        // Back to: assistant (toolUse) + model_usage; the later error/compaction/reply are gone.
        assert_eq!(r.stats().input_tokens, 8537 + 6505);
        assert_eq!(r.stats().context_tokens, Some(8537));
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    fn usage_line(kind: &str, usage: &str) -> String {
        match kind {
            "side" => format!(r#"{{"type":"model_usage","id":"mu","usage":{usage}}}"#),
            _ => format!(
                r#"{{"type":"message","id":"a","message":{{"role":"assistant","content":[],"usage":{usage},"stopReason":"stop"}}}}"#
            ),
        }
    }

    #[test]
    fn fixture_cost_detail_splits_sources_and_recorded_components() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        fs::write(&p, SESSION).unwrap();
        let mut r = SessionStatsReader::new(p);
        r.refresh().unwrap();
        let s = r.stats();
        let (a, side) = (s.cost_detail.assistant, s.cost_detail.side);
        assert_eq!(
            (a.records, a.itemized, a.unitemized_records, a.uncosted),
            (3, 3, 0, 0)
        );
        assert_eq!((side.records, side.itemized), (1, 1));
        assert!(close(a.total, 0.002_818_434), "{}", a.total);
        assert!(close(side.total, 0.002_767_95), "{}", side.total);
        assert!(close(a.total + side.total, s.cost));
        assert!(close(a.input.usd, 0.001_614_45) && a.input.tokens == 8537 + 2226);
        assert!(close(a.output.usd, 0.000_598_8) && a.output.tokens == 157 + 841);
        assert!(close(a.cache_read.usd, 0.000_605_184) && a.cache_read.tokens == 201_728);
        assert_eq!(
            a.cache_write,
            CostPart::default(),
            "explicit zeros are recorded zeros"
        );
        assert!(close(side.input.usd, 0.000_975_75) && side.input.tokens == 6505);
        assert!(close(side.output.usd, 0.001_792_2) && side.output.tokens == 2987);
        assert!(close(a.unreconciled(), 0.0) && close(side.unreconciled(), 0.0));
    }

    #[test]
    fn records_without_a_full_breakdown_keep_their_totals_and_tokens() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let lines = [
            // Older file: total only.
            assistant_line("m", 10, 2, 0.25, "stop", "old"),
            // No cost at all, a negative total, a non-object cost: tokens stay, total unknown.
            usage_line("asst", r#"{"input":5,"output":1}"#),
            usage_line(
                "asst",
                r#"{"input":7,"cost":{"total":-1,"input":0,"output":0,"cacheRead":0,"cacheWrite":0}}"#,
            ),
            usage_line("side", r#"{"input":3,"cost":"n/a"}"#),
            // Side call missing cacheWrite / with an odd component: unitemized, total kept.
            usage_line(
                "side",
                r#"{"input":4,"output":6,"cost":{"total":0.5,"input":0.1,"output":0.4,"cacheRead":0}}"#,
            ),
            usage_line(
                "side",
                r#"{"output":2,"cost":{"total":0.125,"input":0,"output":"x","cacheRead":0,"cacheWrite":0}}"#,
            ),
            // Components that do not add up to the total.
            usage_line(
                "asst",
                r#"{"input":100,"output":10,"cost":{"total":1.0,"input":0.25,"output":0.5,"cacheRead":0,"cacheWrite":0}}"#,
            ),
        ];
        write_lines(&p, &lines.iter().map(String::as_str).collect::<Vec<_>>());
        let mut r = SessionStatsReader::new(p);
        assert!(r.refresh().unwrap());
        let s = r.stats();
        assert_eq!(s.input_tokens, 10 + 5 + 7 + 3 + 4 + 100);
        assert_eq!(s.output_tokens, 2 + 1 + 6 + 2 + 10);
        assert!(close(s.cost, 0.25 + 0.5 + 0.125 + 1.0), "{}", s.cost);

        let a = s.cost_detail.assistant;
        assert_eq!(
            (a.records, a.itemized, a.unitemized_records, a.uncosted),
            (4, 1, 1, 2)
        );
        assert_eq!(a.uncosted_tokens, 5 + 1 + 7);
        assert_eq!(
            a.unitemized,
            CostPart {
                usd: 0.25,
                tokens: 12
            }
        );
        assert!(close(a.total, 1.25));
        assert_eq!((a.input.tokens, a.output.tokens), (100, 10));
        assert!(close(a.unreconciled(), 0.25), "{}", a.unreconciled());

        let side = s.cost_detail.side;
        assert_eq!(
            (
                side.records,
                side.itemized,
                side.unitemized_records,
                side.uncosted
            ),
            (3, 0, 2, 1)
        );
        assert_eq!(side.uncosted_tokens, 3);
        assert!(close(side.unitemized.usd, 0.625) && side.unitemized.tokens == 12);
        assert!(close(side.components(), 0.0));
        assert!(close(side.unreconciled(), 0.0));
    }

    #[test]
    fn cost_detail_follows_appends_and_in_place_resets() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let lines = fixture_lines();
        let full = {
            let q = d.path().join("full.jsonl");
            fs::write(&q, SESSION).unwrap();
            let mut r = SessionStatsReader::new(q);
            r.refresh().unwrap();
            r.stats().cost_detail
        };
        write_lines(&p, &lines[..lines.len() - 4]);
        let mut r = SessionStatsReader::new(p.clone());
        r.refresh().unwrap();
        let prefix = r.stats().cost_detail;
        assert_eq!((prefix.assistant.records, prefix.side.records), (1, 1));

        let mut rest = lines[lines.len() - 4..].join("\n");
        rest.push('\n');
        append(&p, rest.as_bytes());
        assert!(r.refresh().unwrap());
        assert_eq!(r.stats().cost_detail, full, "appended records add once");

        write_lines(&p, &lines[..lines.len() - 4]);
        assert!(r.refresh().unwrap());
        assert_eq!(
            r.stats().cost_detail,
            prefix,
            "a shrunk file is re-summed from scratch"
        );
        assert!(close(
            r.stats().cost,
            prefix.assistant.total + prefix.side.total
        ));
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let d = tempfile::tempdir().unwrap();
        let mut r = SessionStatsReader::new(d.path().join("nope.jsonl"));
        assert_eq!(r.refresh().unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(r.stats(), &SessionStats::default());
    }

    #[test]
    fn assistant_text_is_capped_on_a_char_boundary() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let big = "é".repeat(MAX_ASSISTANT_TEXT); // 2 bytes each → 512 KiB
        write_lines(&p, &[&assistant_line("m", 1, 1, 0.0, "stop", &big)]);
        let mut r = SessionStatsReader::new(p);
        r.refresh().unwrap();
        let t = r.stats().last_assistant_text.as_deref().unwrap();
        assert_eq!(t.len(), MAX_ASSISTANT_TEXT);
        assert!(t.chars().all(|c| c == 'é'));
    }

    #[test]
    fn context_falls_back_through_context_tokens_then_prompt_sum() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let mk = |usage: &str| {
            format!(
                r#"{{"type":"message","id":"a","message":{{"role":"assistant","content":[],"provider":"p","model":"m","usage":{usage},"stopReason":"stop"}}}}"#
            )
        };
        write_lines(
            &p,
            &[&mk(
                r#"{"input":4,"output":717,"cacheRead":22192,"cacheWrite":1085}"#,
            )],
        );
        let mut r = SessionStatsReader::new(p.clone());
        r.refresh().unwrap();
        assert_eq!(r.stats().context_tokens, Some(23_281));
        append(
            &p,
            format!("{}\n", mk(r#"{"input":1,"output":2,"contextTokens":999}"#)).as_bytes(),
        );
        r.refresh().unwrap();
        assert_eq!(r.stats().context_tokens, Some(999));
        append(
            &p,
            format!("{}\n", mk(r#"{"input":0,"output":30,"totalTokens":530}"#)).as_bytes(),
        );
        r.refresh().unwrap();
        assert_eq!(r.stats().context_tokens, Some(500));
    }

    /// Breadcrumb text names absolute paths: on Windows they carry a drive.
    fn abs(text: &str) -> String {
        if cfg!(windows) {
            text.replace("\n/", "\nC:/").replacen('/', "C:/", 1)
        } else {
            text.to_owned()
        }
    }

    #[test]
    fn breadcrumb_parsing_and_path() {
        let b = parse_breadcrumb(&abs(
            "/home/u/proj\n/home/u/.omp/agent/sessions/-p/a.jsonl\ncwdstat 66306 25455169\n",
        ))
        .unwrap();
        assert_eq!(b.cwd, Path::new(&abs("/home/u/proj")));
        assert_eq!(
            b.session,
            Path::new(&abs("/home/u/.omp/agent/sessions/-p/a.jsonl"))
        );
        assert!(!b.fresh);
        let f = parse_breadcrumb(&abs("/tmp/x\n/tmp/s/a.jsonl\nfresh\ncwdstat 1 2")).unwrap();
        assert!(f.fresh);
        assert_eq!(parse_breadcrumb("/tmp/x\nrelative.jsonl\n"), None);
        assert_eq!(parse_breadcrumb("/tmp/x"), None);
        assert_eq!(parse_breadcrumb(""), None);

        assert_eq!(
            breadcrumb_path(Path::new("/a"), "/dev/pts/11"),
            Path::new("/a/terminal-sessions/pts-11")
        );
        assert_eq!(
            breadcrumb_path(Path::new("/a"), "tmux-%3"),
            Path::new("/a/terminal-sessions/tmux-%3")
        );
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("terminal-sessions")).unwrap();
        fs::write(
            d.path().join("terminal-sessions/pts-7"),
            abs("/w\n/w/s.jsonl\nfresh\ncwdstat 1 2\n"),
        )
        .unwrap();
        assert_eq!(
            read_breadcrumb(d.path(), "/dev/pts/7"),
            Some(Breadcrumb {
                cwd: abs("/w").into(),
                session: abs("/w/s.jsonl").into(),
                fresh: true
            })
        );
        assert_eq!(read_breadcrumb(d.path(), "/dev/pts/8"), None);
        assert_eq!(read_breadcrumb(d.path(), ""), None);
    }

    #[test]
    fn breadcrumb_older_than_the_pane_is_distrusted() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("terminal-sessions");
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pts-3");
        fs::write(&f, abs("/w\n/w/s.jsonl\ncwdstat 1 2\n")).unwrap();
        let now = SystemTime::now();
        let past = Some(now - Duration::from_secs(60));
        let future = Some(now + Duration::from_secs(60));
        let tty = "/dev/pts/3";
        let other = PathBuf::from(abs("/w/other.jsonl"));
        assert!(trusted_breadcrumb(d.path(), tty, past, None).is_some());
        // Written before the pane's process started: a previous pty owner's leftovers…
        assert!(trusted_breadcrumb(d.path(), tty, future, None).is_none());
        assert!(trusted_breadcrumb(d.path(), tty, future, Some(other.as_path())).is_none());
        // …unless it names the session the tab already runs (omp skips rewriting an unchanged
        // breadcrumb on `--resume`).
        assert!(
            trusted_breadcrumb(d.path(), tty, future, Some(Path::new(&abs("/w/s.jsonl"))))
                .is_some()
        );
        // Unknown start time and no known session: rejected.
        assert!(trusted_breadcrumb(d.path(), tty, None, None).is_none());
    }

    #[test]
    fn breadcrumb_trust_rule() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(60);
        let s = Path::new("/w/s.jsonl");
        let other = Path::new("/w/other.jsonl");
        // mtime >= start: trusted whatever the tab knew.
        assert!(breadcrumb_trusted(s, Some(now), Some(old), Some(other)));
        // Older than the start: only the known session.
        assert!(breadcrumb_trusted(s, Some(old), Some(now), Some(s)));
        assert!(!breadcrumb_trusted(s, Some(old), Some(now), Some(other)));
        // Unknown start: trusted only when it names the known session; no known session → rejected.
        assert!(breadcrumb_trusted(s, None, None, Some(s)));
        assert!(!breadcrumb_trusted(s, None, None, Some(other)));
        assert!(!breadcrumb_trusted(s, None, None, None));
    }

    #[test]
    fn git_v2_branch_counts() {
        let out = "\
# branch.oid 0123456789abcdef0123456789abcdef01234567
# branch.head feat/status
# branch.upstream origin/feat/status
# branch.ab +3 -1
1 .M N... 100644 100644 100644 aaaa bbbb src/modified.rs
1 A. N... 000000 100644 100644 0000 cccc src/new.rs
1 MM N... 100644 100644 100644 dddd eeee src/both.rs
2 R. N... 100644 100644 100644 ffff 1111 R100 src/renamed.rs\tsrc/old.rs
1 .D N... 100644 100644 000000 2222 2222 src/deleted.rs
u UU N... 100644 100644 100644 100644 3333 4444 5555 src/conflict.rs
? untracked_dir/
? untracked.txt
";
        let g = parse_git_status_v2(out).unwrap();
        assert_eq!(g.branch, "feat/status");
        assert_eq!((g.ahead, g.behind), (3, 1));
        // staged: A., MM, R., u UU? (X='U' no) → 3 ; unstaged: .M, MM, .D, UU → 4
        assert_eq!((g.staged, g.unstaged, g.untracked), (3, 4, 2));
        assert_eq!(g.pr, None);
    }

    #[test]
    fn git_v2_detached_initial_and_clean() {
        let detached = "# branch.oid abcdef\n# branch.head (detached)\n";
        let g = parse_git_status_v2(detached).unwrap();
        assert_eq!(
            g,
            GitInfo {
                branch: "detached".into(),
                ..GitInfo::default()
            }
        );

        let initial = "# branch.oid (initial)\n# branch.head main\n? a.txt\n";
        let g = parse_git_status_v2(initial).unwrap();
        assert_eq!(
            (g.branch.as_str(), g.ahead, g.behind, g.untracked),
            ("main", 0, 0, 1)
        );

        assert_eq!(parse_git_status_v2(""), None, "not a repository");
        assert_eq!(parse_git_status_v2("fatal: not a git repository\n"), None);
    }

    #[test]
    fn git_info_on_a_real_repo() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                    "-C",
                ])
                .arg(d.path())
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q", "-b", "trunk"]);
        fs::write(d.path().join("a"), "1").unwrap();
        fs::write(d.path().join("b"), "1").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "x"]);
        fs::write(d.path().join("a"), "2").unwrap(); // unstaged
        fs::write(d.path().join("c"), "1").unwrap();
        git(&["add", "c"]); // staged
        fs::create_dir(d.path().join("dir")).unwrap();
        fs::write(d.path().join("dir/x"), "1").unwrap();
        fs::write(d.path().join("dir/y"), "1").unwrap(); // one collapsed untracked dir
        let g = git_info(d.path()).unwrap();
        assert_eq!(g.branch, "trunk");
        assert_eq!((g.staged, g.unstaged, g.untracked), (1, 1, 1));
        assert_eq!(git_info(&d.path().join("missing")), None);
    }

    #[test]
    fn pr_view_json() {
        assert_eq!(
            parse_pr_view(br#"{"number":12,"url":"https://github.com/o/r/pull/12"}"#),
            Some(PrInfo {
                number: 12,
                url: "https://github.com/o/r/pull/12".into()
            })
        );
        assert_eq!(parse_pr_view(b"no pull requests found"), None);
    }

    #[test]
    fn run_capture_kills_on_timeout_and_cancel() {
        let mut c = Command::new("sleep");
        c.arg("30");
        let t = Instant::now();
        assert_eq!(run_capture(c, Duration::from_millis(150), None), None);
        assert!(t.elapsed() < Duration::from_secs(5));

        let cancel = AtomicBool::new(true);
        let mut c = Command::new("sleep");
        c.arg("30");
        let t = Instant::now();
        assert_eq!(run_capture(c, Duration::from_secs(30), Some(&cancel)), None);
        assert!(t.elapsed() < Duration::from_secs(5));

        let mut c = Command::new("echo");
        c.arg("hi");
        assert_eq!(
            run_capture(c, Duration::from_secs(5), None).unwrap(),
            b"hi\n"
        );
    }

    /// An assistant entry; `ts` is the request start, as omp stamps `message.timestamp`.
    fn tail_msg(stop: &str, extra: &str, ts: u64) -> String {
        format!(
            r#"{{"type":"message","id":"a","parentId":null,"timestamp":"2026-10-21T13:08:35.545Z","message":{{"role":"assistant","content":[],"stopReason":"{stop}"{extra},"timestamp":{ts}}}}}"#
        )
    }

    #[test]
    fn last_turn_outcome_classifies_the_last_assistant_message() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let user = r#"{"type":"message","id":"u","message":{"role":"user","content":[]}}"#;
        let w = |lines: &[&str]| write_lines(&p, lines);

        w(&[&tail_msg("stop", r#","completedAt":7000"#, 1000), user]);
        assert_eq!(last_turn_outcome(&p), TurnOutcome::Completed);
        assert_eq!(
            last_turn(&p).assistant_at,
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(7000))
        );
        w(&[&tail_msg("toolUse", "", 1)]);
        assert_eq!(last_turn_outcome(&p), TurnOutcome::Completed);
        w(&[&tail_msg("stop", "", 1), &tail_msg("aborted", "", 2)]);
        assert_eq!(last_turn_outcome(&p), TurnOutcome::Aborted);
        w(&[&tail_msg(
            "error",
            r#","errorMessage":"429 rate limited""#,
            3,
        )]);
        assert_eq!(
            last_turn_outcome(&p),
            TurnOutcome::Error(Some("429 rate limited".into()))
        );
        w(&[&tail_msg("error", "", 3)]);
        assert_eq!(last_turn_outcome(&p), TurnOutcome::Error(None));
        let long = format!(r#","errorMessage":"{}""#, "é".repeat(500));
        w(&[&tail_msg("error", &long, 3)]);
        assert_eq!(
            last_turn_outcome(&p),
            TurnOutcome::Error(Some("é".repeat(200)))
        );

        w(&[user, r#"{"type":"title_change","title":"x"}"#]);
        assert_eq!(last_turn_outcome(&p), TurnOutcome::Unknown);
        assert_eq!(last_turn(&p).assistant_at, None);
        assert_eq!(
            last_turn_outcome(&d.path().join("nope.jsonl")),
            TurnOutcome::Unknown
        );
    }

    #[test]
    fn last_turn_outcome_skips_a_truncated_last_line() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let full = tail_msg("aborted", "", 5);
        let cut = tail_msg("stop", "", 6);
        let mut text = format!("{full}\n");
        text.push_str(&cut[..cut.len() - 10]);
        fs::write(&p, text).unwrap();
        assert_eq!(last_turn_outcome(&p), TurnOutcome::Aborted);
    }

    #[test]
    fn last_turn_outcome_reads_only_the_tail_window() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let filler = format!(
            r#"{{"type":"message","id":"u","message":{{"role":"user","content":[{{"type":"text","text":"{}"}}]}}}}"#,
            "x".repeat(64 * 1024)
        );
        // Assistant message beyond the window → Unknown, even though the window's first line is partial.
        let mut lines = vec![tail_msg("stop", "", 1)];
        lines.extend(std::iter::repeat_n(filler.clone(), 5));
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_lines(&p, &refs);
        assert_eq!(last_turn_outcome(&p), TurnOutcome::Unknown);

        // Within the window it is found.
        let mut lines = vec![filler.clone(), tail_msg("error", "", 1)];
        lines.extend(std::iter::repeat_n(filler, 2));
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_lines(&p, &refs);
        assert_eq!(last_turn_outcome(&p), TurnOutcome::Error(None));
    }

    #[test]
    fn last_turn_finds_an_assistant_line_starting_exactly_at_the_window() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let a = tail_msg("error", "", 1);
        let pad = |n: usize| format!(r#"{{"type":"x","pad":"{}"}}"#, "x".repeat(n));
        let base = pad(0).len();
        let f = pad(TAIL_WINDOW as usize - a.len() - 2 - base);
        fs::write(&p, format!("{}\n{a}\n{f}\n", pad(100))).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().len() - TAIL_WINDOW,
            pad(100).len() as u64 + 1
        );
        assert_eq!(last_turn_outcome(&p), TurnOutcome::Error(None));
        // One byte more pushes the line's first byte out of the window.
        fs::write(&p, format!("{}\n{a}\n{f}x\n", pad(100))).unwrap();
        assert_eq!(last_turn_outcome(&p), TurnOutcome::Unknown);
    }

    #[test]
    fn last_turn_time_is_when_the_reply_finished() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let at = |ms: u64| Some(SystemTime::UNIX_EPOCH + Duration::from_millis(ms));
        // Real shape: the request start precedes completion by seconds.
        let both = tail_msg("error", r#","completedAt":1792896486312"#, 1792896480280);
        write_lines(&p, &[&both]);
        assert_eq!(last_turn(&p).assistant_at, at(1792896486312));
        // No completedAt: the entry's ISO timestamp (2026-10-21T13:08:35.545Z), not the start.
        write_lines(&p, &[&tail_msg("stop", "", 1)]);
        assert_eq!(last_turn(&p).assistant_at, at(1_792_588_115_545));
        // Neither: the message's own timestamp.
        let bare = r#"{"type":"message","message":{"role":"assistant","stopReason":"stop","timestamp":77}}"#;
        write_lines(&p, &[bare]);
        assert_eq!(last_turn(&p).assistant_at, at(77));
    }

    #[test]
    fn last_turn_keeps_the_outcome_when_the_time_is_unrepresentable() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let line = r#"{"type":"message","message":{"role":"assistant","stopReason":"aborted","completedAt":1e300,"timestamp":1e300}}"#;
        write_lines(&p, &[line]);
        let t = last_turn(&p);
        assert_eq!(t.outcome, TurnOutcome::Aborted);
        assert_eq!(t.assistant_at, None);
    }
}
