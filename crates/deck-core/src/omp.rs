//! Thin wrapper around the `omp` CLI: settings catalog + editing, update checks, session
//! move/copy argv. omp performs every write to its own store/config; OmpDeck never edits
//! `~/.omp/**` itself, except the user-authored agent definitions ([`crate::agent_defs`]), which
//! omp only reads. Every call here blocks: run them on a worker thread.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value};

mod plugins;
mod sess;
mod usage_reset;

pub use plugins::{
    MarketplaceEntry, MarketplacePlugin, NpmPlugin, PluginConfig, PluginFeature, PluginFeatures,
    PluginList, PluginManifest, PluginScope, PluginSetting, PluginSettingKind,
};
pub use sess::{
    AdvisorDoc, AdvisorEntry, AdvisorSaved, AdvisorScope, ForeignSession, ForeignSource, SessError,
};
pub use usage_reset::{ResetAccount, ResetAccounts, ResetCredit};

use crate::platform;

#[derive(Debug, thiserror::Error)]
pub enum OmpError {
    #[error("failed to run omp: {0}")]
    Spawn(#[from] std::io::Error),
    /// Non-zero exit. `message` = trimmed stderr with a leading "Error: "/"error: " removed
    /// (falls back to stdout when stderr is empty). Never contains a secret value.
    #[error("{message}")]
    Failed { code: Option<i32>, message: String },
    #[error("unexpected omp output: {0}")]
    Parse(String),
}

/// One session to delete: the file and the session id it held when the user selected it. The id
/// is re-checked under the session lease, so a file replaced since selection is never deleted.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeleteTarget {
    pub path: PathBuf,
    pub id: String,
}

/// Why [`Omp::delete_sessions`] did not delete a target.
#[derive(Debug, thiserror::Error)]
pub enum DeleteError {
    /// A live omp holds the session lease; nothing was removed.
    #[error("session is open in a running omp")]
    Live,
    /// Not an omp session file (wrong name, not a regular file, or no session header).
    #[error("not an omp session file")]
    NotASession,
    /// omp or the delete extension failed; the text is omp's own (may mean a partial delete).
    #[error("{0}")]
    Omp(String),
    #[error("delete failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("omp did not finish deleting in time")]
    Timeout,
    /// The file at the path no longer holds the session that was selected (replaced, renamed or
    /// removed); nothing was removed. Not retried.
    #[error("the session file changed since it was selected")]
    Changed,
}

