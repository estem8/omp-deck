//! The one writer of OmpDeck's settings file (`state.json`).
//!
//! Clients never write the file. They read [`Snapshot`]s and submit field-level [`Edit`]s:
//! the fields they changed plus the snapshot they changed them from. A worker thread applies an
//! edit under an exclusive lock on a sibling `state.json.lock` file, against the file's current
//! content (so separate OmpDeck processes — one per folder window — are serialized too):
//!
//! - a field whose stored value still equals the edit's baseline (or already equals the new
//!   value) takes the new value;
//! - a field someone else changed since the baseline is a conflict: the whole edit is refused
//!   with [`SettingsError::Conflict`] — never a silent last-writer-wins;
//! - fields the edit does not touch keep whatever is stored, so disjoint edits from different
//!   clients or processes all survive.
//!
//! The merged config is written with the existing atomic `AppConfig::save_to` (still under the
//! lock, which also serializes its fixed `.tmp` path), so the file format stays the same. All
//! file I/O after [`SettingsService::open`] happens on the worker.
//!
//! Field ownership ([`Field::scope`]): [`Scope::Device`] fields describe this machine and its
//! windows (geometry, open tabs, folders, paths); [`Scope::Shared`] fields are user preferences
//! a later synced profile may carry across devices. Both live in the same file today.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use deck_core::backup::Destination;
use deck_core::config::{
    AppConfig, Density, FolderTabs, GatewayMode, NotifyConfig, ParkedBranch, RemovedWorktree,
    SessionView, SplitState, SyncInterval, SyncRow, TabState, Theme, UiFont, UiStyle,
    WindowGeometry, clamp_ui_scale, config_dir, parse_config,
};
use deck_core::mux::BackendChoice;
use serde::{Deserialize, Serialize};

/// Who a field belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// This machine and its windows.
    Device,
    /// The user's preferences, the same on every device.
    Shared,
}

/// This window's tabs, the active one and the split layout: one value for the compare-and-swap,
/// because an index or split pane only means something together with the tab list it was
/// chosen in (two windows editing the parts separately must conflict, not mix).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabLayout {
    pub open_tabs: Vec<TabState>,
    pub active_tab: Option<usize>,
    pub split: Option<SplitState>,
}

impl TabLayout {
    pub fn of(cfg: &AppConfig) -> TabLayout {
        TabLayout {
            open_tabs: cfg.open_tabs.clone(),
            active_tab: cfg.active_tab,
            split: cfg.split.clone(),
        }
    }

    fn held_by(&self, cfg: &AppConfig) -> bool {
        self.open_tabs == cfg.open_tabs
            && self.active_tab == cfg.active_tab
            && self.split == cfg.split
    }

    fn apply(self, cfg: &mut AppConfig) {
        cfg.open_tabs = self.open_tabs;
        cfg.active_tab = self.active_tab;
        cfg.split = self.split;
    }
}

/// Writes one folder's remembered tabs into `cfg`: `Some` stores them, `None` forgets the folder.
fn put_folder(cfg: &mut AppConfig, key: &Path, value: Option<&FolderTabs>) {
    match value {
        Some(v) => {
            cfg.folder_tabs.insert(key.to_path_buf(), v.clone());
        }
        None => {
            cfg.folder_tabs.remove(key);
        }
    }
}

