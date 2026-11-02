//! The eframe application: a window onto the OmpDeck gateway. Sessions, scans, settings and
//! notifications live in the gateway; this window owns tabs, splits, focus, dialogs and the
//! native effects (pickers, clipboard, window raise), and talks to the gateway asynchronously.

use std::collections::{HashMap, HashSet};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

use crate::icons::{
    ICON_CLIPBOARD_LIST, ICON_COPY, ICON_EXTERNAL_LINK, ICON_TERMINAL, ICON_TRIANGLE_ALERT,
};
use deck_core::agents::AgentsTarget;
use deck_core::config::{
    AppConfig, Density, FolderTabs, GatewayMode, SessionView, TabState, Theme, UiFont, UiStyle,
    WindowGeometry, clamp_ui_scale,
};
use deck_core::mux::{BackendChoice, MuxKind, new_session_name, session_name_for};
use deck_core::omp::{DeleteTarget, MoveKind, MoveOutcome, Setting, UpdateKind, fork_argv};
use deck_core::platform;
use deck_core::sessions::{ProjectGroup, SessionEntry, abbreviate_home};
use deck_core::worktree::Worktree;
use deck_service::controller::{RequestId, SessionTarget};
use deck_service::gateway::client::{ClientEvent, GatewayOptions, Pending};
use deck_service::gateway::protocol::{
    Answer, Done, GatewayError, GatewayStatus, Mutation, Outcome, Query, Role, Tool, Topic,
};
use deck_service::gateway::running::RunningSnapshot;
use deck_service::settings::{self, Edit, SettingsError};
use deck_service::status::TabStatus;
use deck_service::statusfmt::Activity;
use deck_term::{Clipboard, TerminalView};
use egui::{Frame, Key, Margin, Modifiers, RichText};
use serde_json::Value;

use crate::agent_ctl::Req;
use crate::agent_model::AgentsView;
use crate::agent_popup::PopupState;
use crate::cli::Cli;
use crate::clipboard_bridge;
use crate::confirm::{self, Confirm};
use crate::dashboard::DashState;
use crate::folder_scope;
use crate::gw::{Gw, LinkView};
use crate::model_picker::{ModelsCache, PickerState};
use crate::notify_view::NotifyInput;
use crate::omp_settings::{config_dir_display, defaults_from_catalog};
use crate::palette::PaletteState;
use crate::panes::Split;
use crate::settings_page::{self, SettingsAction, SettingsInput, SettingsState};
use crate::sidebar::{self, MenuEnv, SidebarAction, SidebarCache, SidebarInput};
use crate::statusbar;
use crate::tabs::{
    self, Anchor, ClosedTab, CopyPending, MovedTab, ReopenStack, Tab, TabAction, TabBarInput,
    TabRole,
};
use crate::theme::{self, Palette};
use crate::titlebar::{self, BarAction, BarInput, OmpBar};
use crate::toast::{ToastAction, ToastClick, ToastKind, Toasts};
use crate::transfer::{self, FolderDialog, FolderOutcome, TransferKind};
use crate::ui_fonts;
use crate::update::UpdateTracker;
use crate::usage_state::Usage;
use crate::usage_view;
use crate::widgets::{empty_state, icon_text, menu_item};

mod agent_config;
mod agents;
mod app_update;
mod backup;
mod bar;
mod changes;
mod ctl;
mod desktop;
mod mobile;
mod notifications;
mod palette;
mod project_sync;
mod spawn;
mod split;
use split::PaneClaim;
mod session_manager;
mod tree;
mod usage;
mod worktree;

use session_manager::MovePrep;
use spawn::{Focus, Post, SpawnReq};
use worktree::WtUi;

const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);
const REFRESH_SPIN: Duration = Duration::from_millis(800);
const MIN_FONT: f32 = 8.0;
const MAX_FONT: f32 = 40.0;
const DEFAULT_FONT: f32 = 14.0;
/// Toast action id: quit the app (after changing the title-bar style).
const TOAST_QUIT: u64 = 1;
/// How often the sidebar's age chips are refreshed (they only change by minutes).
const AGE_REFRESH: Duration = Duration::from_secs(15);
/// How long exit waits for the gateway to acknowledge the final settings.
const EXIT_WAIT: Duration = Duration::from_secs(5);
/// How long after an installer run the gateway may take to find the new omp.
const INSTALL_WAIT: Duration = Duration::from_secs(5);

pub struct AppInit {
    pub cli: Cli,
    /// Native window geometry and appearance, read before the window opened; the committed
    /// settings come from the gateway and are adopted as soon as they arrive.
    pub cfg: AppConfig,
    /// The gateway profile this window is a client of.
    pub profile: deck_service::gateway::Profile,
    /// UI font bytes, read before the window opens.
    pub ui_fonts: ui_fonts::UiFonts,
    /// The command-line folder, already resolved (`None` = none given).
    pub folder: Option<Result<PathBuf, String>>,
    /// Process working directory at startup: base of relative typed scope paths.
    pub start_dir: PathBuf,
}

enum ShotState {
    Waiting,
    Requested(Arc<AtomicBool>),
}

struct ShotPlan {
    path: PathBuf,
    after: Duration,
    state: ShotState,
}

/// Results from native worker threads, delivered through a channel and drained each frame.
enum AppEvent {
    Toast(ToastKind, String),
}

/// A modal question awaiting the user's answer, with what to do on "yes".
enum PendingConfirm {
    StopSession {
        mux_name: String,
        label: String,
    },
    SwitchChannel(UpdateKind),
    MoveSession {
        entry: SessionEntry,
        dest: PathBuf,
    },
    /// An unsent draft in a tab's composer that a status-bar action would clear.
    ClearDraft {
        uid: u64,
        /// The controller request waiting for the answer.
        id: RequestId,
    },
    /// Kill this subagent through the control link.
    KillAgent {
        id: String,
        /// The tab the question was asked in, its control key and its session file: the kill is
        /// sent only if all three still match when the user confirms.
        uid: u64,
        key: String,
        session: Option<PathBuf>,
    },
    /// Restart a read-only tab's omp with the control extension (tab by `uid`).
    RestartLive {
        uid: u64,
        /// The session file the question was asked about.
        session: PathBuf,
    },
    /// Restart a running session from the sidebar (with or without a tab).
    RestartSession(RestartTarget),
    SwitchView {
        uid: u64,
        session: PathBuf,
        view: SessionView,
    },
    /// Delete a session file (and its artifacts) through omp.
    DeleteSession(SessionEntry),
    /// Delete the sessions checked in the session manager.
    DeleteSessions(Vec<deck_core::omp::DeleteTarget>),
    /// Move the session manager's selection into a folder.
    MoveSessions {
        paths: Vec<PathBuf>,
        dest: PathBuf,
    },
    /// Stop the running sessions checked in the session manager.
    StopSessions(Vec<PathBuf>),
    /// Kill the live sessions (by backend name) checked in the session manager's Running view.
    KillRunning(Vec<String>),
    /// Restore a downloaded config backup over the local omp config.
    RestoreBackup(backup::PendingRestore),
    /// Forget the saved backup server.
    RemoveServer,
    /// Remove a clean, merged worktree (with its branch) or park a clean, pushed one (the
    /// branch stays); the gateway moves the sessions inside it into the primary checkout.
    RemoveWorktree {
        wt: Worktree,
        park: bool,
    },
    /// Check a parked branch out in the main checkout (`open`: and open the clicked session
    /// there), or switch the main checkout back to the branch's base.
    SwitchMain {
        entry: SessionEntry,
        open: bool,
    },
    /// Drop the project-backup row of a worktree that no longer exists.
    DropSyncRow {
        path: PathBuf,
    },
    /// Discard or delete files from the Changes panel.
    Changes(changes::PendingWrite),
}

/// What a sidebar restart was asked about.
pub(super) struct RestartTarget {
    /// The tab showing the session when asked (`None`: only a backend is running it).
    pub uid: Option<u64>,
    pub mux_name: String,
    pub session: PathBuf,
    pub cwd: PathBuf,
    pub title: String,
}

/// The settings this window owns through its live tab and split state instead of through `cfg`
/// (they are projected into the submitted config). It records what the window last agreed on with
/// the committed settings: only a projection that differs from it is this window's own change, so
/// another window's newer commit is never silently overwritten.
///
/// The tab list, the active index and the split slots reference each other, so they are one unit
/// (`Field::TabLayout`): committed together and given up together. A folder-scoped window owns only the entry of its
/// folder in the folder map.
#[derive(Clone, PartialEq)]
struct TabsProj {
    open_tabs: Vec<TabState>,
    active_tab: Option<usize>,
    split: Option<deck_core::config::SplitState>,
    /// The folder this window is scoped to, and its entry in the folder map.
    folder_key: Option<PathBuf>,
    folder: Option<FolderTabs>,
}

impl TabsProj {
    fn of(cfg: &AppConfig, folder_key: Option<&PathBuf>) -> TabsProj {
        TabsProj {
            open_tabs: cfg.open_tabs.clone(),
            active_tab: cfg.active_tab,
            split: cfg.split.clone(),
            folder_key: folder_key.cloned(),
            folder: folder_key.and_then(|k| cfg.folder_tabs.get(k).cloned()),
        }
    }

    fn layout_is(&self, cfg: &AppConfig) -> bool {
        cfg.open_tabs == self.open_tabs
            && cfg.active_tab == self.active_tab
            && cfg.split == self.split
    }

    fn take_layout(&mut self, from: &AppConfig) {
        self.open_tabs = from.open_tabs.clone();
        self.active_tab = from.active_tab;
        self.split = from.split.clone();
    }

    /// In `edited` (a projected config), puts back `base`'s value of what the window has not
    /// changed since it last synced (for the folder map, only this window's own entry).
    fn mask_unchanged(&self, edited: &mut AppConfig, base: &AppConfig) {
        if self.layout_is(edited) {
            edited.open_tabs = base.open_tabs.clone();
            edited.active_tab = base.active_tab;
            edited.split = base.split.clone();
        }
        // The working map keeps every explicit edit of other keys (a removed worktree's entry);
        // only the projected entry of this window's folder is subject to the masking.
        if let Some(k) = &self.folder_key
            && edited.folder_tabs.get(k) == self.folder.as_ref()
        {
            match base.folder_tabs.get(k) {
                Some(v) => edited.folder_tabs.insert(k.clone(), v.clone()),
                None => edited.folder_tabs.remove(k),
            };
        }
    }

    /// An edit that carried `sent` (against `base`) was committed: what it changed is synced.
    fn committed(&mut self, sent: &AppConfig, base: &AppConfig) {
        if sent.open_tabs != base.open_tabs
            || sent.active_tab != base.active_tab
            || sent.split != base.split
        {
            self.take_layout(sent);
        }
        if let Some(k) = &self.folder_key {
            let (new, old) = (sent.folder_tabs.get(k), base.folder_tabs.get(k));
            if new != old {
                self.folder = new.cloned();
            }
        }
    }

    /// `fields` conflicted and the other writer's value stands. The layout counts as synced
    /// whole; a folder entry only when it is this window's own (the conflict is per folder key,
    /// so another folder's change never acknowledges, and never drops, our entry).
    fn yield_fields(&mut self, live: &TabsProj, fields: &[settings::Field]) {
        use settings::Field;
        if fields.contains(&Field::TabLayout) {
            self.open_tabs = live.open_tabs.clone();
            self.active_tab = live.active_tab;
            self.split = live.split.clone();
        }
        if let Some(k) = &self.folder_key
            && fields
                .iter()
                .any(|f| matches!(f, Field::FolderTabs(key) if key == k))
        {
            self.folder = live.folder.clone();
        }
    }
}

/// The settings whose adoption needs a runtime effect beyond a new value in `cfg`.
#[derive(Clone, PartialEq)]
struct Applied {
    rows: Vec<deck_core::config::SyncRow>,
    look: (Theme, UiStyle, Density, UiFont, bool),
    ui_scale: f32,
}

pub struct App {
    ctx: egui::Context,
    /// This window's working copy of the settings.
    cfg: AppConfig,
    /// The committed settings `cfg` was last reconciled with (the baseline of the next edit).
    cfg_base: Arc<AppConfig>,
    /// Settings revision `cfg_base` reflects (0 = the startup read, before the gateway's).
    cfg_rev: u64,
    /// The edit in flight at the gateway, with the config it carried.
    saving: Option<(
        Pending<Result<settings::Snapshot, SettingsError>>,
        AppConfig,
    )>,
    /// The tab/split projection last agreed with the committed settings.
    tabs_synced: TabsProj,
    /// The gateway: link, caches and requests. Everything that is not native goes through it.
    gw: Gw,
    /// A gateway mode change in flight (Settings → Mobile access), and why the last one failed.
    mode_change: Option<Pending<Result<GatewayStatus, GatewayError>>>,
    mode_error: Option<String>,
    root: deck_core::sessions::SessionsRoot,
    root_exists: bool,
    clipboard: Clipboard,

    tabs: Vec<Tab>,
    active: usize,
    closed: ReopenStack<ClosedTab>,
    labels: HashMap<PathBuf, String>,
    /// Revision of the sessions topic `labels` was built from.
    labels_gen: Option<u64>,
    /// Revision of the controllers topic the tabs last took their records from.
    controllers_rev: u64,
    filter: String,
    /// Per-folder mode: the sidebar lists only sessions inside this folder.
    scope: Option<PathBuf>,
    /// Folder given on the command line: this run's tabs are saved under it, not globally.
    launch_folder: Option<PathBuf>,
    /// The native folder dialog for the scope is open.
    picking_scope: bool,
    /// Id of the latest scope validation request; older answers are dropped.
    scope_gen: u64,
    /// Process working directory at startup (base of relative typed paths).
    start_dir: PathBuf,
    /// Zoom factor the current input's viewport rects are expressed with. It lags the active
    /// zoom by the pass in which that changed (egui applies the new zoom after the input was
    /// gathered), so window geometry is converted with it, not with `Context::zoom_factor`.
    viewport_zoom: f32,

    pending_restore: Option<(Vec<TabState>, Option<usize>)>,
    focus_pending: bool,
    terminal_focused: bool,
    /// Split view: the tabs in the panes and which pane has focus.
    split: Split,
    /// Persisted split waiting for the restored tabs (applied once they all exist).
    restore_split: Option<deck_core::config::SplitState>,
    /// Restored tabs by their index in the persisted tab list.
    restore_uids: HashMap<usize, u64>,
    /// Panes reserved for launches still on their way (split-from-sidebar, restarts).
    pane_claims: Vec<PaneClaim>,
    scroll_to_active: bool,
    refresh_until: Option<Instant>,

    toasts: Toasts,
    events_tx: Sender<AppEvent>,
    events_rx: Receiver<AppEvent>,
    confirm: Option<(Confirm, PendingConfirm)>,
    settings: SettingsState,
    /// The Settings window is open.
    settings_open: bool,
    update: UpdateTracker,
    /// Revision of the omp update topic last adopted.
    omp_update_rev: u64,
    /// omp was found and its catalogs were requested from the gateway linked now.
    omp_ready: bool,
    /// Told once that a busy gateway of an older build was kept after an update.
    told_old_gateway: bool,
    /// An installer run finished: waiting for the gateway to report the new omp.
    install_wait: Option<Instant>,
    /// Session files with a move in flight.
    moving: HashSet<PathBuf>,
    sidebar_cache: SidebarCache,
    folder_dialog: Option<FolderDialog>,
    /// Git worktree sessions (dialogs, finishing, relocation).
    wt: WtUi,
    home: Option<PathBuf>,
    /// kdialog or zenity exists, so the folder dialog can offer "Browse…".
    picker_available: bool,

