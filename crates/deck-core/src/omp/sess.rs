//! Session-store operations only omp can perform, run by a headless omp
//! (`omp --mode rpc --no-session --no-extensions -e ompdeck-sess.ts`, like
//! [`Omp::delete_sessions`]): pins, renaming and labelling closed sessions (under omp's session
//! lease), foreign (Claude Code / Codex) session listing and import, and the `WATCHDOG.yml`
//! advisor files. One omp process serves a whole batch of requests ([`Omp::sess_run`]).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Omp, headless, strip_error_prefix};
use crate::platform;

/// A foreign coding-agent session source omp can import from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForeignSource {
    Claude,
    Codex,
}

impl ForeignSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ForeignSource::Claude => "claude",
            ForeignSource::Codex => "codex",
        }
    }
}

/// Which `WATCHDOG.yml` an advisor document is read from or saved to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdvisorScope {
    /// `<vcs root of the cwd, else the cwd>/WATCHDOG.yml`.
    Project,
    /// `<agent dir>/WATCHDOG.yml`.
    User,
}

impl AdvisorScope {
    pub fn as_str(self) -> &'static str {
        match self {
            AdvisorScope::Project => "project",
            AdvisorScope::User => "user",
        }
    }
}

/// A session of another coding agent, as omp lists it (`ForeignSessionInfo`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignSession {
    pub source: ForeignSource,
    /// The foreign session's own id: what [`Omp::import_foreign`] names.
    pub id: String,
    /// The foreign transcript file.
    pub path: PathBuf,
    /// The project directory the session ran in (may no longer exist).
    pub cwd: String,
    #[serde(default)]
    pub title: Option<String>,
    /// Unix milliseconds.
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub modified: i64,
    #[serde(default)]
    pub message_count: Option<u32>,
    #[serde(default)]
    pub first_message: Option<String>,
}

/// One advisor of a `WATCHDOG.yml` (omp's `AdvisorConfig`). Enumerated fields stay text: omp
/// validates them (`reviewMode`: `turn`|`agent-end`; `syncBacklog`: `off`|`1`|`3`|`5`|`strict`;
/// `scope`: `main`|`subagent`|`all`) and reports a bad value as an error or a document warning.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvisorEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `Some([])` = no tools; `None` = omp's default (read, grep, glob).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_interval: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_backlog: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_notes_per_update: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_reasoning: Option<bool>,
    /// Fields omp (or the user) put on the advisor that are not modelled here: kept as read and
    /// written back unchanged.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

/// One `WATCHDOG.yml` as omp edits it (`WatchdogConfigDoc`): raw, not merged with other levels.
/// An empty document ([`AdvisorDoc::is_empty`]) deletes the file when saved.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvisorDoc {
    /// Shared instructions for every advisor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_notes_per_update: Option<u32>,
    #[serde(default)]
    pub advisors: Vec<AdvisorEntry>,
    /// Problems omp found while reading the file (dropped entries, bad YAML). Read-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The file this document came from (the one a save writes). Read-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Top-level keys of the file that are not modelled here: kept as read and written back
    /// unchanged.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

impl AdvisorDoc {
    /// Saving an empty document removes the file (omp's semantics).
    pub fn is_empty(&self) -> bool {
        self.advisors.is_empty()
            && self
                .instructions
                .as_deref()
                .is_none_or(|i| i.trim().is_empty())
            && self.max_notes_per_update.is_none()
            && self.extra.is_empty()
    }
}

/// What [`Omp::save_advisor_config`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvisorSaved {
    /// The file written (or removed).
    pub path: PathBuf,
    /// The document was empty, so the file no longer exists.
    pub removed: bool,
}

/// Why a helper operation failed.
#[derive(Debug, thiserror::Error)]
pub enum SessError {
    /// A running omp owns the session (its lease is held); nothing was changed.
    #[error("the session is open in a running omp")]
    Busy,
    /// omp's own message (or its stderr when the helper died without answering).
    #[error("{0}")]
    Failed(String),
    #[error("failed to run omp: {0}")]
    Io(#[from] std::io::Error),
    #[error("omp did not finish in time")]
    Timeout,
}

impl Omp {
    /// Runs `requests` (`{"op": …, …}` objects) in **one** headless omp and returns one result per
    /// request, in order: the helper's reply object (without `rid`) or the failure. Blocking: up
    /// to 20 s + 2 s per request. A request without a reply (omp died, or an omp-internal module
    /// the operation needs no longer resolves) reports omp's stderr.
    pub(crate) fn sess_run(&self, requests: &[Value]) -> Vec<Result<Value, SessError>> {
        let fail_all = |e: SessError| -> Vec<Result<Value, SessError>> {
            let text = e.to_string();
            requests
                .iter()
                .map(|_| Err(SessError::Failed(text.clone())))
                .collect()
        };
        let work = match tempfile::Builder::new().prefix("ompdeck-sess").tempdir() {
            Ok(w) => w,
            Err(e) => return fail_all(e.into()),
        };
        let ext = match crate::ctl::install_sess_extension(work.path()) {
            Ok(p) => p,
            Err(e) => return fail_all(e.into()),
        };
        let (req_path, out_path, err_path) = (
            work.path().join("requests"),
            work.path().join("results"),
            work.path().join("stderr"),
        );
        let started = (|| -> std::io::Result<_> {
            let body: Vec<String> = requests
                .iter()
                .enumerate()
                .map(|(rid, r)| {
                    let mut r = r.clone();
                    r["rid"] = json!(rid);
                    r.to_string()
                })
                .collect();
            std::fs::write(&req_path, body.join("\n"))?;
            let mut cmd = std::process::Command::new(&self.program);
            cmd.args(["--mode", "rpc", "--no-session", "--no-extensions", "-e"])
                .arg(&ext)
                .current_dir(platform::home_or_root())
                .env("NO_COLOR", "1")
                .env("OMPDECK_SESS_REQ", &req_path)
                .env("OMPDECK_SESS_OUT", &out_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&err_path)?);
            for (k, v) in &self.env {
                cmd.env(k, v);
            }
            headless(&mut cmd);
            platform::spawn_killable(cmd)
        })();
        let mut child = match started {
            Ok(c) => c,
            Err(e) => return fail_all(e.into()),
        };