#[derive(Clone, Debug)]
pub struct Omp {
    program: PathBuf,
    /// Extra environment for every command except the defaults probe (tests / sandboxes).
    env: Vec<(OsString, OsString)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UpdateKind {
    Omp,
    Plugins,
    Canary,
    Stable,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpdateStatus {
    pub current: String,
    /// `None` = up to date.
    pub latest: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MoveOutcome {
    pub new_path: PathBuf,
    pub kind: MoveKind,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MoveKind {
    Created,
    Replaced,
    /// The local copy diverged: omp unpacked a fork with a new id and left the original.
    Forked {
        new_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SettingsCatalog {
    pub tabs: Vec<SettingsTab>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SettingsTab {
    pub id: String,
    pub label: String,
    pub settings: Vec<Setting>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Setting {
    pub key: String,
    pub kind: SettingKind,
    /// Effective value. `None` = unset. For a secret that is set this is the placeholder
    /// string `"********"`, never the real value.
    pub value: Option<Value>,
    pub default: Option<Value>,
    pub description: String,
    pub secret: bool,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SettingKind {
    Bool,
    Number,
    Enum(Vec<String>),
    String,
    Array,
    Record,
}

/// Placeholder stored in [`Setting::value`] for a secret that is set.
pub const SECRET_PLACEHOLDER: &str = "********";

/// Credential keys of omp 18.12.3 (`credential: true` / `secret: true`); omp's JSON only reveals
/// redaction once a value is set.
const KNOWN_SECRET_KEYS: &[&str] = &[
    "auth.broker.token",
    "dev.autoqaPush.token",
    "mnemopi.embeddingApiKey",
    "mnemopi.llmApiKey",
    "hindsight.apiToken",
    "searxng.token",
    "searxng.basicPassword",
    "images.urls.credentials",
];

/// omp's `/settings` tab order, then anything unknown, then `internal`.
const TAB_ORDER: &[(&str, &str)] = &[
    ("appearance", "Appearance"),
    ("model", "Model"),
    ("interaction", "Interaction"),
    ("context", "Context"),
    ("memory", "Memory"),
    ("files", "Files"),
    ("shell", "Shell"),
    ("tools", "Tools"),
    ("tasks", "Tasks"),
    ("providers", "Providers"),
];

impl Setting {
    /// Last key segment humanized: `compactThinkingLevel` → "Compact thinking level".
    pub fn label(&self) -> String {
        let seg = self.key.rsplit('.').next().unwrap_or(&self.key);
        humanize(seg)
    }

    /// Key up to the last '.', "" if none (`statusLine.preset` → `statusLine`).
    pub fn section(&self) -> &str {
        self.key.rsplit_once('.').map_or("", |(s, _)| s)
    }

    /// Effective value differs from the default (secret: a value is set).
    pub fn is_modified(&self) -> bool {
        if self.secret {
            return self.value.is_some();
        }
        match (&self.value, &self.default) {
            (None, _) => false,
            (Some(v), Some(d)) => v != d,
            (Some(_), None) => true,
        }
    }
}

fn humanize(seg: &str) -> String {
    let chars: Vec<char> = seg.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let prev = i.checked_sub(1).map(|p| chars[p]);
        let next = chars.get(i + 1).copied();
        let boundary = c.is_uppercase()
            && prev.is_some_and(|p| {
                p.is_lowercase()
                    || p.is_ascii_digit()
                    || (p.is_uppercase() && next.is_some_and(char::is_lowercase))
            });
        if boundary && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let mut out = String::with_capacity(seg.len() + 4);
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let acronym = w.chars().count() > 1 && w.chars().all(|c| !c.is_lowercase());
        if acronym {
            out.push_str(w);
        } else if i == 0 {
            let mut it = w.chars();
            if let Some(f) = it.next() {
                out.extend(f.to_uppercase());
                out.push_str(&it.as_str().to_lowercase());
            }
        } else {
            out.push_str(&w.to_lowercase());
        }
    }
    out
}

/// argv for a terminal tab (same form as `omp --resume`): `["omp","--fork",<src>,"--cwd",<dest>]`.
pub fn fork_argv(src: &Path, dest_cwd: &Path) -> Vec<String> {
    vec![
        "omp".into(),
        "--fork".into(),
        src.to_string_lossy().into_owned(),
        "--cwd".into(),
        dest_cwd.to_string_lossy().into_owned(),
    ]
}

/// How omp reads a model's token-rate card (`pricingStatus` of `omp models --json`): `Fixed`
/// means the `cost` rates apply; omp reports `Unknown` for an all-zero card it cannot call free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Pricing {
    Fixed,
    Free,
    Included,
    Variable,
    Unknown,
}

/// Output speed measured on this machine: omp's `model_perf` aggregate in `agent.db`, the speed
/// omp's model browser prefers over the catalog estimate.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MeasuredSpeed {
    /// Output tokens per second over the decode window.
    pub tps: f64,
    /// Average time to first token; `None` when no sample measured it.
    pub ttft_ms: Option<f64>,
    /// Recency-weighted sample count (omp halves it past 256, so it can be fractional).
    pub samples: f64,
    /// Service tier of the aggregate (`ultrafast`) when no standard-tier measurement exists.
    pub tier: Option<String>,
}

/// One entry of the `omp models --json` catalog, plus the catalog metrics and measured speed omp's
/// model browser shows. Every metadata field is `None`/empty when omp does not report it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    /// `provider/id`, what `/switch` takes.
    pub selector: String,
    pub provider: String,
    pub id: String,
    pub name: String,
    pub context_window: Option<u64>,
    /// Output-token cap.
    pub max_tokens: Option<u64>,
    pub reasoning: Option<bool>,
    /// Supported effort levels in omp's order; empty = the model has no thinking.
    pub thinking: Vec<String>,
    /// Input modalities (`text`, `image`).
    pub input: Vec<String>,
    /// $ per million tokens.
    pub cost_input: Option<f64>,
    pub cost_output: Option<f64>,
    pub cost_cache_read: Option<f64>,
    pub pricing: Option<Pricing>,
    /// omp's catalog intelligence score (`int`).
    pub intelligence: Option<f64>,
    /// omp's catalog output-speed estimate in tokens/s (`tps`); `None` when unmeasured.
    pub catalog_tps: Option<f64>,
    pub measured: Option<MeasuredSpeed>,
}

/// omp's catalog metrics of one model object: `int` and `tps`. A non-positive speed means
/// "unmeasured" in omp (`catalogMetricsOf`), not a score.
fn catalog_metrics(m: &Value) -> (Option<f64>, Option<f64>) {
    let num = |k: &str| m.get(k).and_then(Value::as_f64).filter(|v| v.is_finite());
    (num("int"), num("tps").filter(|t| *t > 0.0))
}

/// Parses `omp models --json` (`{"models":[{provider,id,selector,name,contextWindow,maxTokens,
/// reasoning,thinking,input,cost:{input,output,cacheRead},pricingStatus}…]}`). Entries without
/// provider or id are skipped; a missing selector becomes `provider/id`, a missing name the id;
/// `thinking: null` → empty. Unknown `pricingStatus` values are left unset.
pub fn parse_models(stdout: &str) -> Result<Vec<ModelInfo>, OmpError> {
    let parse_err = |e: serde_json::Error| OmpError::Parse(format!("models: {e}"));
    let start = stdout.find('{').unwrap_or(0);
    let root: Value = serde_json::from_str(&stdout[start..]).map_err(parse_err)?;
    let list = root
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| OmpError::Parse("models: no \"models\" array".into()))?;
    let text = |m: &Value, k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let strings = |m: &Value, k: &str| -> Vec<String> {
        m.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(list
        .iter()
        .filter_map(|m| {
            let provider = text(m, "provider")?;
            let id = text(m, "id")?;
            let cost = |k: &str| m.get("cost").and_then(|c| c.get(k)).and_then(Value::as_f64);
            let (intelligence, catalog_tps) = catalog_metrics(m);
            Some(ModelInfo {
                selector: text(m, "selector").unwrap_or_else(|| format!("{provider}/{id}")),
                name: text(m, "name").unwrap_or_else(|| id.clone()),
                context_window: m.get("contextWindow").and_then(Value::as_u64),
                max_tokens: m.get("maxTokens").and_then(Value::as_u64),
                reasoning: m.get("reasoning").and_then(Value::as_bool),
                thinking: strings(m, "thinking"),
                input: strings(m, "input"),
                cost_input: cost("input"),
                cost_output: cost("output"),
                cost_cache_read: cost("cacheRead"),
                pricing: match m.get("pricingStatus").and_then(Value::as_str) {
                    Some("fixed") => Some(Pricing::Fixed),
                    Some("free") => Some(Pricing::Free),
                    Some("included") => Some(Pricing::Included),
                    Some("variable") => Some(Pricing::Variable),
                    Some("unknown") => Some(Pricing::Unknown),
                    _ => None,
                },
                intelligence,
                catalog_tps,
                measured: None,
                provider,
                id,
            })
        })
        .collect())
}

/// Fills the `intelligence`/`catalog_tps` that `omp models --json` leaves out from `rpc_models`,
/// the `data.models` of omp's RPC `get_available_models`: the same registry models after omp's
/// catalog-metric pass (scores matched across provider id dialects), keyed by `provider/id`.
pub fn apply_catalog_metrics(models: &mut [ModelInfo], rpc_models: &Value) {
    let Some(list) = rpc_models.as_array() else {
        return;
    };
    let by_key: std::collections::HashMap<String, (Option<f64>, Option<f64>)> = list
        .iter()
        .filter_map(|m| {
            let provider = m.get("provider")?.as_str()?;
            let id = m.get("id")?.as_str()?;
            Some((format!("{provider}/{id}"), catalog_metrics(m)))
        })
        .collect();
    for m in models {
        if let Some(&(int, tps)) = by_key.get(&format!("{}/{}", m.provider, m.id)) {
            m.intelligence = m.intelligence.or(int);
            m.catalog_tps = m.catalog_tps.or(tps);
        }
    }
}

/// One `model_perf` row of omp's `agent.db`: running sums keyed `provider/id`, or
/// `provider/id@tier` for a non-default service tier.
#[derive(Clone, Debug, PartialEq)]
pub struct PerfRow {
    pub key: String,
    pub samples: f64,
    pub output_tokens: f64,
    pub gen_ms: f64,
    pub ttft_samples: f64,
    pub ttft_ms: f64,
}

/// Reads `model_perf` from omp's `agent.db` without writing anything; a missing database or
/// table is no measurements.
pub fn read_model_perf(db: &Path) -> Result<Vec<PerfRow>, OmpError> {
    if !db.exists() {
        return Ok(Vec::new());
    }
    let db_err = |e: rusqlite::Error| OmpError::Parse(format!("model_perf: {e}"));
    let conn = crate::usage::open_stats_readonly(db).map_err(db_err)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))
        .map_err(db_err)?;
    let has_table: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='model_perf')",
            [],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    if !has_table {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT model_key, samples, output_tokens, gen_ms, ttft_samples, ttft_ms \
             FROM model_perf WHERE model_key IS NOT NULL",
        )
        .map_err(db_err)?;
    stmt.query_map([], |r| {
        Ok(PerfRow {
            key: r.get(0)?,
            samples: r.get(1)?,
            output_tokens: r.get(2)?,
            gen_ms: r.get(3)?,
            ttft_samples: r.get(4)?,
            ttft_ms: r.get(5)?,
        })
    })
    .map_err(db_err)?
    .collect::<rusqlite::Result<_>>()
    .map_err(db_err)
}

/// Sets each model's [`MeasuredSpeed`] the way omp's model browser picks it: the standard
/// `provider/id` aggregate, else the fastest `provider/id@tier` one (first wins ties). Rows
/// without output tokens or decode time are unmeasured and skipped.
pub fn apply_measured_speed(models: &mut [ModelInfo], rows: &[PerfRow]) {
    let speed = |r: &PerfRow, tier: Option<&str>| MeasuredSpeed {
        tps: r.output_tokens * 1000.0 / r.gen_ms,
        ttft_ms: (r.ttft_samples > 0.0).then(|| r.ttft_ms / r.ttft_samples),
        samples: r.samples,
        tier: tier.map(str::to_owned),
    };
    let measured = || {
        rows.iter()
            .filter(|r| r.gen_ms > 0.0 && r.output_tokens > 0.0)
    };
    let mut best: std::collections::HashMap<&str, MeasuredSpeed> = std::collections::HashMap::new();
    for r in measured() {
        // Every `@` may end the selector: `a@b@t` is tier `b@t` of `a` and tier `t` of `a@b`.
        for (at, _) in r.key.match_indices('@') {
            let s = speed(r, Some(&r.key[at + 1..]));
            let owner = &r.key[..at];
            if best.get(owner).is_none_or(|b| s.tps > b.tps) {
                best.insert(owner, s);
            }
        }
    }
    for m in models {
        let key = format!("{}/{}", m.provider, m.id);
        m.measured = measured()
            .find(|r| r.key == key)
            .map(|r| speed(r, None))
            .or_else(|| best.get(key.as_str()).cloned());
    }
}

/// argv for an update tab.
pub fn update_argv(kind: UpdateKind) -> Vec<String> {
    let mut v = vec!["omp".to_string(), "update".to_string()];
    match kind {
        UpdateKind::Omp => {}
        UpdateKind::Plugins => v.push("--plugins".into()),
        UpdateKind::Canary => v.push("--canary".into()),
        UpdateKind::Stable => v.push("--stable".into()),
    }
    v
}

/// omp's official installer script (the PowerShell one on Windows).
pub const INSTALL_URL: &str = if cfg!(windows) {
    "https://omp.sh/install.ps1"
} else {
    "https://omp.sh/install"
};

/// argv for an "Install omp" tab. Unix: fetch [`INSTALL_URL`] with curl (else wget) and run it with
/// `--binary`, which installs a prebuilt omp to `~/.local/bin` without bun or root; a failed
/// download fails the whole command instead of running an empty script. Windows: the same
/// installer through PowerShell, installing `%LOCALAPPDATA%\omp\omp.exe`.
pub fn install_argv() -> Vec<String> {
    if cfg!(windows) {
        return powershell_install_argv();
    }
    let script = format!(
        "set -e\n\
         if command -v curl >/dev/null 2>&1; then s=$(curl -fsSL {INSTALL_URL})\n\
         elif command -v wget >/dev/null 2>&1; then s=$(wget -qO- {INSTALL_URL})\n\
         else echo 'Installing omp needs curl or wget.' >&2; exit 1\n\
         fi\n\
         printf '%s\\n' \"$s\" | sh -s -- --binary\n"
    );
    vec!["sh".into(), "-c".into(), script]
}

/// PowerShell argv that runs the `install.ps1` installer in binary mode.
fn powershell_install_argv() -> Vec<String> {
    let command = format!("& ([scriptblock]::Create((irm {INSTALL_URL}))) -Binary");
    [
        "powershell.exe",
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain([command])
    .collect()
}

/// One target's NDJSON line from `ompdeck-del.ts`.
fn delete_result(v: &Value) -> Result<(), DeleteError> {
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    match v.get("error").and_then(Value::as_str) {
        Some("live") => Err(DeleteError::Live),
        Some("changed") => Err(DeleteError::Changed),
        Some(m) => Err(DeleteError::Omp(m.to_string())),
        None => Err(DeleteError::Omp("delete failed".into())),
    }
}

/// `a` and `b` name the same directory (textually, else after resolving links).
fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

fn failed(message: impl Into<String>) -> OmpError {
    OmpError::Failed {
        code: None,
        message: message.into(),
    }
}
/// Ends an rpc child: closing stdin ends rpc mode (omp flushes and exits); a process still alive
/// after 5 s is killed and reaped with its tree. Returns the exit code when it exited on its own.
fn shut_down(child: platform::KillableChild, stdin: std::process::ChildStdin) -> Option<i32> {
    use std::time::{Duration, Instant};

    let mut child = child;
    drop(stdin);
    let end = Instant::now() + Duration::from_secs(5);
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
            _ => break None,
        }
    };
    drop(child);
    code
}

/// Value → the single CLI argument: bool "true"/"false", number as JSON number text, enum/string
/// raw text (JSON-quoted when omp would otherwise alter it: leading/trailing whitespace, a leading
/// quote, line breaks), array/record compact JSON.
pub fn encode_value(kind: &SettingKind, value: &Value) -> Result<String, OmpError> {
    match (kind, value) {
        (SettingKind::Bool, Value::Bool(b)) => Ok(b.to_string()),
        (SettingKind::Number, Value::Number(n)) => Ok(n.to_string()),
        (SettingKind::Enum(choices), Value::String(s)) => {
            if !choices.is_empty() && !choices.iter().any(|c| c == s) {
                return Err(failed(format!(
                    "Invalid value: {s}. Valid values: {}",
                    choices.join(", ")
                )));
            }
            Ok(s.clone())
        }
        (SettingKind::String, Value::String(s)) => {
            let needs_quote = s.trim() != s || s.starts_with('"') || s.contains(['\n', '\r']);
            if needs_quote {
                Ok(Value::String(s.clone()).to_string())
            } else {
                Ok(s.clone())
            }
        }
        (SettingKind::Array, Value::Array(_)) | (SettingKind::Record, Value::Object(_)) => {
            Ok(value.to_string())
        }
        (kind, _) => Err(failed(format!(
            "Expected a {} value",
            match kind {
                SettingKind::Bool => "boolean",
                SettingKind::Number => "number",
                SettingKind::Enum(_) | SettingKind::String => "string",
                SettingKind::Array => "JSON array",
                SettingKind::Record => "JSON object",
            }
        ))),
    }
}

fn strip_error_prefix(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix("Error: ")
        .or_else(|| s.strip_prefix("error: "))
        .unwrap_or(s)
}

/// Builds the error for a non-zero exit. A runtime crash dump (source excerpt, caret, the
/// `error: …` line, stack frames) is reduced to its `error:` line; the full text is logged.
fn failure(code: Option<i32>, stdout: &str, stderr: &str) -> OmpError {
    let src = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let message = match src
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with("error: ") || l.starts_with("Error: "))
    {
        Some(line) if src.trim().lines().nth(1).is_some() => {
            log::debug!("omp failed (exit {code:?}): {}", src.trim());
            strip_error_prefix(line).to_string()
        }
        _ => strip_error_prefix(src).to_string(),
    };
    OmpError::Failed { code, message }
}