    /// The status the gateway follows for the active session tab.
    status: bar::StatusView,
    status_uid: Option<u64>,
    /// What the gateway was last told the bar shows about the model.
    model_sent: Option<(
        SessionTarget,
        deck_service::gateway::protocol::ModelFactsView,
    )>,
    bar_view: statusbar::BarView,
    /// omp's default model/thinking from its settings (refreshed with the catalog).
    omp_defaults: statusbar::Defaults,
    /// omp's `retry.usageReservePct` (refreshed with the catalog); `None` until it is read.
    omp_usage_reserve: Option<u32>,
    picker: PickerState,
    models: ModelsCache,
    dialog: Option<bar::ActionDialog>,
    /// The session (and generation) the open dialog was opened on.
    dialog_target: Option<SessionTarget>,
    next_tab_uid: u64,
    /// Opens (sidebar, reopen) requested before the first snapshot of the backends: attaching a
    /// canonical name blindly could land in a session that now runs another file. Processed on arrival.
    deferred_opens: Vec<ClosedTab>,
    /// Sessions the gateway asked this window to open (the sidebar's scan may not know them yet).
    desktop_opens: Vec<desktop::DesktopOpen>,
    /// The Neutral font family was rebuilt from the final definitions (first frame).
    fonts_finalized: bool,
    /// Frames drawn since `frame_mark` (debug log `omp_deck::frames`, RUST_LOG=omp_deck::frames=debug).
    frame_count: u32,
    frame_mark: Instant,
    repaint_causes: HashMap<String, u32>,
    /// What the agents dashboard follows (reported to the gateway when it changes).
    agents_target: Option<AgentsTarget>,
    agents_view: AgentsView,
    dash: DashState,
    dash_index: agents::DashIndex,
    popup: Option<PopupState>,
    tree: Option<tree::TreeUi>,
    tree_busy: Option<tree::TreeBusy>,
    usage: Usage,
    /// Revision of the usage topic last adopted.
    usage_rev: u64,
    usage_open: bool,
    usage_ui: usage_view::ViewState,
    sessions: session_manager::SessionHub,
    /// Live subagent control of the active session tab.
    live: ctl::LiveFollow,
    /// Commands sent and not answered yet, by request id.
    ctl_reqs: HashMap<u64, Req>,
    ctl_seq: u64,
    /// The last chat error per agent id (shown under the composer).
    ctl_errors: HashMap<String, String>,
    /// Launches waiting for the gateway (processed one at a time, in order).
    spawn: spawn::Pump,
    /// Bumped whenever the active tab changes by the user's hand; a queued launch may take focus
    /// only while this still equals its own claim.
    activation_seq: u64,
    /// The restored tab that was active (focused once all restored tabs exist).
    restore_focus: Option<u64>,

    dirty: bool,
    last_save: Instant,
    last_title: String,
    last_title_at: Instant,

    started: Instant,
    shot: Option<ShotPlan>,
    /// Backups of the omp config (Google Drive or the user's server).
    backup: backup::BackupUi,
    /// Project sync (per-folder `.omp` + sessions backups).
    sync: project_sync::SyncUi,
    mobile: mobile::MobileUi,
    /// Changes panel.
    changes: changes::ChangesUi,
    /// OmpDeck self-update.
    app_update: app_update::AppUpdateUi,
    /// Window raise and what this window last told the gateway about itself.
    notify: notifications::NotifyUi,
    /// File drags from other applications (the Wayland drop target; X11/Windows come via winit).
    file_drops: crate::file_drop::FileDrops,
    /// The command palette / quick switcher while open.
    palette: Option<PaletteState>,
}