        let deadline = Instant::now() + Duration::from_secs(20 + 2 * requests.len() as u64);
        let mut timed_out = false;
        let mut replies: Vec<Option<Value>> = vec![None; requests.len()];
        loop {
            read_replies(&out_path, &mut replies);
            if replies.iter().all(Option::is_some) {
                break;
            }
            match child.try_wait() {
                // The exit code is not trusted (omp exits 1 without a default model after the
                // extension ran); the result lines are.
                Ok(Some(_)) => {
                    read_replies(&out_path, &mut replies);
                    break;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    timed_out = true;
                    break;
                }
                Err(_) => break,
            }
        }
        drop(child); // kills the tree and reaps
        let stderr = std::fs::read(&err_path).unwrap_or_default();
        let stderr = String::from_utf8_lossy(&stderr[..stderr.len().min(64 * 1024)]);
        let stderr = strip_error_prefix(stderr.trim()).to_owned();

        replies
            .into_iter()
            .map(|reply| match reply {
                Some(v) if v.get("ok").and_then(Value::as_bool) == Some(true) => Ok(v),
                Some(v) => {
                    let msg = v.get("error").and_then(Value::as_str).unwrap_or("failed");
                    Err(if msg == "busy" {
                        SessError::Busy
                    } else {
                        SessError::Failed(msg.to_owned())
                    })
                }
                None if timed_out => Err(SessError::Timeout),
                None if stderr.is_empty() => {
                    Err(SessError::Failed("omp produced no result".into()))
                }
                None => Err(SessError::Failed(stderr.clone())),
            })
            .collect()
    }

    fn sess_one(&self, request: Value) -> Result<Value, SessError> {
        self.sess_run(&[request])
            .pop()
            .unwrap_or_else(|| Err(SessError::Failed("omp produced no result".into())))
    }

    /// Pins or unpins session `id` in omp's `session-pins.json` (a no-op when already so).
    /// omp locks and atomically replaces the file; read it with [`crate::pins::read`].
    pub fn set_pinned(&self, id: &str, pinned: bool) -> Result<(), SessError> {
        self.sess_one(json!({"op": "set_pinned", "id": id, "pinned": pinned}))
            .map(drop)
    }

    /// Renames a session that is not open in this process, like omp's resume picker: a user
    /// `title_change` entry plus the header title. The session lease is claimed before the file
    /// is loaded; a live omp owning it → [`SessError::Busy`], nothing written. Returns the title
    /// as stored (omp cleans it).
    pub fn rename_session(&self, file: &Path, title: &str) -> Result<String, SessError> {
        let v = self
            .sess_one(json!({"op": "rename_session", "file": path_text(file)?, "title": title}))?;
        Ok(v.get("title")
            .and_then(Value::as_str)
            .unwrap_or(title)
            .to_owned())
    }

    /// Sets (or, with `None`/blank, removes) the label of `entry` in a session that is not open
    /// in this process: a `label` entry appended under the session lease
    /// ([`SessError::Busy`] when a live omp owns it). An unknown entry is omp's message.
    pub fn set_label(
        &self,
        file: &Path,
        entry: &str,
        label: Option<&str>,
    ) -> Result<(), SessError> {
        self.sess_one(json!({
            "op": "set_label", "file": path_text(file)?, "entryId": entry, "label": label,
        }))
        .map(drop)
    }

    /// The Claude Code or Codex sessions omp finds under the current HOME (`~/.claude`,
    /// `CLAUDE_CONFIG_DIR`, `~/.codex`), newest first.
    pub fn foreign_sessions(
        &self,
        source: ForeignSource,
    ) -> Result<Vec<ForeignSession>, SessError> {
        let v = self.sess_one(json!({"op": "foreign_sessions", "source": source.as_str()}))?;
        serde_json::from_value(v.get("sessions").cloned().unwrap_or(Value::Null))
            .map_err(|e| SessError::Failed(format!("unexpected foreign session list: {e}")))
    }

