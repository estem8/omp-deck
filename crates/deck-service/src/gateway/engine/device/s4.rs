//! Slice-4 device operations and feeds (`docs/notes/mobile-pwa.md`, "Wire additions"):
//! downloads and uploads, the folder browser, the session tree, the session manager's
//! inventory and running view, usage, subagents, worktrees, config backups and project sync,
//! updates, omp settings, Web Push and the device's own settings. Everything is answered in
//! device terms: folders as `~` displays or opaque tokens, sessions by id, running sessions by
//! running id.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use deck_core::backup::{BackupFile, Destination, ProjectBackup, ProjectSyncState};
use deck_core::folders::DirListing;
use deck_core::sessions::ProjectGroup;
use deck_remote::api::{self, ApiError, Feed, FeedValue, Op, ReplyResult, Resp};
use sha2::{Digest, Sha256};

use super::transfer::{ChunkOutcome, Slot, Source};
use super::{
    Canon, DeviceConn, FeedState, api_error, bad, count, cut, fit_feed, live_ref, not_found,
    project_id, resolve_project, resolve_session, session_in, unix_ms, valid_agent, views,
};
use crate::attachments::{AttachError, AttachmentStore};
use crate::controller::{Command, RequestKey, SessionTarget};
use crate::gateway::engine::{Engine, Fin, Sink, lock};
use crate::gateway::jobs::Confine;
use crate::gateway::protocol::{
    self, Answer, Done, GatewayError, LaunchSpec, Mutation, Query, Reply, RunningDetail,
    SessionRole, Topic, TopicValue,
};
use crate::gateway::push::{self, PushError, PushSub};

/// The running view's detail (memory, titles) is re-read this often while followed.
const RUNNING_EVERY: Duration = Duration::from_secs(10);
/// Project sync times are re-read this often while followed.
const SYNC_EVERY: Duration = Duration::from_secs(60);
/// An answered `unpair` revokes at the latest after this (its reply normally goes first).
pub(super) const UNPAIR_GRACE: Duration = Duration::from_secs(5);
const BROWSE_LIMIT: usize = 1000;
/// Largest image handed out by `blob`.
const BLOB_MAX: u64 = 32 * 1024 * 1024;
/// Stats requests remembered (path, modified) so a file that fails is not asked again.
const STATS_ASKED: usize = 20_000;
/// Session files whose stats one worker computes.
const STATS_BATCH: usize = 200;

/// What the engine keeps for the slice-4 feeds and ops, shared by every device.
#[derive(Default)]
pub(in crate::gateway::engine) struct Sources {
    running: Option<Arc<RunningDetail>>,
    running_busy: bool,
    running_at: Option<Instant>,
    running_seen: Option<Arc<crate::gateway::running::RunningSnapshot>>,
    /// Running ids each `running_kill` named (key → (id, mux name)).
    kills: BTreeMap<RequestKey, Vec<(String, String)>>,
    /// The last config-backup listing a device read, with the place it came from: ids resolve
    /// against it only while that place is still where backups go.
    backup_list: Option<(BackupPlace, Vec<BackupFile>)>,
    sync_list: Option<(BackupPlace, Vec<ProjectBackup>)>,
    sync_state: Option<ProjectSyncState>,
    sync_busy: bool,
    sync_at: Option<Instant>,
    sync_backup_busy: bool,
    /// The place `sync_state` is (being) read from.
    sync_dest: Option<BackupPlace>,
    /// Requests of devices to a backup server (their failures are shown in device terms).
    server_keys: BTreeSet<RequestKey>,
    stats_busy: bool,
    stats_asked: HashSet<(PathBuf, SystemTime)>,
}

/// Running-kill records kept at most.
const KILLS: usize = 64;
/// Server requests remembered at most.
const SERVER_KEYS: usize = 256;

/// Where backups go, concretely: what cached listings, ids and the sync state describe. Another
/// Google account, another server or another login or folder on it is another place.
#[derive(Clone, Debug)]
pub(crate) struct BackupPlace {
    destination: Destination,
    /// The signed-in Google account (Drive).
    account: Option<String>,
    /// The saved server (Server).
    server: Option<protocol::ServerView>,
}

impl BackupPlace {
    fn of(v: &protocol::BackupView) -> BackupPlace {
        match v.destination {
            Destination::Drive => BackupPlace {
                destination: Destination::Drive,
                account: v.drive_email.clone(),
                server: None,
            },
            Destination::Server => BackupPlace {
                destination: Destination::Server,
                account: None,
                server: v.server.clone(),
            },
        }
    }

    fn is_server(&self) -> bool {
        self.destination == Destination::Server
    }

    /// The same place: what is there is what the other one saw (the server's own rule for a
    /// replaced configuration).
    fn same(&self, other: &BackupPlace) -> bool {
        self.destination == other.destination
            && self.account == other.account
            && match (&self.server, &other.server) {
                (None, None) => true,
                (Some(a), Some(b)) => a.same_place(b),
                _ => false,
            }
    }
}

/// What a device's query was for.
pub(crate) enum Kind {
    Activity,
    OmpSettings,
    BackupList(BackupPlace),
    SyncBackups(BackupPlace, Option<String>),
}

/// The folder browser's roots (home first when there is one).
struct BrowseRoots {
    paths: Vec<PathBuf>,
    /// `paths[0]` is the home folder.
    home: bool,
}

impl BrowseRoots {
    /// The deepest root `dir` lies at or below, by `below` (the path relative to the base).
    fn containing(
        &self,
        dir: &Path,
        below: impl Fn(&Path, &Path) -> Option<PathBuf>,
    ) -> Option<usize> {
        self.paths
            .iter()
            .enumerate()
            .filter(|(_, root)| below(dir, root).is_some())
            .max_by_key(|(_, root)| root.components().count())
            .map(|(i, _)| i)
    }

    /// What a device sees of each root: the home folder as its `~` display, any other by its
    /// folder name (never the absolute path), told apart by a count when names repeat.
    fn labels(&self, home_display: impl Fn(&Path) -> String) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(self.paths.len());
        for (i, root) in self.paths.iter().enumerate() {
            let base = if self.home && i == 0 {
                home_display(root)
            } else {
                root.file_name()
                    .map_or_else(|| "/".to_owned(), |n| n.to_string_lossy().into_owned())
            };
            let mut label = base.clone();
            let mut n = 1;
            while out.contains(&label) {
                n += 1;
                label = format!("{base} ({n})");
            }
            out.push(label);
        }
        out
    }
}

/// A dir token's folder: as handed out (what the browser shows and lists below) and resolved
/// (where it leads, inside the roots: what every use takes).
struct Folder {
    shown: PathBuf,
    resolved: PathBuf,
}

/// `label/rest` with `/` separators on every platform (just `label` for the root itself).
fn join_label(label: &str, rest: &Path) -> String {
    rest.components().fold(label.to_owned(), |mut acc, c| {
        if !acc.ends_with('/') {
            acc.push('/');
        }
        acc.push_str(&c.as_os_str().to_string_lossy());
        acc
    })
}

/// Results of the device side's workers ([`Engine::on_device_fin`]).
pub(crate) enum DeviceFin {
    Answer {
        conn: u64,
        id: u64,
        kind: Kind,
        reply: Reply,
    },
    Download {
        conn: u64,
        id: u64,
        /// The reserved slot this worker held.
        slot: Slot,
        result: Result<(Source, String, String, u64), ApiError>,
    },
    Stored {
        conn: u64,
        id: u64,
        upload: String,
        result: Result<PathBuf, AttachError>,
    },
    Browse {
        conn: u64,
        id: u64,
        dir: PathBuf,
        result: io::Result<DirListing>,
    },
    PushChecked {
        conn: u64,
        id: u64,
        result: Result<PushSub, PushError>,
    },
    Running(RunningDetail),
    Stats,
    SyncState(BackupPlace, Option<ProjectSyncState>),
}

/// A slice-4 operation (sent only by, and answered only for, `s4` devices).
pub(super) fn s4_op(op: &Op) -> bool {
    !matches!(
        op,
        Op::Subscribe { .. }
            | Op::Unsubscribe { .. }
            | Op::Outcome { .. }
            | Op::Models {}
            | Op::Diff { .. }
            | Op::Attach { .. }
            | Op::Detach { .. }
            | Op::Ping {}
            | Op::NewSession { .. }
            | Op::Resume { .. }
            | Op::Restart { .. }
            | Op::Stop { .. }
            | Op::Kill { .. }
            | Op::Prompt { .. }
            | Op::Interrupt { .. }
            | Op::SwitchModel { .. }
            | Op::SetEffort { .. }
            | Op::Action { .. }
            | Op::Answer { .. }
            | Op::Cancel { .. }
            | Op::InboxOpen { .. }
            | Op::InboxDismiss { .. }
            | Op::InboxClear {}
            | Op::Stage { .. }
            | Op::Unstage { .. }
            | Op::Discard { .. }
            | Op::DeleteUntracked { .. }
            | Op::DeleteSessions { .. }
            | Op::CopySession { .. }
            | Op::MoveSession { .. }
    )
}

/// A slice-4 feed.
pub(super) fn s4_feed(feed: &Feed) -> bool {
    !matches!(
        feed,
        Feed::Projects {}
            | Feed::Sessions { .. }
            | Feed::Live {}
            | Feed::Inbox {}
            | Feed::Cards { .. }
            | Feed::Transcript { .. }
            | Feed::Changes { .. }
    )
}

/// A parked branch's device id.
fn parked_id(primary: &Path, branch: &str) -> String {
    let mut h = Sha256::new();
    h.update(primary.to_string_lossy().as_bytes());
    h.update([0]);
    h.update(branch.as_bytes());
    format!("p-{}", views::hex16(&h.finalize()))
}

fn sync_parts(p: &api::SyncParts) -> deck_core::backup::Parts {
    deck_core::backup::Parts {
        omp: p.omp,
        sessions: p.sessions,
        memory: p.memory,
    }
}

/// A backup failure as a device sees it. A backup server's failures carry its login, key file
/// and folder: a device is told what went wrong in fixed words, never those details (the
/// desktop keeps the full text).
fn backup_error(canon: &Canon, e: &GatewayError, server: bool) -> ApiError {
    match e {
        GatewayError::Failed(m) if server => ApiError::Failed {
            message: views::server_failure(m),
        },
        other => api_error(canon, other),
    }
}

fn push_error(e: &PushError) -> ApiError {
    match e {
        PushError::Store(e) => ApiError::Failed {
            message: e.to_string(),
        },
        other => bad(&other.to_string()),
    }
}