/// Minimum time between two window-title updates.
const WINDOW_TITLE_EVERY: Duration = Duration::from_secs(1);

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, init: AppInit) -> App {
        let ctx = cc.egui_ctx.clone();
        // Order matters: install_fonts replaces the whole font set; icons and the UI face are added after.
        let report = deck_term::install_fonts(&ctx);
        log::info!("terminal fonts: {report:?}");
        let AppInit {
            cli,
            mut cfg,
            profile,
            ui_fonts,
            folder,
            start_dir,
        } = init;
        ui_fonts::install(&ctx, ui_fonts);
        ctx.options_mut(|o| o.zoom_with_keyboard = false);

        if !cfg.font_size.is_finite() {
            cfg.font_size = DEFAULT_FONT;
        }
        cfg.font_size = cfg.font_size.clamp(MIN_FONT, MAX_FONT);
        cfg.ui_scale = clamp_ui_scale(cfg.ui_scale);
        ctx.set_zoom_factor(cfg.ui_scale);
        theme::set_separated(cfg.separated_panels);
        theme::apply(&ctx, cfg.style, cfg.density, cfg.ui_font, cfg.dark());

        let root = deck_core::sessions::SessionsRoot::at(profile.sessions_root.clone());
        let gw = Gw::start(
            &ctx,
            GatewayOptions::new(profile, Role::Gui, {
                let ctx = ctx.clone();
                Arc::new(move || ctx.request_repaint())
            }),
        );

        let mut startup_error = None;
        let launch_folder = match folder {
            Some(Ok(dir)) => Some(dir),
            Some(Err(e)) => {
                startup_error = Some(e);
                None
            }
            None => None,
        };
        // The command-line folder overrides the saved scope for this run.
        let scope = launch_folder.clone().or_else(|| cfg.folder_scope.clone());
        if let Some(dir) = &launch_folder {
            cfg.push_recent_folder(dir.clone());
        }
        // A folder launch restores that folder's own tabs, a plain launch the global ones. Whether
        // the gateway has a backend (without one nothing outlives the app) is known once it
        // answered: restore drops the tabs then.
        let (restore_tabs, restore_active) = match &launch_folder {
            Some(dir) => cfg
                .folder_tabs
                .get(dir)
                .map_or((Vec::new(), None), |f| (f.open_tabs.clone(), f.active_tab)),
            None => (cfg.open_tabs.clone(), cfg.active_tab),
        };
        let pending_restore = (!restore_tabs.is_empty()).then_some((restore_tabs, restore_active));
        let restore_split = pending_restore.as_ref().and_then(|_| match &launch_folder {
            Some(dir) => cfg.folder_tabs.get(dir).and_then(|f| f.split.clone()),
            None => cfg.split.clone(),
        });
        let shot = cli.screenshot.map(|path| ShotPlan {
            path,
            after: Duration::from_secs_f64(cli.screenshot_after),
            state: ShotState::Waiting,
        });
        let (events_tx, events_rx) = mpsc::channel();

        let mut app = App {
            ctx,
            cfg_base: Arc::new(cfg.clone()),
            cfg_rev: 0,
            saving: None,
            tabs_synced: TabsProj::of(&cfg, launch_folder.as_ref()),
            gw,
            mode_change: None,
            mode_error: None,
            cfg,
            root_exists: true,
            root,
            clipboard: Clipboard::new(),
            tabs: Vec::new(),
            active: 0,
            closed: ReopenStack::new(20),
            labels: HashMap::new(),
            labels_gen: None,
            controllers_rev: 0,
            filter: String::new(),
            scope,
            launch_folder,
            picking_scope: false,
            scope_gen: 0,
            start_dir,
            viewport_zoom: 1.0,
            pending_restore,
            split: Split::default(),
            restore_split,
            restore_uids: HashMap::new(),
            pane_claims: Vec::new(),
            focus_pending: true,
            terminal_focused: false,
            scroll_to_active: false,
            refresh_until: None,
            toasts: Toasts::default(),
            events_tx,
            events_rx,
            confirm: None,
            settings: SettingsState::default(),
            settings_open: false,
            update: UpdateTracker::default(),
            omp_update_rev: 0,
            omp_ready: false,
            told_old_gateway: false,
            install_wait: None,
            moving: HashSet::new(),
            sidebar_cache: SidebarCache::default(),
            folder_dialog: None,
            wt: WtUi::new(),
            home: dirs::home_dir(),
            picker_available: transfer::picker_available(),
            status: bar::StatusView::default(),
            status_uid: None,
            model_sent: None,
            bar_view: statusbar::BarView::default(),
            omp_defaults: statusbar::Defaults::default(),
            omp_usage_reserve: None,
            picker: PickerState::default(),
            models: ModelsCache::default(),
            dialog: None,
            dialog_target: None,
            next_tab_uid: 0,
            deferred_opens: Vec::new(),
            desktop_opens: Vec::new(),
            agents_target: None,
            agents_view: AgentsView::default(),
            dash: DashState::default(),
            dash_index: agents::DashIndex::default(),
            popup: None,
            tree: None,
            tree_busy: None,
            usage: Usage::new(false),
            usage_rev: 0,
            usage_open: false,
            usage_ui: usage_view::ViewState::default(),
            sessions: session_manager::SessionHub::default(),
            live: ctl::LiveFollow::default(),
            ctl_reqs: HashMap::new(),
            ctl_seq: 0,
            ctl_errors: HashMap::new(),
            spawn: spawn::Pump::default(),
            activation_seq: 0,
            restore_focus: None,
            fonts_finalized: false,
            frame_count: 0,
            frame_mark: Instant::now(),
            repaint_causes: HashMap::new(),
            dirty: false,
            last_save: Instant::now(),
            last_title: String::new(),
            last_title_at: Instant::now()
                .checked_sub(WINDOW_TITLE_EVERY)
                .unwrap_or_else(Instant::now),
            started: Instant::now(),
            shot,
            backup: backup::BackupUi::new(),
            sync: project_sync::SyncUi::new(),
            mobile: mobile::MobileUi::new(),
            changes: changes::ChangesUi::default(),
            app_update: app_update::AppUpdateUi::new(),
            notify: notifications::NotifyUi::new(&cc.egui_ctx),
            file_drops: crate::file_drop::FileDrops::new(),
            palette: None,
        };
        if let Some(e) = startup_error {
            app.toasts.error(&app.ctx, format!("Folder ignored: {e}"));
        }
        // The gateway started this window for a session or a link: show it as soon as it can.
        if let Some(file) = cli.open {
            app.desktop_opens.push(desktop::DesktopOpen::new(
                file,
                cli.split
                    .unwrap_or(deck_service::gateway::protocol::Split::None),
            ));
        }
        if let Some(url) = cli.open_url {
            app.open_external(url);
        }
        app
    }

    // ---------------------------------------------------------------- gateway

    /// Per tick: completed requests, the events only this window can interpret, and the tabs'
    /// view of the gateway's session records.
    fn pump_gateway(&mut self) {
        let pumped = self.gw.pump();
        for job in pumped.jobs {
            job(self);
        }
        for event in pumped.events {
            self.on_client_event(event);
        }
        self.sync_controllers();
    }

    /// A lookup of an accepted request returned after a reconnect (see `Gw::settle_lookup`).
    pub(crate) fn gw_lookup(
        &mut self,
        key: &deck_service::controller::RequestKey,
        r: Result<Outcome, GatewayError>,
    ) {
        if let Some(job) = self.gw.settle_lookup(key, r) {
            job(self);
        }
    }

    fn on_client_event(&mut self, event: ClientEvent) {
        match event {
            // The gateway keeps no view state across a lost connection: report it all again.
            ClientEvent::Link(link) => {
                self.notify.forget_report();
                self.model_sent = None;
                if let deck_service::gateway::client::Link::Up(w) = link {
                    // One-shot loads (models, omp settings) are asked again of the gateway linked
                    // now: one sent to a previous gateway may have died with it.
                    self.omp_ready = false;
                    let gui = deck_service::gateway::GATEWAY_VERSION;
                    if !self.told_old_gateway && deck_core::selfupdate::is_newer(gui, &w.version) {
                        self.told_old_gateway = true;
                        self.toasts.info(
                            &self.ctx,
                            format!(
                                "OmpDeck's gateway still runs {} because it was busy. Restart OmpDeck \
                                 when your sessions are idle to finish updating it to {gui}.",
                                w.version
                            ),
                        );
                    }
                }
            }
            ClientEvent::Resync => {
                self.notify.forget_report();
                self.model_sent = None;
            }
            ClientEvent::Controller { target, event } => self.on_controller_event(&target, event),
            ClientEvent::Bell(target) => self.on_bell(&target),
            ClientEvent::Activate { target, token } => self.on_activate(&target, token.as_deref()),
            ClientEvent::ModelUnconfirmed { switch, .. } => self.on_model_unconfirmed(&switch),
            ClientEvent::CopyResult { target, result } => self.on_copy_result(&target, result),
            ClientEvent::OpenSession { file, split } => self.on_open_session(file, split),
            ClientEvent::CloseSession { file } => self.on_close_session(&file),
            ClientEvent::OpenUrl { url } => self.open_external(url),
            // Consumed by `Gw` (outcomes, progress) or read from the caches (topic changes).
            ClientEvent::Changed { .. }
            | ClientEvent::Outcome { .. }
            | ClientEvent::Progress { .. } => {}
        }
    }

    /// Every terminal tab takes the gateway's record of its session. A new generation (omp
    /// moved to another file, a restart) ended the tab's view: it attaches a new one.
    fn sync_controllers(&mut self) {
        let revision = self.gw.revision(&Topic::Controllers);
        if revision == self.controllers_rev {
            return;
        }
        self.controllers_rev = revision;
        let Some(infos) = self.gw.controllers() else {
            return;
        };
        let mut reattach = Vec::new();
        for (i, t) in self.tabs.iter_mut().enumerate() {
            let Some(info) = infos.iter().find(|c| c.target.key == t.target().key) else {
                continue;
            };
            t.set_info(info);
            if t.needs_reattach() {
                reattach.push((i, info.target.clone()));
            }
        }
        for (i, target) in reattach {
            let term = self.open_view(&target);
            self.tabs[i].reattach(target, term);
            self.dirty = true;
        }
        if !infos.is_empty() {
            self.dirty = true;
        }
    }

    pub(super) fn groups(&self) -> Option<Arc<Vec<ProjectGroup>>> {
        self.gw.groups()
    }

    /// The latest snapshot of the running sessions (empty before the first one).
    pub(super) fn running_snapshot(&self) -> Arc<RunningSnapshot> {
        self.gw.running().unwrap_or_default()
    }

    pub(super) fn running_blind(&self) -> bool {
        self.gw.running_blind()
    }

    /// The gateway found omp (false until it has said).
    pub(super) fn omp_found(&self) -> bool {
        self.gw.gateway().is_some_and(|g| g.omp_found)
    }

    /// The gateway has a session backend: sessions outlive tabs and the app.
    pub(super) fn backends_available(&self) -> bool {
        self.gw.gateway().is_some_and(|g| !g.backends.is_empty())
    }

    fn host_available(&self) -> bool {
        self.gw
            .gateway()
            .is_some_and(|g| g.backends.contains(&MuxKind::Host))
    }

    /// Per frame: once omp is found, ask for its model catalog and settings; after an installer
    /// run, report whether the gateway found the new omp.
    fn tick_omp(&mut self) {
        let found = self.omp_found();
        if found && !self.omp_ready {
            self.omp_ready = true;
            self.reload_models();
            self.load_catalog();
        } else if !found && self.gw.gateway().is_some() {
            self.omp_ready = false;
        }
        if let Some(since) = self.install_wait {
            if found {
                self.install_wait = None;
                self.toasts.success(&self.ctx, "omp installed");
            } else if since.elapsed() >= INSTALL_WAIT {
                self.install_wait = None;
                self.toasts.error(
                    &self.ctx,
                    format!(
                        "The installer finished, but omp is not in {}",
                        crate::update::OMP_LOOKUP
                    ),
                );
            } else {
                self.ctx.request_repaint_after(Duration::from_millis(250));
            }
        }
    }

    /// Per frame: adopt the gateway's omp version and update state.
    fn tick_omp_update(&mut self) {
        let revision = self.gw.revision(&Topic::OmpUpdate);
        if revision == self.omp_update_rev {
            return;
        }
        self.omp_update_rev = revision;
        let Some(view) = self.gw.omp_update() else {
            return;
        };
        if let Some(done) = self.update.adopt(&view) {
            self.on_update_checked(done);
        }
    }

    fn set_gateway_mode(&mut self, mode: GatewayMode) {
        if self.mode_change.is_some() {
            return;
        }
        self.mode_error = None;
        self.mode_change = Some(self.gw.set_gateway_mode(mode));
    }

    fn poll_gateway_mode(&mut self) {
        let Some(result) = self.mode_change.as_ref().and_then(Pending::try_take) else {
            return;
        };
        self.mode_change = None;
        if let Err(e) = result {
            self.mode_error = Some(e.to_string());
        }
    }

    /// Per strip position: the tab's uid.
    fn tab_uids(&self) -> Vec<u64> {
        self.tabs.iter().map(|t| t.uid).collect()
    }

    /// Focus tab `i`; the agents dashboard (if shown) gives way to it.
    fn activate(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.close_dashboard();
        }
        self.activate_keeping_dashboard(i);
    }

    /// [`Self::activate`] for callers that keep the dashboard on screen.
    fn activate_keeping_dashboard(&mut self, i: usize) {
        if i < self.tabs.len() {
            if i != self.active
                && let Some(prev) = self.tabs.get_mut(self.active)
            {
                prev.terminal_mut().set_focused(false);
            }
            // Only a tab that is drawn has been seen; the dashboard hides every terminal.
            if !self.dash.open {
                self.tabs[i].bell = false;
            }
            self.active = i;
            if let Some(uid) = self.tabs[i].pane_uid() {
                self.split.show(uid);
            }
            self.activation_seq += 1;
            self.focus_pending = true;
            self.scroll_to_active = true;
            self.dirty = true;
        }
    }

    fn open_session(&mut self, entry: &SessionEntry) {
        let view = self
            .gw
            .controllers()
            .and_then(|infos| {
                infos
                    .iter()
                    .find(|i| {
                        i.exit_code.is_none()
                            && i.session_path.as_deref() == Some(entry.path.as_path())
                    })
                    .map(|i| i.view)
            })
            .unwrap_or(self.cfg.session_view);
        self.open_session_view(entry, view);
    }

    fn open_session_view(&mut self, entry: &SessionEntry, view: SessionView) {
        // A removed worktree's folder is gone: its session is moved into the primary checkout
        // first (unless a tab already shows it), then opened there.
        let primary = self
            .cfg
            .removed_worktrees
            .get(&entry.cwd)
            .map(|r| r.primary.clone());
        if let Some(primary) = primary
            && !self
                .tabs
                .iter()
                .any(|t| t.session_path() == Some(entry.path.as_path()))
        {
            self.open_relocated(entry, primary, view);
            return;
        }
        self.open_entry_view(entry, entry.path.clone(), entry.cwd.clone(), view);
    }

    fn open_entry_view(
        &mut self,
        entry: &SessionEntry,
        path: PathBuf,
        cwd: PathBuf,
        view: SessionView,
    ) {
        self.open_resolved(ClosedTab {
            mux_name: Some(session_name_for(&entry.id)),
            cwd,
            session_path: Some(path),
            title: entry.label().to_owned(),
            view,
        });
    }

    /// Focus the tab already showing `req.session_path`, else launch the session that holds that
    /// file (or a unique name when the preferred one now runs another file). Before the first
    /// snapshot of the gateway's backends exists the request waits in `deferred_opens`.
    fn open_resolved(&mut self, req: ClosedTab) {
        self.open_resolved_focus(req, Focus::Claim);
    }

    /// [`open_resolved`](Self::open_resolved) with the focus rule of the new tab (an existing tab
    /// is always activated; a deferred open claims focus when it finally runs).
    fn open_resolved_focus(&mut self, req: ClosedTab, focus: Focus) {
        let Some(snap) = self.gw.running() else {
            self.deferred_opens.push(req);
            self.ctx.request_repaint();
            return;
        };
        let path = req.session_path.as_deref();
        let holds = |name: &str| path.is_some_and(|p| snap.files_of(name).iter().any(|f| f == p));
        let existing = path.and_then(|p| {
            self.tabs
                .iter()
                .position(|t| t.session_path() == Some(p) || t.mux_name().is_some_and(holds))
        });
        if let Some(i) = existing {
            let t = &mut self.tabs[i];
            if t.session_path().is_none()
                && let Some(p) = &req.session_path
            {
                // The tab runs this session but did not know its file yet: tell the gateway too.
                t.hint_session_path(Some(p.clone()));
                let target = t.target().clone();
                self.gw.mutate(
                    Mutation::SetSessionPath {
                        target,
                        path: Some(p.clone()),
                    },
                    |_, result| {
                        if let Err(e) = result {
                            log::debug!("setting the session file failed: {e}");
                        }
                    },
                );
            }
            self.activate(i);
            return;
        }
        if self.spawn_queued_for(req.session_path.as_deref(), None) {
            return; // already on its way
        }
        let name = req
            .mux_name
            .filter(|_| self.backends_available())
            .map(|preferred| {
                snap.resolve_for(
                    self.running_blind(),
                    &preferred,
                    req.session_path.as_deref(),
                    unix_ms(),
                )
                .into_name()
            });
        self.spawn_tab_focus(name, req.cwd, req.session_path, req.title, req.view, focus);
    }

    fn open_deferred(&mut self) {
        if self.pending_restore.is_some() || self.gw.running().is_none() {
            return;
        }
        for req in std::mem::take(&mut self.deferred_opens) {
            self.open_resolved(req);
        }
    }

    /// Queue a session tab. `mux_name` is `None` without a backend.
    fn spawn_tab(
        &mut self,
        mux_name: Option<String>,
        cwd: PathBuf,
        session_path: Option<PathBuf>,
        title: String,
    ) {
        let view = self.cfg.session_view;
        self.spawn_tab_focus(mux_name, cwd, session_path, title, view, Focus::Claim);
    }

    fn spawn_tab_focus(
        &mut self,
        mux_name: Option<String>,
        cwd: PathBuf,
        session_path: Option<PathBuf>,
        title: String,
        view: SessionView,
        focus: Focus,
    ) {
        self.spawn_tab_post(SpawnReq::session(
            mux_name,
            cwd,
            session_path,
            title,
            view,
            Post::None,
            focus,
        ));
    }

    /// Run omp's official installer in a tool tab; one at a time (re-focus a running one).
    fn open_install_tab(&mut self) {
        let running = self
            .tabs
            .iter()
            .position(|t| t.role == TabRole::InstallOmp && !t.terminal().is_exited());
        if let Some(i) = running {
            self.activate(i);
            return;
        }
        let cwd = self.home.clone().unwrap_or_else(platform::home_or_root);
        self.spawn_tab_post(SpawnReq::tool(Tool::Install, cwd));
    }

    /// The installer finished: the gateway looks for the new omp itself; report what it finds.
    fn finish_omp_install(&mut self, code: Option<i32>) {
        if code != Some(0) {
            let code = code.map_or("signal".to_owned(), |c| c.to_string());
            self.toasts.error(
                &self.ctx,
                format!("Installing omp failed (exit {code}) — see the tab for details"),
            );
            return;
        }
        self.install_wait = Some(Instant::now());
    }

    fn new_session(&mut self, cwd: PathBuf) {
        let name = self.new_mux_name();
        self.spawn_tab(name, cwd, None, String::new());
    }

    /// A fresh backend session name, when the gateway has a backend for new sessions.
    fn new_mux_name(&self) -> Option<String> {
        self.gw
            .gateway()
            .is_some_and(|g| g.primary_backend.is_some())
            .then(|| new_session_name(SystemTime::now()))
    }

    /// Open the Settings window (it covers the app; the tabs stay as they are).
    fn open_settings(&mut self) {
        if !self.settings_open {
            self.settings_open = true;
            self.settings.opened();
        }
    }

    fn close_settings(&mut self) {
        self.settings_open = false;
        self.focus_pending = true;
    }

    /// Focus the settings page on one OmpDeck subpage.
    fn open_settings_page(&mut self, page: settings_page::OmpDeckPage) {
        self.open_settings();
        self.settings.show_ompdeck_page(page);
    }

    fn close_tab(&mut self, i: usize, remember: bool) {
        if i >= self.tabs.len() {
            return;
        }
        let len_before = self.tabs.len();
        let tab = self.tabs.remove(i); // dropping the Terminal detaches; tmux keeps running
        let focused_pane = self.split.focused_uid();
        let pane_uid = tab.pane_uid().filter(|u| self.split.slot_of(*u).is_some());
        if remember && tab.role == TabRole::Session {
            let t = &tab;
            self.closed.push(ClosedTab {
                mux_name: t.mux_name.clone(),
                cwd: t.cwd.clone(),
                session_path: t.session_path_owned(),
                title: t.title(&self.labels).to_owned(),
                view: t.view(),
            });
        }
        let launch = (tab.session_path_owned(), tab.mux_name.clone());
        let t = &tab;
        // Detach this window's view; the gateway keeps backend sessions (and accepted work)
        // running, and ends a direct program only because nothing else can reach it.
        self.gw.mutate(
            Mutation::Close {
                target: t.target().clone(),
                kill_direct: t.mux_name.is_none(),
            },
            |_, result| {
                if let Err(e) = result {
                    log::debug!("closing the view failed: {e}");
                }
            },
        );
        drop(tab);
        self.active = tabs::active_after_close(self.active, i, len_before);
        if let Some(uid) = pane_uid {
            if remember {
                // The pane goes with its tab; focus moves to the pane that remains.
                self.split.remove_uid(uid);
                if focused_pane == Some(uid)
                    && let Some(next) = self.split.focused_uid().and_then(|u| self.tab_index(u))
                {
                    self.active = next;
                }
            } else if self.split.is_split() {
                // A restart or move follows: the replacement takes exactly this pane.
                if let Some(ticket) = self.split.reserve_slot_of(uid) {
                    self.pane_claims.push(PaneClaim {
                        session_path: launch.0,
                        mux_name: launch.1,
                        ticket,
                    });
                }
            } else {
                self.split.vacate(uid);
            }
        }
        self.activation_seq += 1;
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.bell = false;
        }
        self.focus_pending = true;
        self.scroll_to_active = true;
        self.dirty = true;
    }

    fn reopen_closed(&mut self) {
        let Some(closed) = self.closed.pop() else {
            return;
        };
        self.open_resolved(closed);
    }

    /// Kill the backend session `name` (the sidebar's "Stop session"); the outcome arrives as a
    /// toast and, once it is stopped, the tabs showing it close.
    fn kill_session(&mut self, name: String, label: String) {
        self.gw.mutate(
            Mutation::KillNames {
                names: vec![name.clone()],
                wait: false,
            },
            move |app, result| match result {
                Ok(Done::Killed(failed)) if !failed.is_empty() => {
                    let why: Vec<&str> = failed.iter().map(|(_, e)| e.as_str()).collect();
                    app.toasts.error(
                        &app.ctx,
                        format!("Could not stop “{label}”: {}", why.join("; ")),
                    );
                }
                Ok(_) => {
                    app.close_tabs_where(|t| t.mux_name() == Some(name.as_str()));
                    app.toasts.success(&app.ctx, format!("Stopped “{label}”"));
                }
                Err(e) => app
                    .toasts
                    .error(&app.ctx, format!("Could not stop “{label}”: {e}")),
            },
        );
        self.refresh_sessions();
    }

    /// Kill the process of tab `i` — only that tab's session — and close the tab once it is
    /// stopped.
    fn kill_tab(&mut self, i: usize) {
        let Some(tab) = self.tabs.get(i) else {
            return;
        };
        let target = tab.target().clone();
        let key = target.key.clone();
        let label = tab.title(&self.labels).to_owned();
        let announce = tab.mux_name.is_some();
        self.gw
            .mutate(Mutation::Kill(target), move |app, result| match result {
                Ok(_) => {
                    app.close_tabs_where(|t| t.target().key == key);
                    if announce {
                        app.toasts.success(&app.ctx, format!("Stopped “{label}”"));
                    }
                }
                Err(e) => app
                    .toasts
                    .error(&app.ctx, format!("Could not stop “{label}”: {e}")),
            });
    }

    /// Close (remembered for Ctrl+Shift+T) every tab `hit` selects: a process the user stopped
    /// on purpose takes its tabs along instead of leaving them on `[process exited]`.
    fn close_tabs_where(&mut self, hit: impl Fn(&Tab) -> bool) {
        for i in (0..self.tabs.len()).rev() {
            if hit(&self.tabs[i]) {
                self.close_tab(i, true);
            }
        }
    }

    fn apply_tab_action(&mut self, action: TabAction) {
        match action {
            TabAction::Activate(i) => self.activate(i),
            TabAction::Close(i) => self.request_close(i),
            TabAction::Menu(a) => self.apply_sidebar_action(a),
        }
    }

    /// Re-open tabs from the previous run once the first backend snapshot is available:
    /// alive session → re-attach; else resume its session file; else drop.
    fn restore_tabs(&mut self, snap: &RunningSnapshot) {
        let Some((states, active)) = self.pending_restore.take() else {
            return;
        };
        if !self.backends_available() {
            return; // without a backend nothing outlives the app: the stored tabs stay as they are
        }
        let blind = self.running_blind();
        for (i, st) in states.iter().enumerate() {
            if self
                .tabs
                .iter()
                .any(|t| t.mux_name() == Some(st.session_name.as_str()))
            {
                continue;
            }
            let path = st.session_path.clone().filter(|p| p.is_file());
            if !snap.is_alive(&st.session_name) && path.is_none() {
                continue;
            }
            let name = snap
                .resolve_for(blind, &st.session_name, path.as_deref(), unix_ms())
                .into_name();
            self.spawn_tab_post(SpawnReq::session(
                Some(name),
                st.cwd.clone(),
                path,
                st.title.clone(),
                st.view,
                Post::Restore {
                    active: Some(i) == active,
                    index: i,
                },
                Focus::Always,
            ));
        }
        self.queue_restore_done();
    }

    fn refresh_sessions_cache(&mut self) {
        let revision = self.gw.revision(&Topic::Sessions);
        if self.labels_gen == Some(revision) {
            return;
        }
        self.labels_gen = Some(revision);
        self.root_exists = self.gw.sessions_root_exists();
        self.labels.clear();
        if let Some(groups) = self.gw.groups() {
            for g in groups.iter() {
                for e in &g.sessions {
                    self.labels.insert(e.path.clone(), e.label().to_owned());
                }
            }
        }
    }

    // ---------------------------------------------------------------- settings & actions

    fn set_font_size(&mut self, size: f32) {
        let size = size.clamp(MIN_FONT, MAX_FONT);
        if (size - self.cfg.font_size).abs() > f32::EPSILON {
            self.cfg.font_size = size;
            self.dirty = true;
        }
    }

    /// App-wide scale (egui zoom, 1.0 = 100 %): text, icons and fixed chrome all scale together.
    /// The terminal compensates via [`Self::terminal_font_size`], so its size stays absolute.
    fn set_ui_scale(&mut self, scale: f32) {
        let scale = clamp_ui_scale((scale * 100.0).round() / 100.0);
        if (scale - self.cfg.ui_scale).abs() > f32::EPSILON {
            self.cfg.ui_scale = scale;
            self.ctx.set_zoom_factor(scale);
            self.dirty = true;
        }
    }

    /// Points handed to the terminal view: the chosen terminal font size compensated for the app
    /// scale, so the grid's on-screen size depends only on "Terminal font size". The scale egui
    /// renders this pass with is used, not [`AppConfig::ui_scale`], so the frame in which a new
    /// scale was set does not compute metrics against the old `pixels_per_point` (a spurious
    /// resize round trip).
    fn terminal_font_size(&self) -> f32 {
        self.cfg.font_size / self.ctx.zoom_factor()
    }

    /// Re-applies theme × style × density × UI font to egui and every open terminal.
    fn restyle(&mut self) {
        theme::set_separated(self.cfg.separated_panels);
        theme::apply(
            &self.ctx,
            self.cfg.style,
            self.cfg.density,
            self.cfg.ui_font,
            self.cfg.dark(),
        );
        let tt = deck_service::gateway::terminal_theme(self.cfg.theme, self.cfg.style);
        for tab in self.tabs.iter_mut() {
            tab.terminal_mut().set_theme(tt);
        }
        self.dirty = true;
    }

    fn set_theme(&mut self, theme: Theme) {
        if self.cfg.theme != theme {
            self.cfg.theme = theme;
            self.restyle();
        }
    }

    fn set_style(&mut self, style: UiStyle) {
        if self.cfg.style != style {
            self.cfg.style = style;
            self.restyle();
        }
    }

    /// Where new sessions go (the gateway reads the choice from the settings). Running sessions
    /// stay where they are.
    fn set_session_backend(&mut self, choice: BackendChoice) {
        if self.cfg.session_backend != choice {
            self.cfg.session_backend = choice;
            self.dirty = true;
        }
    }

    fn set_density(&mut self, density: Density) {
        if self.cfg.density != density {
            self.cfg.density = density;
            self.restyle();
        }
    }

    fn set_separated_panels(&mut self, on: bool) {
        if self.cfg.separated_panels != on {
            self.cfg.separated_panels = on;
            self.restyle();
        }
    }

    fn set_ui_font(&mut self, font: UiFont) {
        if self.cfg.ui_font != font {
            self.cfg.ui_font = font;
            self.restyle();
        }
    }

    fn set_native_decorations(&mut self, native: bool) {
        if self.cfg.native_decorations != native {
            self.cfg.native_decorations = native;
            self.dirty = true;
            self.toasts.with_action(
                &self.ctx,
                ToastKind::Info,
                "Title bar style changes after restarting OmpDeck.",
                ToastAction {
                    label: "Quit now".to_owned(),
                    id: TOAST_QUIT,
                },
            );
        }
    }

    /// Switch the sidebar between all projects (`None`) and one folder. Tabs stay open.
    fn set_scope(&mut self, dir: Option<PathBuf>) {
        if self.scope == dir {
            return;
        }
        if let Some(d) = &dir {
            self.cfg.push_recent_folder(d.clone());
        }
        self.cfg.folder_scope = dir.clone();
        self.scope = dir;
        self.sidebar_cache.scope_ui.error = None;
        self.dirty = true;
    }

    /// Ask the desktop's folder dialog for the scope (native worker; the UI never waits).
    fn browse_scope(&mut self) {
        if self.picking_scope {
            return;
        }
        self.picking_scope = true;
        self.scope_gen += 1;
        let generation = self.scope_gen;
        let start = self
            .scope
            .clone()
            .or_else(|| self.home.clone())
            .unwrap_or_else(deck_core::platform::home_or_root);
        let (home, base) = (self.home.clone(), start.clone());
        self.gw.native(
            move || {
                transfer::pick_folder(&start)
                    .map(|p| folder_scope::resolve(&p.to_string_lossy(), home.as_deref(), &base))
            },
            move |app, result| app.on_scope_chosen(generation, false, result),
        );
    }

    /// Check a typed scope path (existence, symlinks) on a native worker. Relative paths are
    /// relative to the folder OmpDeck was started in.
    fn resolve_scope(&mut self, text: String) {
        self.scope_gen += 1;
        let generation = self.scope_gen;
        let home = self.home.clone();
        let base = self.start_dir.clone();
        self.gw.native(
            move || Some(folder_scope::resolve(&text, home.as_deref(), &base)),
            move |app, result| app.on_scope_chosen(generation, true, result),
        );
    }

    fn on_scope_chosen(
        &mut self,
        generation: u64,
        typed: bool,
        result: Option<Result<PathBuf, String>>,
    ) {
        if !typed {
            self.picking_scope = false;
        }
        // A newer request supersedes this answer.
        if generation != self.scope_gen {
            return;
        }
        match result {
            None => {}
            Some(Ok(dir)) => {
                if typed {
                    self.sidebar_cache.scope_ui.clear_typed();
                }
                self.set_scope(Some(dir));
                self.focus_pending = true;
            }
            Some(Err(e)) if typed => self.sidebar_cache.scope_ui.error = Some(e),
            Some(Err(e)) => self.toasts.error(&self.ctx, e),
        }
    }

    fn refresh_sessions(&mut self) {
        self.gw.mutate(Mutation::RefreshSessions, |_, result| {
            if let Err(e) = result {
                log::debug!("the session refresh was refused: {e}");
            }
        });
        self.refresh_until = Some(Instant::now() + REFRESH_SPIN);
    }

    fn copy_text(&mut self, text: &str, what: &str) {
        self.clipboard.set_text(text);
        self.toasts.success(&self.ctx, format!("Copied {what}"));
    }

    /// Open a link with the desktop's handler off the UI thread; a failure arrives as an error
    /// toast.
    fn open_external(&self, url: String) {
        if !is_openable_url(&url) {
            log::warn!("refusing to open url with disallowed scheme: {url}");
            return;
        }
        let (tx, ctx) = (self.events_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            if let Err(e) = platform::open_path(url.as_ref()) {
                log::warn!("open {url}: {e}");
                let _ = tx.send(AppEvent::Toast(
                    ToastKind::Error,
                    format!("Could not open the link: {e}"),
                ));
                ctx.request_repaint();
            }
        });
    }

    /// Chat nodes may activate local files; ordinary terminal URLs retain their scheme policy.
    fn open_chat_link(&self, url: String) {
        let Some(path) = chat_file_path(&url) else {
            self.open_external(url);
            return;
        };
        let (tx, ctx) = (self.events_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            if let Err(e) = platform::open_path(path.as_os_str()) {
                let _ = tx.send(AppEvent::Toast(
                    ToastKind::Error,
                    format!("Could not open the file: {e}"),
                ));
                ctx.request_repaint();
            }
        });
    }

    /// Open `dir` in the desktop file manager, entirely off the UI thread.
    fn open_folder(&self, dir: PathBuf) {
        let (tx, ctx) = (self.events_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let outcome = if !dir.is_absolute() || !dir.is_dir() {
                Err(format!("Folder not found: {}", dir.display()))
            } else {
                platform::open_path(dir.as_os_str())
                    .map_err(|e| format!("Could not open the folder: {e}"))
            };
            if let Err(msg) = outcome {
                let _ = tx.send(AppEvent::Toast(ToastKind::Error, msg));
                ctx.request_repaint();
            }
        });
    }

    /// Session currently running `entry`: the holder of its file, a live tab on it, else the
    /// canonical name.
    fn running_session_name(&self, entry: &SessionEntry) -> Option<String> {
        let snap = self.gw.running();
        let holder = snap
            .as_ref()
            .and_then(|s| s.holder(&entry.path).map(str::to_owned));
        let tab = || {
            self.tabs.iter().find_map(|t| {
                (t.session_path() == Some(entry.path.as_path()) && !t.terminal().is_exited())
                    .then(|| t.mux_name.clone())
                    .flatten()
            })
        };
        holder.or_else(tab).or_else(|| {
            let canonical = session_name_for(&entry.id);
            // Alive but holding another file (after /new, /fork) is not this session.
            snap.is_some_and(|s| s.is_alive(&canonical) && s.files_of(&canonical).is_empty())
                .then_some(canonical)
        })
    }

    /// Ask before stopping the session that runs `entry`.
    fn request_stop(&mut self, entry: &SessionEntry) {
        let Some(mux_name) = self.running_session_name(entry) else {
            self.toasts
                .info(&self.ctx, "That session is not running anymore.");
            return;
        };
        self.confirm_stop(mux_name, entry.label().to_owned());
    }

    /// Ask before stopping the live session `mux_name`, called `label` in the question.
    fn confirm_stop(&mut self, mux_name: String, label: String) {
        self.confirm = Some((
            Confirm {
                title: "Stop session process?".to_owned(),
                body: format!(
                    "“{label}” will be terminated and its open tab closes. Its session file stays in the sidebar, so you can resume it later."
                ),
                confirm_label: "Stop session".to_owned(),
                danger: true,
            },
            PendingConfirm::StopSession { mux_name, label },
        ));
    }

    /// Show the live session `name` that has no listed session file: focus its tab, else open a
    /// tab that attaches to it (the launch never starts omp for an alive name).
    fn attach_running(&mut self, name: String, title: String) {
        if let Some(i) = self
            .tabs
            .iter()
            .position(|t| t.mux_name() == Some(name.as_str()))
        {
            self.activate(i);
            return;
        }
        let cwd = self.home.clone().unwrap_or_else(platform::home_or_root);
        self.open_resolved(ClosedTab {
            mux_name: Some(name),
            cwd,
            session_path: None,
            title,
            view: self.cfg.session_view,
        });
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let ctrl_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        self.palette_shortcuts(ctx);
        self.split_shortcuts(ctx);
        let session_tab = self.active_session_tab();
        // Most specific first (egui ignores extra Shift/Alt when matching).
        let tree_key = (session_tab || self.tree.is_some()) && self.tree_shortcut(ctx);
        let usage_key = self.usage_shortcut(ctx);
        let sessions_key = self.sessions_shortcut(ctx);
        let changes_key =
            self.changes_available() && ctx.input_mut(|i| i.consume_key(ctrl_shift, Key::G));
        let (reopen, prev, next, bigger, smaller, reset, dashboard) = ctx.input_mut(|i| {
            (
                i.consume_key(ctrl_shift, Key::T),
                i.consume_key(Modifiers::COMMAND, Key::PageUp),
                i.consume_key(Modifiers::COMMAND, Key::PageDown),
                i.consume_key(ctrl_shift, Key::Plus) | i.consume_key(ctrl_shift, Key::Equals),
                i.consume_key(ctrl_shift, Key::Minus),
                i.consume_key(ctrl_shift, Key::Num0),
                i.consume_key(ctrl_shift, Key::A),
            )
        });
        if self.consume_popup_escape(ctx) {
            self.close_agent_popup();
        }
        if self.consume_dashboard_escape(ctx) {
            self.close_dashboard();
        } else if dashboard {
            self.toggle_dashboard(false);
        }
        if changes_key {
            self.toggle_changes_panel();
        }
        if tree_key {
            self.toggle_tree();
        }
        if usage_key {
            self.toggle_usage();
        }
        if sessions_key {
            self.toggle_sessions();
        }
        if reopen {
            self.reopen_closed();
        }
        // A modal question belongs to the tab it was asked about: switching away is blocked.
        let modal = self.modal_open();
        if prev && !modal && !self.tabs.is_empty() {
            self.activate(tabs::wrap_index(self.active, self.tabs.len(), -1));
        }
        if next && !modal && !self.tabs.is_empty() {
            self.activate(tabs::wrap_index(self.active, self.tabs.len(), 1));
        }
        if bigger {
            self.set_font_size(self.cfg.font_size + 1.0);
        }
        if smaller {
            self.set_font_size(self.cfg.font_size - 1.0);
        }
        if reset {
            self.set_font_size(DEFAULT_FONT);
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.events_rx.try_recv() {
            let AppEvent::Toast(kind, text) = event;
            self.toasts.notify(ctx, kind, text);
        }
    }

    // ---------------------------------------------------------------- panels

    fn show_bar(&mut self, ui: &mut egui::Ui, maximized: bool) {
        let active = self.tabs.get(self.active);
        let groups = self.groups();
        let inbox = self.gw.inbox();
        let omp_found = self.omp_found();
        let live_tabs: Vec<&Path> = self
            .tabs
            .iter()
            .filter(|t| !t.terminal().is_exited())
            .filter_map(|t| t.session_path())
            .collect();
        let place = self.backup_place();
        let input = BarInput {
            tabs: TabBarInput {
                tabs: &self.tabs,
                labels: &self.labels,
                active: self.active,
                scroll_to_active: self.scroll_to_active,
                modal_open: self.modal_open(),
                dashboard: self.dash.open,
                shown: if self.split_shown() {
                    self.split.slots()
                } else {
                    &[]
                },
                menu: MenuEnv {
                    groups: groups.as_deref().map_or(&[][..], Vec::as_slice),
                    omp_available: omp_found,
                    can_stop: self.backends_available(),
                    default_view: self.cfg.session_view,
                    host_available: self.host_available(),
                    tabs: &self.tabs,
                    busy: &self.moving,
                    backup: crate::sidebar::BackupMenu {
                        place: &place,
                        off: self.backup_menu_off(),
                    },
                    live_tabs: &live_tabs,
                    finishing: &self.wt.finishing,
                    parked: crate::worktree_ui::ParkView {
                        records: &self.cfg.parked_branches,
                        states: &self.wt.park_states,
                        parkable: &self.wt.parkable,
                    },
                },
            },
            csd: !self.cfg.native_decorations,
            maximized,
            settings_active: self.settings_open,
            can_reopen: !self.closed.is_empty(),
            font_size: self.cfg.font_size,
            theme: if self.cfg.dark() {
                Theme::Dark
            } else {
                Theme::Light
            },
            dark_only: theme::dark_only_reason(self.cfg.style),
            native_decorations: self.cfg.native_decorations,
            strip: titlebar::StripInput {
                title: active
                    .map(|t| titlebar::strip_omp_prefix(t.title(&self.labels)))
                    .unwrap_or_default(),
                cwd: active.map(|t| t.cwd.as_path()),
                session: active.and_then(Tab::session_path),
            },
            omp: omp_found.then(|| OmpBar {
                version: self.update.version.as_deref(),
                state: &self.update.state,
                available: self.update.available(),
                channel: self.update.channel(),
            }),
            app_update: self.app_update_bar(),
            pairing: self.pairing_pill(),
            notifications: crate::notify_panel::PanelInput {
                entries: inbox.as_deref().map_or(&[][..], |v| v.entries.as_slice()),
                badge: inbox.as_deref().and_then(|v| v.badge.as_deref()),
                warn: inbox.as_deref().is_some_and(|v| v.warns),
                now: std::time::SystemTime::now(),
            },
        };
        let action = titlebar::show(ui, &input);
        self.scroll_to_active = false;
        let Some(action) = action else {
            return;
        };
        self.focus_pending = true;
        match action {
            BarAction::Tab(a) => self.apply_tab_action(a),
            BarAction::OpenSettings => self.open_settings(),
            BarAction::Reopen => self.reopen_closed(),
            BarAction::Refresh => self.refresh_sessions(),
            BarAction::FontLarger => self.set_font_size(self.cfg.font_size + 1.0),
            BarAction::FontSmaller => self.set_font_size(self.cfg.font_size - 1.0),
            BarAction::OpenSessions => self.open_sessions(None),
            BarAction::OpenAgents => self.toggle_dashboard(false),
            BarAction::FontReset => self.set_font_size(DEFAULT_FONT),
            BarAction::SetTheme(t) => self.set_theme(t),
            BarAction::SetNativeDecorations(v) => self.set_native_decorations(v),
            BarAction::Quit => self.ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            BarAction::RunUpdate(kind) => self.open_update_tab(kind),
            BarAction::SwitchChannel(kind) => self.confirm_channel_switch(kind),
            BarAction::CheckUpdates => self.check_updates_manually(),
            BarAction::AppUpdate(a) => self.apply_app_update_action(a),
            BarAction::Inbox(a) => self.apply_inbox_action(a),
            BarAction::ShowPairing => self.mobile.restore(),
        }
    }

    fn show_banners(&mut self, ui: &mut egui::Ui) {
        let pal = Palette::of_ui(ui);
        let link = match self.gw.link_view() {
            LinkView::Up => None,
            LinkView::Connecting => Some("Connecting to the OmpDeck gateway…".to_owned()),
            LinkView::Offline(why) => Some(format!(
                "The OmpDeck gateway is not reachable ({why}). Showing the last known state, read-only; reconnecting…"
            )),
            LinkView::Refused(why) => Some(format!(
                "The running OmpDeck gateway refused this window: {why}"
            )),
            LinkView::Maintenance => Some(
                "OmpDeck is being updated or removed: the gateway is draining and everything is read-only."
                    .to_owned(),
            ),
        };
        let backend = self.gw.gateway().and_then(|g| g.backend_banner.clone());
        for (id, text) in [("link-banner", link), ("backend-banner", backend)] {
            let Some(text) = text else {
                continue;
            };
            egui::Panel::top(id)
                .show_separator_line(false)
                .frame(
                    Frame::NONE
                        .fill(theme::mix(pal.bg, pal.warn, 0.14))
                        .stroke(egui::Stroke::new(1.0, pal.warn.gamma_multiply(0.30)))
                        .inner_margin(Margin::symmetric(16, 8)),
                )
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        ui.label(icon_text(ICON_TRIANGLE_ALERT, 18.0, pal.warn));
                        ui.label(RichText::new(text.as_str()).color(pal.text_strong));
                    });
                });
        }
    }

    fn show_sidebar(&mut self, ui: &mut egui::Ui, snap: &RunningSnapshot) {
        let pal = Palette::of_ui(ui);
        let refreshing = self.refresh_until.is_some_and(|t| Instant::now() < t);
        if refreshing {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
        let groups = self.groups();
        let groups_ref: Option<&[ProjectGroup]> = groups.as_deref().map(Vec::as_slice);
        let active_path = self.tabs.get(self.active).and_then(Tab::session_path);
        // A session open in a live tab counts as running even before tmux's fd mapping shows it.
        let open_paths: Vec<&Path> = self
            .tabs
            .iter()
            .filter(|t| !t.terminal().is_exited())
            .filter_map(Tab::session_path)
            .collect();
        // Their rows show the tab's live omp state instead of the running LED.
        let tab_activity: HashMap<&Path, TabStatus> = self
            .tabs
            .iter()
            .filter_map(|t| Some((t.session_path()?, t.status(false))))
            .filter(|(_, s)| matches!(s, TabStatus::Busy | TabStatus::Attention))
            .collect();
        let generation = self.gw.sessions_generation();
        let place = self.backup_place();
        let input = SidebarInput {
            groups: groups_ref,
            running: snap,
            root: &self.root,
            root_exists: self.root_exists,
            active_path,
            active_cwd: self.tabs.get(self.active).map(|t| t.cwd.as_path()),
            now: SystemTime::now(),
            refreshing,
            can_stop: self.backends_available(),
            default_view: self.cfg.session_view,
            host_available: self.host_available(),
            omp_available: self.omp_found(),
            busy: &self.moving,
            generation,
            open_paths: &open_paths,
            scope: self.scope.as_deref(),
            recent: &self.cfg.recent_folders,
            picker_available: self.picker_available,
            picking: self.picking_scope,
            tabs: &self.tabs,
            labels: &self.labels,
            active_tab: self.active,
            tab_activity: &tab_activity,
            backup: crate::sidebar::BackupMenu {
                place: &place,
                off: self.backup_menu_off(),
            },
            finishing: &self.wt.finishing,
            removed: &self.cfg.removed_worktrees,
            parked: crate::worktree_ui::ParkView {
                records: &self.cfg.parked_branches,
                states: &self.wt.park_states,
                parkable: &self.wt.parkable,
            },
        };
        let margin = if theme::compact() {
            Margin::symmetric(8, 8)
        } else {
            Margin::symmetric(10, 12)
        };
        let panel = egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(self.cfg.sidebar_width)
            .size_range(220.0..=600.0)
            .show_separator_line(theme::skin::panel_separator_line())
            .frame(theme::skin::sidebar_frame(pal, margin))
            .show(ui, |ui| {
                sidebar::show(ui, &input, &mut self.filter, &mut self.sidebar_cache)
            });
        if theme::look().region == theme::Region::Fade {
            theme::skin::paint_edge(
                ui.painter(),
                panel.response.rect,
                theme::skin::Edge::Right,
                pal,
            );
        }
        let width = panel.response.rect.width();
        if (width - self.cfg.sidebar_width).abs() > 1.0 {
            self.cfg.sidebar_width = width;
            self.dirty = true;
        }
        let out = panel.inner;
        if out.interacted {
            self.focus_pending = true;
        }
        for action in out.actions {
            self.apply_sidebar_action(action);
        }
    }

    fn apply_sidebar_action(&mut self, action: SidebarAction) {
        match action {
            SidebarAction::Open(entry) => self.open_session(&entry),
            SidebarAction::OpenView(entry, view) => self.open_session_view(&entry, view),
            SidebarAction::SwitchView { uid, view } => self.request_switch_view(uid, view),
            SidebarAction::NewView(cwd, view) => {
                let name = self.new_mux_name();
                self.spawn_tab_focus(name, cwd, None, String::new(), view, Focus::Claim);
            }
            SidebarAction::New(cwd) => self.new_session(cwd),
            SidebarAction::Refresh => self.refresh_sessions(),
            SidebarAction::ManageSessions(scope) => self.open_sessions(scope),
            SidebarAction::CopyText { text, what } => self.copy_text(&text, what),
            SidebarAction::OpenFolder(dir) => self.open_folder(dir),
            SidebarAction::Stop(entry) => self.request_stop(&entry),
            SidebarAction::AttachRunning { name, title } => self.attach_running(name, title),
            SidebarAction::StopRunning { name, label } => self.confirm_stop(name, label),
            SidebarAction::Restart(entry) => self.request_restart_session(&entry),
            SidebarAction::Delete(entry) => self.request_delete(entry),
            SidebarAction::Tree(entry) => {
                let title = entry.label().to_owned();
                self.open_tree(entry.path, entry.cwd, title, Some(entry.id));
            }
            SidebarAction::Transfer { kind, entry, dest } => {
                self.start_transfer(kind, entry, dest);
            }
            SidebarAction::SetScope(dir) => self.set_scope(dir),
            SidebarAction::BrowseScope => self.browse_scope(),
            SidebarAction::TypedScope(text) => self.resolve_scope(text),
            SidebarAction::ActivateTab(i) => self.activate(i),
            SidebarAction::CloseTab(i) => self.request_close(i),
            SidebarAction::KillTab(i) => self.kill_tab(i),
            SidebarAction::SplitTab { index, side } => self.split_tab(index, side),
            SidebarAction::SplitOpen { entry, side } => self.split_session(&entry, side),
            SidebarAction::BackupProject(dir) => self.open_backup_dialog(dir),
            SidebarAction::RestoreProject(dir) => self.open_restore(Some(dir)),
            SidebarAction::NewWorktree(dir) => self.open_new_worktree(dir),
            SidebarAction::MoveToWorktree(entry) => self.open_move_to_worktree(&entry),
            SidebarAction::FinishWorktree(wt) => self.request_finish(wt),
            SidebarAction::CancelFinish(path) => self.cancel_finish(&path),
            SidebarAction::RemoveWorktree(wt) => self.request_remove(wt),
            SidebarAction::ContinueInWorktree(entry) => self.continue_in_worktree(entry),
            SidebarAction::ContinueInMain(entry) => self.continue_in_main(entry),
            SidebarAction::SwitchMainBack(entry) => self.switch_main_back(entry),
        }
    }

    fn show_central(&mut self, ui: &mut egui::Ui) {
        if self.dash.open {
            self.show_dashboard(ui);
            return;
        }
        let pal = Palette::of_ui(ui);
        match self.tabs.get(self.active) {
            None => {
                self.terminal_focused = false;
                // Nothing is claimed missing before the gateway has said what it found.
                let omp_missing = self.gw.gateway().is_some_and(|g| !g.omp_found);
                let install = theme::skin::central(ui, pal, false, |ui| {
                        theme::skin::paint_canvas(ui.painter(), ui.max_rect(), pal);
                        ui.add_space((ui.available_height() / 2.0 - 90.0).max(20.0));
                        if !omp_missing {
                            empty_state(
                                ui,
                                ICON_TERMINAL,
                                pal.accent,
                                "No session open",
                                "Pick a session from the sidebar, or press + next to a project to start a new one.",
                            );
                            return false;
                        }
                        empty_state(
                            ui,
                            ICON_TERMINAL,
                            pal.warn,
                            "omp is not installed",
                            &format!("OmpDeck runs the omp coding agent, which was not found in {}.", crate::update::OMP_LOOKUP),
                        );
                        ui.add_space(12.0);
                        ui.vertical_centered(crate::widgets::install_omp_button).inner
                    })
                    .inner;
                if install {
                    self.open_install_tab();
                }
            }
            Some(_) if self.split_shown() => self.show_split(ui),
            Some(_) => self.show_terminal(ui),
        }
    }

    /// Per frame: draw the Settings window while it is open and run what it asks for.
    fn show_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let gateway = self.gw.gateway();
        let mobile = self.gw.mobile();
        let backends = gateway
            .as_deref()
            .map_or(&[][..], |g| g.backends.as_slice());
        let account = self.backup_card();
        let rows = self.sync_rows();
        let input = SettingsInput {
            font_size: self.cfg.font_size,
            session_view: self.cfg.session_view,
            font_range: (MIN_FONT, MAX_FONT),
            font_default: DEFAULT_FONT,
            ui_scale: self.cfg.ui_scale,
            theme: self.cfg.theme,
            style: self.cfg.style,
            density: self.cfg.density,
            separated_panels: self.cfg.separated_panels,
            ui_font: self.cfg.ui_font,
            native_decorations: self.cfg.native_decorations,
            omp_found: self.omp_found(),
            omp_version: self.update.version.as_deref(),
            check_updates: self.cfg.check_updates,
            update: &self.update.state,
            backend: settings_page::BackendCard {
                choice: self.cfg.session_backend,
                tmux: backends.contains(&MuxKind::Tmux),
                host: backends.contains(&MuxKind::Host),
            },
            backup: account.clone(),
            sync: self.sync_page(&account, &rows),
            app_update: self.app_update.card(self.cfg.check_app_updates),
            notify: NotifyInput {
                cfg: &self.cfg.notify,
            },
            mobile: settings_page::MobileCard {
                status: gateway.as_deref(),
                link_up: self.gw.is_up(),
                pending: self.mode_change.is_some(),
                view: mobile.as_deref(),
                now_ms: mobile::now_ms(),
                error: self.mode_error.as_deref(),
            },
            agents: crate::agent_config::AgentsInput {
                models: &self.models,
                favorites: &self.cfg.favorite_models,
                provider: self.cfg.model_provider_filter.as_deref(),
            },
        };
        // Dialogs of the app (and the other windows) in front of it own the keys and clicks.
        let blocked = self.confirm.is_some()
            || self.dialog.is_some()
            || self.folder_dialog.is_some()
            || self.wt.dialog.is_some()
            || self.backup.modal_open()
            || self.sync.modal_open()
            || self.sessions.open
            || self.palette.is_some()
            || self.popup.is_some()
            || self.tree.is_some()
            || self.usage_open;
        let actions = settings_page::window(ctx, &mut self.settings, &input, blocked);
        for a in actions {
            match a {
                SettingsAction::Close => self.close_settings(),
                SettingsAction::FontSize(s) => self.set_font_size(s),
                SettingsAction::UiScale(s) => self.set_ui_scale(s),
                SettingsAction::Theme(t) => self.set_theme(t),
                SettingsAction::Style(v) => self.set_style(v),
                SettingsAction::Density(v) => self.set_density(v),
                SettingsAction::SeparatedPanels(v) => self.set_separated_panels(v),
                SettingsAction::UiFont(v) => self.set_ui_font(v),
                SettingsAction::SessionBackend(v) => self.set_session_backend(v),
                SettingsAction::SessionView(v) => {
                    self.cfg.session_view = v;
                    self.dirty = true;
                }
                SettingsAction::NativeDecorations(v) => self.set_native_decorations(v),
                SettingsAction::SetCheckUpdates(v) => {
                    self.cfg.check_updates = v;
                    self.dirty = true;
                }
                SettingsAction::CheckUpdates => self.check_updates_manually(),
                SettingsAction::LoadCatalog => self.load_catalog(),
                SettingsAction::WriteSetting { setting, value } => {
                    self.write_setting(*setting, value);
                }
                SettingsAction::ResetSetting { key } => self.reset_setting(key),
                SettingsAction::CopyKey(key) => self.copy_text(&key, "setting key"),
                SettingsAction::CopyDetails(text) => {
                    self.copy_text(&text, "error details");
                }
                SettingsAction::InstallOmp => {
                    // The installer runs in a tab: show it.
                    self.close_settings();
                    self.open_install_tab();
                }
                SettingsAction::Backup(a) => self.apply_backup_action(a),
                SettingsAction::Sync(a) => self.apply_sync_action(a),
                SettingsAction::Notify(a) => self.apply_notify_action(a),
                SettingsAction::AppUpdate(a) => self.apply_app_update_setting(a),
                SettingsAction::GatewayMode(mode) => self.set_gateway_mode(mode),
                SettingsAction::Agents(a) => self.apply_agents_action(a),
                SettingsAction::Mobile(a) => self.apply_mobile_action(a),
            }
        }
    }

    fn show_terminal(&mut self, ui: &mut egui::Ui) {
        let mut open_url = None;
        let mut chat_url = None;
        let mut copy_link = None;
        let mut refused = None;
        let pal = Palette::of_ui(ui);
        let font_size = self.terminal_font_size();
        theme::skin::central(ui, pal, true, |ui| {
            let Some(tab) = self.tabs.get_mut(self.active) else {
                return;
            };
            // Clicking app chrome (status bar, tabs, menus, dialogs) must not toggle the terminal's
            // focus: each toggle sends `ESC[O`/`ESC[I` to omp, which merges with an injected lone
            // `ESC` (Stop) or lands inside an injected sequence. Only the terminal widget ignores
            // "click elsewhere"; text fields keep egui's rule. Keys are held while a dialog or
            // menu is open (see `drive_injectors`), so the terminal keeping focus is harmless.
            let ctx = ui.ctx().clone();
            let surrender = ctx.options(|o| o.input_options.surrender_focus_on);
            ctx.options_mut(|o| o.input_options.surrender_focus_on = egui::SurrenderFocusOn::Never);
            let uid = tab.uid;
            let chat = tab.terminal().tsp().live();
            let (response, url) = if chat {
                let out = crate::chat_view::ChatView::new(tab.terminal_mut())
                    .id_salt(uid)
                    .font_size(font_size)
                    .show(ui);
                refused = out.refused;
                (out.response, out.open_url)
            } else {
                let out = TerminalView::new(tab.terminal_mut())
                    .id_salt(uid)
                    .font_size(font_size)
                    .show(ui);
                (out.response, out.open_url)
            };
            ctx.options_mut(|o| o.input_options.surrender_focus_on = surrender);
            if self.focus_pending {
                response.request_focus();
                self.focus_pending = false;
                ui.ctx().request_repaint();
            }
            self.terminal_focused = response.has_focus();
            tab.bell = false;
            if chat {
                chat_url = url;
            } else {
                open_url = url;
            }
            match (!chat)
                .then(|| terminal_menu(&response, tab.terminal()))
                .flatten()
            {
                Some(TermMenu::Copy) => {
                    tab.terminal().copy_selection();
                }
                Some(TermMenu::Paste) => paste_from_menu(tab.terminal_mut()),
                Some(TermMenu::OpenLink(url)) => open_url = Some(url),
                Some(TermMenu::CopyLink(url)) => copy_link = Some(url),
                None => {}
            }
        });
        if let Some(reason) = refused {
            self.tsp_refused(reason);
        }
        if let Some(url) = chat_url {
            self.open_chat_link(url);
        }
        if let Some(url) = open_url {
            self.open_external(url);
        }
        if let Some(url) = copy_link {
            self.copy_text(&url, "link");
        }
    }

    fn tsp_refused(&mut self, reason: deck_term::TspRefused) {
        use deck_term::TspRefused;
        let message = match reason {
            TspRefused::Held => "omp is busy with a command; try again",
            TspRefused::NotLive => "The chat view is not ready yet; try again",
            TspRefused::ReadOnly => "This session is read-only",
            TspRefused::Exited => "This session's process has stopped",
            TspRefused::Detached => "This session is disconnected; reconnect and try again",
        };
        self.toasts.info(&self.ctx, message);
    }

    /// A confirmation or form dialog (or the Settings window) is open.
    fn modal_open(&self) -> bool {
        self.confirm.is_some()
            || self.dialog.is_some()
            || self.folder_dialog.is_some()
            || self.wt.dialog.is_some()
            || self.backup.modal_open()
            || self.sync.modal_open()
            || self.mobile_modal_open()
            || self.sessions.open
            || self.palette.is_some()
            || self.settings_open
    }

    fn show_confirm(&mut self, ctx: &egui::Context) {
        let Some((question, _)) = &self.confirm else {
            return;
        };
        let Some(answer) = confirm::show(ctx, "app", question) else {
            return;
        };
        let Some((_, pending)) = self.confirm.take() else {
            return;
        };
        self.focus_pending = true;
        if let PendingConfirm::ClearDraft { uid, id } = pending {
            self.answer_draft(uid, id, answer);
            return;
        }
        if let PendingConfirm::RestoreBackup(prepared) = pending {
            self.answer_restore(prepared, answer);
            return;
        }
        if !answer {
            return;
        }
        match pending {
            PendingConfirm::StopSession { mux_name, label } => {
                self.kill_session(mux_name, label);
            }
            PendingConfirm::SwitchChannel(kind) => self.open_update_tab(kind),
            PendingConfirm::MoveSession { entry, dest } => self.execute_move(entry, dest),
            PendingConfirm::KillAgent {
                id,
                uid,
                key,
                session,
            } => self.confirm_kill_agent(id, uid, &key, session.as_deref()),
            PendingConfirm::RestartLive { uid, session } => {
                self.execute_restart_live(uid, &session);
            }
            PendingConfirm::RestartSession(target) => self.execute_restart_session(target),
            PendingConfirm::SwitchView { uid, session, view } => {
                self.execute_switch_view(uid, &session, view)
            }
            PendingConfirm::RemoveServer => self.remove_server(),
            PendingConfirm::DeleteSession(entry) => self.execute_delete(entry),
            PendingConfirm::DeleteSessions(paths) => self.execute_delete_sessions(paths),
            PendingConfirm::MoveSessions { paths, dest } => {
                self.execute_move_sessions(paths, dest);
            }
            PendingConfirm::StopSessions(paths) => self.execute_stop_sessions(paths),
            PendingConfirm::KillRunning(names) => self.execute_kill_running(names),
            PendingConfirm::RemoveWorktree { wt, park } => self.execute_remove(wt, park),
            PendingConfirm::SwitchMain { entry, open } => self.execute_switch_main(entry, open),
            PendingConfirm::DropSyncRow { path } => self.drop_sync_row(&path),
            PendingConfirm::Changes(w) => self.execute_changes_write(w),
            PendingConfirm::ClearDraft { .. } | PendingConfirm::RestoreBackup(_) => {}
        }
    }

    // ---------------------------------------------------------------- housekeeping

    fn track_geometry(&mut self, ctx: &egui::Context) {
        // egui gathered the viewport rects before this pass applied a queued zoom, so they are
        // in the points of `viewport_zoom`; `viewport_rect()` is already in this pass's points.
        let viewport_zoom = self.viewport_zoom;
        let (inner, outer, size, maximized) = ctx.input(|i| {
            let v = i.viewport();
            (
                v.inner_rect,
                v.outer_rect,
                i.viewport_rect().size(),
                v.maximized.unwrap_or(false),
            )
        });
        let inner = inner.map(|r| native_points(r, viewport_zoom));
        let outer = outer.map(|r| native_points(r, viewport_zoom));
        let g = update_geometry(
            self.cfg.window,
            inner,
            outer,
            size * ctx.zoom_factor(),
            maximized,
        );
        // Next frame's viewport rects are gathered with the zoom this pass settled on.
        self.viewport_zoom = ctx.zoom_factor();
        if self.cfg.window != Some(g) {
            self.cfg.window = Some(g);
            self.dirty = true;
        }
    }

    fn snapshot_config(&self) -> AppConfig {
        let mut cfg = self.cfg.clone();
        // Before restore ran (or without a backend) the stored tab list is left untouched.
        if self.backends_available() && self.pending_restore.is_none() {
            // Update and installer tabs are never persisted; `active_tab` indexes the persisted list.
            let mut active_tab = None;
            let mut open_tabs = Vec::new();
            let mut persisted: Vec<u64> = Vec::new();
            for (i, t) in self.tabs.iter().enumerate() {
                // Update/installer tabs are one-off processes, never restored.
                if t.role != TabRole::Session {
                    continue;
                }
                let Some(mux_name) = t.mux_name.clone() else {
                    continue;
                };
                if i == self.active {
                    active_tab = Some(open_tabs.len());
                }
                open_tabs.push(TabState {
                    session_name: mux_name,
                    cwd: t.cwd.clone(),
                    session_path: t.session_path_owned(),
                    title: t.title(&self.labels).to_owned(),
                    view: t.view(),
                });
                persisted.push(t.uid);
            }
            let active_tab = active_tab.or_else(|| (!open_tabs.is_empty()).then_some(0));
            let split = if open_tabs.is_empty() {
                None
            } else if self.restore_split.is_some() {
                // Restore still running: the stored layout stays as it was.
                self.restore_split.clone()
            } else {
                self.split.to_state(&persisted)
            };
            match &self.launch_folder {
                Some(dir) if open_tabs.is_empty() => {
                    cfg.folder_tabs.remove(dir);
                }
                Some(dir) => {
                    cfg.folder_tabs.insert(
                        dir.clone(),
                        FolderTabs {
                            open_tabs,
                            active_tab,
                            split,
                        },
                    );
                }
                None => {
                    cfg.open_tabs = open_tabs;
                    cfg.active_tab = active_tab;
                    cfg.split = split;
                }
            }
        }
        cfg
    }

    /// This window's config as it would be submitted: the working copy with the live tab and
    /// split state projected in, where they changed since they last agreed with the commit.
    fn pending_config(&self) -> AppConfig {
        let mut edited = self.snapshot_config();
        self.tabs_synced.mask_unchanged(&mut edited, &self.cfg_base);
        edited
    }

    /// Submits what this window changed since the last reconciliation as field edits against
    /// its baseline. Never blocks: the gateway persists on its own thread.
    fn submit_settings(&mut self) {
        if self.gw.config().is_none() {
            return; // nothing committed to compare against yet
        }
        let edited = self.pending_config();
        let edit = Edit::between(self.cfg_base.clone(), &edited);
        if edit.changes.is_empty() {
            return;
        }
        self.saving = Some((self.gw.settings().submit(edit), edited));
    }

    /// Per tick: the outcome of the edit in flight, and commits other windows made.
    fn poll_settings(&mut self) {
        if let Some((pending, _)) = &self.saving
            && let Some(result) = pending.try_take()
            && let Some((_, sent)) = self.saving.take()
        {
            self.on_settings_result(result, sent);
        }
        if self.saving.is_none()
            && let Some((revision, config)) = self.gw.config()
            && revision != self.cfg_rev
        {
            // The first commit the gateway reported, or another window's: take over the fields
            // this window did not touch.
            let old = self.cfg_base.clone();
            self.adopt_settings(&old, config, revision);
        }
    }

    fn on_settings_result(
        &mut self,
        result: Result<settings::Snapshot, SettingsError>,
        sent: AppConfig,
    ) {
        match result {
            Ok(snap) => {
                self.tabs_synced.committed(&sent, &self.cfg_base);
                self.adopt_settings(&sent, snap.config, snap.revision);
            }
            Err(SettingsError::Conflict { fields, current }) => {
                // Someone else changed these fields since this window's baseline: theirs stand,
                // this window's other changes are sent again against the newer baseline.
                log::warn!("settings changed elsewhere ({fields:?}): keeping the other value");
                let names: Vec<String> = fields.iter().map(|f| format!("{f:?}")).collect();
                self.toasts.info(
                    &self.ctx,
                    format!(
                        "Settings changed elsewhere; kept the other value for: {}",
                        names.join(", ")
                    ),
                );
                let before = self.applied_settings();
                let live = TabsProj::of(
                    &self.snapshot_config(),
                    self.tabs_synced.folder_key.as_ref(),
                );
                self.tabs_synced.yield_fields(&live, &fields);
                settings::adopt(&mut self.cfg, &current.config, &fields);
                let old = self.cfg_base.clone();
                settings::rebase(&mut self.cfg, &old, &current.config);
                self.cfg_base = current.config;
                self.cfg_rev = current.revision;
                self.reconcile_settings(before);
                self.dirty = true;
            }
            Err(e) => {
                log::warn!("saving settings failed: {e}");
                self.toasts
                    .error(&self.ctx, format!("Saving settings failed: {e}"));
                // The baseline did not advance: the same changes go out again later.
                self.dirty = true;
            }
        }
    }

    /// Moves the working copy onto `new` (a committed snapshot): every field this window has not
    /// changed since `old` takes the committed value, its own pending changes stay.
    fn adopt_settings(&mut self, old: &AppConfig, new: Arc<AppConfig>, revision: u64) {
        let before = self.applied_settings();
        settings::rebase(&mut self.cfg, old, &new);
        self.cfg_base = new;
        self.cfg_rev = revision;
        self.reconcile_settings(before);
    }

    /// The settings whose change needs more than a new value in `cfg`.
    fn applied_settings(&self) -> Applied {
        Applied {
            look: (
                self.cfg.theme,
                self.cfg.style,
                self.cfg.density,
                self.cfg.ui_font,
                self.cfg.separated_panels,
            ),
            ui_scale: self.cfg.ui_scale,
            rows: self.cfg.project_sync.clone(),
        }
    }

    /// Carries out the runtime side effects of settings that changed from `before` through
    /// adoption, as their settings-page handlers would: restyle, and the project-sync rows.
    /// Everything else (backend choice, schedules) is applied by the gateway from the commit.
    fn reconcile_settings(&mut self, before: Applied) {
        // A committed font size or app scale is trusted no further than the settings page's.
        if !self.cfg.font_size.is_finite() {
            self.cfg.font_size = DEFAULT_FONT;
        }
        self.cfg.font_size = self.cfg.font_size.clamp(MIN_FONT, MAX_FONT);
        self.cfg.ui_scale = clamp_ui_scale(self.cfg.ui_scale);
        let now = self.applied_settings();
        if before.look != now.look {
            self.restyle();
        }
        if before.ui_scale != now.ui_scale {
            self.ctx.set_zoom_factor(now.ui_scale);
        }
        if before.rows != now.rows {
            self.reconcile_sync_rows(&before.rows);
        }
    }

    fn housekeeping(&mut self, ctx: &egui::Context) {
        self.track_geometry(ctx);

        let tab_title = self
            .tabs
            .get(self.active)
            .map(|t| titlebar::strip_omp_prefix(t.title(&self.labels)));
        let title = match &self.scope {
            Some(dir) => titlebar::window_title(tab_title, Some(&folder_scope::folder_name(dir))),
            None => titlebar::window_title(tab_title, None),
        };
        // omp animates a spinner in its title (~12/s); every window-title change costs a viewport
        // command and a repaint, so changes are applied at most once per second.
        if title != self.last_title {
            let since = self.last_title_at.elapsed();
            if since >= WINDOW_TITLE_EVERY {
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
                self.last_title = title;
                self.last_title_at = Instant::now();
            } else {
                // The deferred update needs a frame of its own once the throttle opens.
                ctx.request_repaint_after(WINDOW_TITLE_EVERY - since);
            }
        }

        clipboard_bridge::forward_copies(ctx, &self.clipboard);

        if self.dirty {
            let since = self.last_save.elapsed();
            if since < SAVE_DEBOUNCE {
                ctx.request_repaint_after(SAVE_DEBOUNCE - since);
            } else if self.saving.is_none() {
                self.submit_settings();
                self.dirty = false;
                self.last_save = Instant::now();
            } else {
                // The previous edit is still at the owner; its result arrives through `logic`.
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }

        self.drive_screenshot(ctx);
    }

    fn drive_screenshot(&mut self, ctx: &egui::Context) {
        let Some(plan) = &mut self.shot else {
            return;
        };
        match &plan.state {
            ShotState::Waiting => {
                let elapsed = self.started.elapsed();
                if elapsed < plan.after {
                    ctx.request_repaint_after(plan.after - elapsed);
                    return;
                }
                let done = Arc::new(AtomicBool::new(false));
                let (flag, path, repaint) = (Arc::clone(&done), plan.path.clone(), ctx.clone());
                ctx.request_screenshot(move |img| {
                    if let Err(e) = write_png(&path, &img) {
                        log::error!("screenshot {}: {e}", path.display());
                    }
                    flag.store(true, Ordering::SeqCst);
                    repaint.request_repaint();
                });
                plan.state = ShotState::Requested(done);
            }
            ShotState::Requested(done) => {
                if done.load(Ordering::SeqCst) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else {
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
            }
        }
    }
}

// -------------------------------------------------------------------- omp CLI features

impl App {
    /// "Check for omp updates now".
    fn check_updates_manually(&mut self) {
        if self.update.is_checking() {
            return;
        }
        self.start_update_check(None);
    }

    /// Asks the gateway to check omp for an update; `announce` = the update this check follows.
    fn start_update_check(&mut self, announce: Option<UpdateKind>) {
        if !self.omp_found() {
            return;
        }
        self.update.begin(announce);
        self.send_update_check();
    }

    fn send_update_check(&mut self) {
        self.gw.mutate(Mutation::OmpUpdateCheck, |app, result| {
            if let Err(e) = result {
                app.update.abandon();
                app.toasts.error(
                    &app.ctx,
                    format!("Update check failed: {}", crate::summary::strip_context(&e)),
                );
            }
        });
    }

    fn confirm_channel_switch(&mut self, kind: UpdateKind) {
        let (title, body, label) = match kind {
            UpdateKind::Canary => (
                "Switch to the canary channel?",
                "omp will switch to the canary channel and install the latest canary build now. Canary builds are pre-releases and may be unstable; you can switch back to stable at any time.",
                "Switch to canary",
            ),
            _ => (
                "Switch back to the stable channel?",
                "omp will switch to the stable channel and install the latest stable build now.",
                "Switch to stable",
            ),
        };
        self.confirm = Some((
            Confirm {
                title: title.to_owned(),
                body: body.to_owned(),
                confirm_label: label.to_owned(),
                danger: false,
            },
            PendingConfirm::SwitchChannel(kind),
        ));
    }

    /// When an update or install tab's process has exited: re-check, and report the outcome. A
    /// successful update closes its tab (the toast reports the result); a failed one stays open
    /// with the output.
    fn check_update_tabs(&mut self) {
        let mut finished = Vec::new();
        for tab in self.tabs.iter_mut() {
            if matches!(tab.role, TabRole::Update(_) | TabRole::InstallOmp)
                && !tab.exit_handled
                && tab.terminal().is_exited()
            {
                tab.exit_handled = true;
                finished.push((
                    tab.role,
                    tab.terminal().exit_code(),
                    tab.target().key.clone(),
                ));
            }
        }
        for (role, code, key) in finished {
            let TabRole::Update(kind) = role else {
                self.finish_omp_install(code);
                continue;
            };
            match code {
                Some(0) => {
                    self.close_tabs_where(|t| t.target().key == key);
                    self.update.recheck.push_back(Some(kind));
                }
                other => {
                    let code = other.map_or("signal".to_owned(), |c| c.to_string());
                    self.toasts.error(
                        &self.ctx,
                        format!(
                            "{} failed (exit {code}) — see the tab for details",
                            crate::update::tab_title(kind)
                        ),
                    );
                    self.update.recheck.push_back(None);
                }
            }
        }
    }

    /// Start the next owed post-update check once no other check is in flight; the announcement
    /// belongs to that new check, never to an older one that was still running.
    fn pump_update_queue(&mut self) {
        if self.omp_found() && self.update.next_queued().is_some() {
            self.send_update_check();
        }
    }

    fn on_update_checked(&mut self, done: crate::update::Finished) {
        let crate::update::Finished {
            result,
            announce,
            manual,
        } = done;
        match (result, announce) {
            (Ok(_), Some(UpdateKind::Plugins)) => {
                self.toasts.success(&self.ctx, "omp plugins updated");
            }
            (Ok(status), Some(_)) => self.toasts.success(
                &self.ctx,
                format!(
                    "omp updated to {} — running sessions keep the old version until restarted",
                    status.current
                ),
            ),
            (Err(_), Some(_)) => self.toasts.success(
                &self.ctx,
                "omp update finished — running sessions keep the old version until restarted",
            ),
            (Ok(status), None) if manual => match status.latest {
                Some(latest) => self
                    .toasts
                    .info(&self.ctx, format!("omp {latest} is available")),
                None => self
                    .toasts
                    .info(&self.ctx, format!("omp {} is up to date", status.current)),
            },
            (Err(e), None) if manual => self.toasts.error(
                &self.ctx,
                format!("Update check failed: {}", crate::summary::strip_context(&e)),
            ),
            _ => {}
        }
    }

    /// Asks the gateway for omp's settings catalog (and the defaults the status bar shows).
    fn load_catalog(&mut self) {
        if !self.omp_found() {
            return;
        }
        self.gw.query(Query::OmpSettings, |app, result| {
            let catalog = result.and_then(|a| match a {
                Answer::OmpSettings(c) => Ok(c),
                other => Err(format!("unexpected answer {other:?}")),
            });
            if let Ok(catalog) = &catalog {
                app.omp_defaults = defaults_from_catalog(catalog);
                app.omp_usage_reserve = crate::omp_settings::usage_reserve_from_catalog(catalog);
            }
            let env = std::env::var("PI_CODING_AGENT_DIR").ok();
            let dir = config_dir_display(env.as_deref(), app.home.as_deref());
            app.settings.catalog_loaded(catalog, dir);
        });
    }

    fn write_setting(&mut self, setting: Setting, value: Value) {
        let key = setting.key.clone();
        // A secret's value is never echoed back.
        let saved = if setting.secret {
            Value::Null
        } else {
            value.clone()
        };
        self.gw.mutate(
            Mutation::OmpSet {
                key: key.clone(),
                value,
            },
            move |app, result| app.on_setting_done(&key, result.map(|_| Some(saved))),
        );
    }

    fn reset_setting(&mut self, key: String) {
        self.gw.mutate(
            Mutation::OmpReset { key: key.clone() },
            move |app, result| app.on_setting_done(&key, result.map(|_| None)),
        );
    }

    /// `outcome`: `Ok(Some(v))` = saved value (`Null` for secrets), `Ok(None)` = reset done.
    fn on_setting_done(&mut self, key: &str, outcome: Result<Option<Value>, String>) {
        let reset = matches!(outcome, Ok(None));
        let err = outcome.as_ref().err().cloned();
        let label = self.settings.write_done(key, outcome);
        match err {
            None if reset => self
                .toasts
                .success(&self.ctx, format!("Reset “{label}” to its default")),
            None => self.toasts.success(&self.ctx, format!("Saved “{label}”")),
            Some(e) => self.toasts.error(
                &self.ctx,
                format!("{label}: {}", crate::summary::strip_context(&e)),
            ),
        }
    }

    // ------------------------------------------------------------ copy / move

    fn start_transfer(&mut self, kind: TransferKind, entry: SessionEntry, dest: Option<PathBuf>) {
        if !self.omp_found() || self.moving.contains(&entry.path) {
            return;
        }
        match dest {
            Some(dest) => self.begin_transfer(kind, entry, dest),
            None => self.folder_dialog = Some(FolderDialog::new(kind, entry)),
        }
    }

    fn begin_transfer(&mut self, kind: TransferKind, entry: SessionEntry, dest: PathBuf) {
        match kind {
            TransferKind::Copy => self.copy_session(&entry, &dest),
            TransferKind::Move => self.request_move(entry, dest),
        }
    }

    /// Copy = a new tab running `omp --fork <src> --cwd <dest>` in the gateway. The tab starts
    /// "copy pending": success is only announced once the gateway found the new session file
    /// (`ClientEvent::CopyResult`).
    fn copy_session(&mut self, entry: &SessionEntry, dest: &Path) {
        let mut req = SpawnReq::session(
            self.new_mux_name(),
            dest.to_path_buf(),
            None,
            entry.label().to_owned(),
            self.cfg.session_view,
            Post::Copy {
                label: entry.label().to_owned(),
                dest: dest.to_path_buf(),
            },
            Focus::Claim,
        );
        req.argv = fork_argv(&entry.path, dest);
        req.copy_of = Some(entry.id.clone());
        self.spawn_tab_post(req);
    }

    fn request_move(&mut self, entry: SessionEntry, dest: PathBuf) {
        let running = self.running_session_name(&entry).is_some()
            || self.tabs.iter().any(|t| {
                t.session_path() == Some(entry.path.as_path()) && !t.terminal().is_exited()
            });
        let mut body = format!(
            "Move “{}” to {}? omp moves the session file and its artifacts; the original is removed from {}.",
            entry.label(),
            abbreviate_home(&dest),
            abbreviate_home(&entry.cwd),
        );
        if running {
            body.push_str(" It is running and will be stopped first.");
        }
        self.confirm = Some((
            Confirm {
                title: "Move session?".to_owned(),
                body,
                confirm_label: "Move".to_owned(),
                danger: running,
            },
            PendingConfirm::MoveSession { entry, dest },
        ));
    }

    /// Stop the session if it runs (kill every session involved + close its tabs), then the
    /// gateway moves the files. A tab that was open comes back afterwards: on the new path after
    /// success, on the original path (re-attached if its session survived) after a failure.
    fn execute_move(&mut self, entry: SessionEntry, dest: PathBuf) {
        if !self.omp_found() || !self.moving.insert(entry.path.clone()) {
            return;
        }
        let prep = self.prepare_move(entry);
        let title = prep.entry.label().to_owned();
        self.toasts.info(
            &self.ctx,
            format!("Moving “{title}” to {}…", abbreviate_home(&dest)),
        );
        let src = prep.entry.path.clone();
        let op = Mutation::Move {
            file: src.clone(),
            dest_cwd: dest.clone(),
            stop: prep.kill_names.clone(),
        };
        self.gw.mutate(op, move |app, result| {
            let result = result.and_then(|d| match d {
                Done::Moved(outcome) => Ok(outcome),
                other => Err(format!("unexpected answer {other:?}")),
            });
            let tab = app.finish_move(prep, &result, &dest);
            app.on_move_done(src, dest, title, tab, result);
        });
    }

    /// Capture the tab state of a session about to be moved and close its tabs. The monitor
    /// snapshot can lag behind a tab that was just opened: the session each matching tab
    /// recorded is killed as well, not only the snapshot's holder.
    pub(super) fn prepare_move(&mut self, entry: SessionEntry) -> MovePrep {
        let holder = self.running_session_name(&entry);
        let mut kill_names: Vec<String> = holder.iter().cloned().collect();
        let mut tab_state: Option<ClosedTab> = None;
        let (mut slot, mut was_active) = (0, false);
        for i in (0..self.tabs.len()).rev() {
            let hit = {
                let t = &self.tabs[i];
                t.session_path() == Some(entry.path.as_path())
                    || (holder.is_some() && t.mux_name == holder)
            };
            if !hit {
                continue;
            }
            let t = &mut self.tabs[i];
            match &t.mux_name {
                Some(name) => {
                    if !kill_names.contains(name) {
                        kill_names.push(name.clone());
                    }
                }
                None => t.terminal_mut().kill(),
            }
            if tab_state.is_none() {
                slot = i;
                tab_state = Some(ClosedTab {
                    mux_name: t.mux_name.clone(),
                    cwd: t.cwd.clone(),
                    session_path: Some(entry.path.clone()),
                    title: entry.label().to_owned(),
                    view: t.view(),
                });
            } else if i < slot {
                slot -= 1; // a tab in front of the first one is closed too
            }
            was_active |= i == self.active;
            self.close_tab(i, false);
        }
        // The reopened tab takes focus only if this was the active tab and the user has not
        // switched since.
        let refocus = was_active.then_some(self.activation_seq);
        let anchor = Anchor::at(&self.tab_uids(), slot);
        MovePrep {
            entry,
            kill_names,
            tab: tab_state,
            anchor,
            refocus,
        }
    }

    fn request_delete(&mut self, entry: SessionEntry) {
        if !self.omp_found() || self.moving.contains(&entry.path) {
            return;
        }
        let label = entry.label().to_owned();
        let running = self.running_session_name(&entry).is_some();
        let busy = self.tabs.iter().any(|t| {
            !t.terminal().is_exited()
                && t.session_path() == Some(entry.path.as_path())
                && Activity::of_terminal(t.terminal()) != Activity::Idle
        });
        let live = if running {
            " Its running omp process is stopped first."
        } else {
            ""
        };
        let warn = if busy {
            " omp is working; deleting stops the current turn."
        } else {
            ""
        };
        let size = std::fs::metadata(&entry.path)
            .map(|m| format!("\nSize: {}", human_size(m.len())))
            .unwrap_or_default();
        let project = abbreviate_home(&entry.cwd);
        self.confirm = Some((
            Confirm {
                title: "Delete session?".to_owned(),
                body: format!(
                    "“{label}”\nProject: {project}{size}\n\
                     Subagent transcripts and artifacts are deleted too. This cannot be undone.{live}{warn}"
                ),
                confirm_label: "Delete session".to_owned(),
                danger: true,
            },
            PendingConfirm::DeleteSession(entry),
        ));
    }

    /// Stop whatever runs the session, then let the gateway delete it.
    fn execute_delete(&mut self, entry: SessionEntry) {
        if !self.omp_found() || !self.moving.insert(entry.path.clone()) {
            return;
        }
        let stop = self.session_kill_names(&entry, true);
        let path = entry.path.clone();
        let title = entry.label().to_owned();
        let target = DeleteTarget {
            path: path.clone(),
            id: entry.id.clone(),
        };
        self.toasts
            .info(&self.ctx, format!("Deleting “{}”…", entry.label()));
        self.gw.mutate(
            Mutation::Delete {
                targets: vec![target],
                stop,
            },
            move |app, result| {
                let result = result.and_then(|d| match d {
                    Done::Deleted(failed) => match failed.into_iter().find(|(p, _)| *p == path) {
                        Some((_, Some(why))) => Err(why),
                        _ => Ok(()),
                    },
                    other => Err(format!("unexpected answer {other:?}")),
                });
                app.on_delete_done(&path, &title, result);
            },
        );
    }

    /// Kill list for stopping `entry`: the backend session that holds it, plus the ones its
    /// tabs recorded (the monitor snapshot can lag behind a tab that was just opened). Tabs
    /// without a backend session are killed on the spot; with `close_tabs` every tab on the
    /// session is closed without being remembered and the reopen stack forgets the session.
    pub(super) fn session_kill_names(
        &mut self,
        entry: &SessionEntry,
        close_tabs: bool,
    ) -> Vec<String> {
        let holder = self.running_session_name(entry);
        let mut kill_names: Vec<String> = holder.iter().cloned().collect();
        for i in (0..self.tabs.len()).rev() {
            let hit = {
                let t = &self.tabs[i];
                t.session_path() == Some(entry.path.as_path())
                    || (holder.is_some() && t.mux_name == holder)
            };
            if !hit {
                continue;
            }
            let t = &mut self.tabs[i];
            match &t.mux_name {
                Some(name) => {
                    if !kill_names.contains(name) {
                        kill_names.push(name.clone());
                    }
                }
                None => t.terminal_mut().kill(),
            }
            if close_tabs {
                self.close_tab(i, false);
            }
        }
        if close_tabs {
            self.closed
                .retain(|c| c.session_path.as_deref() != Some(entry.path.as_path()));
        }
        kill_names
    }

    fn on_delete_done(&mut self, path: &std::path::Path, title: &str, result: Result<(), String>) {
        self.moving.remove(path);
        self.refresh_sessions();
        match result {
            Ok(()) => self.toasts.success(&self.ctx, format!("Deleted “{title}”")),
            Err(e) => self.toasts.error(
                &self.ctx,
                format!(
                    "Could not delete “{title}”: {}",
                    crate::summary::strip_context(&e)
                ),
            ),
        }
    }

    fn on_move_done(
        &mut self,
        src: PathBuf,
        dest: PathBuf,
        title: String,
        tab: Option<Box<MovedTab>>,
        result: Result<MoveOutcome, String>,
    ) {
        self.moving.remove(&src);
        self.refresh_sessions();
        match result {
            Ok(outcome) => {
                let to = abbreviate_home(&dest);
                let text = match outcome.kind {
                    MoveKind::Forked { .. } => {
                        format!("Moved “{title}” to {to} — original kept, moved as a new session")
                    }
                    _ => format!("Moved “{title}” to {to}"),
                };
                self.toasts.success(&self.ctx, text);
            }
            Err(e) => self.toasts.error(
                &self.ctx,
                format!(
                    "Could not move “{title}”: {}",
                    crate::summary::strip_context(&e)
                ),
            ),
        }
        self.restore_moved_tab(&src, tab);
    }

    /// Bring back the tab that was open on a moved session: on the new path after a successful
    /// move, on the original one (re-attached if its session survived) after a failure.
    pub(super) fn restore_moved_tab(&mut self, src: &Path, tab: Option<Box<MovedTab>>) {
        let Some(MovedTab {
            tab: t,
            anchor,
            refocus,
        }) = tab.map(|b| *b)
        else {
            return;
        };
        self.retarget_pane_claim(src, t.session_path_owned(), t.mux_name.clone());
        self.spawn_tab_post(SpawnReq::session(
            t.mux_name,
            t.cwd,
            t.session_path,
            t.title,
            t.view,
            Post::Anchor(anchor),
            if refocus == Some(self.activation_seq) {
                Focus::Claim
            } else {
                Focus::Never
            },
        ));
    }

    fn show_folder_dialog(&mut self, ctx: &egui::Context) {
        let Some(dlg) = self.folder_dialog.as_mut() else {
            return;
        };
        match dlg.show(ctx, self.home.as_deref(), self.picker_available) {
            FolderOutcome::Open => {}
            FolderOutcome::Cancel => {
                self.folder_dialog = None;
                self.focus_pending = true;
            }
            FolderOutcome::Confirm(dest) => {
                if let Some(d) = self.folder_dialog.take() {
                    self.focus_pending = true;
                    self.finish_folder_dialog(d, dest);
                }
            }
            FolderOutcome::Browse => {
                dlg.browsing = true;
                let start = dlg.start_dir(self.home.as_deref());
                self.gw.native(
                    move || transfer::pick_folder(&start),
                    |app, path| {
                        if let Some(dlg) = &mut app.folder_dialog {
                            dlg.browsing = false;
                            if let Some(p) = path {
                                dlg.set_text(p.to_string_lossy().into_owned(), app.home.as_deref());
                            }
                        }
                        app.dispatch_folder_check();
                    },
                );
            }
        }
        self.dispatch_folder_check();
    }

    /// Run the dialog's pending filesystem check (is it a directory? is it the source project?)
    /// on a native worker; the verdict comes back through `apply_check`.
    fn dispatch_folder_check(&mut self) {
        let Some(request) = self
            .folder_dialog
            .as_mut()
            .and_then(FolderDialog::take_request)
        else {
            return;
        };
        self.gw.native(
            move || request.run(),
            |app, (generation, result)| {
                if let Some(dlg) = &mut app.folder_dialog {
                    dlg.apply_check(generation, result);
                }
            },
        );
    }
}

impl App {
    /// Drains every terminal's events so titles, exit state and bells stay current whether or
    /// not a frame is painted (the widget's own poll only runs while it is drawn).
    fn poll_tabs(&mut self) {
        let split_on = self.split_shown();
        let dash_open = self.dash.open;
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            let shown = !dash_open
                && if split_on {
                    self.split.slot_of(tab.uid).is_some()
                } else {
                    i == self.active
                };
            // A shown tab's bell is moot; the others show the marker.
            if tab.terminal_mut().poll().bell && !shown {
                tab.bell = true;
            }
        }
    }

    /// One debug line per second with the frame rate and what requested the repaints
    /// (repaint-storm diagnosis: `RUST_LOG=omp_deck::frames=debug`).
    fn count_frame(&mut self, ctx: &egui::Context) {
        self.frame_count += 1;
        if log::log_enabled!(target: "omp_deck::frames", log::Level::Debug) {
            for c in ctx.repaint_causes() {
                *self
                    .repaint_causes
                    .entry(format!("{}:{} {}", c.file, c.line, c.reason))
                    .or_default() += 1;
            }
        }
        let elapsed = self.frame_mark.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let mut causes: Vec<_> = self.repaint_causes.drain().collect();
            causes.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            causes.truncate(10);
            log::debug!(
                target: "omp_deck::frames",
                "{:.1} frames/s; top causes: {causes:?}",
                f64::from(self.frame_count) / elapsed.as_secs_f64()
            );
            self.frame_count = 0;
            self.frame_mark = Instant::now();
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        Palette::of(visuals.dark_mode).bg.to_normalized_gamma_f32()
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        if !self.terminal_focused {
            clipboard_bridge::rewrite_raw_input(raw, &self.clipboard);
        }
        self.file_drops.feed(ctx, raw);
    }

    /// Runs every tick, also while the window is minimized or on another desktop (when eframe
    /// skips `ui`): gateway results, terminal polling and what the gateway is told about this
    /// window must not depend on painting.
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.file_drops.attach_window(ctx, frame);
        self.pump_gateway();
        self.run_desktop_opens();
        self.poll_tabs();
        self.poll_settings();
        self.poll_gateway_mode();
        self.drive_notifications(ctx, frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if !self.fonts_finalized {
            self.fonts_finalized = true;
            ui_fonts::finalize(&ctx);
        }
        self.count_frame(&ctx);
        self.handle_shortcuts(&ctx);
        self.drain_events(&ctx);

        if self.gw.gateway().is_some()
            && let Some(snap) = self.gw.running()
        {
            self.restore_tabs(&snap);
            self.open_deferred();
        }
        let snap = self.running_snapshot();
        self.refresh_sessions_cache();
        self.tick_omp();
        self.tick_omp_update();
        self.backup_tick(&ctx);

        // Panes follow the tab list; then only the tabs on screen repaint promptly on output
        // (the others coalesce). Polling for title/bell/exit happens in `logic`.
        self.reconcile_panes();
        let split_on = self.split_shown();
        // The dashboard covers the terminal area: no tab is on screen (still polled below).
        let dash_open = self.dash.open;
        for (i, tab) in self.tabs.iter().enumerate() {
            let shown = !dash_open
                && if split_on {
                    self.split.slot_of(tab.uid).is_some()
                } else {
                    i == self.active
                };
            tab.terminal().set_visible(shown);
        }

        // The terminal keeps keyboard focus: take it back whenever nothing else (a text field,
        // an open menu, a modal) legitimately holds it.
        if self.confirm.is_none()
            && self.folder_dialog.is_none()
            && self.wt.dialog.is_none()
            && self.dialog.is_none()
            && !self.sync.modal_open()
            && !self.mobile_modal_open()
            && self.popup.is_none()
            && !self.dash.open
            && self.tree.is_none()
            && !self.settings_open
            && !self.usage_open
            && !self.sessions.open
            && self.palette.is_none()
            && !egui::Popup::is_any_open(&ctx)
            && ctx.memory(|m| m.focused()).is_none()
        {
            self.focus_pending = true;
        }

        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        theme::skin::paint_backdrop(ui.painter(), ui.max_rect(), Palette::of_ui(ui));
        self.show_bar(ui, maximized);
        self.show_banners(ui);
        self.drive_status();
        self.drive_ctl();
        self.drive_usage();
        self.drive_agents();
        self.drive_dashboard();
        self.drive_changes(&ctx);
        self.show_status_bar(ui);
        self.show_sidebar(ui, &snap);
        self.show_changes_panel(ui);
        self.show_central(ui);
        self.show_settings(&ctx);
        self.show_agent_popup(&ctx);
        self.show_tree(&ctx);
        self.show_usage(&ctx);
        self.show_sessions(&ctx);
        self.show_backup_dialogs(&ctx);
        self.show_sync_dialogs(&ctx);
        self.show_mobile_dialogs(&ctx);
        self.show_confirm(&ctx);
        self.show_palette(&ctx);
        self.show_action_dialog(&ctx);
        self.show_folder_dialog(&ctx);
        self.show_worktree_dialog(&ctx);
        self.drive_worktrees();
        self.check_update_tabs();
        self.pump_update_queue();
        self.tick_app_update(&ctx);
        // Age chips tick over without any other event.
        ctx.request_repaint_after(AGE_REFRESH);
        match self.toasts.show(&ctx) {
            Some(ToastClick::Action(TOAST_QUIT)) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Some(ToastClick::Details(text)) => self.copy_text(&text, "error details"),
            Some(ToastClick::Action(_)) | None => {}
        }
        if !self.cfg.native_decorations {
            titlebar::window_chrome(&ctx, maximized);
        }
        self.housekeeping(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Closing the window detaches its views only: the gateway keeps the sessions (and any
        // accepted work) running. What is left is this window's last settings. Whatever is still
        // in flight settles first; then the final state goes through the same compare-and-set
        // path and the gateway is flushed so it is on disk before exit. A conflict drops this
        // window's conflicting fields and leaves the rest dirty: those go out again, so a lost
        // race on one field never loses the others. Every conflicting round gives up at least one
        // of the fields still to send, so the fields in flight plus the fields still dirty, plus
        // one final round, bound the loop. A failed write, or a gateway that does not answer in
        // time, ends it.
        if !self.gw.is_up() || self.gw.config().is_none() {
            return;
        }
        let in_flight = self.saving.as_ref().map_or(0, |(_, sent)| {
            settings::Setting::diff(&self.cfg_base, sent).len()
        });
        let dirty = settings::Setting::diff(&self.cfg_base, &self.pending_config()).len();
        for _ in 0..in_flight + dirty + 1 {
            if self.saving.is_none() {
                self.submit_settings();
            }
            let Some((pending, sent)) = self.saving.take() else {
                break;
            };
            let Some(result) = pending.wait(EXIT_WAIT) else {
                break;
            };
            let failed = matches!(result, Err(SettingsError::Io(_) | SettingsError::Gone));
            self.on_settings_result(result, sent);
            if failed {
                break;
            }
        }
        if self.gw.flush().wait(EXIT_WAIT).is_none() {
            log::warn!("the gateway did not acknowledge the final settings in time");
        }
    }
}

/// Choice from the terminal's right-click menu.
enum TermMenu {
    Copy,
    Paste,
    OpenLink(String),
    CopyLink(String),
}

/// Right-click menu of the terminal: clipboard actions, plus link actions when a link (OSC 8 or
/// a plain URL) was under the click.
fn terminal_menu(resp: &egui::Response, term: &deck_term::Terminal) -> Option<TermMenu> {
    let mut pick = None;
    theme::context_menu(resp, 220.0, |ui| {
        let copy_enabled = term.has_selection();
        if menu_item(
            ui,
            ICON_COPY,
            "Copy",
            Some("Ctrl+Shift+C"),
            copy_enabled,
            false,
        )
        .clicked()
        {
            pick = Some(TermMenu::Copy);
            ui.close();
        }
        if menu_item(
            ui,
            ICON_CLIPBOARD_LIST,
            "Paste",
            Some("Ctrl+Shift+V"),
            true,
            false,
        )
        .clicked()
        {
            pick = Some(TermMenu::Paste);
            ui.close();
        }
        if let Some(link) = term.context_link() {
            ui.separator();
            let openable = is_openable_url(link);
            if menu_item(
                ui,
                ICON_EXTERNAL_LINK,
                "Open link",
                Some("Ctrl+Click"),
                openable,
                false,
            )
            .clicked()
            {
                pick = Some(TermMenu::OpenLink(link.to_owned()));
                ui.close();
            }
            if menu_item(ui, ICON_COPY, "Copy link", None, true, false).clicked() {
                pick = Some(TermMenu::CopyLink(link.to_owned()));
                ui.close();
            }
        }
    });
    pick
}

/// The menu's Paste is a deliberate click, yet it lands while the menu is still up, so the hold
/// that keeps keys meant for an open menu or dialog away from omp (`hold_input_during_modals`,
/// renewed every frame this menu showed) would drop it. That hold ends first; a hold the gateway
/// publishes for an injected command still drops the paste (`Terminal::input_held`).
fn paste_from_menu(term: &mut deck_term::Terminal) {
    term.hold_input_until(Instant::now());
    term.paste_clipboard();
}

fn chat_file_path(url: &str) -> Option<PathBuf> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    // File links target this machine: ignore the authority (including localhost).
    let slash = rest.find('/')?;
    let path = rest[slash..].split(['?', '#']).next()?;
    let mut bytes = Vec::with_capacity(path.len());
    let mut input = path.bytes();
    while let Some(byte) = input.next() {
        let byte = if byte == b'%' {
            let hi = (input.next()? as char).to_digit(16)?;
            let lo = (input.next()? as char).to_digit(16)?;
            (hi * 16 + lo) as u8
        } else {
            byte
        };
        if byte == 0 {
            return None;
        }
        bytes.push(byte);
    }
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    };
    #[cfg(windows)]
    let path = {
        let text = String::from_utf8(bytes).ok()?;
        let text = if text.as_bytes().get(2) == Some(&b':') {
            &text[1..]
        } else {
            &text
        };
        PathBuf::from(text)
    };
    path.is_absolute().then_some(path)
}