/// Every [`AppConfig`] field is one setting, except the three of [`TabLayout`] (one setting)
/// and `folder_tabs` (one setting per folder).
macro_rules! settings {
    ($($variant:ident($field:ident: $ty:ty) = $scope:ident,)*) => {
        /// A new value for one setting.
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
        pub enum Setting {
            $($variant($ty),)*
            TabLayout(TabLayout),
            /// One folder's remembered tabs (its tabs, active tab and split together); `None`
            /// forgets the folder.
            FolderTabs {
                key: PathBuf,
                value: Option<FolderTabs>,
            },
        }

        /// One setting: the unit of the compare-and-swap.
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum Field {
            $($variant,)*
            TabLayout,
            /// The remembered tabs of this folder.
            FolderTabs(PathBuf),
        }

        impl Setting {
            pub fn field(&self) -> Field {
                match self {
                    $(Setting::$variant(_) => Field::$variant,)*
                    Setting::TabLayout(_) => Field::TabLayout,
                    Setting::FolderTabs { key, .. } => Field::FolderTabs(key.clone()),
                }
            }

            fn apply(self, cfg: &mut AppConfig) {
                match self {
                    $(Setting::$variant(v) => cfg.$field = v,)*
                    Setting::TabLayout(v) => v.apply(cfg),
                    Setting::FolderTabs { key, value: Some(v) } => {
                        cfg.folder_tabs.insert(key, v);
                    }
                    Setting::FolderTabs { key, value: None } => {
                        cfg.folder_tabs.remove(&key);
                    }
                }
            }

            /// `cfg` already holds this value.
            fn held_by(&self, cfg: &AppConfig) -> bool {
                match self {
                    $(Setting::$variant(v) => cfg.$field == *v,)*
                    Setting::TabLayout(v) => v.held_by(cfg),
                    Setting::FolderTabs { key, value } => {
                        cfg.folder_tabs.get(key) == value.as_ref()
                    }
                }
            }

            /// `a` and `b` hold the same value of this setting (a folder key is compared
            /// borrowed, never cloned into a [`Field`]).
            fn same_in(&self, a: &AppConfig, b: &AppConfig) -> bool {
                match self {
                    Setting::FolderTabs { key, .. } => {
                        a.folder_tabs.get(key) == b.folder_tabs.get(key)
                    }
                    fixed => fixed.field().same(a, b),
                }
            }

            /// The settings `edited` changed relative to `base`, with their new values: one per
            /// added, changed or removed folder.
            pub fn diff(base: &AppConfig, edited: &AppConfig) -> Vec<Setting> {
                let mut out = Vec::new();
                $(
                    if base.$field != edited.$field {
                        out.push(Setting::$variant(edited.$field.clone()));
                    }
                )*
                if !Field::TabLayout.same(base, edited) {
                    out.push(Setting::TabLayout(TabLayout::of(edited)));
                }
                for (key, value) in &edited.folder_tabs {
                    if base.folder_tabs.get(key) != Some(value) {
                        out.push(Setting::FolderTabs {
                            key: key.clone(),
                            value: Some(value.clone()),
                        });
                    }
                }
                for key in base.folder_tabs.keys() {
                    if !edited.folder_tabs.contains_key(key) {
                        out.push(Setting::FolderTabs {
                            key: key.clone(),
                            value: None,
                        });
                    }
                }
                out
            }
        }

        impl Field {
            pub fn scope(&self) -> Scope {
                match self {
                    $(Field::$variant => Scope::$scope,)*
                    Field::TabLayout | Field::FolderTabs(_) => Scope::Device,
                }
            }

            fn same(&self, a: &AppConfig, b: &AppConfig) -> bool {
                match self {
                    $(Field::$variant => a.$field == b.$field,)*
                    Field::TabLayout => {
                        a.open_tabs == b.open_tabs
                            && a.active_tab == b.active_tab
                            && a.split == b.split
                    }
                    Field::FolderTabs(key) => a.folder_tabs.get(key) == b.folder_tabs.get(key),
                }
            }

            /// Copies this setting from `source` into `local`.
            fn copy(&self, local: &mut AppConfig, source: &AppConfig) {
                match self {
                    $(Field::$variant => local.$field = source.$field.clone(),)*
                    Field::TabLayout => TabLayout::of(source).apply(local),
                    Field::FolderTabs(key) => put_folder(local, key, source.folder_tabs.get(key)),
                }
            }
        }

        /// [`rebase`] for every setting but the per-folder ones.
        fn rebase_fixed(local: &mut AppConfig, old_base: &AppConfig, new_base: &AppConfig) {
            $(
                if local.$field == old_base.$field && local.$field != new_base.$field {
                    local.$field = new_base.$field.clone();
                }
            )*
            if Field::TabLayout.same(local, old_base) && !Field::TabLayout.same(local, new_base) {
                TabLayout::of(new_base).apply(local);
            }
        }
    };
}

/// Moves a client's working copy onto a newer snapshot: every setting the client has not
/// changed (`local` still equals `old_base`) takes `new_base`'s value — per folder for the
/// remembered folder tabs, additions and removals included; the client's own unsubmitted
/// changes stay.
pub fn rebase(local: &mut AppConfig, old_base: &AppConfig, new_base: &AppConfig) {
    rebase_fixed(local, old_base, new_base);
    let keys: Vec<PathBuf> = old_base
        .folder_tabs
        .keys()
        .chain(
            new_base
                .folder_tabs
                .keys()
                .filter(|k| !old_base.folder_tabs.contains_key(*k)),
        )
        .filter(|k| {
            let mine = local.folder_tabs.get(*k);
            mine == old_base.folder_tabs.get(*k) && mine != new_base.folder_tabs.get(*k)
        })
        .cloned()
        .collect();
    for key in keys {
        put_folder(local, &key, new_base.folder_tabs.get(&key));
    }
}

/// Copies exactly `fields` from `source` into `local` (after a [`SettingsError::Conflict`]:
/// take the other writer's value for the conflicted settings).
pub fn adopt(local: &mut AppConfig, source: &AppConfig, fields: &[Field]) {
    for f in fields {
        f.copy(local, source);
    }
}