/// Runs `f` on a thread of its own (inline when none can be started: it always answers).
fn detached(name: &str, f: impl FnOnce() + Send + 'static) {
    let slot = Arc::new(std::sync::Mutex::new(Some(f)));
    let theirs = slot.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("device-{name}"))
        .spawn(move || {
            if let Some(f) = lock(&theirs).take() {
                f();
            }
        });
    if spawned.is_err()
        && let Some(f) = lock(&slot).take()
    {
        f();
    }
}

/// `nodes` of a tree page as one reply: as many as fit, the rest on the next page.
fn tree_reply(page: super::tree::Page, start: usize) -> ReplyResult {
    let super::tree::Page {
        nodes,
        active_leaf,
        next,
    } = page;
    let all = nodes.len();
    let make = |n: usize| {
        let next = if n < all { Some(start + n) } else { next };
        ReplyResult::Ok(Resp::Tree {
            nodes: nodes[..n].to_vec(),
            active_leaf: active_leaf.clone(),
            next_cursor: next.map(super::tree::cursor),
            truncated: n < all,
        })
    };
    let size = |n: usize| {
        serde_json::to_vec(&api::Reply {
            id: u64::MAX,
            result: make(n),
        })
        .map_or(usize::MAX, |b| b.len())
    };
    if size(all) <= api::MAX_REPLY {
        return make(all);
    }
    let (mut lo, mut hi) = (0, all);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if size(mid) <= api::MAX_REPLY {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    make(lo)
}

impl Engine {
    // ---- shared with the notifier -------------------------------------------------------------

    /// Device `device_id` has the app in the foreground on a live connection (no Web Push).
    pub(in crate::gateway::engine) fn device_visible(&self, device_id: &str) -> bool {
        self.conns
            .values()
            .any(|c| c.client_id == device_id && matches!(&c.sink, Sink::Device(d) if d.visible))
    }

    /// What a push for `target` opens: its session's conversation by id, else (its file is not in
    /// the latest scan yet) by live key. `None`: no such live session.
    pub(in crate::gateway::engine) fn push_opens(
        &self,
        target: &SessionTarget,
    ) -> Option<crate::gateway::push::Opens> {
        use crate::gateway::push::Opens;
        let m = self
            .sess
            .managed
            .iter()
            .find(|m| m.ctl.target().key == target.key)?;
        let groups = self.groups();
        Some(
            m.ctl
                .session_path()
                .and_then(|p| session_in(&groups, &self.devices.canon, p))
                .map_or_else(
                    || Opens::Live(target.key.to_string()),
                    |(_, s)| Opens::Session(s.id.clone()),
                ),
        )
    }

    // ---- requests -----------------------------------------------------------------------------

    pub(super) fn device_conn(&self, conn: u64) -> Option<&DeviceConn> {
        match &self.conns.get(&conn)?.sink {
            Sink::Device(d) => Some(d),
            Sink::Local(_) => None,
        }
    }

    fn device_query(&self, conn: u64, id: u64, kind: Kind, q: Query) {
        let ctx = self.job_ctx();
        self.spawn_worker("device-query", move || {
            let reply = crate::gateway::jobs::query(&ctx, &q)
                .unwrap_or_else(|| Err(GatewayError::Failed("not a worker query".to_owned())));
            Fin::Device(Box::new(DeviceFin::Answer {
                conn,
                id,
                kind,
                reply,
            }))
        });
    }

    /// A non-journaled slice-4 operation (`device`: its id).
    pub(super) fn on_s4_request(&mut self, conn: u64, id: u64, device: &str, op: Op) {
        let r = self.s4_request(conn, id, device, op);
        // `Ok(None)`: answered later.
        match r {
            Ok(Some(resp)) => self.device_answer(conn, id, Ok(resp)),
            Ok(None) => {}
            Err(e) => self.device_answer(conn, id, Err(e)),
        }
    }

    fn s4_request(
        &mut self,
        conn: u64,
        id: u64,
        device: &str,
        op: Op,
    ) -> Result<Option<Resp>, ApiError> {
        let now = Instant::now();
        let groups = self.groups();
        match op {
            Op::LastReply { session } => {
                let path = resolve_session(&groups, &session)?.path.clone();
                let out = self.device_out(conn)?;
                detached("last-reply", move || {
                    let mut reader = deck_core::status::SessionStatsReader::new(path);
                    let result = match reader.refresh() {
                        Err(_) => ReplyResult::Err(ApiError::Failed {
                            message: "the session could not be read".to_owned(),
                        }),
                        Ok(_) => match &reader.stats().last_assistant_text {
                            None => ReplyResult::Err(not_found("reply")),
                            Some(text) => ReplyResult::Ok(Resp::Text {
                                text: cut(text, api::MAX_LAST_REPLY),
                                truncated: text.len() > api::MAX_LAST_REPLY,
                            }),
                        },
                    };
                    out.reply(id, result);
                });
                Ok(None)
            }
            Op::Export { session } => {
                let path = resolve_session(&groups, &session)?.path.clone();
                let program = self
                    .omp
                    .as_ref()
                    .map(|o| o.program().to_path_buf())
                    .ok_or_else(|| ApiError::Failed {
                        message: "omp was not found".to_owned(),
                    })?;
                self.device_mut(conn)
                    .ok_or_else(|| bad("the session ended"))?
                    .transfers
                    .reserve(Slot::Export, now)?;
                self.spawn_worker("device-export", move || {
                    let dest = crate::session_actions::exports_dir();
                    // The message is scrubbed on the engine thread.
                    let result = crate::session_actions::export_html(&program, &path, &dest)
                        .map_err(|message| ApiError::Failed { message })
                        .and_then(|file| {
                            let size = std::fs::metadata(&file)
                                .map_err(|_| ApiError::Failed {
                                    message: "the export could not be read".to_owned(),
                                })?
                                .len();
                            let name = file.file_name().map_or_else(
                                || "session.html".to_owned(),
                                |n| n.to_string_lossy().into_owned(),
                            );
                            Ok((Source::File(file), name, "text/html".to_owned(), size))
                        });
                    Fin::Device(Box::new(DeviceFin::Download {
                        conn,
                        id,
                        slot: Slot::Export,
                        result,
                    }))
                });
                Ok(None)
            }
            Op::Blob {
                session,
                agent,
                blob,
            } => {
                if !views::valid_blob(&blob) {
                    return Err(bad("not a blob id"));
                }
                let s = resolve_session(&groups, &session)?;
                let transcript = match agent {
                    None => s.path.clone(),
                    Some(a) if !valid_agent(&a) => return Err(bad("not a subagent id")),
                    Some(a) => {
                        super::agent_transcript(&s.path, &a).ok_or_else(|| not_found("subagent"))?
                    }
                };
                self.device_mut(conn)
                    .ok_or_else(|| bad("the session ended"))?
                    .transfers
                    .reserve(Slot::Blob, now)?;
                self.spawn_worker("device-blob", move || {
                    let result = match deck_core::agents::read_blob(&transcript, &blob, BLOB_MAX) {
                        Ok(Some((mime, bytes))) if !views::raster_image(&mime, &bytes) => {
                            Err(bad("the blob is not a raster image"))
                        }
                        Ok(Some((mime, bytes))) => {
                            let size = bytes.len() as u64;
                            let name = views::blob_name(&blob, &mime);
                            Ok((Source::Bytes(Arc::new(bytes)), name, mime, size))
                        }
                        Ok(None) => Err(not_found("image")),
                        Err(_) => Err(ApiError::Failed {
                            message: "the image could not be read".to_owned(),
                        }),
                    };
                    Fin::Device(Box::new(DeviceFin::Download {
                        conn,
                        id,
                        slot: Slot::Blob,
                        result,
                    }))
                });
                Ok(None)
            }
            Op::Download { download, offset } => {
                let d = self
                    .device_mut(conn)
                    .ok_or_else(|| bad("the session ended"))?;
                let job = d.transfers.chunk(&download, offset, now)?;
                let out = d.out.clone();
                detached("download", move || {
                    out.reply(
                        id,
                        match job.read() {
                            Ok(r) => ReplyResult::Ok(r),
                            Err(e) => ReplyResult::Err(e),
                        },
                    );
                });
                Ok(None)
            }
            Op::Tree {
                session,
                filter,
                cursor,
                limit,
            } => {
                let path = resolve_session(&groups, &session)?.path.clone();
                let start = match cursor.as_deref() {
                    None => 0,
                    Some(c) => super::tree::offset(c).ok_or_else(|| bad("not a tree cursor"))?,
                };
                // "You are here": the live session's current entry.
                let leaf = self
                    .sess
                    .managed
                    .iter()
                    .filter(|m| {
                        m.ctl
                            .session_path()
                            .is_some_and(|p| super::same_spelling(p, &path))
                    })
                    .find_map(|m| m.live.as_ref()?.client.latest()?.leaf.clone());
                let out = self.device_out(conn)?;
                detached("tree", move || {
                    let result = match deck_core::tree::SessionTree::load(&path) {
                        Ok(tree) => tree_reply(
                            super::tree::page(
                                &tree,
                                filter,
                                leaf.as_deref(),
                                start,
                                limit as usize,
                            ),
                            start,
                        ),
                        Err(_) => ReplyResult::Err(ApiError::Failed {
                            message: "the session could not be read".to_owned(),
                        }),
                    };
                    out.reply(id, result);
                });
                Ok(None)
            }
            Op::Browse { dir } => match dir {
                None => Ok(Some(self.browse_roots(conn)?)),
                Some(token) => {
                    let folder = self.folder_of(conn, &token)?;
                    let confine = self.browse_confine();
                    self.spawn_worker("device-browse", move || {
                        // Listed where it leads (checked again here); an entry leading out of
                        // every root is not offered. Entries keep the spelling handed out.
                        let roots = confine.resolve();
                        let result = match roots.admit(&folder.resolved) {
                            Some(real) => deck_core::folders::list_dirs(&real, BROWSE_LIMIT).map(
                                |mut listing| {
                                    listing.entries.retain_mut(|e| {
                                        e.path = folder.shown.join(&e.name);
                                        roots.admit(&real.join(&e.name)).is_some()
                                    });
                                    listing
                                },
                            ),
                            None => Err(std::io::ErrorKind::NotFound.into()),
                        };
                        let dir = folder.shown;
                        Fin::Device(Box::new(DeviceFin::Browse {
                            conn,
                            id,
                            dir,
                            result,
                        }))
                    });
                    Ok(None)
                }
            },
            Op::UsageActivity { days } => {
                self.device_query(conn, id, Kind::Activity, Query::UsageActivity { days });
                Ok(None)
            }
            Op::UsageRefresh {} => {
                if self.omp.is_none() {
                    return Err(ApiError::Failed {
                        message: "omp was not found".to_owned(),
                    });
                }
                if !self.sched.usage_fetching() {
                    self.sched.begin_usage_fetch(now);
                    self.dirty.usage = true;
                    let ctx = self.job_ctx();
                    self.spawn_worker("usage-refresh", move || {
                        crate::gateway::engine::Sched::Usage(
                            crate::gateway::engine::mutate::fetch_usage(&ctx),
                        )
                        .into()
                    });
                }
                Ok(Some(Resp::Unit {}))
            }
            Op::OmpSettings {} => {
                self.device_query(conn, id, Kind::OmpSettings, Query::OmpSettings);
                Ok(None)
            }
            Op::BackupList {} => {
                let place = self.backup_place();
                self.device_query(conn, id, Kind::BackupList(place), Query::BackupList);
                Ok(None)
            }
            Op::SyncBackups { project } => {
                let place = self.backup_place();
                self.device_query(
                    conn,
                    id,
                    Kind::SyncBackups(place, project),
                    Query::ProjectBackups,
                );
                Ok(None)
            }
            Op::Presence { visible } => {
                let d = self
                    .device_mut(conn)
                    .ok_or_else(|| bad("the session ended"))?;
                d.visible = visible;
                Ok(Some(Resp::Unit {}))
            }
            Op::PushSubscribe {
                endpoint,
                p256dh,
                auth,
            } => {
                self.spawn_worker("push-check", move || {
                    let result = push::check_subscription(&endpoint, &p256dh, &auth);
                    Fin::Device(Box::new(DeviceFin::PushChecked { conn, id, result }))
                });
                Ok(None)
            }
            Op::PushUnsubscribe {} => {
                let reg = self
                    .mobile
                    .registry_mut()
                    .ok_or_else(|| bad("mobile access is off"))?;
                match push::unsubscribe(reg, device) {
                    Ok(()) | Err(PushError::NotSubscribed) => {}
                    Err(e) => return Err(push_error(&e)),
                }
                self.notify.push.purge(device);
                self.dirty.mobile = true;
                Ok(Some(Resp::Unit {}))
            }
            Op::PushPrefs { prefs } => {
                let reg = self
                    .mobile
                    .registry_mut()
                    .ok_or_else(|| bad("mobile access is off"))?;
                push::set_prefs(reg, device, prefs).map_err(|e| push_error(&e))?;
                self.dirty.mobile = true;
                Ok(Some(Resp::Unit {}))
            }
            Op::DeviceRename { name } => {
                match self.mobile_execute(Mutation::DeviceRename {
                    id: device.to_owned(),
                    name,
                }) {
                    Ok(_) => Ok(Some(Resp::Unit {})),
                    Err(GatewayError::Refused(m)) => Err(bad(&m)),
                    Err(e) => Err(api_error(&self.devices.canon, &e)),
                }
            }
            Op::Unpair {} => {
                let d = self
                    .device_mut(conn)
                    .ok_or_else(|| bad("the session ended"))?;
                d.unpair_at = Some(now);
                self.waker.now();
                Ok(Some(Resp::Unit {}))
            }
            Op::UploadBegin { name, size, .. } => {
                let d = self
                    .device_mut(conn)
                    .ok_or_else(|| bad("the session ended"))?;
                let upload = d.transfers.begin_upload(&name, size, now)?;
                Ok(Some(Resp::Upload { upload }))
            }
            Op::UploadChunk {
                upload,
                offset,
                data,
            } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data.as_bytes())
                    .map_err(|_| bad("an upload chunk is base64"))?;
                let d = self
                    .device_mut(conn)
                    .ok_or_else(|| bad("the session ended"))?;
                match d.transfers.upload_chunk(&upload, offset, &bytes, now)? {
                    ChunkOutcome::More => Ok(Some(Resp::Unit {})),
                    ChunkOutcome::Complete { name, bytes } => {
                        // Counted here, before a revocation on this thread could pass it.
                        let writer = self.devices.revoked.writer(device);
                        self.spawn_worker("attachment", move || {
                            let result = writer.and_then(|w| {
                                AttachmentStore::open().and_then(|s| s.store_for(&w, &name, &bytes))
                            });
                            Fin::Device(Box::new(DeviceFin::Stored {
                                conn,
                                id,
                                upload,
                                result,
                            }))
                        });
                        Ok(None)
                    }
                }
            }
            Op::UploadCancel { upload } => {
                let d = self
                    .device_mut(conn)
                    .ok_or_else(|| bad("the session ended"))?;
                if d.transfers.cancel_upload(&upload) {
                    Ok(Some(Resp::Unit {}))
                } else {
                    Err(not_found("upload"))
                }
            }
            Op::UpdateCheck {} => {
                self.device_update_check();
                Ok(Some(Resp::Unit {}))
            }
            // Journaled ops never come here.
            _ => Err(bad("seq is required")),
        }
    }

    fn device_out(
        &self,
        conn: u64,
    ) -> Result<Arc<crate::gateway::remote::device::DeviceOut>, ApiError> {
        self.device_conn(conn)
            .map(|d| d.out.clone())
            .ok_or_else(|| bad("the session ended"))
    }

    /// Checks omp and OmpDeck for updates (each unless a check runs).
    fn device_update_check(&mut self) {
        let omp_view = self.sched.omp_view();
        if !omp_view.checking
            && let Some(omp) = self.omp.clone()
        {
            self.sched.begin_omp_check();
            self.dirty.omp = true;
            self.spawn_worker("omp-update-check", move || {
                crate::gateway::engine::Sched::OmpChecked(
                    omp.check_update().map_err(|e| e.to_string()),
                )
                .into()
            });
        }
        if !matches!(self.sched.app_view().phase, protocol::UpdatePhase::Checking) {
            self.sched.begin_app_check();
            self.dirty.app = true;
            self.spawn_worker("app-update-check", move || {
                crate::gateway::engine::Sched::AppChecked(
                    deck_core::selfupdate::latest().map_err(|e| e.to_string()),
                )
                .into()
            });
        }
    }

    /// The folder browser's roots: the home folder, then the folders holding projects outside
    /// it (a project directly below a file system root is its own root: browsing never starts
    /// at `/`).
    fn browse_root_paths(&self) -> BrowseRoots {
        let groups = self.groups();
        let home = dirs::home_dir().filter(|h| !h.as_os_str().is_empty());
        let mut paths: Vec<PathBuf> = home.iter().cloned().collect();
        for g in groups.iter() {
            let inside = home.as_deref().is_some_and(|h| g.cwd.starts_with(h));
            if inside {
                continue;
            }
            let root = match g.cwd.parent() {
                Some(parent) if parent.parent().is_some() => parent,
                _ => g.cwd.as_path(),
            };
            if !paths.iter().any(|r| r == root) && root.is_dir() {
                paths.push(root.to_path_buf());
            }
        }
        BrowseRoots {
            paths,
            home: home.is_some(),
        }
    }

    /// `dir` if it lies at or below a browse root (the only folders a device may browse or
    /// start a session in).
    fn dir_in_roots(&self, roots: &BrowseRoots, dir: &Path) -> Option<usize> {
        roots.containing(dir, |p, base| self.devices.canon.below(p, base))
    }

    fn browse_roots(&mut self, conn: u64) -> Result<Resp, ApiError> {
        let groups = self.groups();
        let roots = self.browse_root_paths();
        let labels = roots.labels(|r| self.devices.canon.display(r));
        let d = self
            .device_mut(conn)
            .ok_or_else(|| bad("the session ended"))?;
        let entries: Vec<api::DirEntry> = roots
            .paths
            .iter()
            .zip(labels)
            .map(|(r, name)| api::DirEntry {
                name,
                dir: d.dirs.token(r),
                project: groups
                    .iter()
                    .find(|g| super::same_spelling(&g.cwd, r))
                    .map(|g| project_id(&g.cwd)),
                repo: r.join(".git").exists(),
            })
            .collect();
        Ok(Resp::Dir {
            dir: None,
            display: String::new(),
            parent: None,
            entries,
            truncated: false,
        })
    }

    // ---- worker results -----------------------------------------------------------------------

    pub(in crate::gateway::engine) fn on_device_fin(&mut self, f: DeviceFin) {
        match f {
            DeviceFin::Answer {
                conn,
                id,
                kind,
                reply,
            } => {
                let r = self.answer_s4(kind, reply);
                if let Some(d) = self.device_conn(conn) {
                    d.out.reply(id, r);
                }
            }
            DeviceFin::Download {
                conn,
                id,
                slot,
                result,
            } => {
                let result = result.map_err(|e| match e {
                    ApiError::Failed { message } => ApiError::Failed {
                        message: self.devices.canon.scrub(&message),
                    },
                    e => e,
                });
                let Some(d) = self.device_mut(conn) else {
                    // The device session ended: its transfers (and slot) went with it.
                    if let Ok((source, ..)) = result {
                        source.discard();
                    }
                    return;
                };
                let r = match result {
                    Err(e) => {
                        d.transfers.release(slot);
                        Err(e)
                    }
                    Ok((source, name, mime, size)) => d
                        .transfers
                        .fulfil(slot, source, size, Instant::now())
                        .map(|download| Resp::Download {
                            download,
                            name,
                            mime,
                            size,
                        })
                        .ok_or(ApiError::TooManyRequests {}),
                };
                self.device_answer(conn, id, r);
            }
            DeviceFin::Stored {
                conn,
                id,
                upload,
                result,
            } => {
                let r = match result {
                    Ok(path) => {
                        if let Some(d) = self.device_mut(conn) {
                            d.transfers.stored(&upload, Some(path), Instant::now());
                        }
                        Ok(Resp::Unit {})
                    }
                    Err(e) => {
                        if let Some(d) = self.device_mut(conn) {
                            d.transfers.stored(&upload, None, Instant::now());
                        }
                        Err(match e {
                            AttachError::TooLarge => ApiError::TooLarge {},
                            e => ApiError::Failed {
                                message: self.devices.canon.scrub(&e.to_string()),
                            },
                        })
                    }
                };
                self.device_answer(conn, id, r);
            }
            DeviceFin::Browse {
                conn,
                id,
                dir,
                result,
            } => {
                let r = match result {
                    Ok(listing) => self.browse_reply(conn, &dir, listing),
                    Err(_) => ReplyResult::Err(ApiError::Failed {
                        message: "the folder could not be read".to_owned(),
                    }),
                };
                if let Some(d) = self.device_conn(conn) {
                    d.out.reply(id, r);
                }
            }
            DeviceFin::PushChecked { conn, id, result } => {
                let device = self.conns.get(&conn).map(|c| c.client_id.clone());
                let r = match (result, device) {
                    (_, None) => return,
                    (Err(e), _) => Err(push_error(&e)),
                    (Ok(sub), Some(device)) => match self.mobile.registry_mut() {
                        None => Err(bad("mobile access is off")),
                        Some(reg) => push::subscribe(reg, &device, sub)
                            .map(|()| Resp::Unit {})
                            .map_err(|e| push_error(&e)),
                    },
                };
                if r.is_ok() {
                    self.dirty.mobile = true;
                }
                self.device_answer(conn, id, r);
            }
            DeviceFin::Running(detail) => {
                let s = &mut self.devices.s4;
                s.running_busy = false;
                s.running_at = Some(Instant::now());
                s.running = Some(Arc::new(detail));
                self.s4_dirty(|f| matches!(f, Feed::Running {}));
            }
            DeviceFin::Stats => {
                self.devices.s4.stats_busy = false;
                self.s4_dirty(|f| matches!(f, Feed::Inventory { .. }));
            }
            DeviceFin::SyncState(dest, state) => {
                let current = self.backup_place();
                let s = &mut self.devices.s4;
                s.sync_busy = false;
                s.sync_at = Some(Instant::now());
                // The place changed while it was read: the next look reads the new one.
                if dest.same(&current) {
                    s.sync_state = state;
                }
                self.s4_dirty(|f| matches!(f, Feed::Sync {}));
            }
        }
    }

    fn answer_s4(&mut self, kind: Kind, reply: Reply) -> ReplyResult {
        let canon = &self.devices.canon;
        match (kind, reply) {
            (Kind::BackupList(place) | Kind::SyncBackups(place, _), Err(e)) => {
                ReplyResult::Err(backup_error(canon, &e, place.is_server()))
            }
            (_, Err(e)) => ReplyResult::Err(api_error(canon, &e)),
            (Kind::Activity, Ok(Answer::UsageActivity(a))) => ReplyResult::Ok(Resp::Activity {
                days: views::days(&a),
            }),
            (Kind::OmpSettings, Ok(Answer::OmpSettings(c))) => {
                views::fit_reply(&views::settings_groups(&c), |groups, truncated| {
                    Resp::OmpSettings { groups, truncated }
                })
            }
            (Kind::BackupList(place), Ok(Answer::BackupList(files))) => {
                let list = views::newest_backups(&files);
                self.devices.s4.backup_list = Some((place, list.clone()));
                views::fit_reply(&list, |files, truncated| Resp::BackupList {
                    hosts: views::backup_hosts(&files),
                    truncated,
                })
            }
            (Kind::SyncBackups(place, project), Ok(Answer::ProjectBackups(list))) => {
                self.devices.s4.sync_list = Some((place, list.clone()));
                let mut rows: Vec<&ProjectBackup> = list
                    .iter()
                    .filter(|b| {
                        project
                            .as_ref()
                            .is_none_or(|p| project_id(Path::new(&b.path)) == *p)
                    })
                    .collect();
                rows.sort_by_key(|b| {
                    std::cmp::Reverse(deck_core::usage::parse_iso_ms(&b.file.created))
                });
                let backups: Vec<api::SyncBackupView> =
                    rows.iter().map(|b| views::sync_backup(b, canon)).collect();
                views::fit_reply(&backups, |backups, truncated| Resp::SyncBackups {
                    backups,
                    truncated,
                })
            }
            (_, Ok(_)) => ReplyResult::Err(ApiError::Failed {
                message: "unexpected answer".to_owned(),
            }),
        }
    }

    fn browse_reply(&mut self, conn: u64, dir: &Path, listing: DirListing) -> ReplyResult {
        let groups = self.groups();
        let roots = self.browse_root_paths();
        let labels = roots.labels(|r| self.devices.canon.display(r));
        let Some(at) = self.dir_in_roots(&roots, dir) else {
            return ReplyResult::Err(not_found("folder"));
        };
        let display = if roots.home && at == 0 {
            self.devices.canon.display(dir)
        } else {
            let rest = self
                .devices
                .canon
                .below(dir, &roots.paths[at])
                .unwrap_or_default();
            join_label(&labels[at], &rest)
        };
        // The parent is offered only while it stays inside a root.
        let parent = dir
            .parent()
            .filter(|p| self.dir_in_roots(&roots, p).is_some())
            .map(Path::to_path_buf);
        let Some(d) = self.device_mut(conn) else {
            return ReplyResult::Err(bad("the session ended"));
        };
        let token = d.dirs.token(dir);
        let parent = parent.map(|p| d.dirs.token(&p));
        let entries: Vec<api::DirEntry> = listing
            .entries
            .iter()
            .map(|e| api::DirEntry {
                name: e.name.clone(),
                dir: d.dirs.token(&e.path),
                project: groups
                    .iter()
                    .find(|g| super::same_spelling(&g.cwd, &e.path))
                    .map(|g| project_id(&g.cwd)),
                repo: e.repo,
            })
            .collect();
        let cut_listing = listing.truncated;
        views::fit_reply(&entries, |entries, truncated| Resp::Dir {
            dir: Some(token.clone()),
            display: display.clone(),
            parent: parent.clone(),
            entries,
            truncated: truncated || cut_listing,
        })
    }

    /// Marks the matching feeds of every device for recomputation.
    fn s4_dirty(&mut self, hit: impl Fn(&Feed) -> bool) {
        for c in self.conns.values_mut() {
            if let Sink::Device(d) = &mut c.sink {
                for (feed, st) in &mut d.feeds {
                    if hit(feed) {
                        st.dirty = true;
                    }
                }
            }
        }
    }

    // ---- sources ------------------------------------------------------------------------------

    /// Starts the workers the followed slice-4 feeds read (running detail, session stats,
    /// project sync times).
    pub(super) fn step_s4_sources(&mut self, now: Instant) {
        let mut running = false;
        let mut sync = false;
        let mut inventory: Vec<(Option<String>, u32)> = Vec::new();
        for c in self.conns.values() {
            if let Sink::Device(d) = &c.sink {
                for feed in d.feeds.keys() {
                    match feed {
                        Feed::Running {} => running = true,
                        Feed::Sync {} => sync = true,
                        Feed::Inventory { project, limit } => {
                            inventory.push((project.clone(), *limit));
                        }
                        _ => {}
                    }
                }
            }
        }
        if running {
            self.kick_running(now);
        }
        if sync {
            self.kick_sync(now);
        }
        if !inventory.is_empty() {
            self.kick_stats(&inventory);
        }
    }

    fn kick_running(&mut self, now: Instant) {
        let snap = self.running.latest();
        let s = &mut self.devices.s4;
        if s.running_busy {
            return;
        }
        let moved = match (&s.running_seen, &snap) {
            (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
            (None, None) => false,
            _ => true,
        };
        let due = s.running_at.is_none_or(|at| now >= at + RUNNING_EVERY);
        if !(moved || due || s.running.is_none()) {
            let wait = (s.running_at.unwrap_or(now) + RUNNING_EVERY).saturating_duration_since(now);
            self.wake_within(wait);
            return;
        }
        s.running_seen = snap;
        s.running_busy = true;
        let ctx = self.job_ctx();
        self.spawn_worker("device-running", move || {
            let detail = match crate::gateway::jobs::query(&ctx, &Query::RunningDetail) {
                Some(Ok(Answer::RunningDetail(d))) => d,
                _ => RunningDetail {
                    sessions: Vec::new(),
                    tmux: false,
                    tmux_server: None,
                },
            };
            Fin::Device(Box::new(DeviceFin::Running(detail)))
        });
    }

    fn kick_sync(&mut self, now: Instant) {
        let backup_busy = self.backup_view().busy.is_some();
        let place = self.backup_place();
        let s = &mut self.devices.s4;
        // Another place has other times; a finished backup-lane job (a backup) changes them.
        let switched = s.sync_dest.as_ref().is_none_or(|d| !d.same(&place));
        if switched {
            s.sync_state = None;
        }
        let finished = (s.sync_backup_busy && !backup_busy) || switched;
        s.sync_backup_busy = backup_busy;
        if s.sync_busy {
            return;
        }
        let due = s.sync_at.is_none_or(|at| now >= at + SYNC_EVERY);
        if !(due || finished) {
            let wait = (s.sync_at.unwrap_or(now) + SYNC_EVERY).saturating_duration_since(now);
            self.wake_within(wait);
            return;
        }
        s.sync_busy = true;
        s.sync_dest = Some(place.clone());
        let ctx = self.job_ctx();
        self.spawn_worker("device-sync-state", move || {
            let state = match crate::gateway::jobs::query(&ctx, &Query::ProjectSyncState) {
                Some(Ok(Answer::ProjectSyncState(s))) => Some(s),
                _ => None,
            };
            Fin::Device(Box::new(DeviceFin::SyncState(place, state)))
        });
    }

    fn kick_stats(&mut self, feeds: &[(Option<String>, u32)]) {
        if self.devices.s4.stats_busy {
            return;
        }
        let entries = lock(&self.inventory_shared).clone();
        let mut want: Vec<PathBuf> = Vec::new();
        {
            let stats = lock(&self.stats);
            let s = &mut self.devices.s4;
            if s.stats_asked.len() > STATS_ASKED {
                s.stats_asked.clear();
            }
            for (project, limit) in feeds {
                for e in inventory_page(&entries, project.as_deref(), *limit).1 {
                    if want.len() >= STATS_BATCH {
                        break;
                    }
                    if stats.get(&e.path).is_none()
                        && s.stats_asked.insert((e.path.clone(), e.modified))
                    {
                        want.push(e.path.clone());
                    }
                }
            }
        }
        if want.is_empty() {
            return;
        }
        self.devices.s4.stats_busy = true;
        let ctx = self.job_ctx();
        self.spawn_worker("device-stats", move || {
            // The query fills the shared statistics cache.
            let _ = crate::gateway::jobs::query(&ctx, &Query::Stats(want));
            Fin::Device(Box::new(DeviceFin::Stats))
        });
    }

    /// `kill_names` results in device terms (running ids, failures by id).
    pub(super) fn killed_view(
        &self,
        key: Option<&RequestKey>,
        failed: &[(String, String)],
    ) -> Vec<api::KillResult> {
        let epoch = self.epoch.to_hex();
        let named = key.and_then(|k| self.devices.s4.kills.get(k));
        let canon = &self.devices.canon;
        let error = |name: &str| {
            failed
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, e)| canon.scrub(e))
        };
        match named {
            Some(list) => list
                .iter()
                .map(|(id, name)| api::KillResult {
                    id: id.clone(),
                    error: error(name),
                })
                .collect(),
            None => failed
                .iter()
                .map(|(name, e)| api::KillResult {
                    id: views::running_id(name, &epoch),
                    error: Some(canon.scrub(e)),
                })
                .collect(),
        }
    }

    // ---- feeds --------------------------------------------------------------------------------

    /// A slice-4 feed's encoded value; `None`: its source is not ready (a worker result marks
    /// the feed again).
    pub(super) fn s4_feed_value(
        &self,
        device: &str,
        feed: &Feed,
        st: &FeedState,
    ) -> Option<Option<Vec<u8>>> {
        let canon = &self.devices.canon;
        Some(match feed {
            Feed::Running {} => {
                let detail = self.devices.s4.running.as_ref()?;
                let list = self.running_views(detail);
                let server_bytes = detail.tmux_server.as_ref().map(|m| m.bytes);
                fit_feed(feed, &list, |sessions, truncated| FeedValue::Running {
                    sessions,
                    server_bytes,
                    truncated,
                })
            }
            Feed::Inventory { project, limit } => {
                let entries = lock(&self.inventory_shared).clone();
                let (total, page) = inventory_page(&entries, project.as_deref(), *limit);
                let running = self.running.latest();
                let stats = lock(&self.stats);
                let list: Vec<api::InventoryView> = page
                    .iter()
                    .map(|e| {
                        views::inventory(
                            views::InventoryRow {
                                entry: e,
                                project: project_id(&e.cwd),
                                parent: e.parent.as_deref().and_then(|p| {
                                    entries.iter().find(|x| x.path == p).map(|x| x.id.clone())
                                }),
                                running: running
                                    .as_deref()
                                    .is_some_and(|r| r.holder(&e.path).is_some()),
                                stats: stats.get(&e.path),
                            },
                            canon,
                        )
                    })
                    .collect();
                fit_feed(feed, &list, |entries, truncated| FeedValue::Inventory {
                    project: project.clone(),
                    total,
                    entries,
                    truncated,
                })
            }
            Feed::Context { live } => {
                let context = self
                    .sess
                    .managed
                    .iter()
                    .find(|m| m.ctl.target().key.as_str() == live.key)
                    .and_then(|m| m.live.as_ref())
                    .filter(|l| l.client.connected())
                    .and_then(|l| l.client.latest())
                    .and_then(|s| s.context.as_ref().map(views::context));
                serde_json::to_vec(&api::Event::Feed {
                    feed: feed.clone(),
                    value: FeedValue::Context {
                        live: live.clone(),
                        context,
                    },
                })
                .ok()
            }
            Feed::Usage {} => {
                let v = self.sched.usage_view();
                let usage = v.snapshot.as_ref().map(views::usage);
                let reports = usage
                    .as_ref()
                    .map(|u| u.reports.clone())
                    .unwrap_or_default();
                let error = v.error.as_deref().map(|e| canon.scrub(e));
                fit_feed(feed, &reports, |reports, truncated| FeedValue::Usage {
                    usage: usage.as_ref().map(|u| api::UsageView {
                        reports,
                        tightest_remaining: u.tightest_remaining,
                        unavailable: u.unavailable.clone(),
                    }),
                    fetched_unix_ms: v.fetched_unix_ms,
                    error: error.clone(),
                    fetching: v.fetching,
                    truncated,
                })
            }
            Feed::Agents { session } => {
                let path = st.path.as_deref()?;
                let snap = self.fleet_latest()?;
                let f = snap.sessions.iter().find(|f| f.session == path)?;
                let live = self
                    .sess
                    .managed
                    .iter()
                    .filter(|m| {
                        m.ctl
                            .session_path()
                            .is_some_and(|p| super::same_spelling(p, path))
                    })
                    .find_map(|m| m.live.as_ref()?.client.latest());
                let list: Vec<api::AgentView> = views::agent_order(&f.agents)
                    .into_iter()
                    .map(|a| {
                        let l = live
                            .as_deref()
                            .and_then(|s| s.agents.iter().find(|x| x.id == a.id));
                        views::agent(a, l, canon)
                    })
                    .collect();
                fit_feed(feed, &list, |agents, truncated| FeedValue::Agents {
                    session: session.clone(),
                    agents,
                    truncated,
                })
            }
            Feed::Worktrees { project } => {
                let dir = st.path.as_ref()?;
                let Ok(TopicValue::Worktrees(v)) =
                    self.topic_value(&Topic::Worktrees(dir.clone()), true)
                else {
                    return None;
                };
                let repo = v.repo.as_ref()?;
                let groups = self.groups();
                let worktree = repo.as_ref().ok().map(|r| api::WorktreeView {
                    linked: v.worktree.is_some(),
                    branch: r.current_branch.clone(),
                    base: v.worktree.as_ref().and_then(|w| {
                        v.linked
                            .iter()
                            .find(|e| e.worktree.path == w.path)
                            .and_then(|e| e.base.clone())
                    }),
                    primary: groups
                        .iter()
                        .find(|g| super::same_spelling(&g.cwd, &r.primary))
                        .map(|g| project_id(&g.cwd)),
                    primary_display: canon.display(&r.primary),
                    branches: r.branches.clone(),
                    has_commits: r.has_commits,
                });
                let parked: Vec<api::ParkedView> = v
                    .parked
                    .iter()
                    .map(|p| api::ParkedView {
                        id: parked_id(&p.record.primary, &p.record.branch),
                        branch: p.record.branch.clone(),
                        base: p.record.base.clone(),
                        sessions: count(p.record.sessions.len()),
                        parked_unix_ms: p.record.parked_ms,
                        exists: p.state.as_ref().is_none_or(|s| s.exists),
                        merged: p.state.as_ref().is_some_and(|s| s.merged),
                        checked_out: p.state.as_ref().is_some_and(|s| s.checked_out.is_some()),
                    })
                    .collect();
                fit_feed(feed, &parked, |parked, truncated| FeedValue::Worktrees {
                    project: project.clone(),
                    worktree: worktree.clone(),
                    parked,
                    truncated,
                })
            }
            Feed::Backup {} => {
                let v = self.backup_view();
                serde_json::to_vec(&api::Event::Feed {
                    feed: feed.clone(),
                    value: FeedValue::Backup {
                        destination: views::backup_destination(&v, canon),
                        available: match v.destination {
                            Destination::Drive => v.drive_available,
                            Destination::Server => true,
                        },
                        connected: v.linked(),
                        account: match v.destination {
                            Destination::Drive => v.drive_email.clone(),
                            Destination::Server => None,
                        },
                        last_backup_unix_ms: v
                            .last_backup
                            .as_ref()
                            .and_then(|b| u64::try_from(b.unix.saturating_mul(1000)).ok()),
                        interval: views::sync_interval(self.cfg.drive_backup_interval),
                        busy: v.busy.as_deref().map(|b| canon.scrub(b)),
                        error: v.last_error.as_deref().map(|e| match v.destination {
                            Destination::Server => views::server_failure(e),
                            Destination::Drive => canon.scrub(e),
                        }),
                    },
                })
                .ok()
            }
            Feed::Sync {} => {
                let state = self.devices.s4.sync_state.as_ref();
                let rows: Vec<api::SyncRowView> = self
                    .cfg
                    .project_sync
                    .iter()
                    .map(|r| api::SyncRowView {
                        project: project_id(&r.path),
                        display: canon.display(&r.path),
                        parts: api::SyncParts {
                            omp: r.omp,
                            sessions: r.sessions,
                            memory: r.memory,
                        },
                        excluded: r.excluded_sessions.iter().cloned().collect(),
                        last_backup_unix_ms: state
                            .and_then(|s| s.projects.get(&r.path))
                            .and_then(|p| u64::try_from(p.last.unix.saturating_mul(1000)).ok()),
                    })
                    .collect();
                let busy = self.backup_view().busy.map(|b| canon.scrub(&b));
                let interval = views::sync_interval(self.cfg.project_sync_interval);
                fit_feed(feed, &rows, |rows, truncated| FeedValue::Sync {
                    rows,
                    interval,
                    busy: busy.clone(),
                    truncated,
                })
            }
            Feed::Updates {} => serde_json::to_vec(&api::Event::Feed {
                feed: feed.clone(),
                value: FeedValue::Updates {
                    omp: views::omp_update(&self.sched.omp_view(), canon),
                    app: views::app_update(&self.sched.app_view(), canon),
                },
            })
            .ok(),
            Feed::Device {} => {
                let d = self
                    .mobile
                    .registry()?
                    .devices()
                    .iter()
                    .find(|d| d.id == device)?;
                serde_json::to_vec(&api::Event::Feed {
                    feed: feed.clone(),
                    value: FeedValue::Device {
                        name: d.name.clone(),
                        permissions: super::api_permissions(d.permissions),
                        push: push::state_view(d),
                        gateway_version: crate::gateway::GATEWAY_VERSION.to_owned(),
                        mode: views::gateway_mode(self.cfg.gateway_mode),
                    },
                })
                .ok()
            }
            // v1 feeds are projected by `refresh_feed`.
            _ => None,
        })
    }

    fn running_views(&self, detail: &RunningDetail) -> Vec<api::RunningView> {
        let groups = self.groups();
        let canon = &self.devices.canon;
        let epoch = self.epoch.to_hex();
        detail
            .sessions
            .iter()
            .map(|r| {
                let group = r
                    .cwd
                    .as_deref()
                    .and_then(|c| groups.iter().find(|g| super::same_spelling(&g.cwd, c)));
                api::RunningView {
                    id: views::running_id(&r.name, &epoch),
                    sessions: r
                        .files
                        .iter()
                        .filter_map(|f| session_in(&groups, canon, f).map(|(_, s)| s.id.clone()))
                        .collect(),
                    project: group.map(|g| project_id(&g.cwd)),
                    display: r.cwd.as_deref().map(|c| canon.display(c)),
                    title: r
                        .title
                        .as_deref()
                        .map(|t| super::cut_chars(t, api::TREE_TEXT_CHARS)),
                    backend: match r.backend {
                        deck_core::mux::MuxKind::Tmux => api::MuxBackend::Tmux,
                        deck_core::mux::MuxKind::Host => api::MuxBackend::Host,
                    },
                    started_unix_ms: r.started.map(unix_ms),
                    pid: r.pid,
                    memory_bytes: r.memory.as_ref().map(|m| m.bytes),
                    processes: r.memory.as_ref().map(|m| m.processes),
                    live: self
                        .sess
                        .managed
                        .iter()
                        .find(|m| m.mux_name.as_deref() == Some(r.name.as_str()))
                        .map(|m| live_ref(m.ctl.target())),
                }
            })
            .collect()
    }

    /// The topics a slice-4 feed follows (besides the always-published ones).
    pub(super) fn s4_interest(
        &self,
        feed: &Feed,
        st: &FeedState,
        subs: &mut BTreeSet<Topic>,
        fleet: &mut Vec<deck_core::fleet::FleetTarget>,
    ) {
        match feed {
            Feed::Inventory { .. } => {
                subs.insert(Topic::Inventory);
            }
            Feed::Context { live } => {
                if let Some(m) = self
                    .sess
                    .managed
                    .iter()
                    .find(|m| m.ctl.target().key.as_str() == live.key)
                {
                    subs.insert(Topic::Live(m.ctl.target().key.clone()));
                }
            }
            Feed::Agents { .. } => {
                subs.insert(Topic::Fleet);
                let Some(path) = &st.path else { return };
                if fleet.iter().any(|t| t.session == *path) {
                    return;
                }
                let groups = self.groups();
                let Some((g, _)) = session_in(&groups, &self.devices.canon, path) else {
                    return;
                };
                fleet.push(deck_core::fleet::FleetTarget {
                    session: path.clone(),
                    cwd: g.cwd.clone(),
                    owner_alive: self
                        .running
                        .latest()
                        .as_deref()
                        .is_some_and(|r| r.holder(path).is_some()),
                });
            }
            Feed::Worktrees { .. } => {
                if let Some(p) = &st.path {
                    subs.insert(Topic::Worktrees(p.clone()));
                }
            }
            _ => {}
        }
    }

    /// The folder a slice-4 subscription follows (checked against current state).
    pub(super) fn s4_feed_path(
        &self,
        groups: &[ProjectGroup],
        feed: &Feed,
    ) -> Result<Option<PathBuf>, ApiError> {
        Ok(match feed {
            Feed::Inventory {
                project: Some(p), ..
            } => {
                resolve_project(groups, p)?;
                None
            }
            Feed::Context { live } => {
                self.live_target(live)?;
                None
            }
            Feed::Agents { session } => Some(resolve_session(groups, session)?.path.clone()),
            Feed::Worktrees { project } => Some(resolve_project(groups, project)?.to_path_buf()),
            _ => None,
        })
    }

    // ---- journaled operations -----------------------------------------------------------------

    /// The gateway mutation a journaled slice-4 operation becomes.
    pub(super) fn s4_mutation(
        &mut self,
        conn: u64,
        key: &RequestKey,
        op: Op,
    ) -> Result<Mutation, ApiError> {
        let groups = self.groups();
        let backend = !self.backends.is_empty();
        if matches!(
            op,
            Op::BackupRun {}
                | Op::BackupDelete { .. }
                | Op::BackupRestorePrepare { .. }
                | Op::BackupRestoreApply { .. }
                | Op::BackupRestoreDiscard { .. }
                | Op::SyncBackup { .. }
                | Op::SyncDelete { .. }
                | Op::SyncRestorePrepare { .. }
                | Op::SyncRestoreApply { .. }
        ) && self.cfg.backup_destination == Destination::Server
        {
            let keys = &mut self.devices.s4.server_keys;
            keys.insert(key.clone());
            while keys.len() > SERVER_KEYS {
                keys.pop_first();
            }
        }
        Ok(match op {
            Op::Compose {
                live,
                text,
                attachments,
                mode,
            } => {
                let d = self
                    .device_mut(conn)
                    .ok_or_else(|| bad("the session ended"))?;
                let attachments = d.transfers.attachments(&attachments, Instant::now())?;
                Mutation::Submit {
                    target: self.live_target(&live)?,
                    command: Command::Compose {
                        attachments,
                        text,
                        mode: match mode {
                            api::ComposeMode::Send => crate::inject::ComposeMode::Send,
                            api::ComposeMode::Steer => crate::inject::ComposeMode::Steer,
                            api::ComposeMode::FollowUp => crate::inject::ComposeMode::FollowUp,
                            api::ComposeMode::Insert => crate::inject::ComposeMode::Insert,
                        },
                    },
                }
            }
            Op::Review {
                live,
                comments,
                general,
                send,
            } => {
                let target = self.live_target(&live)?;
                let project = self
                    .live_sessions(&key.client)
                    .into_iter()
                    .find(|s| s.live.key == live.key)
                    .and_then(|s| s.project)
                    .ok_or_else(|| not_found("live session"))?;
                let (root, comments) = if comments.is_empty() {
                    if general.as_deref().is_none_or(|g| g.trim().is_empty()) {
                        return Err(bad("nothing to review"));
                    }
                    let cwd = resolve_project(&groups, &project)?;
                    (cwd.to_path_buf(), Vec::new())
                } else {
                    let files: Vec<String> = comments.iter().map(|c| c.file.clone()).collect();
                    let (root, found) = self.changed_files(&project, &files)?;
                    let items = comments
                        .into_iter()
                        .zip(found)
                        .map(|(c, file)| protocol::ReviewItem {
                            file,
                            side: match c.side {
                                api::DiffSide::Staged => deck_core::changes::DiffSide::Staged,
                                api::DiffSide::Unstaged => deck_core::changes::DiffSide::Unstaged,
                                api::DiffSide::Untracked => deck_core::changes::DiffSide::Untracked,
                            },
                            start: c.start,
                            end: c.end,
                            text: c.text,
                            line_side: None,
                        })
                        .collect();
                    (root, items)
                };
                Mutation::Review {
                    target,
                    root,
                    comments,
                    general,
                    send,
                }
            }
            Op::AnswerUi {
                live,
                request,
                value,
            } => Mutation::AnswerUi {
                target: self.live_target(&live)?,
                request: views::ui_request_id(&request).ok_or_else(|| bad("not a UI request"))?,
                answer: views::ui_answer(value),
            },
            Op::AgentKill { live, agent } => {
                let (target, root) = self.ctl_root(&live, &agent)?;
                Mutation::Ctl {
                    target,
                    command: deck_core::ctl::CtlCommand::Kill { id: agent, root },
                }
            }
            Op::AgentRevive { live, agent } => {
                let (target, root) = self.ctl_root(&live, &agent)?;
                Mutation::Ctl {
                    target,
                    command: deck_core::ctl::CtlCommand::Revive { id: agent, root },
                }
            }
            Op::AgentChat { live, agent, text } => {
                if text.trim().is_empty() {
                    return Err(bad("an empty message"));
                }
                let (target, root) = self.ctl_root(&live, &agent)?;
                Mutation::Ctl {
                    target,
                    command: deck_core::ctl::CtlCommand::Chat {
                        id: agent,
                        text,
                        root,
                    },
                }
            }
            Op::ForkAt { session, entry } => Mutation::ForkAt {
                file: resolve_session(&groups, &session)?.path.clone(),
                entry_id: entry,
            },
            Op::Navigate { live, entry } => {
                let target = self.live_target(&live)?;
                let root = self.managed_root(&target)?;
                Mutation::Ctl {
                    target,
                    command: deck_core::ctl::CtlCommand::Navigate {
                        root,
                        entry_id: entry,
                        summary: deck_core::ctl::BranchSummary::None,
                    },
                }
            }
            Op::NewSessionIn { dir } => {
                let cwd = self.dir_of(conn, &dir)?;
                Mutation::Launch(LaunchSpec {
                    mux_name: backend.then(|| deck_core::mux::new_session_name(SystemTime::now())),
                    cwd,
                    session_path: None,
                    label: String::new(),
                    argv: crate::gateway::engine::sessions::omp_argv(None),
                    role: SessionRole::Session,
                    copy_of: None,
                    view: self.cfg.session_view,
                })
            }
            Op::RunningKill { ids } => {
                if ids.is_empty() {
                    return Err(bad("no running sessions"));
                }
                let epoch = self.epoch.to_hex();
                let mut names: Vec<String> = self
                    .running
                    .latest()
                    .map(|r| r.alive.iter().cloned().collect())
                    .unwrap_or_default();
                if let Some(d) = &self.devices.s4.running {
                    names.extend(d.sessions.iter().map(|s| s.name.clone()));
                }
                let named = ids
                    .iter()
                    .map(|id| {
                        names
                            .iter()
                            .find(|n| views::running_id(n, &epoch) == *id)
                            .map(|n| (id.clone(), n.clone()))
                            .ok_or_else(|| not_found("running session"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let s = &mut self.devices.s4;
                if s.kills.len() >= KILLS
                    && let Some(oldest) = s.kills.keys().next().cloned()
                {
                    s.kills.remove(&oldest);
                }
                let names = named.iter().map(|(_, n)| n.clone()).collect();
                s.kills.insert(key.clone(), named);
                Mutation::KillNames { names, wait: true }
            }
            Op::WorktreeCreate {
                project,
                branch,
                base,
                open,
            } => worktree_create(resolve_project(&groups, &project)?, branch, base, open),
            Op::WorktreeMove { live, branch } => worktree_move(self.live_target(&live)?, branch),
            Op::WorktreeFinish { project } => worktree_finish(project_worktree(&groups, &project)?),
            Op::WorktreeRemove { project, park } => {
                worktree_remove(project_worktree(&groups, &project)?, park)
            }
            Op::WorktreeContinue { parked, r#where } => {
                let record = self
                    .cfg
                    .parked_branches
                    .iter()
                    .find(|p| parked_id(&p.primary, &p.branch) == parked)
                    .ok_or_else(|| not_found("parked branch"))?;
                worktree_continue(record, r#where)
            }
            Op::BackupRun {} => Mutation::Backup { force: true },
            Op::SyncBackup { project } => {
                let row = self
                    .cfg
                    .project_sync
                    .iter()
                    .find(|r| project_id(&r.path) == project)
                    .ok_or_else(|| not_found("project sync"))?;
                Mutation::ProjectBackup {
                    project: row.path.clone(),
                    parts: deck_core::backup::Parts {
                        omp: row.omp,
                        sessions: row.sessions,
                        memory: row.memory,
                    },
                    excluded: row.excluded_sessions.clone(),
                    force: true,
                }
            }
            Op::OpenOnDesktop { session, split } => Mutation::OpenOnDesktop {
                file: resolve_session(&groups, &session)?.path.clone(),
                split: match split {
                    api::Split::None => protocol::Split::None,
                    api::Split::Right => protocol::Split::Right,
                    api::Split::Down => protocol::Split::Down,
                },
            },
            Op::CloseOnDesktop { session } => Mutation::CloseOnDesktop {
                file: resolve_session(&groups, &session)?.path.clone(),
            },
            Op::OmpSet { key, value } => Mutation::OmpSet { key, value },
            Op::OmpReset { key } => Mutation::OmpReset { key },
            Op::UpdateRun { kind } => Mutation::RunTool(protocol::Tool::Update(match kind {
                api::UpdateKind::Omp => deck_core::omp::UpdateKind::Omp,
                api::UpdateKind::Plugins => deck_core::omp::UpdateKind::Plugins,
                api::UpdateKind::Stable => deck_core::omp::UpdateKind::Stable,
                api::UpdateKind::Canary => deck_core::omp::UpdateKind::Canary,
            })),
            Op::AppUpdate { step } => match step {
                api::AppUpdateStep::Download => Mutation::AppUpdateDownload,
                api::AppUpdateStep::Apply => {
                    if !views::apply_allowed(&self.sched.app_view().install) {
                        return Err(bad("install this update at the desktop"));
                    }
                    Mutation::AppUpdateApply
                }
            },
            Op::BackupConnect {} => Mutation::BackupConnectOnDesktop,
            Op::BackupDelete { ids } => Mutation::DeleteBackups(
                bulk(&ids)?
                    .iter()
                    .map(|id| self.backup_file(id))
                    .collect::<Result<_, _>>()?,
            ),
            Op::BackupRestorePrepare { id } => Mutation::PrepareRestore(self.backup_file(&id)?),
            Op::BackupRestoreApply { handle } => Mutation::ApplyRestore(handle),
            Op::BackupRestoreDiscard { handle } => Mutation::DiscardRestore(handle),
            Op::SyncAdd { dir } => {
                let path = self.dir_of(conn, &dir)?;
                self.sync_edit(|rows| {
                    if rows.iter().any(|r| super::same_spelling(&r.path, &path)) {
                        return Err(bad("the folder is synced already"));
                    }
                    // As on the desktop: a new row backs up nothing until parts are chosen.
                    rows.push(deck_core::config::SyncRow {
                        path: path.clone(),
                        omp: false,
                        sessions: false,
                        memory: false,
                        excluded_sessions: BTreeSet::new(),
                    });
                    Ok(())
                })?
            }
            Op::SyncSet {
                project,
                parts,
                excluded,
            } => self.sync_edit(|rows| {
                let row = rows
                    .iter_mut()
                    .find(|r| project_id(&r.path) == project)
                    .ok_or_else(|| not_found("project sync"))?;
                row.omp = parts.omp;
                row.sessions = parts.sessions;
                row.memory = parts.memory;
                row.excluded_sessions = excluded.into_iter().collect();
                Ok(())
            })?,
            Op::SyncRemove { project } => self.sync_edit(|rows| {
                let before = rows.len();
                rows.retain(|r| project_id(&r.path) != project);
                if rows.len() == before {
                    return Err(not_found("project sync"));
                }
                Ok(())
            })?,
            Op::SyncDelete { ids } => Mutation::ProjectDeleteBackups(
                bulk(&ids)?
                    .iter()
                    .map(|id| self.sync_backup(id))
                    .collect::<Result<_, _>>()?,
            ),
            Op::SyncRestorePrepare {
                backup,
                target,
                parts,
            } => {
                let backup = self.sync_backup(&backup)?;
                let target = match target {
                    api::RestoreTarget::Project { project } => {
                        resolve_project(&groups, &project)?.to_path_buf()
                    }
                    api::RestoreTarget::Dir { dir } => self.dir_of(conn, &dir)?,
                };
                Mutation::ProjectPrepareRestore {
                    backup,
                    target,
                    parts: sync_parts(&parts),
                }
            }
            Op::SyncRestoreApply {
                handle,
                conflicts,
                excluded,
            } => Mutation::ProjectApplyRestore {
                handle,
                conflicts: conflicts
                    .into_iter()
                    .map(|c| {
                        (
                            c.session,
                            match c.choice {
                                api::ConflictResolution::Keep => {
                                    deck_core::backup::ConflictChoice::KeepLocal
                                }
                                api::ConflictResolution::Overwrite => {
                                    deck_core::backup::ConflictChoice::Overwrite
                                }
                                api::ConflictResolution::Both => {
                                    deck_core::backup::ConflictChoice::KeepBoth
                                }
                            },
                        )
                    })
                    .collect(),
                excluded: excluded.into_iter().collect(),
                stop: Vec::new(),
            },
            _ => return Err(bad("not a journaled operation")),
        })
    }

    /// The folder of a dir token (handed to this device session), if it leads — symlinks
    /// resolved — at or below a browse root: the resolved folder, the one every use takes.
    fn dir_of(&self, conn: u64, dir: &str) -> Result<PathBuf, ApiError> {
        self.folder_of(conn, dir).map(|f| f.resolved)
    }

    /// [`dir_of`](Self::dir_of) with the spelling the token was handed out under (the folder
    /// browser shows and hands out paths below it).
    fn folder_of(&self, conn: u64, dir: &str) -> Result<Folder, ApiError> {
        let shown = self
            .device_conn(conn)
            .and_then(|d| d.dirs.resolve(dir))
            .map(Path::to_path_buf)
            .ok_or_else(|| not_found("folder"))?;
        let resolved = self
            .browse_confine()
            .admit(&shown)
            .map_err(|_| not_found("folder"))?;
        Ok(Folder { shown, resolved })
    }

    /// The folders a device may act in: the browse roots and every project folder.
    fn browse_confine(&self) -> Confine {
        let mut roots = self.browse_root_paths().paths;
        roots.extend(self.groups().iter().map(|g| g.cwd.clone()));
        Confine::new(roots)
    }

    /// [`browse_confine`](Self::browse_confine) for a paired device's request (`client`), to
    /// check its folder again on the worker that uses it; `None` for any other client.
    pub(in crate::gateway::engine) fn confine_for(&self, client: &str) -> Option<Confine> {
        crate::gateway::engine::remote::is_device_id(client).then(|| self.browse_confine())
    }

    /// Where backups go now.
    fn backup_place(&self) -> BackupPlace {
        BackupPlace::of(&self.backup_view())
    }

    /// `key` asked for work at a backup server (its failure is shown in device terms).
    pub(super) fn is_server_request(&self, key: &RequestKey) -> bool {
        self.devices.s4.server_keys.contains(key)
    }

    /// A config backup of the last listing, while the place it came from is still where
    /// backups go.
    fn backup_file(&self, id: &str) -> Result<BackupFile, ApiError> {
        let place = self.backup_place();
        cached(&self.devices.s4.backup_list, &place)
            .iter()
            .find(|f| f.id == id)
            .cloned()
            .ok_or_else(|| not_found("backup"))
    }

    fn sync_backup(&self, id: &str) -> Result<ProjectBackup, ApiError> {
        let place = self.backup_place();
        cached(&self.devices.s4.sync_list, &place)
            .iter()
            .find(|b| b.file.id == id)
            .cloned()
            .ok_or_else(|| not_found("backup"))
    }

    /// A settings edit of the project sync rows.
    fn sync_edit(
        &self,
        edit: impl FnOnce(&mut Vec<deck_core::config::SyncRow>) -> Result<(), ApiError>,
    ) -> Result<Mutation, ApiError> {
        let snap = self.settings.snapshot();
        let mut c = (*snap.config).clone();
        edit(&mut c.project_sync)?;
        Ok(Mutation::Settings(crate::settings::Edit::between(
            snap.config.clone(),
            &c,
        )))
    }

    /// The managed session's file (the root its ctl commands name).
    fn managed_root(&self, target: &SessionTarget) -> Result<PathBuf, ApiError> {
        self.sess
            .managed
            .iter()
            .find(|m| m.ctl.target().key == target.key)
            .and_then(|m| m.ctl.session_path())
            .map(Path::to_path_buf)
            .ok_or_else(|| bad("the session has no file yet"))
    }

    fn ctl_root(
        &self,
        live: &api::LiveRef,
        agent: &str,
    ) -> Result<(SessionTarget, PathBuf), ApiError> {
        if !valid_agent(agent) {
            return Err(bad("not a subagent id"));
        }
        let target = self.live_target(live)?;
        let root = self.managed_root(&target)?;
        Ok((target, root))
    }
}

/// The ids of a bulk delete: one to [`protocol::MAX_BULK_DELETE`], each named once.
fn bulk(ids: &[String]) -> Result<&[String], ApiError> {
    if ids.is_empty() {
        return Err(bad("no backups"));
    }
    if ids.len() > protocol::MAX_BULK_DELETE {
        return Err(bad("at most 100 backups at a time"));
    }
    if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
        return Err(bad("a backup is named twice"));
    }
    Ok(ids)
}

/// The linked worktree project `project` is.
fn project_worktree(
    groups: &[ProjectGroup],
    project: &str,
) -> Result<deck_core::worktree::Worktree, ApiError> {
    let cwd = resolve_project(groups, project)?;
    groups
        .iter()
        .find(|g| g.cwd == cwd)
        .and_then(|g| g.worktree.clone())
        .or_else(|| deck_core::worktree::resolve(cwd))
        .ok_or_else(|| bad("the project is not a linked worktree"))
}

fn worktree_create(repo: &Path, branch: String, base: Option<String>, open: bool) -> Mutation {
    Mutation::WorktreeNew {
        repo: repo.to_path_buf(),
        branch,
        base_ref: base,
        dest: None,
        open,
    }
}

fn worktree_move(target: SessionTarget, branch: String) -> Mutation {
    Mutation::WorktreeMoveSession { target, branch }
}

fn worktree_finish(worktree: deck_core::worktree::Worktree) -> Mutation {
    Mutation::WorktreeFinish { worktree }
}

fn worktree_remove(worktree: deck_core::worktree::Worktree, park: bool) -> Mutation {
    Mutation::WorktreeRetire { worktree, park }
}

fn worktree_continue(
    record: &deck_core::config::ParkedBranch,
    place: api::ContinueWhere,
) -> Mutation {
    Mutation::WorktreeContinue {
        primary: record.primary.clone(),
        branch: record.branch.clone(),
        session: None,
        place: match place {
            api::ContinueWhere::Worktree => protocol::ContinueIn::Worktree,
            api::ContinueWhere::Main => protocol::ContinueIn::Main,
            api::ContinueWhere::SwitchBack => protocol::ContinueIn::SwitchBack,
        },
    }
}

/// The inventory entries a feed lists: of `project` (all when `None`), newest first, at most
/// `limit`; and how many there are.
fn inventory_page<'a>(
    entries: &'a [deck_core::inventory::InventoryEntry],
    project: Option<&str>,
    limit: u32,
) -> (u32, Vec<&'a deck_core::inventory::InventoryEntry>) {
    let mut list: Vec<&deck_core::inventory::InventoryEntry> = entries
        .iter()
        .filter(|e| project.is_none_or(|p| project_id(&e.cwd) == p))
        .collect();
    let total = count(list.len());
    list.sort_by_key(|e| std::cmp::Reverse(e.modified));
    list.truncate(limit as usize);
    (total, list)
}

/// Translations of a done result only `s4` devices understand.
pub(super) fn s4_done(this: &Engine, d: &Done, key: Option<&RequestKey>) -> Option<api::DoneView> {
    let canon = &this.devices.canon;
    Some(match d {
        Done::Killed(failed) => api::DoneView::Killed {
            results: this.killed_view(key, failed),
        },
        Done::Prepared { handle, plan } => {
            let (plan, truncated) = views::backup_plan(plan, canon);
            api::DoneView::RestorePlan {
                handle: *handle,
                plan,
                truncated,
            }
        }
        Done::ProjectPrepared { handle, plan } => {
            let groups = this.groups();
            let (plan, truncated) = views::project_plan(plan, canon, |cwd| {
                groups
                    .iter()
                    .find(|g| super::same_spelling(&g.cwd, cwd))
                    .map_or_else(|| project_id(cwd), |g| project_id(&g.cwd))
            });
            api::DoneView::RestorePlan {
                handle: *handle,
                plan,
                truncated,
            }
        }
        _ => return None,
    })
}

/// The listing `cache` holds while it was read at `place` (nothing once backups go elsewhere).
fn cached<'a, T>(cache: &'a Option<(BackupPlace, Vec<T>)>, place: &BackupPlace) -> &'a [T] {
    match cache {
        Some((listed, list)) if listed.same(place) => list,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_operations_and_feeds_are_not_gated() {
        assert!(!s4_op(&Op::Ping {}));
        assert!(!s4_op(&Op::InboxClear {}));
        assert!(s4_op(&Op::UpdateCheck {}));
        assert!(s4_op(&Op::Unpair {}));
        assert!(!s4_feed(&Feed::Live {}));
        assert!(s4_feed(&Feed::Running {}));
        assert!(s4_feed(&Feed::Device {}));
    }

    fn rel(p: &Path, base: &Path) -> Option<PathBuf> {
        p.strip_prefix(base).ok().map(Path::to_path_buf)
    }

    #[test]
    fn browsing_is_clamped_to_the_roots_and_never_shows_absolute_paths() {
        let roots = BrowseRoots {
            paths: vec![PathBuf::from("/home/u"), PathBuf::from("/srv/work")],
            home: true,
        };
        // Inside a root: yes. Above or beside: no (so no `parent` token past a root).
        assert_eq!(roots.containing(Path::new("/home/u"), rel), Some(0));
        assert_eq!(roots.containing(Path::new("/home/u/a/b"), rel), Some(0));
        assert_eq!(roots.containing(Path::new("/srv/work/p"), rel), Some(1));
        for outside in [
            "/",
            "/home",
            "/srv",
            "/etc",
            "/home/other",
            "/home/ux",
            "/srv/other",
        ] {
            assert_eq!(roots.containing(Path::new(outside), rel), None, "{outside}");
        }
        // The parent of a root is outside every root; the parent of a child is not.
        let parent_ok = |d: &str| {
            Path::new(d)
                .parent()
                .is_some_and(|p| roots.containing(p, rel).is_some())
        };
        assert!(!parent_ok("/home/u") && !parent_ok("/srv/work"));
        assert!(parent_ok("/home/u/a") && parent_ok("/srv/work/p"));
        // Labels: `~` for home, the folder name elsewhere, never an absolute path.
        let labels = roots.labels(|_| "~".to_owned());
        assert_eq!(labels, ["~", "work"]);
        assert_eq!(join_label(&labels[1], Path::new("p/q")), "work/p/q");
        assert_eq!(join_label(&labels[1], Path::new("")), "work");
        let twins = BrowseRoots {
            paths: vec![PathBuf::from("/a/work"), PathBuf::from("/b/work")],
            home: false,
        };
        assert_eq!(twins.labels(|_| String::new()), ["work", "work (2)"]);
    }

    #[test]
    fn parked_ids_are_opaque() {
        let id = parked_id(Path::new("/home/u/repo"), "feature");
        assert!(id.starts_with("p-") && id.len() == 18);
        assert_ne!(id, parked_id(Path::new("/home/u/repo"), "other"));
        assert!(!id.contains("home"));
    }

    #[test]
    fn deletes_take_one_to_a_hundred_distinct_backups() {
        let ids = |n: usize| (0..n).map(|i| format!("id{i}")).collect::<Vec<_>>();
        assert_eq!(bulk(&ids(1)).map(<[_]>::len).ok(), Some(1));
        assert_eq!(bulk(&ids(100)).map(<[_]>::len).ok(), Some(100));
        assert!(bulk(&[]).is_err());
        assert!(bulk(&ids(101)).is_err());
        assert!(bulk(&["a".to_owned(), "a".to_owned()]).is_err());
    }

    #[test]
    fn inventory_pages_are_newest_first_and_counted() {
        let at = |s: u64| std::time::UNIX_EPOCH + Duration::from_secs(s);
        let mk = |id: &str, cwd: &str, m: u64| deck_core::inventory::InventoryEntry {
            id: id.to_owned(),
            path: PathBuf::from(format!("{cwd}/{id}.jsonl")),
            cwd: PathBuf::from(cwd),
            title: None,
            first_prompt: None,
            created: None,
            modified: at(m),
            jsonl_bytes: 0,
            artifact_bytes: 0,
            artifact_files: 0,
            subagents: 0,
            parent: None,
            cwd_exists: true,
        };
        let list = vec![mk("a", "/p", 1), mk("b", "/p", 3), mk("c", "/q", 2)];
        let (total, page) = inventory_page(&list, None, 2);
        assert_eq!(total, 3);
        let ids: Vec<&str> = page.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["b", "c"]);
        let p = project_id(Path::new("/q"));
        let (total, page) = inventory_page(&list, Some(&p), 10);
        assert_eq!((total, page.len()), (1, 1));
    }

    fn server_account(
        protocol: deck_core::backup::server::Protocol,
        host: &str,
        user: &str,
        folder: &str,
    ) -> deck_core::backup::server::ServerAccount {
        use deck_core::backup::server::{ServerAccount, ServerAuth, ServerConfig};
        ServerAccount {
            config: ServerConfig {
                protocol,
                host: host.to_owned(),
                port: 0,
                auth: ServerAuth::Password {
                    user: user.to_owned(),
                    password: "pw".to_owned(),
                },
                folder: folder.to_owned(),
                pinned: None,
            },
            last_backup: None,
            synced_fingerprint: None,
        }
    }

    fn place_of(
        destination: Destination,
        email: Option<&str>,
        server: Option<&deck_core::backup::server::ServerAccount>,
    ) -> BackupPlace {
        BackupPlace::of(&protocol::BackupView {
            destination,
            drive_available: true,
            drive_signed_in: email.is_some(),
            drive_email: email.map(str::to_owned),
            server: server.map(protocol::ServerView::of),
            last_backup: None,
            busy: None,
            last_error: None,
            last_auto_unix_ms: None,
        })
    }

    #[test]
    fn cached_backup_ids_follow_the_concrete_place() {
        use deck_core::backup::server::Protocol;
        let a = server_account(Protocol::Sftp, "nas.lan", "me", "backups");
        let place = |s: &deck_core::backup::server::ServerAccount| {
            place_of(Destination::Server, None, Some(s))
        };
        let listed = Some((place(&a), vec![1, 2, 3]));
        // The same place: whatever does not define it (the port spelled out, a host in other
        // case, the password, the pinned key) keeps the listing.
        let mut same = server_account(Protocol::Sftp, "NAS.lan", "me", "backups");
        same.config.port = 22;
        same.config.pinned = Some("SHA256:x".to_owned());
        assert_eq!(cached(&listed, &place(&same)), [1, 2, 3]);
        // Another server, login, folder or protocol: another place.
        for other in [
            server_account(Protocol::Sftp, "nas2.lan", "me", "backups"),
            server_account(Protocol::Sftp, "nas.lan", "you", "backups"),
            server_account(Protocol::Sftp, "nas.lan", "me", "elsewhere"),
            server_account(Protocol::Ftps, "nas.lan", "me", "backups"),
        ] {
            assert!(cached(&listed, &place(&other)).is_empty(), "{other:?}");
        }
        // The same server's settings with another port.
        let mut port = a.clone();
        port.config.port = 2222;
        assert!(cached(&listed, &place(&port)).is_empty());
        // No server saved any more, or Drive instead.
        assert!(cached(&listed, &place_of(Destination::Server, None, None)).is_empty());
        assert!(
            cached(
                &listed,
                &place_of(Destination::Drive, Some("me@x"), Some(&a))
            )
            .is_empty()
        );
        // A Google listing follows the signed-in account.
        let drive = Some((place_of(Destination::Drive, Some("me@x"), None), vec![7]));
        assert_eq!(
            cached(
                &drive,
                &place_of(Destination::Drive, Some("me@x"), Some(&a))
            ),
            [7]
        );
        assert!(cached(&drive, &place_of(Destination::Drive, Some("you@x"), None)).is_empty());
        assert!(cached(&drive, &place_of(Destination::Drive, None, None)).is_empty());
        assert!(cached::<i32>(&None, &place(&a)).is_empty());
    }

    #[test]
    fn the_places_view_agrees_with_the_saved_configurations_rule() {
        use deck_core::backup::server::Protocol;
        let base = server_account(Protocol::Sftp, "nas.lan", "me", "backups");
        let mut cases = vec![
            server_account(Protocol::Sftp, "NAS.LAN", "me", "backups"),
            server_account(Protocol::Ftp, "nas.lan", "me", "backups"),
            server_account(Protocol::Sftp, "nas.lan", "you", "backups"),
            server_account(Protocol::Sftp, "nas.lan", "me", "other"),
            server_account(Protocol::Sftp, "nas.lan", "me", "backups"),
        ];
        let mut port = base.clone();
        port.config.port = 22;
        cases.push(port);
        let mut port = base.clone();
        port.config.port = 2022;
        cases.push(port);
        for c in &cases {
            assert_eq!(
                protocol::ServerView::of(&base).same_place(&protocol::ServerView::of(c)),
                base.config.same_place(&c.config),
                "{c:?}"
            );
        }
    }
}