/// Only `http`, `https`, `mailto` (scheme matched case-insensitively) may reach the desktop's
/// opener.
fn is_openable_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    !rest.is_empty()
        && ["http", "https", "mailto"]
            .iter()
            .any(|s| scheme.eq_ignore_ascii_case(s))
}

fn write_png(path: &Path, img: &egui::ColorImage) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(BufWriter::new(file), img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(img.as_raw())?;
    Ok(())
}

/// Viewport rects reach egui in egui points (physical / (zoom × native scale)); stored geometry
/// is in native logical points, so a non-1.0 app scale does not shrink or grow the restored
/// window.
fn native_points(r: egui::Rect, zoom: f32) -> egui::Rect {
    egui::Rect::from_min_size(r.min * zoom, r.size() * zoom)
}

/// Pure geometry update. `inner`/`outer` are absent on Wayland, where only the viewport
/// `size` is known; position then stays as previously stored.
fn update_geometry(
    prev: Option<WindowGeometry>,
    inner: Option<egui::Rect>,
    outer: Option<egui::Rect>,
    size: egui::Vec2,
    maximized: bool,
) -> WindowGeometry {
    let size = inner.map_or(size, |r| r.size());
    let mut g = prev.unwrap_or(WindowGeometry {
        x: None,
        y: None,
        width: size.x,
        height: size.y,
        maximized,
    });
    g.maximized = maximized;
    if !maximized {
        g.width = size.x;
        g.height = size.y;
        if let Some(o) = outer {
            g.x = Some(o.min.x);
            g.y = Some(o.min.y);
        }
    }
    g
}