/// Parses `omp update --check` stdout.
pub fn parse_update_check(stdout: &str) -> Result<UpdateStatus, OmpError> {
    let mut current = None;
    let mut latest = None;
    let mut up_to_date = false;
    for line in stdout.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("Current version:") {
            current = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("New version available:") {
            latest = Some(v.trim().to_string());
        } else if line.ends_with("Already up to date") {
            up_to_date = true;
        }
    }
    match (current, latest, up_to_date) {
        (Some(current), Some(latest), _) if !latest.is_empty() => Ok(UpdateStatus {
            current,
            latest: Some(latest),
        }),
        (Some(current), None, true) => Ok(UpdateStatus {
            current,
            latest: None,
        }),
        _ => Err(OmpError::Parse(stdout.trim().to_string())),
    }
}

/// Parses a successful `omp session unpack`: stdout = new path, stderr = mode line.
pub fn parse_unpack(stdout: &str, stderr: &str) -> Result<MoveOutcome, OmpError> {
    let path = stdout
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .ok_or_else(|| OmpError::Parse("unpack printed no session path".into()))?;
    let mut kind = None;
    for line in stderr.lines().map(str::trim) {
        if line.starts_with("Replaced the local copy of this session") {
            kind = Some(MoveKind::Replaced);
        } else if let Some(rest) = line.strip_prefix("The local copy of this session diverged;") {
            let id = rest
                .rsplit_once('(')
                .and_then(|(_, r)| r.split_once(')'))
                .map(|(id, _)| id.to_string())
                .ok_or_else(|| OmpError::Parse(line.to_string()))?;
            kind = Some(MoveKind::Forked { new_id: id });
        } else if line.starts_with("Unpacked session") {
            kind = Some(MoveKind::Created);
        }
    }
    let kind = kind
        .ok_or_else(|| OmpError::Parse(format!("unpack mode not recognized: {}", stderr.trim())))?;
    Ok(MoveOutcome {
        new_path: PathBuf::from(path),
        kind,
    })
}

struct TextRow<'a> {
    key: &'a str,
    group: &'a str,
}

/// `  key = value (group)` → (key, group); the group is the text after the LAST " (".
fn parse_text_row(line: &str) -> Option<TextRow<'_>> {
    let rest = line.strip_prefix("  ")?;
    let (key, tail) = rest.split_once(" = ")?;
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    let body = tail.strip_suffix(')')?;
    let (_, group) = body.rsplit_once(" (")?;
    Some(TextRow { key, group })
}

fn tab_header(line: &str) -> Option<&str> {
    let id = line.strip_prefix('[')?.strip_suffix(']')?;
    (!id.is_empty() && id.chars().all(|c| c.is_alphanumeric() || c == '_')).then_some(id)
}

fn kind_from(type_name: Option<&str>, group: Option<&str>) -> Result<SettingKind, OmpError> {
    let choices = |g: Option<&str>| match g {
        Some(g) if !matches!(g, "string" | "number" | "boolean" | "array" | "record") => {
            g.split('|').map(str::to_string).collect()
        }
        _ => Vec::new(),
    };
    match type_name.or(group) {
        Some("boolean") => Ok(SettingKind::Bool),
        Some("number") => Ok(SettingKind::Number),
        Some("string") => Ok(SettingKind::String),
        Some("array") => Ok(SettingKind::Array),
        Some("record") => Ok(SettingKind::Record),
        Some("enum") => Ok(SettingKind::Enum(choices(group))),
        Some(_) if type_name.is_none() => Ok(SettingKind::Enum(choices(group))),
        other => Err(OmpError::Parse(format!("unknown setting type {other:?}"))),
    }
}

fn json_map(text: &str, what: &str) -> Result<Map<String, Value>, OmpError> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(OmpError::Parse(format!("{what}: expected a JSON object"))),
        Err(e) => Err(OmpError::Parse(format!("{what}: {e}"))),
    }
}

fn tab_label(id: &str) -> String {
    if id == "internal" {
        return "Advanced".into();
    }
    if let Some((_, l)) = TAB_ORDER.iter().find(|(i, _)| *i == id) {
        return (*l).into();
    }
    let mut c = id.chars();
    c.next().map_or_else(String::new, |f| {
        f.to_uppercase().chain(c).collect::<String>()
    })
}