settings! {
    Window(window: Option<WindowGeometry>) = Device,
    SidebarWidth(sidebar_width: f32) = Device,
    FontSize(font_size: f32) = Device,
    Theme(theme: Theme) = Shared,
    Style(style: UiStyle) = Shared,
    StyleRev(style_rev: u32) = Shared,
    ViewRev(view_rev: u32) = Shared,
    Density(density: Density) = Shared,
    UiFont(ui_font: UiFont) = Shared,
    UiScale(ui_scale: f32) = Shared,
    SeparatedPanels(separated_panels: bool) = Shared,
    NativeDecorations(native_decorations: bool) = Device,
    CheckUpdates(check_updates: bool) = Shared,
    FavoriteModels(favorite_models: Vec<String>) = Shared,
    ModelProviderFilter(model_provider_filter: Option<String>) = Shared,
    FolderScope(folder_scope: Option<PathBuf>) = Device,
    RecentFolders(recent_folders: Vec<PathBuf>) = Device,
    DriveBackupInterval(drive_backup_interval: SyncInterval) = Shared,
    BackupDestination(backup_destination: Destination) = Shared,
    ProjectSync(project_sync: Vec<SyncRow>) = Device,
    ProjectSyncInterval(project_sync_interval: SyncInterval) = Shared,
    SessionBackend(session_backend: BackendChoice) = Device,
    WorktreeBases(worktree_bases: BTreeMap<PathBuf, String>) = Device,
    RemovedWorktrees(removed_worktrees: BTreeMap<PathBuf, RemovedWorktree>) = Device,
    ParkedBranches(parked_branches: Vec<ParkedBranch>) = Device,
    Notify(notify: NotifyConfig) = Shared,
    CheckAppUpdates(check_app_updates: bool) = Shared,
    SkippedAppVersion(skipped_app_version: Option<String>) = Shared,
    PaletteRecent(palette_recent: Vec<String>) = Device,
    ChangesPanelWidth(changes_panel_width: f32) = Device,
    GatewayMode(gateway_mode: GatewayMode) = Device,
    SessionView(session_view: SessionView) = Shared,
}

/// The committed settings as one client read them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub config: Arc<AppConfig>,
    /// Grows whenever this process saw the committed settings change.
    pub revision: u64,
}

/// Field changes made from `base` (the snapshot config the client edited).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edit {
    pub base: Arc<AppConfig>,
    pub changes: Vec<Setting>,
}

impl Edit {
    /// The edit that turns `base` into `edited` (only the fields that differ).
    pub fn between(base: Arc<AppConfig>, edited: &AppConfig) -> Edit {
        let changes = Setting::diff(&base, edited);
        Edit { base, changes }
    }
}

#[derive(Clone, Debug, thiserror::Error, Serialize, Deserialize)]
pub enum SettingsError {
    /// Someone else changed these fields since the edit's baseline; nothing was applied.
    /// `current` holds the committed settings to rebase onto.
    #[error("settings changed elsewhere: {fields:?}")]
    Conflict {
        fields: Vec<Field>,
        current: Snapshot,
    },
    /// Locking, reading or writing the settings file failed; nothing was applied.
    #[error("saving settings failed: {0}")]
    Io(String),
    /// The settings worker is gone.
    #[error("the settings worker stopped")]
    Gone,
}

/// The outcome of one submitted edit (see [`SettingsService::submit`]).
pub struct PendingEdit {
    rx: Receiver<Result<Snapshot, SettingsError>>,
}

impl PendingEdit {
    /// `None` while the worker has not decided yet.
    pub fn try_result(&self) -> Option<Result<Snapshot, SettingsError>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(SettingsError::Gone)),
        }
    }

    /// Blocks until the worker decided.
    pub fn wait(self) -> Result<Snapshot, SettingsError> {
        self.rx.recv().unwrap_or(Err(SettingsError::Gone))
    }
}

type Listener = Arc<dyn Fn() + Send + Sync>;

enum Cmd {
    Edit(Edit, Sender<Result<Snapshot, SettingsError>>),
    /// Pick up changes other processes committed.
    Reload,
    /// Answered once every earlier command is done.
    Flush(Sender<()>),
}

struct Shared {
    path: PathBuf,
    snapshot: Mutex<Snapshot>,
    listeners: Mutex<Vec<Listener>>,
}

impl Shared {
    fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn listeners(&self) -> Vec<Listener> {
        self.listeners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The worker's side: publishes snapshots and tells the notifier, never running client code.
struct Writer {
    shared: Arc<Shared>,
    /// Capacity one: changes that arrive while the notifier is busy fold into one notice.
    changed: SyncSender<()>,
}

impl Writer {
    /// Publishes `config` when it differs from the current snapshot; returns the snapshot.
    fn publish(&self, config: AppConfig) -> Snapshot {
        let snap = {
            let mut cur = self
                .shared
                .snapshot
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if *cur.config == config {
                return cur.clone();
            }
            *cur = Snapshot {
                config: Arc::new(config),
                revision: cur.revision + 1,
            };
            cur.clone()
        };
        let _ = self.changed.try_send(());
        snap
    }
}

struct Inner {
    shared: Arc<Shared>,
    tx: Mutex<Option<Sender<Cmd>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(t) = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = t.join();
        }
    }
}