/// `1536` → `1.5 KB` (binary steps, one decimal above bytes).
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::{TabsProj, WindowGeometry, is_openable_url, native_points, update_geometry};
    use deck_core::config::{AppConfig, FolderTabs};
    use deck_service::settings::Field;
    use std::path::PathBuf;

    fn folder(active: usize) -> FolderTabs {
        FolderTabs {
            active_tab: Some(active),
            ..FolderTabs::default()
        }
    }

    #[test]
    fn an_unchanged_projection_never_overwrites_the_committed_layout() {
        let key = PathBuf::from("/p/a");
        let local = AppConfig::default();
        let synced = TabsProj::of(&local, Some(&key));
        let mut base = local.clone();
        base.active_tab = Some(3);
        base.folder_tabs.insert(key.clone(), folder(2));
        let mut edited = local.clone();
        synced.mask_unchanged(&mut edited, &base);
        assert_eq!(edited.active_tab, Some(3));
        assert_eq!(edited.folder_tabs, base.folder_tabs);
    }

    #[test]
    fn only_the_own_folder_entry_is_masked_and_other_keys_keep_explicit_edits() {
        let (mine, other, gone) = (
            PathBuf::from("/p/a"),
            PathBuf::from("/p/b"),
            PathBuf::from("/wt/x"),
        );
        let mut base = AppConfig::default();
        base.folder_tabs.insert(other.clone(), folder(5));
        base.folder_tabs.insert(gone.clone(), folder(7));
        base.folder_tabs.insert(mine.clone(), folder(2));
        // Working config: a removed worktree entry (explicit edit); own view unchanged.
        let mut synced = TabsProj::of(&AppConfig::default(), Some(&mine));
        synced.folder = Some(folder(9));
        let mut edited = base.clone();
        edited.folder_tabs.remove(&gone);
        edited.folder_tabs.insert(mine.clone(), folder(9));
        synced.mask_unchanged(&mut edited, &base);
        assert!(!edited.folder_tabs.contains_key(&gone), "removal persists");
        assert_eq!(edited.folder_tabs.get(&other), Some(&folder(5)));
        assert_eq!(
            edited.folder_tabs.get(&mine),
            Some(&folder(2)),
            "own view masked"
        );
        // A changed own view is sent.
        let mut moved = base.clone();
        moved.folder_tabs.insert(mine.clone(), folder(1));
        synced.mask_unchanged(&mut moved, &base);
        assert_eq!(moved.folder_tabs.get(&mine), Some(&folder(1)));
    }

    #[test]
    fn only_a_conflict_of_the_own_folder_key_is_yielded() {
        let (mine, other) = (PathBuf::from("/p/a"), PathBuf::from("/p/b"));
        let local = AppConfig::default();
        let mut synced = TabsProj::of(&local, Some(&mine));
        let mut edited = local.clone();
        edited.folder_tabs.insert(mine.clone(), folder(1));
        let live = TabsProj::of(&edited, Some(&mine));
        // A conflict on another folder's key: our entry is not acknowledged and stays pending.
        synced.yield_fields(&live, &[Field::FolderTabs(other.clone())]);
        assert_eq!(synced.folder, None);
        let mut committed = local.clone();
        committed.folder_tabs.insert(other, folder(5));
        let mut resend = committed.clone();
        resend.folder_tabs.insert(mine.clone(), folder(1));
        synced.mask_unchanged(&mut resend, &committed);
        assert_eq!(
            resend.folder_tabs.get(&mine),
            Some(&folder(1)),
            "still sent"
        );
        // A conflict on our own key: the other value stands, the live entry is the baseline.
        synced.yield_fields(&live, &[Field::FolderTabs(mine)]);
        assert_eq!(synced.folder, Some(folder(1)));
    }

    #[test]
    fn geometry_falls_back_to_viewport_size_without_inner_rect() {
        // Wayland: no inner/outer rect, only the viewport size.
        let g = update_geometry(None, None, None, egui::vec2(900.0, 700.0), false);
        assert_eq!(
            g,
            WindowGeometry {
                x: None,
                y: None,
                width: 900.0,
                height: 700.0,
                maximized: false
            }
        );
        // Resize updates; maximized keeps the previous size.
        let g2 = update_geometry(Some(g), None, None, egui::vec2(1000.0, 600.0), false);
        assert_eq!((g2.width, g2.height), (1000.0, 600.0));
        let g3 = update_geometry(Some(g2), None, None, egui::vec2(1920.0, 1080.0), true);
        assert_eq!((g3.width, g3.height, g3.maximized), (1000.0, 600.0, true));
    }

    #[test]
    fn window_geometry_is_stored_independent_of_the_app_zoom() {
        // One 1280×800-pixel window reaches egui as fewer points as the zoom grows; both
        // conversions store the same native size and position.
        let inner_100 =
            egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(1280.0, 800.0));
        let inner_130 = egui::Rect::from_min_size(
            egui::pos2(100.0 / 1.3, 50.0 / 1.3),
            egui::vec2(1280.0 / 1.3, 800.0 / 1.3),
        );
        let at_100 = update_geometry(
            None,
            Some(native_points(inner_100, 1.0)),
            Some(native_points(inner_100, 1.0)),
            egui::Vec2::ZERO,
            false,
        );
        let at_130 = update_geometry(
            None,
            Some(native_points(inner_130, 1.3)),
            Some(native_points(inner_130, 1.3)),
            egui::Vec2::ZERO,
            false,
        );
        for g in [&at_100, &at_130] {
            assert!((g.width - 1280.0).abs() < 0.01, "{g:?}");
            assert!((g.height - 800.0).abs() < 0.01, "{g:?}");
            assert!((g.x.unwrap() - 100.0).abs() < 0.01, "{g:?}");
            assert!((g.y.unwrap() - 50.0).abs() < 0.01, "{g:?}");
        }
    }

    #[test]
    fn url_scheme_allowlist() {
        for ok in [
            "http://a.b",
            "https://a.b/x?y",
            "HTTPS://A.B",
            "mailto:a@b.c",
            "MailTo:a@b.c",
        ] {
            assert!(is_openable_url(ok), "{ok}");
        }
        for bad in [
            "",
            "file:///etc/passwd",
            "-o",
            "--help",
            "-http://x",
            "javascript:alert(1)",
            "ftp://x",
            "http",
            "http:",
            "httpx://x",
            " http://x",
            "/etc/passwd",
            "ssh://h",
            "data:text/html,x",
            "x-http://y",
        ] {
            assert!(!is_openable_url(bad), "{bad:?}");
        }
    }
}