    /// Imports foreign session `id` as a new omp session (fresh omp id, a
    /// `foreign_session_import` entry recording the source) and returns its file. `cwd` replaces
    /// the foreign project directory when that no longer exists.
    pub fn import_foreign(
        &self,
        source: ForeignSource,
        id: &str,
        cwd: &Path,
    ) -> Result<PathBuf, SessError> {
        let v = self.sess_one(json!({
            "op": "import_foreign", "source": source.as_str(), "id": id, "cwd": path_text(cwd)?,
        }))?;
        v.get("file")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| SessError::Failed("the import produced no file".into()))
    }

    /// Reads the `WATCHDOG.yml` of `scope` for the project of `cwd` (an absent or unreadable file
    /// is an empty document; problems are in [`AdvisorDoc::warnings`]).
    pub fn advisor_config(&self, cwd: &Path, scope: AdvisorScope) -> Result<AdvisorDoc, SessError> {
        let v = self.sess_one(json!({
            "op": "advisor_config", "cwd": path_text(cwd)?, "scope": scope.as_str(),
        }))?;
        let mut doc: AdvisorDoc =
            serde_json::from_value(v.get("doc").cloned().unwrap_or(Value::Null))
                .map_err(|e| SessError::Failed(format!("unexpected advisor document: {e}")))?;
        doc.path = v.get("path").and_then(Value::as_str).map(PathBuf::from);
        Ok(doc)
    }

    /// Writes `doc` to the `WATCHDOG.yml` of `scope` (an empty document deletes the file). A live
    /// omp picks it up through ctl `AdvisorConfig`; this does not touch running sessions.
    pub fn save_advisor_config(
        &self,
        cwd: &Path,
        scope: AdvisorScope,
        doc: &AdvisorDoc,
    ) -> Result<AdvisorSaved, SessError> {
        let v = self.sess_one(json!({
            "op": "save_advisor_config", "cwd": path_text(cwd)?, "scope": scope.as_str(), "doc": doc,
        }))?;
        let path = v
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| SessError::Failed("the save reported no file".into()))?;
        Ok(AdvisorSaved {
            path,
            removed: v.get("removed").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

/// Paths travel as JSON text.
fn path_text(p: &Path) -> Result<&str, SessError> {
    p.to_str()
        .ok_or_else(|| SessError::Failed("path is not valid UTF-8".into()))
}

/// Fills `replies` from the result file's complete lines.
fn read_replies(out: &Path, replies: &mut [Option<Value>]) {
    let Ok(text) = std::fs::read_to_string(out) else {
        return;
    };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let slot = v
            .get("rid")
            .and_then(Value::as_u64)
            .and_then(|r| replies.get_mut(r as usize));
        if let Some(slot) = slot
            && slot.is_none()
        {
            *slot = Some(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advisor_doc_sends_only_what_is_set_and_reads_omp_output() {
        let doc: AdvisorDoc = serde_json::from_value(json!({
            "instructions": "be brief", "maxNotesPerUpdate": 3,
            "advisors": [
                { "name": "Rev", "tools": [], "reviewMode": "turn", "syncBacklog": "3", "scope": "main", "unknown": 1 },
                { "name": "Bare" },
            ],
            "warnings": ["w"],
        }))
        .unwrap();
        assert_eq!(
            doc.advisors[0].tools,
            Some(vec![]),
            "[] means no tools, not default"
        );
        assert_eq!(doc.advisors[1].tools, None);
        let sent = serde_json::to_value(&doc.advisors[1]).unwrap();
        assert_eq!(
            sent,
            json!({ "name": "Bare" }),
            "unset fields are omitted, never null"
        );
        let sent = serde_json::to_value(&doc.advisors[0]).unwrap();
        assert_eq!(sent["tools"], json!([]));
        assert!(AdvisorDoc::default().is_empty());
        assert!(!doc.is_empty());
        let only_blank = AdvisorDoc {
            instructions: Some("  \n".into()),
            ..AdvisorDoc::default()
        };
        assert!(
            only_blank.is_empty(),
            "omp deletes the file when nothing but blanks remains"
        );
    }

    #[test]
    fn replies_are_matched_by_rid_and_partial_lines_wait() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("o");
        std::fs::write(&out, "{\"ok\":true,\"rid\":1}\n{\"ok\":tr").unwrap();
        let mut replies = vec![None, None, None];
        read_replies(&out, &mut replies);
        assert!(replies[0].is_none() && replies[2].is_none());
        assert_eq!(replies[1].as_ref().unwrap()["rid"], 1);
        std::fs::write(
            &out,
            "{\"ok\":true,\"rid\":1}\n{\"ok\":false,\"error\":\"busy\",\"rid\":0}\n",
        )
        .unwrap();
        read_replies(&out, &mut replies);
        assert_eq!(replies[0].as_ref().unwrap()["error"], "busy");
    }
}