/// Merges the human list (tab + enum choices + key order), `list --json` (type/value/description/
/// redacted) and the defaults JSON into a catalog.
pub fn parse_settings(
    list_text: &str,
    list_json: &str,
    defaults_json: &str,
) -> Result<SettingsCatalog, OmpError> {
    let json = json_map(list_json, "config list --json")?;
    let defaults = json_map(defaults_json, "defaults config list --json")?;

    // (tab id, key, group) in text order.
    let mut rows: Vec<(&str, &str, &str)> = Vec::new();
    let mut tab: Option<&str> = None;
    for line in list_text.lines() {
        if let Some(id) = tab_header(line) {
            tab = Some(id);
        } else if let (Some(t), Some(r)) = (tab, parse_text_row(line)) {
            rows.push((t, r.key, r.group));
        }
    }
    if rows.is_empty() {
        return Err(OmpError::Parse("config list printed no settings".into()));
    }

    let mut by_tab: Vec<(String, Vec<Setting>)> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut push = |tab: &str, s: Setting| {
        match by_tab.iter_mut().find(|(id, _)| id == tab) {
            Some((_, v)) => v.push(s),
            None => by_tab.push((tab.to_string(), vec![s])),
        };
    };

    let build = |key: &str, group: Option<&str>| -> Result<Setting, OmpError> {
        let entry = json.get(key).and_then(Value::as_object);
        let type_name = entry.and_then(|e| e.get("type")).and_then(Value::as_str);
        let kind = kind_from(type_name, group)?;
        let redacted = entry
            .and_then(|e| e.get("redacted"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let secret = redacted || KNOWN_SECRET_KEYS.contains(&key);
        let value = if secret {
            redacted.then(|| Value::String(SECRET_PLACEHOLDER.into()))
        } else {
            entry.and_then(|e| e.get("value")).cloned()
        };
        let default = if secret {
            None
        } else {
            defaults
                .get(key)
                .and_then(Value::as_object)
                .and_then(|e| e.get("value"))
                .cloned()
        };
        let description = entry
            .and_then(|e| e.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(Setting {
            key: key.to_string(),
            kind,
            value,
            default,
            description,
            secret,
        })
    };

    for &(t, key, group) in &rows {
        if seen.insert(key) {
            push(t, build(key, Some(group))?);
        }
    }
    for key in json.keys() {
        if !seen.contains(key.as_str()) {
            push("internal", build(key, None)?);
        }
    }

    let mut tabs: Vec<SettingsTab> = Vec::with_capacity(by_tab.len());
    let mut take = |id: &str, by_tab: &mut Vec<(String, Vec<Setting>)>| {
        if let Some(pos) = by_tab.iter().position(|(i, _)| i == id) {
            let (id, settings) = by_tab.remove(pos);
            tabs.push(SettingsTab {
                label: tab_label(&id),
                id,
                settings,
            });
        }
    };
    for (id, _) in TAB_ORDER {
        take(id, &mut by_tab);
    }
    let internal = by_tab
        .iter()
        .position(|(i, _)| i == "internal")
        .map(|p| by_tab.remove(p));
    for (id, settings) in by_tab {
        tabs.push(SettingsTab {
            label: tab_label(&id),
            id,
            settings,
        });
    }
    if let Some((id, settings)) = internal {
        tabs.push(SettingsTab {
            label: tab_label(&id),
            id,
            settings,
        });
    }
    Ok(SettingsCatalog { tabs })
}

/// Where omp's installers put the binary when PATH does not list it. Unix: `~/.local/bin/omp`.
/// Windows: `%LOCALAPPDATA%\omp\omp.exe` (install.ps1 -Binary), `~\.bun\bin\omp.exe` (bun's shim).
fn install_locations(
    home: Option<PathBuf>,
    local_data: Option<PathBuf>,
    windows: bool,
) -> Vec<PathBuf> {
    if !windows {
        return home.map(|h| h.join(".local/bin/omp")).into_iter().collect();
    }
    local_data
        .map(|d| d.join("omp").join("omp.exe"))
        .into_iter()
        .chain(home.map(|h| h.join(".bun").join("bin").join("omp.exe")))
        .collect()
}

/// What omp keys its terminal breadcrumb on (`docs/notes/project-sync.md`): helper runs drop them
/// so a headless omp never writes (or overwrites) the breadcrumb of the terminal the GUI was
/// started from.
const TERMINAL_VARS: [&str; 8] = [
    "TMUX",
    "TMUX_PANE",
    "ZELLIJ_PANE_ID",
    "CMUX_SURFACE_ID",
    "KITTY_WINDOW_ID",
    "WEZTERM_PANE",
    "TERM_SESSION_ID",
    "WT_SESSION",
];

/// Removes [`TERMINAL_VARS`] from `cmd`'s environment and hides its console window.
fn headless(cmd: &mut Command) -> &mut Command {
    for var in TERMINAL_VARS {
        cmd.env_remove(var);
    }
    platform::no_window(cmd)
}

struct Captured {
    stdout: String,
    stderr: String,
}

impl Omp {
    /// First executable `omp` on PATH (`PATHEXT` on Windows), else the installers' default
    /// locations ([`install_locations`]).
    pub fn detect() -> Option<Omp> {
        platform::find_on_path("omp")
            .or_else(|| {
                install_locations(dirs::home_dir(), dirs::data_local_dir(), cfg!(windows))
                    .into_iter()
                    .find(|p| platform::is_executable(p))
            })
            .map(Omp::at)
    }

    pub fn at(program: impl Into<PathBuf>) -> Omp {
        Omp {
            program: program.into(),
            env: Vec::new(),
        }
    }

    /// Adds an environment variable to every command except the defaults probe (sandboxing in
    /// tests: `PI_CODING_AGENT_DIR`).
    #[must_use]
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Omp {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Value configured through [`Omp::with_env`] (the last one wins).
    pub(crate) fn env_value(&self, key: &str) -> Option<&OsString> {
        self.env
            .iter()
            .rev()
            .find(|(k, _)| k == std::ffi::OsStr::new(key))
            .map(|(_, v)| v)
    }

    pub(crate) fn command(&self, args: &[&std::ffi::OsStr]) -> Command {
        let mut c = Command::new(&self.program);
        c.args(args)
            .current_dir(platform::root_dir())
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &self.env {
            c.env(k, v);
        }
        headless(&mut c);
        c
    }

    /// Runs `omp <args>`; non-zero exit → [`OmpError::Failed`]. `redact` text is masked in the
    /// failure message (secret values).
    fn run(&self, args: &[&str], redact: Option<&str>) -> Result<Captured, OmpError> {
        let os: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
        self.finish(self.command(&os), redact)
    }

    fn finish(&self, mut cmd: Command, redact: Option<&str>) -> Result<Captured, OmpError> {
        let out = cmd.output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if out.status.success() {
            return Ok(Captured { stdout, stderr });
        }
        let mut err = failure(out.status.code(), &stdout, &stderr);
        if let (Some(secret), OmpError::Failed { message, .. }) = (redact, &mut err)
            && !secret.is_empty()
        {
            *message = message.replace(secret, SECRET_PLACEHOLDER);
        }
        Err(err)
    }

    /// `omp --version` → `omp/18.12.3` → "18.12.3".
    pub fn version(&self) -> Result<String, OmpError> {
        let out = self.run(&["--version"], None)?;
        let line = out.stdout.trim();
        Ok(line.strip_prefix("omp/").unwrap_or(line).to_string())
    }

    /// `omp update --check`.
    pub fn check_update(&self) -> Result<UpdateStatus, OmpError> {
        parse_update_check(&self.run(&["update", "--check"], None)?.stdout)
    }

    /// `omp models --json`, plus what omp's model browser shows beyond it: catalog intelligence
    /// and speed from a concurrent headless RPC `get_available_models`, and the measured speed in
    /// omp's `agent.db`. Either extra source failing only leaves those fields unset. Can take
    /// >1 s; call from a worker.
    pub fn models(&self) -> Result<Vec<ModelInfo>, OmpError> {
        let (cli, rpc) = std::thread::scope(|s| {
            let rpc = s.spawn(|| self.rpc_models());
            let cli = self.run(&["models", "--json"], None);
            let rpc = rpc
                .join()
                .unwrap_or_else(|_| Err(failed("models rpc worker panicked")));
            (cli, rpc)
        });
        let mut models = parse_models(&cli?.stdout)?;
        match rpc {
            Ok(list) => apply_catalog_metrics(&mut models, &list),
            Err(e) => log::warn!("model catalog metrics unavailable: {e}"),
        }
        match read_model_perf(&self.sessions_root().agent_dir().join("agent.db")) {
            Ok(rows) => apply_measured_speed(&mut models, &rows),
            Err(e) => log::warn!("measured model speed unavailable: {e}"),
        }
        Ok(models)
    }

    /// The registry models of a headless `omp --mode rpc --no-session --no-extensions`
    /// (`get_available_models`), as omp's JSON. 20 s timeout.
    fn rpc_models(&self) -> Result<Value, OmpError> {
        let args = [
            OsString::from("--no-session"),
            OsString::from("--no-extensions"),
        ];
        let request = serde_json::json!({"id": "1", "type": "get_available_models"});
        self.rpc_exchange(
            &args,
            platform::root_dir(),
            &[request],
            std::time::Duration::from_secs(20),
            |id, v| {
                (id == Some("1")).then(|| {
                    v.pointer("/data/models").cloned().ok_or_else(|| {
                        OmpError::Parse("get_available_models without models".into())
                    })
                })
            },
        )
    }

    /// Merges `config list`, `config list --json` and the defaults (the same command in an empty
    /// sandbox agent dir/cwd with every other `PI_*` variable removed). Runs the three commands
    /// concurrently.
    pub fn load_settings(&self) -> Result<SettingsCatalog, OmpError> {
        let agent = tempfile::tempdir()?;
        let cwd = tempfile::tempdir()?;
        let (text, json, defaults) = std::thread::scope(|s| {
            let text = s.spawn(|| self.run(&["config", "list"], None));
            let json = s.spawn(|| self.run(&["config", "list", "--json"], None));
            let defaults = s.spawn(|| {
                let mut c = Command::new(&self.program);
                c.args(["config", "list", "--json"])
                    .current_dir(cwd.path())
                    .env("NO_COLOR", "1")
                    .env("PI_CODING_AGENT_DIR", agent.path())
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                for (k, _) in std::env::vars_os() {
                    if k.to_string_lossy().starts_with("PI_") && k != "PI_CODING_AGENT_DIR" {
                        c.env_remove(k);
                    }
                }
                headless(&mut c);
                self.finish(c, None)
            });
            (text.join(), json.join(), defaults.join())
        });
        let join = |r: std::thread::Result<Result<Captured, OmpError>>| {
            r.unwrap_or_else(|_| Err(failed("settings worker panicked")))
        };
        let (text, json, defaults) = (join(text)?, join(json)?, join(defaults)?);
        parse_settings(&text.stdout, &json.stdout, &defaults.stdout)
    }

    /// `omp config set <key> --json -- <encoded>`. `--json` has to come BEFORE `--` (everything
    /// after `--` is the value); `--` keeps values starting with '-' from being read as flags.
    /// Returns the saved value (secrets: `Value::Null`; their stdout is never returned or logged).
    pub fn set_setting(&self, setting: &Setting, value: &Value) -> Result<Value, OmpError> {
        let encoded = encode_value(&setting.kind, value)?;
        self.set_encoded(&setting.key, &encoded, setting.secret)
    }

    fn set_encoded(&self, key: &str, encoded: &str, secret: bool) -> Result<Value, OmpError> {
        if key.is_empty() || key.starts_with('-') {
            return Err(failed(format!("Invalid setting key: {key}")));
        }
        if secret {
            log::debug!("omp config set {key} (secret, value not logged)");
        } else {
            log::debug!("omp config set {key} = {encoded}");
        }
        let out = self.run(
            &["config", "set", key, "--json", "--", encoded],
            secret.then_some(encoded),
        )?;
        if secret {
            return Ok(Value::Null);
        }
        match serde_json::from_str::<Value>(out.stdout.trim()) {
            Ok(Value::Object(m)) => Ok(m.get("value").cloned().unwrap_or(Value::Null)),
            _ => Err(OmpError::Parse(out.stdout.trim().to_string())),
        }
    }

    /// Replaces `provider`'s entries of `auth.accountPolicies` with `policies`, keeping every
    /// other entry as omp has it (read, merged and written back). Returns the saved array.
    pub fn set_account_policies(
        &self,
        provider: &str,
        policies: &[crate::usage::AccountPolicy],
    ) -> Result<Value, OmpError> {
        const KEY: &str = "auth.accountPolicies";
        let current = self.get_setting(KEY)?;
        let next = crate::usage::replace_account_policies(&current, provider, policies);
        self.set_encoded(KEY, &next.to_string(), false)
    }

    /// `omp config reset <key> --json`.
    pub fn reset_setting(&self, key: &str) -> Result<(), OmpError> {
        if key.is_empty() || key.starts_with('-') {
            return Err(failed(format!("Invalid setting key: {key}")));
        }
        self.run(&["config", "reset", key, "--json"], None)?;
        Ok(())
    }

    /// `omp config get <key> --json` → its `value` (`Null` when unset). Not for secrets: the
    /// value is returned as omp prints it.
    fn get_setting(&self, key: &str) -> Result<Value, OmpError> {
        if key.is_empty() || key.starts_with('-') {
            return Err(failed(format!("Invalid setting key: {key}")));
        }
        let out = self.run(&["config", "get", key, "--json"], None)?;
        match serde_json::from_str::<Value>(out.stdout.trim()) {
            Ok(Value::Object(m)) => Ok(m.get("value").cloned().unwrap_or(Value::Null)),
            _ => Err(OmpError::Parse(out.stdout.trim().to_string())),
        }
    }

    /// What omp's config says about task agents: `task.agentModelOverrides`,
    /// `task.disabledAgents` and `modelRoles` (read concurrently).
    pub fn agent_settings(&self) -> Result<crate::agent_defs::AgentSettings, OmpError> {
        use crate::agent_defs::{AgentSettings, DISABLED_KEY, OVERRIDES_KEY, ROLES_KEY};
        let (overrides, disabled, roles) = std::thread::scope(|s| {
            let disabled = s.spawn(|| self.get_setting(DISABLED_KEY));
            let roles = s.spawn(|| self.get_setting(ROLES_KEY));
            let overrides = self.get_setting(OVERRIDES_KEY);
            let join = |h: std::thread::ScopedJoinHandle<'_, Result<Value, OmpError>>| {
                h.join()
                    .unwrap_or_else(|_| Err(failed("settings worker panicked")))
            };
            (overrides, join(disabled), join(roles))
        });
        Ok(AgentSettings::from_values(&overrides?, &disabled?, &roles?))
    }

    /// omp's built-in agent definitions as `(file name, content)`: `omp agents unpack --dir
    /// <temp dir> --json` into a private temp dir, read back, the dir removed.
    pub fn bundled_agent_sources(&self) -> Result<Vec<(String, String)>, OmpError> {
        let tmp = tempfile::Builder::new()
            .prefix("omp-deck-agents-")
            .tempdir()?;
        let dir = tmp.path().as_os_str();
        let args = [
            std::ffi::OsStr::new("agents"),
            "unpack".as_ref(),
            "--dir".as_ref(),
            dir,
            "--json".as_ref(),
        ];
        let out = self.finish(self.command(&args), None)?;
        let report: Value = serde_json::from_str(out.stdout.trim())
            .map_err(|_| OmpError::Parse(out.stdout.trim().to_string()))?;
        let written = report
            .get("written")
            .and_then(Value::as_array)
            .ok_or_else(|| OmpError::Parse(out.stdout.trim().to_string()))?;
        let mut sources = Vec::with_capacity(written.len());
        for path in written.iter().filter_map(Value::as_str).map(Path::new) {
            // Only what omp wrote into the temp dir is read.
            let (Some(name), true) = (path.file_name(), path.parent() == Some(tmp.path())) else {
                continue;
            };
            let content = std::fs::read_to_string(path)?;
            sources.push((name.to_string_lossy().into_owned(), content));
        }
        Ok(sources)
    }

    /// Forks the session file at `entry_id` into a new file via a headless
    /// `omp --mode rpc --resume <file>` (cwd = the header's cwd, else `$HOME`): sends
    /// `{"type":"fork","entryId"}` then `get_state` for the new `sessionFile`, closes stdin and
    /// reaps the process. omp cuts a tool batch after its last result, like `/fork`; the source
    /// file is untouched. Fails with omp's own text when a live omp holds the session lease. 30 s
    /// timeout. Blocking: run on a worker.
    pub fn fork_at(&self, session_file: &Path, entry_id: &str) -> Result<PathBuf, OmpError> {
        let file = std::path::absolute(session_file)?;
        let cwd = crate::sessions::header_cwd(&file)
            .filter(|d| d.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(platform::root_dir);
        let args = vec![OsString::from("--resume"), file.into_os_string()];
        self.rpc_session_file(
            &args,
            cwd,
            Some(entry_id),
            std::time::Duration::from_secs(30),
        )
    }

    /// Moves a closed session (jsonl + sibling artifacts) into `dest`'s bucket with omp's own
    /// `/move`, headless: `omp --mode rpc --resume <file> --no-extensions` with process cwd
    /// `dest`, one `{"type":"prompt","message":"/move <dest>"}` (no model call, nothing appended
    /// to the session), then `get_state` for the new session file (else the same file name under
    /// `dest`'s bucket, verified by its header cwd). `dest` must be an existing absolute
    /// directory without control characters; `file` a `.jsonl` regular file. A header cwd that
    /// already is `dest` returns `file` without running omp. A live omp holding the session is
    /// retried for ~3 s before its message is returned. 60 s timeout, process tree killed.
    /// Blocking: run on a worker.
    pub fn relocate_session(&self, file: &Path, dest: &Path) -> Result<PathBuf, OmpError> {
        use std::time::{Duration, Instant};

        let file = std::path::absolute(file)?;
        if !dest.is_absolute() {
            return Err(failed(format!(
                "Destination is not an absolute path: {}",
                dest.display()
            )));
        }
        if dest.to_string_lossy().chars().any(char::is_control) {
            return Err(failed("Destination contains control characters"));
        }
        if !dest.is_dir() {
            return Err(failed(format!(
                "Destination is not a directory: {}",
                dest.display()
            )));
        }
        if file.extension().is_none_or(|e| e != "jsonl") || !file.is_file() {
            return Err(failed(format!("Not a session file: {}", file.display())));
        }
        if crate::sessions::header_cwd(&file).is_some_and(|c| same_dir(&c, dest)) {
            return Ok(file);
        }
        let message = format!("/move {}", dest.display());
        let args = vec![
            OsString::from("--resume"),
            file.clone().into_os_string(),
            OsString::from("--no-extensions"),
        ];
        let requests = [
            serde_json::json!({"id": "1", "type": "prompt", "message": message}),
            serde_json::json!({"id": "2", "type": "get_state"}),
        ];
        let name = file.file_name().map(PathBuf::from).unwrap_or_default();
        let give_up = Instant::now() + Duration::from_secs(3);
        loop {
            let mut moved = false;
            let mut state: Option<Option<PathBuf>> = None;
            let verified = |p: &Path| {
                p.is_file() && crate::sessions::header_cwd(p).is_some_and(|c| same_dir(&c, dest))
            };
            let result = self.rpc_exchange(
                &args,
                dest.to_path_buf(),
                &requests,
                Duration::from_secs(60),
                |id, v| {
                    match id {
                        Some("1") => moved = true,
                        Some("2") => {
                            state = Some(
                                v.pointer("/data/sessionFile")
                                    .and_then(Value::as_str)
                                    .map(PathBuf::from)
                                    .filter(|p| verified(p)),
                            );
                        }
                        _ => return None,
                    }
                    // `get_state` may answer before the move finished: only an answer that
                    // verifies counts; otherwise the bucket scan below decides.
                    match (&state, moved) {
                        (Some(Some(p)), true) => Some(Ok(Some(p.clone()))),
                        (Some(None), true) => Some(Ok(None)),
                        _ => None,
                    }
                },
            );
            match result {
                Ok(Some(new)) => return Ok(new),
                Ok(None) => {
                    return self.find_in_bucket(&name, dest).ok_or_else(|| {
                        failed("omp moved the session but its new file was not found")
                    });
                }
                Err(e)
                    if Instant::now() < give_up
                        && e.to_string().contains("another live omp process") =>
                {
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// `<bucket>/<name>` under the sessions root whose header cwd is `dest`.
    fn find_in_bucket(&self, name: &Path, dest: &Path) -> Option<PathBuf> {
        let root = self.sessions_root();
        std::fs::read_dir(&root.path)
            .ok()?
            .flatten()
            .map(|b| b.path().join(name))
            .find(|p| {
                p.is_file() && crate::sessions::header_cwd(p).is_some_and(|c| same_dir(&c, dest))
            })
    }

    /// Copies a whole session into `dest_cwd` without a terminal: a headless
    /// `omp --mode rpc --fork <src> --cwd <dest>` (the same copy as the tab Copy: fresh id,
    /// header `parentSession` = the source, sibling artifacts copied, source untouched), then
    /// `get_state` for the new `sessionFile`, stdin closed and the process reaped. Returns the new
    /// file. 120 s timeout (artifacts are copied). Blocking: run on a worker.
    pub fn copy_session(&self, src: &Path, dest_cwd: &Path) -> Result<PathBuf, OmpError> {
        let src = std::path::absolute(src)?;
        let dest = std::path::absolute(dest_cwd)?;
        let argv = fork_argv(&src, &dest);
        // argv = ["omp", "--fork", src, "--cwd", dest]; the program is ours.
        let args: Vec<OsString> = argv.into_iter().skip(1).map(OsString::from).collect();
        // Slack for coarse file-system timestamps.
        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(2);
        let err = match self.rpc_session_file(
            &args,
            platform::home_or_root(),
            None,
            std::time::Duration::from_secs(120),
        ) {
            Ok(file) => return Ok(file),
            Err(e) => e,
        };
        // omp writes the fork (with artifacts) before it restores the model and the rest of its
        // start-up: a failure afterwards (e.g. no credentials for the session's model) leaves a
        // complete copy behind. Reporting failure would make a retry duplicate it.
        if let Some(file) = self.find_created_copy(&src, &dest, since) {
            log::debug!(
                "copy_session: omp failed after forking ({err}); using {}",
                file.display()
            );
            return Ok(file);
        }
        Err(err)
    }

    /// The fork `omp --fork <src> --cwd <dest>` created since `since`, if any.
    fn find_created_copy(
        &self,
        src: &Path,
        dest: &Path,
        since: std::time::SystemTime,
    ) -> Option<PathBuf> {
        let root = self.sessions_root();
        let id = crate::sessions::parse_file(src)
            .ok()
            .flatten()
            .map(|p| p.id);
        let path = src.to_string_lossy();
        let mut parents = vec![path.as_ref()];
        parents.extend(id.as_deref());
        crate::sessions::find_copy(&root, &parents, dest, since)
    }

    /// The sessions root this omp uses: its [`Omp::with_env`] `PI_CODING_AGENT_DIR`, else the
    /// process environment's.
    fn sessions_root(&self) -> crate::sessions::SessionsRoot {
        use crate::sessions::SessionsRoot;
        let dir = self
            .env
            .iter()
            .rev()
            .find(|(k, v)| k == "PI_CODING_AGENT_DIR" && !v.is_empty());
        match dir {
            Some((_, dir)) => SessionsRoot::at(PathBuf::from(dir).join("sessions")),
            None => SessionsRoot::resolve(),
        }
    }

    /// Runs `omp --mode rpc <args>` headless, optionally sends a `fork` of `fork_entry`, then
    /// `get_state`, and returns the reported `sessionFile`. stdin is closed afterwards.
    fn rpc_session_file(
        &self,
        args: &[OsString],
        cwd: PathBuf,
        fork_entry: Option<&str>,
        timeout: std::time::Duration,
    ) -> Result<PathBuf, OmpError> {
        let mut requests = Vec::new();
        if let Some(entry_id) = fork_entry {
            requests.push(serde_json::json!({"id": "1", "type": "fork", "entryId": entry_id}));
        }
        requests.push(serde_json::json!({"id": "2", "type": "get_state"}));
        // Without a fork request there is nothing to wait for before `get_state`.
        let mut forked = fork_entry.is_none();
        self.rpc_exchange(args, cwd, &requests, timeout, |id, v| match id {
            Some("1") => {
                if v.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
                    return Some(Err(failed("omp cancelled the fork")));
                }
                forked = true;
                None
            }
            Some("2") if forked => Some(
                match v.pointer("/data/sessionFile").and_then(Value::as_str) {
                    Some(f) => Ok(PathBuf::from(f)),
                    None => Err(OmpError::Parse("get_state without sessionFile".into())),
                },
            ),
            _ => None,
        })
    }

    /// Runs `omp --mode rpc <args>` headless, sends `requests`, and feeds every successful
    /// response (`id`, whole message) to `handle` until it returns the outcome; a failed response
    /// is the error. stdin is closed and the process reaped afterwards.
    fn rpc_exchange<T>(
        &self,
        args: &[OsString],
        cwd: PathBuf,
        requests: &[Value],
        timeout: std::time::Duration,
        mut handle: impl FnMut(Option<&str>, &Value) -> Option<Result<T, OmpError>>,
    ) -> Result<T, OmpError> {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::sync::mpsc::{RecvTimeoutError, channel};
        use std::time::Instant;

        let mut cmd = Command::new(&self.program);
        cmd.args(["--mode", "rpc"])
            .args(args)
            .current_dir(cwd)
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        headless(&mut cmd);
        let mut child = platform::spawn_killable(cmd)?;
        let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(failed("omp rpc pipes unavailable"));
        };
        let (tx, rx) = channel::<String>();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        // stderr is only needed for the error text; bound it.
        let err_reader = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = (&mut stderr).take(64 * 1024).read_to_string(&mut text);
            text
        });
        let sent = requests
            .iter()
            .try_for_each(|r| writeln!(stdin, "{r}"))
            .and_then(|()| stdin.flush());
        match sent {
            Ok(()) => {}
            // omp exited before reading its requests: its stderr says why.
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                let code = shut_down(child, stdin);
                let _ = reader.join();
                let err = err_reader.join().unwrap_or_default();
                return Err(failure(code, "", &err));
            }
            Err(e) => return Err(e.into()),
        }

        let deadline = Instant::now() + timeout;
        let outcome: Result<T, OmpError> = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = match rx.recv_timeout(left) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) => break Err(failed("omp timed out")),
                Err(RecvTimeoutError::Disconnected) => {
                    // omp closed stdout before answering: its stderr says why.
                    let code = shut_down(child, stdin);
                    let _ = reader.join();
                    let err = err_reader.join().unwrap_or_default();
                    return Err(failure(code, "", &err));
                }
            };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) != Some("response") {
                continue;
            }
            let id = v.get("id").and_then(Value::as_str);
            if v.get("success").and_then(Value::as_bool) != Some(true) {
                let msg = v
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("request failed");
                break Err(failed(strip_error_prefix(msg).to_string()));
            }
            if let Some(outcome) = handle(id, &v) {
                break outcome;
            }
        };
        shut_down(child, stdin);
        let _ = reader.join();
        let _ = err_reader.join();
        outcome
    }

    /// Deletes closed sessions the way omp's own picker does, **one** headless omp process per
    /// batch: `omp --mode rpc --no-session --no-extensions -e <ompdeck-del.ts>` (cwd `$HOME`,
    /// stdin null) takes each target's session lease in turn (held by a live omp →
    /// [`DeleteError::Live`], nothing removed for that target), re-reads the file's header id
    /// under that lease (unless it equals [`DeleteTarget::id`] the target is
    /// [`DeleteError::Changed`], never retried, nothing removed) and runs
    /// `FileSessionStorage.deleteSessionWithArtifacts`: the `.jsonl`, its sibling `<stem>/`
    /// directory and stale `.jsonl.<n>.bak` files. Pins, blobs and breadcrumbs are untouched.
    ///
    /// `on_result` runs once per distinct file, on the calling thread, as soon as that target's
    /// outcome is known (the extension reports one NDJSON line per target while it works). Targets
    /// that are not `.jsonl` regular files are [`DeleteError::NotASession`] (a missing file is
    /// [`DeleteError::Io`]) without starting omp. Targets answering `Live` are retried in a fresh
    /// batch, up to 5 rounds ~250 ms apart, before `Live` is reported. A round times out after
    /// 20 s + 2 s per target; its unanswered targets report [`DeleteError::Timeout`]. A target
    /// with no result line (omp died) reports omp's stderr. Blocking: run on a worker.
    pub fn delete_sessions(
        &self,
        targets: &[DeleteTarget],
        mut on_result: impl FnMut(&Path, Result<(), DeleteError>),
    ) {
        use std::time::Duration;

        const ROUNDS: u32 = 5;
        const ROUND_DELAY: Duration = Duration::from_millis(250);

        let mut pending: Vec<(&Path, String, &str)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for target in targets {
            let file = target.path.as_path();
            let abs = match std::path::absolute(file) {
                Ok(a) => a,
                Err(e) => {
                    on_result(file, Err(e.into()));
                    continue;
                }
            };
            if !seen.insert(abs.clone()) {
                continue;
            }
            if target.id.is_empty() || abs.extension().is_none_or(|e| e != "jsonl") {
                on_result(file, Err(DeleteError::NotASession));
                continue;
            }
            match std::fs::metadata(&abs) {
                Ok(m) if m.is_file() => {}
                Ok(_) => {
                    on_result(file, Err(DeleteError::NotASession));
                    continue;
                }
                Err(e) => {
                    on_result(file, Err(e.into()));
                    continue;
                }
            }
            // The target list is NDJSON, read as UTF-8 by the extension.
            match abs.to_str() {
                Some(text) => pending.push((file, text.to_string(), target.id.as_str())),
                None => on_result(
                    &target.path,
                    Err(DeleteError::Omp("path is not valid UTF-8".into())),
                ),
            }
        }
        if pending.is_empty() {
            return;
        }
        let setup = tempfile::Builder::new()
            .prefix("ompdeck-del")
            .tempdir()
            .and_then(|work| crate::ctl::install_delete_extension(work.path()).map(|e| (work, e)));
        let (work, ext) = match setup {
            Ok(w) => w,
            Err(e) => {
                let msg = e.to_string();
                for (file, _, _) in &pending {
                    on_result(
                        file,
                        Err(DeleteError::Io(std::io::Error::other(msg.clone()))),
                    );
                }
                return;
            }
        };
        for round in 0..ROUNDS {
            if round > 0 {
                std::thread::sleep(ROUND_DELAY);
            }
            let last = round + 1 == ROUNDS;
            let targets: Vec<(&str, &str)> =
                pending.iter().map(|(_, t, id)| (t.as_str(), *id)).collect();
            let mut live = vec![false; pending.len()];
            self.delete_round(
                work.path(),
                &ext,
                round,
                &targets,
                &mut |i, result| match result {
                    Err(DeleteError::Live) if !last => live[i] = true,
                    other => on_result(pending[i].0, other),
                },
            );
            let mut idx = 0;
            pending.retain(|_| {
                idx += 1;
                live[idx - 1]
            });
            if pending.is_empty() {
                return;
            }
        }
    }

    /// One batch: one omp process for `targets`; calls `report(index, result)` exactly once per
    /// target, as its result line arrives or, failing that, when omp exits or times out.
    fn delete_round(
        &self,
        work: &Path,
        ext: &Path,
        round: u32,
        targets: &[(&str, &str)],
        report: &mut dyn FnMut(usize, Result<(), DeleteError>),
    ) {
        use std::io::{Read, Seek, SeekFrom};
        use std::time::{Duration, Instant};

        let timeout = Duration::from_secs(20 + 2 * targets.len() as u64);
        let list = work.join(format!("targets-{round}"));
        let out = work.join(format!("result-{round}"));
        let err_path = work.join(format!("stderr-{round}"));
        let started = (|| -> Result<_, DeleteError> {
            let body: Vec<String> = targets
                .iter()
                .map(|(path, id)| serde_json::json!({ "path": path, "id": id }).to_string())
                .collect();
            std::fs::write(&list, body.join("\n"))?;
            let mut cmd = Command::new(&self.program);
            cmd.args(["--mode", "rpc", "--no-session", "--no-extensions", "-e"])
                .arg(ext)
                .current_dir(platform::home_or_root())
                .env("NO_COLOR", "1")
                .env("OMPDECK_DEL_TARGETS", &list)
                .env("OMPDECK_DEL_OUT", &out)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&err_path)?);
            for (k, v) in &self.env {
                cmd.env(k, v);
            }
            headless(&mut cmd);
            Ok(platform::spawn_killable(cmd)?)
        })();
        let mut child = match started {
            Ok(c) => c,
            Err(e) => {
                let msg = e.to_string();
                for i in 0..targets.len() {
                    report(i, Err(DeleteError::Omp(msg.clone())));
                }
                return;
            }
        };

        let index: std::collections::HashMap<&str, usize> =
            targets.iter().enumerate().map(|(i, t)| (t.0, i)).collect();
        let mut done = vec![false; targets.len()];
        let mut left = targets.len();
        let (mut offset, mut carry) = (0u64, Vec::<u8>::new());
        // Reads the result lines that appeared since the last call.
        let mut drain = |done: &mut [bool], left: &mut usize| {
            let Ok(mut f) = std::fs::File::open(&out) else {
                return;
            };
            if f.seek(SeekFrom::Start(offset)).is_err() {
                return;
            }
            let mut fresh = Vec::new();
            if f.read_to_end(&mut fresh).is_err() {
                return;
            }
            offset += fresh.len() as u64;
            carry.extend_from_slice(&fresh);
            while let Some(nl) = carry.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = carry.drain(..=nl).collect();
                let Ok(v) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                let Some(i) = v
                    .get("target")
                    .and_then(Value::as_str)
                    .and_then(|t| index.get(t))
                    .copied()
                else {
                    continue;
                };
                if std::mem::replace(&mut done[i], true) {
                    continue;
                }
                *left -= 1;
                report(i, delete_result(&v));
            }
        };
        let deadline = Instant::now() + timeout;
        let mut timed_out = false;
        loop {
            drain(&mut done, &mut left);
            if left == 0 {
                break;
            }
            match child.try_wait() {
                Ok(Some(_)) => {
                    // The exit code is not trusted (omp exits 1 without a default model after the
                    // extension already ran); the result lines are. Read what is left.
                    drain(&mut done, &mut left);
                    break;
                }
                Ok(None) => {}
                Err(_) => break,
            }
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(child);
        if left == 0 {
            return;
        }
        let stderr = std::fs::read(&err_path).unwrap_or_default();
        let text = String::from_utf8_lossy(&stderr[..stderr.len().min(64 * 1024)]);
        let text = strip_error_prefix(text.trim()).to_string();
        for (i, d) in done.iter().enumerate() {
            if *d {
                continue;
            }
            report(
                i,
                Err(if timed_out {
                    DeleteError::Timeout
                } else if text.is_empty() {
                    DeleteError::Omp("omp produced no delete result".into())
                } else {
                    DeleteError::Omp(text.clone())
                }),
            );
        }
    }

    /// Omp-native move: `omp session pack <src> <tmp.tar>` then `omp session unpack <tmp.tar>
    /// --cwd <dest>`; the tar is always removed. omp deletes the original when it is a prefix of
    /// the archive (`Replaced`). Fails with nothing changed while a live omp writes the session.
    pub fn move_session(&self, src: &Path, dest_cwd: &Path) -> Result<MoveOutcome, OmpError> {
        // Commands run in the filesystem root, so relative paths must be made absolute first.
        let src = std::path::absolute(src)?;
        let dest = std::path::absolute(dest_cwd)?;
        let tmp = tempfile::tempdir()?;
        let tar = tmp.path().join("session.tar");
        let pack = self.command(&[
            "session".as_ref(),
            "pack".as_ref(),
            src.as_os_str(),
            tar.as_os_str(),
        ]);
        self.finish(pack, None)?;
        let unpack = self.command(&[
            "session".as_ref(),
            "unpack".as_ref(),
            tar.as_os_str(),
            "--cwd".as_ref(),
            dest.as_os_str(),
        ]);
        let out = self.finish(unpack, None)?;
        parse_unpack(&out.stdout, &out.stderr)
    }

    /// `omp session unpack <tar> --cwd <cwd>` for a hand-built pack-format archive (project
    /// restore). `cwd` must exist. stdin is null and the terminal-identifying env vars are
    /// removed so omp writes no `terminal-sessions` breadcrumb; 120 s timeout, the process group
    /// is killed on timeout. Blocking: run on a worker.
    pub fn unpack_session(&self, tar: &Path, cwd: &Path) -> Result<MoveOutcome, OmpError> {
        let tar = std::path::absolute(tar)?;
        let cwd = std::path::absolute(cwd)?;
        let (stdout, stderr) = self.run_detached(
            &[
                "session".as_ref(),
                "unpack".as_ref(),
                tar.as_os_str(),
                "--cwd".as_ref(),
                cwd.as_os_str(),
            ],
            "unpacking",
        )?;
        parse_unpack(&stdout, &stderr)
    }

    /// `omp session pack <src> <out_tar>`: saves one session (jsonl, artifacts, blobs) as an
    /// archive without touching it, detached exactly like [`Omp::unpack_session`] (stdin null,
    /// terminal env vars removed, 120 s timeout, process group killed on timeout). Fails with
    /// omp's own text while a live omp process writes the session. Returns the absolute
    /// archive path. Blocking: run on a worker.
    pub fn pack_session(&self, src: &Path, out_tar: &Path) -> Result<PathBuf, OmpError> {
        let src = std::path::absolute(src)?;
        let out = std::path::absolute(out_tar)?;
        self.run_detached(
            &[
                "session".as_ref(),
                "pack".as_ref(),
                src.as_os_str(),
                out.as_os_str(),
            ],
            "packing",
        )?;
        Ok(out)
    }

    /// Runs `args` with the null-stdin / no-terminal-env command setup, bounded to 120 s;
    /// returns (stdout, stderr) of a zero exit.
    fn run_detached(
        &self,
        args: &[&std::ffi::OsStr],
        what: &str,
    ) -> Result<(String, String), OmpError> {
        use crate::usage::{UsageError, run_bounded};
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
        let cmd = self.command(args);
        let (code, stdout, stderr) = run_bounded(cmd, TIMEOUT).map_err(|e| match e {
            UsageError::Spawn(e) => OmpError::Spawn(e),
            _ => OmpError::Failed {
                code: None,
                message: format!("omp did not finish {what} within {} s", TIMEOUT.as_secs()),
            },
        })?;
        if code != Some(0) {
            return Err(failure(code, &stdout, &stderr));
        }
        Ok((stdout, stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LIST_TXT: &str = include_str!("../tests/fixtures/omp/list.txt");
    const LIST_JSON: &str = include_str!("../tests/fixtures/omp/list.json");
    const DEFAULTS_JSON: &str = include_str!("../tests/fixtures/omp/defaults.json");

    fn catalog() -> SettingsCatalog {
        parse_settings(LIST_TXT, LIST_JSON, DEFAULTS_JSON).unwrap()
    }

    fn find<'a>(c: &'a SettingsCatalog, key: &str) -> (&'a SettingsTab, &'a Setting) {
        c.tabs
            .iter()
            .find_map(|t| t.settings.iter().find(|s| s.key == key).map(|s| (t, s)))
            .unwrap_or_else(|| panic!("missing {key}"))
    }

    #[test]
    fn catalog_counts_match_real_omp_output() {
        let c = catalog();
        let ids: Vec<_> = c.tabs.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "appearance",
                "model",
                "interaction",
                "context",
                "memory",
                "files",
                "shell",
                "tools",
                "tasks",
                "providers",
                "internal"
            ]
        );
        assert_eq!(c.tabs.last().unwrap().label, "Advanced");
        let all: Vec<_> = c.tabs.iter().flat_map(|t| &t.settings).collect();
        assert_eq!(all.len(), 559);
        assert_eq!(
            all.iter()
                .filter(|s| matches!(s.kind, SettingKind::Enum(_)))
                .count(),
            98
        );
        assert_eq!(
            c.tabs
                .iter()
                .find(|t| t.id == "internal")
                .unwrap()
                .settings
                .len(),
            140
        );
    }

    #[test]
    fn values_defaults_and_choices() {
        let c = catalog();
        let (tab, s) = find(&c, "symbolPreset");
        assert_eq!(tab.id, "appearance");
        assert_eq!(
            s.kind,
            SettingKind::Enum(vec!["unicode".into(), "nerd".into(), "ascii".into()])
        );
        assert_eq!(s.value, Some(json!("nerd")));
        assert_eq!(s.default, Some(json!("unicode")));
        assert!(s.is_modified());

        let (_, s) = find(&c, "autocompleteMaxVisible");
        assert_eq!(s.kind, SettingKind::Number);
        assert_eq!(s.value, Some(json!(12)));
        assert!(s.is_modified());

        let (_, s) = find(&c, "compaction.enabled");
        assert_eq!(s.value, Some(json!(false)));
        assert_eq!(s.default, Some(json!(true)));

        let (_, s) = find(&c, "enabledModels");
        assert_eq!(s.kind, SettingKind::Array);
        assert_eq!(s.value, Some(json!(["a/b", "c"])));

        let (_, s) = find(&c, "modelRoles");
        assert_eq!(s.kind, SettingKind::Record);
        assert_eq!(s.value, Some(json!({"default": "x/y"})));

        let (_, s) = find(&c, "theme.light");
        assert!(!s.is_modified());
        assert_eq!(s.section(), "theme");
        assert_eq!(s.label(), "Light");
    }

    #[test]
    fn unset_optional_has_no_value_or_default() {
        let c = catalog();
        let (_, s) = find(&c, "compaction.reserveTokens");
        assert_eq!(s.value, None);
        assert_eq!(s.default, None);
        assert!(!s.is_modified());
    }

    #[test]
    fn secrets_never_carry_a_value() {
        let c = catalog();
        let (_, set) = find(&c, "searxng.token");
        assert!(set.secret);
        assert_eq!(set.value, Some(json!(SECRET_PLACEHOLDER)));
        assert!(set.is_modified());
        let (_, unset) = find(&c, "auth.broker.token");
        assert!(unset.secret, "known credential key without redaction");
        assert_eq!(unset.value, None);
        assert!(!unset.is_modified());
        assert!(!LIST_TXT.contains("fixture-not-a-secret"));
    }

    #[test]
    fn keys_missing_from_text_go_to_internal() {
        let txt = "Settings:\n\n[model]\n  a.b = 1 (number)\n";
        let js = r#"{"a.b":{"value":1,"type":"number","description":"d"},
                     "z":{"value":true,"type":"boolean","description":""}}"#;
        let c = parse_settings(txt, js, "{}").unwrap();
        assert_eq!(c.tabs.len(), 2);
        assert_eq!(c.tabs[1].id, "internal");
        assert_eq!(c.tabs[1].settings[0].key, "z");
        assert_eq!(c.tabs[0].settings[0].description, "d");
    }

    #[test]
    fn row_value_may_contain_parentheses() {
        let txt = "[tools]\n  k.x = see (this) thing (string)\n";
        let js = r#"{"k.x":{"value":"see (this) thing","type":"string","description":""}}"#;
        let c = parse_settings(txt, js, "{}").unwrap();
        assert_eq!(c.tabs[0].settings[0].kind, SettingKind::String);
    }

    #[test]
    fn labels_are_humanized() {
        let mk = |key: &str| Setting {
            key: key.into(),
            kind: SettingKind::Bool,
            value: None,
            default: None,
            description: String::new(),
            secret: false,
        };
        assert_eq!(
            mk("statusLine.compactThinkingLevel").label(),
            "Compact thinking level"
        );
        assert_eq!(mk("simple").label(), "Simple");
        assert_eq!(mk("a.apiURLPath").label(), "Api URL path");
        assert_eq!(mk("noDot").section(), "");
    }

    #[test]
    fn encode_value_boundaries() {
        let s = SettingKind::String;
        assert_eq!(
            encode_value(&s, &json!("plain text")).unwrap(),
            "plain text"
        );
        assert_eq!(encode_value(&s, &json!("  pad ")).unwrap(), "\"  pad \"");
        assert_eq!(encode_value(&s, &json!("\"q\"")).unwrap(), r#""\"q\"""#);
        assert_eq!(encode_value(&s, &json!("a\nb")).unwrap(), "\"a\\nb\"");
        assert_eq!(encode_value(&s, &json!("")).unwrap(), "");
        assert_eq!(encode_value(&s, &json!("-dash")).unwrap(), "-dash");
        let n = SettingKind::Number;
        assert_eq!(encode_value(&n, &json!(-5)).unwrap(), "-5");
        assert_eq!(encode_value(&n, &json!(12.5)).unwrap(), "12.5");
        assert_eq!(
            encode_value(&SettingKind::Bool, &json!(false)).unwrap(),
            "false"
        );
        let nested = json!({"a": [1, {"b": "c d"}], "e": null});
        assert_eq!(
            encode_value(&SettingKind::Record, &nested).unwrap(),
            r#"{"a":[1,{"b":"c d"}],"e":null}"#
        );
        assert_eq!(
            encode_value(&SettingKind::Array, &json!(["x", " y"])).unwrap(),
            r#"["x"," y"]"#
        );
    }

    #[test]
    fn encode_value_rejects_wrong_shapes_and_enum_members() {
        let e = SettingKind::Enum(vec!["a".into(), "b".into()]);
        assert_eq!(encode_value(&e, &json!("a")).unwrap(), "a");
        let err = encode_value(&e, &json!("c")).unwrap_err().to_string();
        assert_eq!(err, "Invalid value: c. Valid values: a, b");
        assert!(encode_value(&SettingKind::Number, &json!("5")).is_err());
        assert!(encode_value(&SettingKind::Bool, &json!("true")).is_err());
        assert!(encode_value(&SettingKind::Array, &json!({"a": 1})).is_err());
        assert!(encode_value(&SettingKind::Record, &json!([1])).is_err());
    }

    #[test]
    fn update_check_variants() {
        assert_eq!(
            parse_update_check("Current version: 18.12.3\nNew version available: 18.12.5\n")
                .unwrap(),
            UpdateStatus {
                current: "18.12.3".into(),
                latest: Some("18.12.5".into())
            }
        );
        assert_eq!(
            parse_update_check("Current version: 18.12.3\n✔ Already up to date\n").unwrap(),
            UpdateStatus {
                current: "18.12.3".into(),
                latest: None
            }
        );
        assert_eq!(
            parse_update_check(
                "Current version: 18.12.3\nCurrent channel: canary\nNew version available: 18.12.4-canary.2\n"
            )
            .unwrap()
            .latest
            .as_deref(),
            Some("18.12.4-canary.2")
        );
        assert!(matches!(
            parse_update_check(
                "/x/omp is installed and kept up to date by brew; update it from brew.\n"
            ),
            Err(OmpError::Parse(_))
        ));
        assert!(parse_update_check("").is_err());
    }

    #[test]
    fn unpack_modes() {
        let p = "/home/u/.omp/agent/sessions/-x/2026_abc.jsonl\n";
        let o = parse_unpack(p, "Unpacked session (abc).\n").unwrap();
        assert_eq!(o.kind, MoveKind::Created);
        assert_eq!(o.new_path, PathBuf::from(p.trim()));
        assert_eq!(
            parse_unpack(p, "Replaced the local copy of this session (abc).\n")
                .unwrap()
                .kind,
            MoveKind::Replaced
        );
        assert_eq!(
            parse_unpack(
                p,
                "The local copy of this session diverged; unpacked as a fork (def456).\n"
            )
            .unwrap()
            .kind,
            MoveKind::Forked {
                new_id: "def456".into()
            }
        );
        assert!(matches!(
            parse_unpack("", "Unpacked session (a).\n"),
            Err(OmpError::Parse(_))
        ));
        assert!(matches!(
            parse_unpack(p, "something else\n"),
            Err(OmpError::Parse(_))
        ));
    }

    #[test]
    fn failure_messages_strip_prefix_and_fall_back_to_stdout() {
        let e = failure(Some(1), "", "Error: Invalid number: abc\n");
        assert!(
            matches!(&e, OmpError::Failed { code: Some(1), message } if message == "Invalid number: abc")
        );
        let e = failure(Some(1), "", "error: Session \"x\" not found.\n");
        assert_eq!(e.to_string(), "Session \"x\" not found.");
        let e = failure(Some(1), "Current version: 1\n", "");
        assert_eq!(e.to_string(), "Current version: 1");
    }

    #[test]
    fn argv_forms() {
        assert_eq!(
            fork_argv(Path::new("/s/a.jsonl"), Path::new("/p/d")),
            ["omp", "--fork", "/s/a.jsonl", "--cwd", "/p/d"]
        );
        assert_eq!(update_argv(UpdateKind::Omp), ["omp", "update"]);
        assert_eq!(
            update_argv(UpdateKind::Plugins),
            ["omp", "update", "--plugins"]
        );
        assert_eq!(
            update_argv(UpdateKind::Canary),
            ["omp", "update", "--canary"]
        );
        assert_eq!(
            update_argv(UpdateKind::Stable),
            ["omp", "update", "--stable"]
        );
    }

    #[test]
    fn powershell_install_runs_the_ps1_installer_in_binary_mode() {
        let argv = powershell_install_argv();
        assert_eq!(
            &argv[..5],
            [
                "powershell.exe",
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command"
            ]
        );
        assert_eq!(
            argv[5],
            format!("& ([scriptblock]::Create((irm {INSTALL_URL}))) -Binary")
        );
        assert_eq!(argv.len(), 6);
    }

    #[test]
    fn install_locations_per_os() {
        let home = || Some(PathBuf::from("/h"));
        let local = || Some(PathBuf::from("/h/AppData/Local"));
        assert_eq!(
            install_locations(home(), local(), false),
            [PathBuf::from("/h/.local/bin/omp")]
        );
        assert_eq!(
            install_locations(home(), local(), true),
            [
                PathBuf::from("/h/AppData/Local/omp/omp.exe"),
                PathBuf::from("/h/.bun/bin/omp.exe")
            ]
        );
        assert!(install_locations(None, None, true).is_empty());
    }

    /// Executable `dest` created by an external process so this process never holds a write fd to
    /// the executed inode (a sibling thread's fork would leak it and exec would fail with ETXTBSY).
    #[cfg(unix)]
    fn install_exec(dest: &std::path::Path, content: &str) {
        let mut stage = dest.as_os_str().to_owned();
        stage.push(".src");
        std::fs::write(&stage, content).unwrap();
        let status = std::process::Command::new("install")
            .args(["-m", "755"])
            .arg(&stage)
            .arg(dest)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Runs `install_argv()` with a fake `curl` (body `curl_body`) first on PATH.
    #[cfg(unix)]
    fn run_installer(curl_body: &str) -> (bool, Option<String>) {
        let dir = tempfile::tempdir().unwrap();
        let curl = dir.path().join("curl");
        install_exec(&curl, &format!("#!/bin/sh\n{curl_body}\n"));
        let out = dir.path().join("ran");
        let argv = install_argv();
        let ok = Command::new(&argv[0])
            .args(&argv[1..])
            .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
            .env("OUT", &out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        (ok, std::fs::read_to_string(&out).ok())
    }

    #[cfg(unix)]
    #[test]
    fn installer_runs_the_downloaded_script_with_binary_flag() {
        let (ok, ran) = run_installer(r#"echo 'echo "args:$*" > "$OUT"'"#);
        assert!(ok);
        assert_eq!(ran.as_deref(), Some("args:--binary\n"));
    }

    #[cfg(unix)]
    #[test]
    fn installer_fails_without_running_anything_when_the_download_fails() {
        let (ok, ran) = run_installer("echo 'echo partial > \"$OUT\"'; exit 22");
        assert!(!ok);
        assert_eq!(ran, None);
    }
}

#[cfg(test)]
mod models_tests {
    use super::*;

    const MODELS: &str = include_str!("../tests/fixtures/status/models.json");

    #[test]
    fn catalog_fixture_maps_thinking_window_and_cost() {
        let m = parse_models(MODELS).unwrap();
        assert_eq!(m.len(), 4);
        let fable = m
            .iter()
            .find(|m| m.selector == "anthropic/claude-fable-5")
            .unwrap();
        assert_eq!(fable.provider, "anthropic");
        assert_eq!(fable.id, "claude-fable-5");
        assert_eq!(fable.name, "Claude Fable 5");
        assert_eq!(fable.context_window, Some(1_000_000));
        assert_eq!(fable.thinking, ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(
            (fable.cost_input, fable.cost_output),
            (Some(10.0), Some(50.0))
        );
        // `thinking: null` = no thinking support.
        let old = m
            .iter()
            .find(|m| m.selector == "anthropic/claude-3-5-sonnet-20240620")
            .unwrap();
        assert!(old.thinking.is_empty());
        assert_eq!(old.context_window, Some(200_000));
        assert_eq!(old.max_tokens, Some(8192));
        assert_eq!(old.reasoning, Some(false));
        assert_eq!(old.input, ["text", "image"]);
        assert_eq!(old.cost_cache_read, Some(0.3));
        assert_eq!(old.pricing, Some(Pricing::Fixed));
        // The CLI catalog carries no metrics; they come from RPC / agent.db.
        assert_eq!((old.intelligence, old.catalog_tps), (None, None));
    }

    #[test]
    fn sparse_entries_and_noise() {
        let out = r#"warning: something
{"models":[{"provider":"p","id":"i"},{"provider":"","id":"x"},{"id":"y"},{"provider":"q","id":"j","selector":"q/custom","name":"J","thinking":["low"]}]}"#;
        let m = parse_models(out).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].selector, "p/i");
        assert_eq!(m[0].name, "i");
        assert_eq!(m[0].context_window, None);
        assert_eq!(m[0].cost_input, None);
        assert_eq!(
            (m[0].max_tokens, m[0].reasoning, m[0].pricing),
            (None, None, None)
        );
        assert!(m[0].input.is_empty());
        assert_eq!(m[1].selector, "q/custom");
        assert_eq!(m[1].thinking, ["low"]);
        assert!(matches!(parse_models("[]"), Err(OmpError::Parse(_))));
        assert!(matches!(parse_models("nope"), Err(OmpError::Parse(_))));
    }

    #[test]
    fn rpc_metrics_fill_by_provider_and_id_and_zero_speed_is_unmeasured() {
        let out = r#"{"models":[
            {"provider":"a","id":"m","selector":"a/custom"},
            {"provider":"b","id":"m","pricingStatus":"someday"},
            {"provider":"c","id":"n","int":7}]}"#;
        let mut m = parse_models(out).unwrap();
        assert_eq!(m[1].pricing, None);
        let rpc = serde_json::json!([
            {"provider":"a","id":"m","int":52.4,"tps":118.2},
            {"provider":"b","id":"m","int":40,"tps":0},
            {"provider":"c","id":"n","int":99,"tps":12.5},
            {"id":"orphan","int":1}
        ]);
        apply_catalog_metrics(&mut m, &rpc);
        assert_eq!(
            (m[0].intelligence, m[0].catalog_tps),
            (Some(52.4), Some(118.2))
        );
        assert_eq!((m[1].intelligence, m[1].catalog_tps), (Some(40.0), None));
        // A metric the CLI already reported is kept.
        assert_eq!(
            (m[2].intelligence, m[2].catalog_tps),
            (Some(7.0), Some(12.5))
        );
        apply_catalog_metrics(&mut m, &serde_json::json!({"not": "a list"}));
        assert_eq!(m[0].intelligence, Some(52.4));
    }

    #[test]
    fn measured_speed_prefers_standard_then_fastest_tier() {
        let row = |key: &str, out: f64, gen_ms: f64, ttft: (f64, f64)| PerfRow {
            key: key.to_owned(),
            samples: 3.0,
            output_tokens: out,
            gen_ms,
            ttft_samples: ttft.0,
            ttft_ms: ttft.1,
        };
        let rows = [
            row("p/a@fast", 3000.0, 1000.0, (0.0, 0.0)),
            row("p/a", 1000.0, 2000.0, (2.0, 1800.0)),
            row("p/b@slow", 100.0, 1000.0, (0.0, 0.0)),
            row("p/b@fast", 400.0, 1000.0, (1.0, 300.0)),
            row("p/c", 0.0, 1000.0, (0.0, 0.0)),
            row("p/d", 10.0, 0.0, (0.0, 0.0)),
        ];
        let model = |id: &str| ModelInfo {
            provider: "p".into(),
            id: id.into(),
            ..Default::default()
        };
        let mut m = vec![model("a"), model("b"), model("c"), model("d")];
        apply_measured_speed(&mut m, &rows);
        assert_eq!(
            m[0].measured,
            Some(MeasuredSpeed {
                tps: 500.0,
                ttft_ms: Some(900.0),
                samples: 3.0,
                tier: None
            })
        );
        assert_eq!(
            m[1].measured,
            Some(MeasuredSpeed {
                tps: 400.0,
                ttft_ms: Some(300.0),
                samples: 3.0,
                tier: Some("fast".into())
            })
        );
        // No output tokens / no decode time: unmeasured, not 0 t/s.
        assert_eq!((&m[2].measured, &m[3].measured), (&None, &None));
    }
}