#[cfg(test)]
mod chat_link_tests {
    use super::{chat_file_path, is_openable_url};
    use std::path::PathBuf;

    #[test]
    fn local_file_links_decode_paths_and_drop_authority() {
        #[cfg(unix)]
        let (href, expected) = ("file:///tmp/a%20b%23c.rs", "/tmp/a b#c.rs");
        #[cfg(windows)]
        let (href, expected) = ("file:///C:/tmp/a%20b%23c.rs", "C:/tmp/a b#c.rs");
        assert_eq!(chat_file_path(href), Some(PathBuf::from(expected)));
        assert_eq!(
            chat_file_path(&href.replace("file:///", "FILE://localhost/")),
            Some(PathBuf::from(expected))
        );
        assert_eq!(
            chat_file_path(&href.replace("file:///", "file://desktop/")),
            Some(PathBuf::from(expected))
        );
        assert_eq!(
            chat_file_path(&format!("{href}?line=2#anchor")),
            Some(PathBuf::from(expected))
        );
        assert!(
            !is_openable_url(href),
            "ordinary terminal URL policy must not widen"
        );
    }

    #[test]
    fn invalid_file_links_and_other_schemes_are_not_paths() {
        for href in [
            "file://",
            "file:relative",
            "file://localhost",
            "file:///tmp/%",
            "file:///tmp/%XZ",
            "file:///tmp/%00x",
            "https://example.com/file",
            "javascript:alert(1)",
        ] {
            assert_eq!(chat_file_path(href), None, "{href}");
        }
    }
}