/// Handle to the settings owner; clones share it. The worker stops (after finishing queued
/// edits) when the last handle is dropped.
#[derive(Clone)]
pub struct SettingsService {
    inner: Arc<Inner>,
}

static NEXT_WORKER: AtomicU64 = AtomicU64::new(1);

impl SettingsService {
    /// The app's settings file: `state.json` in OmpDeck's config directory.
    pub fn open_default() -> SettingsService {
        SettingsService::open(config_dir().join("state.json"))
    }

    /// Loads `path` (missing or unreadable → defaults, logged) and starts the worker and the
    /// change notifier. The initial read is the only file access on the caller's thread: call it
    /// at startup. Edits re-read the file strictly: while it cannot be read or parsed they fail
    /// with [`SettingsError::Io`] and the file is left as it is.
    pub fn open(path: PathBuf) -> SettingsService {
        let config = AppConfig::load_from(&path);
        let shared = Arc::new(Shared {
            path,
            snapshot: Mutex::new(Snapshot {
                config: Arc::new(config),
                revision: 0,
            }),
            listeners: Mutex::new(Vec::new()),
        });
        let n = NEXT_WORKER.fetch_add(1, Ordering::Relaxed);
        let (changed, notices) = mpsc::sync_channel(1);
        let notifier = shared.clone();
        // Listeners run here, off the worker: one may block on the service (flush, wait).
        // The thread ends once the worker, the only sender, is gone.
        if let Err(e) = std::thread::Builder::new()
            .name(format!("omp-deck-settings-notify-{n}"))
            .spawn(move || {
                for () in notices {
                    for l in notifier.listeners() {
                        l();
                    }
                }
            })
        {
            log::error!("cannot start the settings notifier: {e}");
        }
        let (tx, rx) = mpsc::channel();
        let writer = Writer {
            shared: shared.clone(),
            changed,
        };
        let thread = std::thread::Builder::new()
            .name(format!("omp-deck-settings-{n}"))
            .spawn(move || run(&writer, &rx))
            .map_err(|e| log::error!("cannot start the settings worker: {e}"))
            .ok();
        SettingsService {
            inner: Arc::new(Inner {
                shared,
                tx: Mutex::new(Some(tx)),
                thread: Mutex::new(thread),
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.inner.shared.path
    }

    /// The latest committed settings this process knows.
    pub fn snapshot(&self) -> Snapshot {
        self.inner.shared.snapshot()
    }

    /// Called whenever the snapshot changed, on the service's notifier thread (never the
    /// writer): it may call back into the service. Changes during a call fold into one more call.
    pub fn on_change(&self, listener: impl Fn() + Send + Sync + 'static) {
        self.inner
            .shared
            .listeners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::new(listener));
    }

    fn send(&self, cmd: Cmd) -> bool {
        self.inner
            .tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|tx| tx.send(cmd).is_ok())
    }

    /// Queues `edit`; its outcome arrives on the returned handle. An edit without changes
    /// commits nothing and returns the current snapshot.
    pub fn submit(&self, edit: Edit) -> PendingEdit {
        let (tx, rx) = mpsc::channel();
        if !self.send(Cmd::Edit(edit, tx.clone())) {
            let _ = tx.send(Err(SettingsError::Gone));
        }
        PendingEdit { rx }
    }

    /// Re-reads the file (under the lock) to pick up other processes' commits.
    pub fn reload(&self) {
        self.send(Cmd::Reload);
    }

    /// Blocks until every edit submitted before is committed or refused (shutdown).
    pub fn flush(&self) {
        let (tx, rx) = mpsc::channel();
        if self.send(Cmd::Flush(tx)) {
            let _ = rx.recv();
        }
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// Holds the cross-process lock while alive.
fn lock_file(path: &Path) -> Result<File, String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let lock = lock_path(path);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock)
        .map_err(|e| format!("{}: {e}", lock.display()))?;
    file.lock()
        .map_err(|e| format!("locking {}: {e}", lock.display()))?;
    Ok(file)
}

/// The stored settings, read under the lock. Only a missing file means defaults; a file that
/// cannot be read or parsed is an error, so an edit never overwrites settings it could not see.
fn read_stored(path: &Path) -> Result<AppConfig, SettingsError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AppConfig::default()),
        Err(e) => return Err(SettingsError::Io(format!("{}: {e}", path.display()))),
    };
    // One read, one decode (with the core's legacy migrations) of exactly these bytes.
    parse_config(&bytes)
        .map_err(|e| SettingsError::Io(format!("{} is not valid settings: {e}", path.display())))
}

fn run(writer: &Writer, rx: &Receiver<Cmd>) {
    for cmd in rx {
        match cmd {
            Cmd::Edit(edit, reply) => {
                let _ = reply.send(commit(writer, edit));
            }
            Cmd::Reload => {
                // On failure the last good snapshot stays.
                let read = lock_file(&writer.shared.path)
                    .map_err(SettingsError::Io)
                    .and_then(|_lock| read_stored(&writer.shared.path));
                match read {
                    Ok(config) => {
                        writer.publish(config);
                    }
                    Err(e) => log::warn!("reloading settings failed: {e}"),
                }
            }
            Cmd::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// Applies `edit` to the stored settings under the file lock.
fn commit(writer: &Writer, edit: Edit) -> Result<Snapshot, SettingsError> {
    let path = &writer.shared.path;
    let _lock = lock_file(path).map_err(SettingsError::Io)?;
    let stored = read_stored(path)?;
    let mut conflicts: Vec<Field> = Vec::new();
    for c in &edit.changes {
        if c.held_by(&stored) || c.same_in(&edit.base, &stored) {
            continue;
        }
        let f = c.field();
        if !conflicts.contains(&f) {
            conflicts.push(f);
        }
    }
    if !conflicts.is_empty() {
        let current = writer.publish(stored);
        return Err(SettingsError::Conflict {
            fields: conflicts,
            current,
        });
    }
    let mut merged = stored.clone();
    for c in edit.changes {
        c.apply(&mut merged);
    }
    // The app scale is a hard range for every writer (not just this window), kept at the owner.
    merged.ui_scale = clamp_ui_scale(merged.ui_scale);
    if merged != stored {
        merged
            .save_to(path)
            .map_err(|e| SettingsError::Io(e.to_string()))?;
    }
    Ok(writer.publish(merged))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(dir: &tempfile::TempDir) -> SettingsService {
        SettingsService::open(dir.path().join("state.json"))
    }

    #[test]
    fn disjoint_edits_from_two_processes_from_the_same_baseline_both_survive() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Two owners of the same file, as two OmpDeck windows have.
        let a = open(&dir);
        let b = open(&dir);
        let base = a.snapshot().config;
        let mut ea = (*base).clone();
        ea.font_size = 17.0;
        let mut eb = (*base).clone();
        eb.notify.desktop = false;
        a.submit(Edit::between(base.clone(), &ea))
            .wait()
            .expect("a commits");
        let snap = b
            .submit(Edit::between(base, &eb))
            .wait()
            .expect("b commits");
        assert_eq!(snap.config.font_size, 17.0, "a's change kept");
        assert!(!snap.config.notify.desktop);
        let stored = AppConfig::load_from(&dir.path().join("state.json"));
        assert_eq!(stored.font_size, 17.0);
        assert!(!stored.notify.desktop);
    }

    #[test]
    fn a_conflicting_edit_is_refused_whole_and_reports_the_committed_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        // An explicit saved baseline, so both edits really change what is stored.
        let saved = AppConfig {
            sidebar_width: 250.0,
            font_size: 13.0,
            ..AppConfig::default()
        };
        saved
            .save_to(&dir.path().join("state.json"))
            .expect("save baseline");
        let a = open(&dir);
        let b = open(&dir);
        let base = a.snapshot().config;
        let mut ea = (*base).clone();
        ea.sidebar_width = 300.0;
        a.submit(Edit::between(base.clone(), &ea))
            .wait()
            .expect("a commits");
        let mut eb = (*base).clone();
        eb.sidebar_width = 200.0;
        eb.font_size = 19.0;
        let err = b
            .submit(Edit::between(base, &eb))
            .wait()
            .expect_err("same field changed since the baseline");
        let SettingsError::Conflict { fields, current } = err else {
            panic!("not a conflict: {err}");
        };
        assert_eq!(fields, vec![Field::SidebarWidth]);
        assert_eq!(current.config.sidebar_width, 300.0);
        assert_eq!(
            current.config.font_size, 13.0,
            "nothing of the edit applied"
        );
        assert_eq!(b.snapshot().config.sidebar_width, 300.0);
    }

    #[test]
    fn rebase_keeps_local_changes_and_adopts_the_rest() {
        let old = AppConfig::default();
        let mut new = old.clone();
        new.font_size = 20.0;
        new.sidebar_width = 333.0;
        let mut local = old.clone();
        local.sidebar_width = 111.0;
        rebase(&mut local, &old, &new);
        assert_eq!(local.font_size, 20.0);
        assert_eq!(local.sidebar_width, 111.0);
    }

    #[test]
    fn app_scale_is_clamped_at_the_owner() {
        let dir = tempfile::tempdir().expect("temp dir");
        let a = open(&dir);
        let base = a.snapshot().config;
        let mut edited = (*base).clone();
        edited.ui_scale = 4.0;
        let snap = a
            .submit(Edit::between(base, &edited))
            .wait()
            .expect("commits");
        assert_eq!(snap.config.ui_scale, deck_core::config::MAX_UI_SCALE);
        let stored = AppConfig::load_from(&dir.path().join("state.json"));
        assert_eq!(stored.ui_scale, deck_core::config::MAX_UI_SCALE);
    }

    #[test]
    fn the_session_view_is_a_shared_setting_that_round_trips() {
        assert_eq!(Field::SessionView.scope(), Scope::Shared);
        let dir = tempfile::tempdir().expect("temp dir");
        let a = open(&dir);
        let b = open(&dir);
        let base = a.snapshot().config;
        assert_eq!(base.session_view, SessionView::Chat);
        let mut edited = (*base).clone();
        edited.session_view = SessionView::Terminal;
        let edit = Edit::between(base.clone(), &edited);
        assert_eq!(edit.changes.len(), 1, "only the view differs");
        let snap = a.submit(edit).wait().expect("commits");
        assert_eq!(snap.config.session_view, SessionView::Terminal);
        let stored = AppConfig::load_from(&dir.path().join("state.json"));
        assert_eq!(stored.session_view, SessionView::Terminal);
        // A window that only changed its font keeps the view the other window chose, both
        // by rebasing and when its edit commits.
        let mut untouched = (*base).clone();
        rebase(&mut untouched, &base, &snap.config);
        assert_eq!(untouched.session_view, SessionView::Terminal);
        let mut own = (*base).clone();
        own.font_size = 19.0;
        let merged = b
            .submit(Edit::between(base, &own))
            .wait()
            .expect("an unrelated edit merges");
        assert_eq!(merged.config.session_view, SessionView::Terminal);
        assert_eq!(merged.config.font_size, 19.0);
    }

    #[test]
    fn model_preferences_survive_wire_edits_rebases_and_owner_restart() {
        assert_eq!(Field::FavoriteModels.scope(), Scope::Shared);
        assert_eq!(Field::ModelProviderFilter.scope(), Scope::Shared);
        for mode in [
            GatewayMode::Off,
            GatewayMode::WhileOpen,
            GatewayMode::Background,
        ] {
            let dir = tempfile::tempdir().expect("temp dir");
            AppConfig {
                gateway_mode: mode,
                ..AppConfig::default()
            }
            .save_to(&dir.path().join("state.json"))
            .expect("save baseline");
            let owner = open(&dir);
            let other = open(&dir);
            let base = owner.snapshot().config;
            let mut edited = (*base).clone();
            edited.toggle_favorite("fake/fake-1");
            edited.model_provider_filter = Some("fake".into());
            let edit = Edit::between(base.clone(), &edited);
            assert_eq!(
                edit.changes.iter().map(Setting::field).collect::<Vec<_>>(),
                [Field::FavoriteModels, Field::ModelProviderFilter]
            );
            // The gateway transports the same edit and snapshot as JSON.
            let bytes = serde_json::to_vec(&edit).expect("encode edit");
            let edit = serde_json::from_slice(&bytes).expect("decode edit");
            let committed = owner.submit(edit).wait().expect("commit model preferences");
            let bytes = serde_json::to_vec(&committed).expect("encode snapshot");
            let committed: Snapshot = serde_json::from_slice(&bytes).expect("decode snapshot");
            let mut local = (*base).clone();
            local.check_updates = false;
            rebase(&mut local, &base, &committed.config);
            assert_eq!(local.favorite_models, ["fake/fake-1"]);
            assert_eq!(local.model_provider_filter.as_deref(), Some("fake"));
            assert!(!local.check_updates, "unsubmitted shared edits stay");
            // A stale window's unrelated Shared-field edit must not restore empty favorites.
            let mut stale = (*base).clone();
            stale.check_updates = false;
            other
                .submit(Edit::between(base, &stale))
                .wait()
                .expect("merge shared edit");
            owner.flush();
            other.flush();
            drop(other);
            drop(owner);
            let restarted = open(&dir);
            let cfg = restarted.snapshot().config;
            assert_eq!(cfg.favorite_models, ["fake/fake-1"], "{mode:?}");
            assert_eq!(
                cfg.model_provider_filter.as_deref(),
                Some("fake"),
                "{mode:?}"
            );
            assert!(!cfg.check_updates, "{mode:?}");
            assert_eq!(cfg.gateway_mode, mode);
            let mut edited = (*cfg).clone();
            edited.toggle_favorite("fake/fake-1");
            edited.model_provider_filter = None;
            restarted
                .submit(Edit::between(cfg, &edited))
                .wait()
                .expect("remove preferences");
            restarted.flush();
            drop(restarted);
            let cfg = open(&dir).snapshot().config;
            assert!(cfg.favorite_models.is_empty());
            assert_eq!(cfg.model_provider_filter, None);
        }
    }

    fn tab(name: &str) -> TabState {
        TabState {
            session_name: name.to_owned(),
            cwd: PathBuf::from("/w"),
            session_path: None,
            title: name.to_owned(),
            view: SessionView::Terminal,
        }
    }

    #[test]
    fn a_tab_list_and_an_index_chosen_in_another_list_never_mix() {
        let dir = tempfile::tempdir().expect("temp dir");
        let saved = AppConfig {
            open_tabs: vec![tab("s1"), tab("s2")],
            active_tab: Some(0),
            font_size: 13.0,
            ..AppConfig::default()
        };
        saved
            .save_to(&dir.path().join("state.json"))
            .expect("save baseline");
        let a = open(&dir);
        let b = open(&dir);
        let base = a.snapshot().config;
        // Window A replaces s2 with s3 and activates the first tab.
        let mut ea = (*base).clone();
        ea.open_tabs = vec![tab("s1"), tab("s3")];
        ea.active_tab = Some(0);
        a.submit(Edit::between(base.clone(), &ea))
            .wait()
            .expect("a commits");
        // Window B, from the old list, selects s2 (index 1) and splits; also changes the font.
        let mut eb = (*base).clone();
        eb.active_tab = Some(1);
        eb.split = Some(SplitState::default());
        eb.font_size = 19.0;
        let err = b
            .submit(Edit::between(base.clone(), &eb))
            .wait()
            .expect_err("the index belongs to the replaced list");
        let SettingsError::Conflict { fields, current } = err else {
            panic!("not a conflict: {err}");
        };
        assert_eq!(fields, vec![Field::TabLayout]);
        assert_eq!(TabLayout::of(&current.config), TabLayout::of(&ea));
        assert_eq!(
            current.config.font_size, 13.0,
            "nothing of the edit applied"
        );
        // An unrelated preference from the same window still merges with A's layout.
        let mut font_only = (*current.config).clone();
        font_only.font_size = 19.0;
        let snap = b
            .submit(Edit::between(current.config.clone(), &font_only))
            .wait()
            .expect("b's font commits");
        assert_eq!(snap.config.font_size, 19.0);
        assert_eq!(TabLayout::of(&snap.config), TabLayout::of(&ea));
        let stored = AppConfig::load_from(&dir.path().join("state.json"));
        assert_eq!(TabLayout::of(&stored), TabLayout::of(&ea));
        assert_eq!(stored.font_size, 19.0);
    }

    fn folder(tabs: &[&str], active: usize) -> FolderTabs {
        FolderTabs {
            open_tabs: tabs.iter().map(|t| tab(t)).collect(),
            active_tab: Some(active),
            split: None,
        }
    }

    /// Two windows, each opened on the same stored settings with folders `/p` and `/q`.
    fn two_folder_windows(
        dir: &tempfile::TempDir,
    ) -> (SettingsService, SettingsService, Arc<AppConfig>) {
        let saved = AppConfig {
            folder_tabs: BTreeMap::from([
                (PathBuf::from("/p"), folder(&["p1", "p2"], 0)),
                (PathBuf::from("/q"), folder(&["q1"], 0)),
            ]),
            font_size: 13.0,
            ..AppConfig::default()
        };
        saved
            .save_to(&dir.path().join("state.json"))
            .expect("save baseline");
        let a = open(dir);
        let b = open(dir);
        let base = a.snapshot().config;
        (a, b, base)
    }

    #[test]
    fn edits_of_different_folders_merge() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (a, b, base) = two_folder_windows(&dir);
        let mut ea = (*base).clone();
        ea.folder_tabs.insert("/p".into(), folder(&["p3"], 0));
        a.submit(Edit::between(base.clone(), &ea))
            .wait()
            .expect("a commits");
        let mut eb = (*base).clone();
        eb.folder_tabs.insert("/q".into(), folder(&["q1", "q2"], 1));
        eb.folder_tabs.insert("/r".into(), folder(&["r1"], 0));
        let snap = b
            .submit(Edit::between(base, &eb))
            .wait()
            .expect("another folder merges");
        let stored = AppConfig::load_from(&dir.path().join("state.json"));
        for cfg in [&*snap.config, &stored] {
            assert_eq!(cfg.folder_tabs[Path::new("/p")], folder(&["p3"], 0));
            assert_eq!(cfg.folder_tabs[Path::new("/q")], folder(&["q1", "q2"], 1));
            assert_eq!(cfg.folder_tabs[Path::new("/r")], folder(&["r1"], 0));
        }
    }

    #[test]
    fn edits_of_the_same_folder_conflict_and_apply_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (a, b, base) = two_folder_windows(&dir);
        let mut ea = (*base).clone();
        ea.folder_tabs.insert("/p".into(), folder(&["p3"], 0));
        a.submit(Edit::between(base.clone(), &ea))
            .wait()
            .expect("a commits");
        // B selects p2 in the list A replaced, opens a folder and changes the font.
        let mut eb = (*base).clone();
        eb.folder_tabs.insert("/p".into(), folder(&["p1", "p2"], 1));
        eb.folder_tabs.insert("/r".into(), folder(&["r1"], 0));
        eb.font_size = 19.0;
        let err = b
            .submit(Edit::between(base, &eb))
            .wait()
            .expect_err("the same folder changed since the baseline");
        let SettingsError::Conflict { fields, current } = err else {
            panic!("not a conflict: {err}");
        };
        assert_eq!(fields, vec![Field::FolderTabs(PathBuf::from("/p"))]);
        assert_eq!(current.config.folder_tabs, ea.folder_tabs);
        assert_eq!(current.config.font_size, 13.0);
        // Taking A's value for the conflicted folder keeps B's other local changes.
        let mut local = eb.clone();
        adopt(&mut local, &current.config, &fields);
        assert_eq!(local.folder_tabs[Path::new("/p")], folder(&["p3"], 0));
        assert_eq!(local.folder_tabs[Path::new("/r")], folder(&["r1"], 0));
        assert_eq!(local.font_size, 19.0);
    }

    #[test]
    fn removing_a_folder_and_editing_another_keeps_both() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (a, b, base) = two_folder_windows(&dir);
        let mut ea = (*base).clone();
        ea.folder_tabs.remove(Path::new("/p"));
        a.submit(Edit::between(base.clone(), &ea))
            .wait()
            .expect("a commits");
        let mut eb = (*base).clone();
        eb.folder_tabs.insert("/q".into(), folder(&["q2"], 0));
        let snap = b
            .submit(Edit::between(base.clone(), &eb))
            .wait()
            .expect("another folder merges");
        let expected = BTreeMap::from([(PathBuf::from("/q"), folder(&["q2"], 0))]);
        assert_eq!(snap.config.folder_tabs, expected);
        assert_eq!(
            AppConfig::load_from(&dir.path().join("state.json")).folder_tabs,
            expected
        );
        // A window still holding the old snapshot rebases per folder: the removal and the
        // other edit arrive, its own unsubmitted folder stays.
        let mut local = (*base).clone();
        local.folder_tabs.insert("/s".into(), folder(&["s1"], 0));
        rebase(&mut local, &base, &snap.config);
        assert!(!local.folder_tabs.contains_key(Path::new("/p")));
        assert_eq!(local.folder_tabs[Path::new("/q")], folder(&["q2"], 0));
        assert_eq!(local.folder_tabs[Path::new("/s")], folder(&["s1"], 0));
    }

    #[test]
    fn an_edit_onto_a_corrupt_file_fails_and_leaves_its_bytes_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("state.json");
        let s = SettingsService::open(path.clone());
        // Another writer left a file this version cannot read.
        let corrupt = b"{\"font_size\": 17.0, \"theme\": ";
        std::fs::write(&path, corrupt).expect("write");
        let base = s.snapshot().config;
        let mut edited = (*base).clone();
        edited.sidebar_width = 222.0;
        let err = s
            .submit(Edit::between(base.clone(), &edited))
            .wait()
            .expect_err("the stored settings are unreadable");
        assert!(matches!(err, SettingsError::Io(_)), "{err}");
        assert_eq!(std::fs::read(&path).expect("read"), corrupt);
        // A reload keeps the last good snapshot.
        s.reload();
        s.flush();
        assert_eq!(s.snapshot().config, base);
    }

    #[test]
    fn a_listener_may_call_back_into_the_service() {
        let dir = tempfile::tempdir().expect("temp dir");
        let s = open(&dir);
        let (seen, notices) = mpsc::channel();
        let inner = s.clone();
        let seen = Mutex::new(seen);
        s.on_change(move || {
            // Blocking on the writer from the listener must not deadlock it.
            inner.flush();
            let _ = seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(inner.snapshot().revision);
        });
        let base = s.snapshot().config;
        let mut edited = (*base).clone();
        edited.font_size = 21.0;
        let snap = s
            .submit(Edit::between(base, &edited))
            .wait()
            .expect("commits");
        let revision = notices
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the listener ran to completion");
        assert!(revision >= snap.revision);
        // The writer still serves requests after the listener blocked on it.
        s.flush();
    }
}
