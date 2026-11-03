//! Persistent app configuration (`$XDG_CONFIG_HOME/omp-deck/state.json`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::backup::Destination;
use crate::mux::BackendChoice;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config I/O error at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("config serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

const MAX_RECENT_FOLDERS: usize = 10;

/// `omp-deck` under the platform config directory (`dirs::config_dir()`: `$XDG_CONFIG_HOME` or
/// `~/.config`, `%APPDATA%`). Without any config directory the path is relative to the cwd.
pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_default().join("omp-deck")
}

fn state_path() -> PathBuf {
    config_dir().join("state.json")
}

/// Decodes stored settings with the same legacy migrations as [`AppConfig::load_from`].
/// Unlike the convenience loader, invalid JSON or fields are returned as an error; no defaults
/// replace unreadable settings. This lets transactional writers preserve a failed read.
pub fn parse_config(bytes: &[u8]) -> Result<AppConfig, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes)?;
    if let Some(map) = value.as_object_mut() {
        let legacy = map.remove("drive_auto_backup");
        if legacy == Some(serde_json::Value::Bool(false))
            && !map.contains_key("drive_backup_interval")
        {
            map.insert("drive_backup_interval".into(), "Off".into());
        }
        if !map.contains_key("style_rev") {
            let light = map.get("theme").and_then(|t| t.as_str()) == Some("Light");
            let neutral_or_absent = map
                .get("style")
                .is_none_or(|s| s.as_str() == Some("Neutral"));
            if neutral_or_absent {
                // A light-theme user keeps Neutral (Blueprint is dark-only).
                map.insert(
                    "style".into(),
                    if light { "Neutral" } else { "Blueprint" }.into(),
                );
            }
            map.insert("style_rev".into(), STYLE_REV.into());
        }
        if !map.contains_key("view_rev") {
            map.insert("session_view".into(), "chat".into());
            map.insert("view_rev".into(), VIEW_REV.into());
        }
    }
    let mut cfg: AppConfig = serde_json::from_value(value)?;
    cfg.plain_folder_paths();
    cfg.ui_scale = clamp_ui_scale(cfg.ui_scale);
    Ok(cfg)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppConfig {
    pub window: Option<WindowGeometry>,
    pub sidebar_width: f32,
    pub font_size: f32,
    pub theme: Theme,
    pub open_tabs: Vec<TabState>,
    pub active_tab: Option<usize>,
    pub style: UiStyle,
    /// Style-set version this file was written with (see [`STYLE_REV`]).
    pub style_rev: u32,
    pub density: Density,
    /// Typeface of the chrome's UI text (`Auto` = the style's own font).
    pub ui_font: UiFont,
    /// Scale of the whole app (egui zoom: 1.0 = 100 %). The terminal compensates so its own
    /// font size stays absolute.
    pub ui_scale: f32,
    /// Sidebar and main area as separate rounded, bordered panels on the window backdrop
    /// (VS Code-like) in the styles that otherwise divide them with a line.
    pub separated_panels: bool,
    /// false → OmpDeck draws its own title bar; true → the window manager's frame.
    pub native_decorations: bool,
    /// Run `omp update --check` at startup and every 6 h.
    pub check_updates: bool,
    /// `ModelInfo.selector`s, insertion order.
    pub favorite_models: Vec<String>,
    /// `None` = "All providers".
    pub model_provider_filter: Option<String>,
    /// Last sidebar scope (`None` = global view).
    pub folder_scope: Option<PathBuf>,
    /// Most-recent first, deduped, at most 10.
    pub recent_folders: Vec<PathBuf>,
    /// Per-folder open tabs (folder mode launch/restore).
    pub folder_tabs: BTreeMap<PathBuf, FolderTabs>,
    /// Back up the omp config automatically when it changed (while the destination is set up).
    pub drive_backup_interval: SyncInterval,
    /// Where config backups and project sync go (one destination at a time).
    pub backup_destination: Destination,
    /// Project folders backed up to the active destination (Settings → Sync), in table order.
    pub project_sync: Vec<SyncRow>,
    /// How often the rows are backed up automatically.
    pub project_sync_interval: SyncInterval,
    /// Which session backend new tabs use (live sessions keep theirs until closed).
    pub session_backend: BackendChoice,
    /// Worktree path → base branch recorded when OmpDeck created / moved a session into it;
    /// pruned when the worktree is removed.
    pub worktree_bases: BTreeMap<PathBuf, String>,
    /// Worktrees OmpDeck removed, keyed by their former path (so their sessions can be grouped
    /// under the primary repo).
    pub removed_worktrees: BTreeMap<PathBuf, RemovedWorktree>,
    /// Worktrees parked before their PR was merged (branch kept; never deleted by OmpDeck).
    pub parked_branches: Vec<ParkedBranch>,
    /// Desktop notification settings.
    pub notify: NotifyConfig,
    /// Look for new OmpDeck releases at startup and every 6 h.
    pub check_app_updates: bool,
    /// Version the user chose to skip ("Skip this version").
    pub skipped_app_version: Option<String>,
    /// Command-palette command ids, newest first, at most [`MAX_PALETTE_RECENT`].
    pub palette_recent: Vec<String>,
    /// Width of the changes panel in points (0 = default).
    pub changes_panel_width: f32,
    /// Split-view layout of the global tab set (`panes` index into `open_tabs`).
    pub split: Option<SplitState>,
    /// Lifetime of the per-user mobile-companion gateway (`Off` for configs that predate it).
    pub gateway_mode: GatewayMode,
    /// How new tabs open: the terminal grid or the native chat view (omp's TSP surface).
    pub session_view: SessionView,
    /// New-tab view version this file was written with (see [`VIEW_REV`]).
    pub view_rev: u32,
}

/// How a session is shown. `Chat` runs omp in Tern Surface Protocol mode on the OmpDeck session
/// host and renders its surface natively; `Terminal` is the plain terminal grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionView {
    #[default]
    Terminal,
    Chat,
}

/// How long the per-user gateway owner lives. `Off` disables mobile access and autostart (the
/// native client still uses a private local gateway); `WhileOpen` keeps it while a GUI or accepted
/// work remains; `Background` additionally keeps it supervised across logins and crashes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GatewayMode {
    #[default]
    Off,
    WhileOpen,
    Background,
}

impl GatewayMode {
    pub const ALL: [GatewayMode; 3] = [
        GatewayMode::Off,
        GatewayMode::WhileOpen,
        GatewayMode::Background,
    ];

    /// The lifetime chosen when mobile access is switched on without an explicit choice.
    pub const ENABLED_DEFAULT: GatewayMode = GatewayMode::Background;

    /// Mobile access (and so the gateway lifetime beyond the GUI) is enabled.
    pub fn enabled(self) -> bool {
        self != GatewayMode::Off
    }

    /// An OS autostart/supervision registration is wanted.
    pub fn supervised(self) -> bool {
        self == GatewayMode::Background
    }

    pub fn label(self) -> &'static str {
        match self {
            GatewayMode::Off => "Off",
            GatewayMode::WhileOpen => "While OmpDeck is open",
            GatewayMode::Background => "In the background",
        }
    }
}

/// Most palette commands remembered in [`AppConfig::palette_recent`].
pub const MAX_PALETTE_RECENT: usize = 20;

/// Which events raise a desktop notification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    /// Desktop pop-ups (the in-app inbox is governed by the per-event toggles only).
    #[serde(alias = "enabled")]
    pub desktop: bool,
    /// A busy tab became idle (turn finished).
    pub turn_finished: bool,
    /// A tab needs attention (question / approval).
    pub attention: bool,
    /// A tab's process exited.
    pub exited: bool,
    /// `false` also notifies for background tabs while the window has focus.
    pub only_when_unfocused: bool,
    /// The in-app inbox (title-bar bell) also records events for the tab being looked at.
    pub while_viewing: bool,
    /// Desktop pop-ups ask the notification server for a sound (Linux; Windows toasts stay silent).
    pub sound: bool,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        NotifyConfig {
            desktop: true,
            turn_finished: true,
            attention: true,
            exited: true,
            only_when_unfocused: true,
            while_viewing: true,
            sound: true,
        }
    }
}

/// What is remembered about a removed worktree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RemovedWorktree {
    pub primary: PathBuf,
    pub branch: Option<String>,
}

/// A worktree parked before its PR was merged: the folder is gone, the local branch is kept
/// (OmpDeck never deletes it). Key = `primary` + `branch`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ParkedBranch {
    pub primary: PathBuf,
    pub branch: String,
    /// The base the worktree was created from / finished into.
    pub base: String,
    /// Ids of every session of the worktree when it was parked.
    pub sessions: Vec<String>,
    pub parked_ms: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            window: None,
            sidebar_width: 300.0,
            font_size: 14.0,
            theme: Theme::Dark,
            open_tabs: Vec::new(),
            active_tab: None,
            style: UiStyle::default(),
            style_rev: STYLE_REV,
            density: Density::default(),
            ui_font: UiFont::default(),
            ui_scale: 1.0,
            separated_panels: true,
            native_decorations: false,
            check_updates: true,
            favorite_models: Vec::new(),
            model_provider_filter: None,
            folder_scope: None,
            recent_folders: Vec::new(),
            folder_tabs: BTreeMap::new(),
            drive_backup_interval: SyncInterval::Every10Minutes,
            backup_destination: Destination::default(),
            project_sync: Vec::new(),
            project_sync_interval: SyncInterval::default(),
            session_backend: BackendChoice::default(),
            worktree_bases: BTreeMap::new(),
            notify: NotifyConfig::default(),
            check_app_updates: true,
            skipped_app_version: None,
            palette_recent: Vec::new(),
            changes_panel_width: 0.0,
            removed_worktrees: BTreeMap::new(),
            parked_branches: Vec::new(),
            split: None,
            gateway_mode: GatewayMode::Off,
            session_view: SessionView::Chat,
            view_rev: VIEW_REV,
        }
    }
}

impl AppConfig {
    /// Whether to render dark: the `Dark` theme, or any dark-only style regardless of theme.
    pub fn dark(&self) -> bool {
        self.theme == Theme::Dark || self.style.dark_only()
    }
}

/// One project folder of the Sync page: which parts of it are kept on Drive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRow {
    pub path: PathBuf,
    /// The project's `<folder>/.omp` directory.
    pub omp: bool,
    /// The project's omp sessions.
    pub sessions: bool,
    /// The project's omp "sharpshooter" memory bank (off for new rows).
    #[serde(default)]
    pub memory: bool,
    /// Session ids opted out of the backup; every other session (including ones created later) is
    /// backed up.
    #[serde(default)]
    pub excluded_sessions: BTreeSet<String>,
}

/// How often the Sync rows are backed up automatically.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncInterval {
    Off,
    Every10Minutes,
    Every30Minutes,
    Hourly,
    #[default]
    Every6Hours,
    Daily,
}

impl SyncInterval {
    pub const ALL: [SyncInterval; 6] = [
        SyncInterval::Off,
        SyncInterval::Every10Minutes,
        SyncInterval::Every30Minutes,
        SyncInterval::Hourly,
        SyncInterval::Every6Hours,
        SyncInterval::Daily,
    ];

    /// `None` = automatic backup is off.
    pub fn period(self) -> Option<Duration> {
        let minutes = match self {
            SyncInterval::Off => return None,
            SyncInterval::Every10Minutes => 10,
            SyncInterval::Every30Minutes => 30,
            SyncInterval::Hourly => 60,
            SyncInterval::Every6Hours => 6 * 60,
            SyncInterval::Daily => 24 * 60,
        };
        Some(Duration::from_secs(minutes * 60))
    }

    pub fn label(self) -> &'static str {
        match self {
            SyncInterval::Off => "Off",
            SyncInterval::Every10Minutes => "Every 10 minutes",
            SyncInterval::Every30Minutes => "Every 30 minutes",
            SyncInterval::Hourly => "Every hour",
            SyncInterval::Every6Hours => "Every 6 hours",
            SyncInterval::Daily => "Every day",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowGeometry {
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// Version of the style set; configs saved before it existed are migrated in `parse_config`.
pub const STYLE_REV: u32 = 1;

/// Version of the new-tab view default; older configs migrate to Chat in [`parse_config`].
pub const VIEW_REV: u32 = 1;

/// Smallest and largest [`AppConfig::ui_scale`] (80 %–160 %).
pub const MIN_UI_SCALE: f32 = 0.8;
pub const MAX_UI_SCALE: f32 = 1.6;

/// `scale` limited to [`MIN_UI_SCALE`]..=[`MAX_UI_SCALE`]; non-finite values mean 100 %.
pub fn clamp_ui_scale(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.clamp(MIN_UI_SCALE, MAX_UI_SCALE)
    } else {
        1.0
    }
}

/// Visual style axis (second to light/dark `Theme`).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum UiStyle {
    Neutral,
    Neon,
    ZincContrast,
    LayeredDepth,
    TealSignal,
    SlateInk,
    MonoWire,
    GraphiteIslands,
    Aurora,
    ObsidianGlass,
    #[default]
    Blueprint,
    Ember,
    IndigoNight,
    Phosphor,
    NeoBrutal,
    RoseNoir,
    Spotlight,
    TwoToneFrame,
}

impl UiStyle {
    /// Every style in picker order (Blueprint, Neutral, Neon, then the gallery).
    pub const ALL: [UiStyle; 18] = [
        UiStyle::Blueprint,
        UiStyle::Neutral,
        UiStyle::Neon,
        UiStyle::ZincContrast,
        UiStyle::LayeredDepth,
        UiStyle::TealSignal,
        UiStyle::SlateInk,
        UiStyle::MonoWire,
        UiStyle::GraphiteIslands,
        UiStyle::Aurora,
        UiStyle::ObsidianGlass,
        UiStyle::Ember,
        UiStyle::IndigoNight,
        UiStyle::Phosphor,
        UiStyle::NeoBrutal,
        UiStyle::RoseNoir,
        UiStyle::Spotlight,
        UiStyle::TwoToneFrame,
    ];

    pub fn label(self) -> &'static str {
        match self {
            UiStyle::Neutral => "Neutral",
            UiStyle::Neon => "Neon",
            UiStyle::ZincContrast => "Zinc Contrast",
            UiStyle::LayeredDepth => "Layered Depth",
            UiStyle::TealSignal => "Teal Signal",
            UiStyle::SlateInk => "Slate Ink",
            UiStyle::MonoWire => "Mono Wire",
            UiStyle::GraphiteIslands => "Graphite Islands",
            UiStyle::Aurora => "Aurora",
            UiStyle::ObsidianGlass => "Obsidian Glass",
            UiStyle::Blueprint => "Blueprint",
            UiStyle::Ember => "Ember",
            UiStyle::IndigoNight => "Indigo Night",
            UiStyle::Phosphor => "Phosphor",
            UiStyle::NeoBrutal => "Neo-Brutal",
            UiStyle::RoseNoir => "Rosé Noir",
            UiStyle::Spotlight => "Spotlight",
            UiStyle::TwoToneFrame => "Two-Tone Frame",
        }
    }

    /// Stable snake_case identifier.
    pub fn key(self) -> &'static str {
        match self {
            UiStyle::Neutral => "neutral",
            UiStyle::Neon => "neon",
            UiStyle::ZincContrast => "zinc_contrast",
            UiStyle::LayeredDepth => "layered_depth",
            UiStyle::TealSignal => "teal_signal",
            UiStyle::SlateInk => "slate_ink",
            UiStyle::MonoWire => "mono_wire",
            UiStyle::GraphiteIslands => "graphite_islands",
            UiStyle::Aurora => "aurora",
            UiStyle::ObsidianGlass => "obsidian_glass",
            UiStyle::Blueprint => "blueprint",
            UiStyle::Ember => "ember",
            UiStyle::IndigoNight => "indigo_night",
            UiStyle::Phosphor => "phosphor",
            UiStyle::NeoBrutal => "neo_brutal",
            UiStyle::RoseNoir => "rose_noir",
            UiStyle::Spotlight => "spotlight",
            UiStyle::TwoToneFrame => "two_tone_frame",
        }
    }

    /// Styles that only exist as dark palettes (the light/dark `Theme` is ignored).
    pub fn dark_only(self) -> bool {
        !matches!(self, UiStyle::Neutral | UiStyle::Neon)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Density {
    #[default]
    Compact,
    Comfortable,
}

/// Typeface of the chrome's UI text. `Auto` keeps each style's own font (JetBrains Mono for
/// the flat styles, the system sans for Neon, monospace for Phosphor); `System` is the platform
/// UI sans; every other variant is a bundled face used for every style.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum UiFont {
    #[default]
    Auto,
    System,
    #[serde(rename = "jetbrains-mono")]
    JetBrainsMono,
    Geist,
    Inter,
    IbmPlexSans,
    Manrope,
    SpaceGrotesk,
    PlusJakartaSans,
    Onest,
    GeistMono,
    RedHatMono,
    MartianMono,
    Outfit,
    Sora,
    BricolageGrotesque,
    Lexend,
    Figtree,
    Urbanist,
    VictorMono,
}

/// Grouping of a [`UiFont`] in the picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiFontKind {
    /// `Auto` and `System`: not one fixed bundled face.
    Default,
    Sans,
    Mono,
}

impl UiFont {
    /// Every choice in picker order: the defaults, then sans faces, then mono faces.
    pub const ALL: [UiFont; 20] = [
        UiFont::Auto,
        UiFont::System,
        UiFont::Inter,
        UiFont::Geist,
        UiFont::IbmPlexSans,
        UiFont::Manrope,
        UiFont::SpaceGrotesk,
        UiFont::PlusJakartaSans,
        UiFont::Onest,
        UiFont::Outfit,
        UiFont::Sora,
        UiFont::BricolageGrotesque,
        UiFont::Lexend,
        UiFont::Figtree,
        UiFont::Urbanist,
        UiFont::JetBrainsMono,
        UiFont::GeistMono,
        UiFont::RedHatMono,
        UiFont::MartianMono,
        UiFont::VictorMono,
    ];

    pub fn label(self) -> &'static str {
        match self {
            UiFont::Auto => "Style default",
            UiFont::System => "System",
            UiFont::JetBrainsMono => "JetBrains Mono",
            UiFont::Geist => "Geist",
            UiFont::Inter => "Inter",
            UiFont::IbmPlexSans => "IBM Plex Sans",
            UiFont::Manrope => "Manrope",
            UiFont::SpaceGrotesk => "Space Grotesk",
            UiFont::PlusJakartaSans => "Plus Jakarta Sans",
            UiFont::Onest => "Onest",
            UiFont::GeistMono => "Geist Mono",
            UiFont::RedHatMono => "Red Hat Mono",
            UiFont::MartianMono => "Martian Mono",
            UiFont::Outfit => "Outfit",
            UiFont::Sora => "Sora",
            UiFont::BricolageGrotesque => "Bricolage Grotesque",
            UiFont::Lexend => "Lexend",
            UiFont::Figtree => "Figtree",
            UiFont::Urbanist => "Urbanist",
            UiFont::VictorMono => "Victor Mono",
        }
    }

    /// Stable kebab-case identifier (the on-disk value).
    pub fn key(self) -> &'static str {
        match self {
            UiFont::Auto => "auto",
            UiFont::System => "system",
            UiFont::JetBrainsMono => "jetbrains-mono",
            UiFont::Geist => "geist",
            UiFont::Inter => "inter",
            UiFont::IbmPlexSans => "ibm-plex-sans",
            UiFont::Manrope => "manrope",
            UiFont::SpaceGrotesk => "space-grotesk",
            UiFont::PlusJakartaSans => "plus-jakarta-sans",
            UiFont::Onest => "onest",
            UiFont::GeistMono => "geist-mono",
            UiFont::RedHatMono => "red-hat-mono",
            UiFont::MartianMono => "martian-mono",
            UiFont::Outfit => "outfit",
            UiFont::Sora => "sora",
            UiFont::BricolageGrotesque => "bricolage-grotesque",
            UiFont::Lexend => "lexend",
            UiFont::Figtree => "figtree",
            UiFont::Urbanist => "urbanist",
            UiFont::VictorMono => "victor-mono",
        }
    }

    pub fn kind(self) -> UiFontKind {
        match self {
            UiFont::Auto | UiFont::System => UiFontKind::Default,
            UiFont::JetBrainsMono
            | UiFont::GeistMono
            | UiFont::RedHatMono
            | UiFont::MartianMono
            | UiFont::VictorMono => UiFontKind::Mono,
            _ => UiFontKind::Sans,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TabState {
    /// Name of the tab's backend session (tmux session or host session). Old state files call
    /// it `tmux_name`.
    #[serde(alias = "tmux_name")]
    pub session_name: String,
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,
    pub title: String,
    /// The view the tab was opened in; used when a restore has to create the session again.
    #[serde(default)]
    pub view: SessionView,
}

/// Open tabs remembered per folder (folder-mode launch/restore).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FolderTabs {
    pub open_tabs: Vec<TabState>,
    pub active_tab: Option<usize>,
    /// Split-view layout of this folder's tabs (`panes` index into `open_tabs`).
    pub split: Option<SplitState>,
}

/// Pane arrangement of the split view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitLayout {
    #[default]
    Single,
    Columns2,
    Rows2,
    Columns3,
    Grid4,
}

/// Persisted split view: one slot per pane, each an index into the persisted `open_tabs`
/// (`None` = empty slot); dangling indices are dropped on restore.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SplitState {
    pub layout: SplitLayout,
    pub panes: Vec<Option<usize>>,
    pub focused: usize,
    pub ratios: Vec<f32>,
}

impl AppConfig {
    pub fn load() -> AppConfig {
        Self::load_from(&state_path())
    }

    /// Missing file → default silently; unreadable or corrupt → default, logged.
    pub fn load_from(path: &Path) -> AppConfig {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    log::warn!("cannot read config {}: {e}", path.display());
                }
                return AppConfig::default();
            }
        };
        match parse_config(&bytes) {
            Ok(cfg) => cfg,
            Err(e) => {
                log::warn!("corrupt config {}: {e}; using defaults", path.display());
                AppConfig::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&state_path())
    }

    /// Atomic: write a sibling temp file, then rename over the target.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let io_err = |source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        };
        let json = serde_json::to_vec_pretty(self)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(io_err)?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        fs::write(&tmp, json).map_err(io_err)?;
        fs::rename(&tmp, path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            io_err(e)
        })
    }

    /// Toggles `selector` in `favorite_models`; returns the new state (true = favorite).
    pub fn toggle_favorite(&mut self, selector: &str) -> bool {
        if let Some(i) = self.favorite_models.iter().position(|s| s == selector) {
            self.favorite_models.remove(i);
            false
        } else {
            self.favorite_models.push(selector.to_owned());
            true
        }
    }

    /// Moves `dir` to the front of `recent_folders`, deduped, capped at 10.
    pub fn push_recent_folder(&mut self, dir: PathBuf) {
        self.recent_folders.retain(|p| *p != dir);
        self.recent_folders.insert(0, dir);
        self.recent_folders.truncate(MAX_RECENT_FOLDERS);
    }

    /// Older versions stored folders picked or typed on Windows in the verbatim form
    /// (`\\?\C:\x`, from `canonicalize`): it matches no session cwd and displays malformed. The
    /// same goes for the cwd of a tab started in such a folder. Rewrites them to the plain form;
    /// where both forms exist the plain entry wins.
    fn plain_folder_paths(&mut self) {
        use crate::platform::strip_verbatim;
        self.folder_scope = self.folder_scope.take().map(strip_verbatim);
        let mut recent: Vec<PathBuf> = Vec::with_capacity(self.recent_folders.len());
        for dir in std::mem::take(&mut self.recent_folders) {
            let dir = strip_verbatim(dir);
            if !recent.contains(&dir) {
                recent.push(dir);
            }
        }
        self.recent_folders = recent;
        let mut tabs = BTreeMap::new();
        let mut converted = Vec::new();
        for (dir, folder) in std::mem::take(&mut self.folder_tabs) {
            let plain = strip_verbatim(dir.clone());
            if plain == dir {
                tabs.insert(dir, folder);
            } else {
                converted.push((plain, folder));
            }
        }
        for (dir, folder) in converted {
            tabs.entry(dir).or_insert(folder);
        }
        fn plain_tab(tab: &mut TabState) {
            tab.cwd = strip_verbatim(std::mem::take(&mut tab.cwd));
        }
        self.open_tabs.iter_mut().for_each(plain_tab);
        for folder in tabs.values_mut() {
            folder.open_tabs.iter_mut().for_each(plain_tab);
        }
        self.folder_tabs = tabs;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AppConfig {
        AppConfig {
            window: Some(WindowGeometry {
                x: Some(10.0),
                y: None,
                width: 1200.0,
                height: 800.0,
                maximized: true,
            }),
            sidebar_width: 321.0,
            font_size: 16.5,
            theme: Theme::Light,
            open_tabs: vec![TabState {
                session_name: "omp-abc".into(),
                cwd: "/home/u/x".into(),
                session_path: Some("/s/a.jsonl".into()),
                title: "T".into(),
                view: SessionView::Chat,
            }],
            active_tab: Some(0),
            style: UiStyle::Neon,
            native_decorations: true,
            check_updates: false,
            ..AppConfig::default()
        }
    }

    #[test]
    fn old_state_without_wave2_keys_gets_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, r#"{"sidebar_width": 250.0, "font_size": 12.0}"#).unwrap();
        let cfg = AppConfig::load_from(&path);
        assert_eq!(cfg.sidebar_width, 250.0);
        assert!(!cfg.native_decorations);
        assert!(cfg.check_updates);
    }

    #[test]
    fn gateway_mode_defaults_off_and_survives_a_roundtrip() {
        assert_eq!(
            parse_config(br#"{"font_size": 12.0}"#)
                .unwrap()
                .gateway_mode,
            GatewayMode::Off
        );
        let cfg = AppConfig {
            gateway_mode: GatewayMode::Background,
            ..AppConfig::default()
        };
        let bytes = serde_json::to_vec(&cfg).unwrap();
        assert_eq!(
            parse_config(&bytes).unwrap().gateway_mode,
            GatewayMode::Background
        );
        assert!(parse_config(br#"{"gateway_mode": "Nope"}"#).is_err());
    }

    #[test]
    fn worktree_bases_default_empty_and_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, r#"{"font_size": 12.0}"#).unwrap();
        let old = AppConfig::load_from(&path);
        assert!(old.worktree_bases.is_empty() && old.removed_worktrees.is_empty());
        assert!(old.parked_branches.is_empty());
        let mut cfg = AppConfig::default();
        cfg.worktree_bases.insert("/wt/a".into(), "main".into());
        cfg.parked_branches.push(ParkedBranch {
            primary: "/repo".into(),
            branch: "wt/x".into(),
            base: "main".into(),
            sessions: vec!["s1".into(), "s2".into()],
            parked_ms: 1_700_000_000_000,
        });
        cfg.removed_worktrees.insert(
            "/wt/b".into(),
            RemovedWorktree {
                primary: "/repo".into(),
                branch: Some("b".into()),
            },
        );
        cfg.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path), cfg);
    }

    #[test]
    fn roundtrip_creates_dirs_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/state.json");
        sample().save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path), sample());
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("state.json")]);
    }

    #[test]
    fn missing_or_corrupt_gives_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            AppConfig::load_from(&dir.path().join("nope.json")),
            AppConfig::default()
        );
        let bad = dir.path().join("bad.json");
        fs::write(&bad, b"{ not json").unwrap();
        assert_eq!(AppConfig::load_from(&bad), AppConfig::default());
    }

    #[test]
    fn partial_json_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        fs::write(
            &p,
            br#"{"font_size": 20.0, "theme": "Light", "future_field": 1}"#,
        )
        .unwrap();
        let cfg = AppConfig::load_from(&p);
        assert_eq!(cfg.font_size, 20.0);
        assert_eq!(cfg.theme, Theme::Light);
        assert_eq!(cfg.sidebar_width, 300.0);
        assert!(cfg.open_tabs.is_empty());
    }

    #[test]
    fn old_state_without_wave4_keys_gets_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        fs::write(&p, br#"{"open_tabs": [], "check_updates": false}"#).unwrap();
        let cfg = AppConfig::load_from(&p);
        assert!(!cfg.check_updates);
        assert!(cfg.favorite_models.is_empty());
        assert_eq!(cfg.model_provider_filter, None);
        assert_eq!(cfg.folder_scope, None);
        assert!(cfg.recent_folders.is_empty());
        assert!(cfg.folder_tabs.is_empty());
    }

    #[test]
    fn old_state_gets_neutral_compact_and_ignores_collapsed_groups() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        fs::write(
            &p,
            br#"{"theme": "Light", "collapsed_groups": ["/home/u/y"], "font_size": 15.0}"#,
        )
        .unwrap();
        let cfg = AppConfig::load_from(&p);
        assert_eq!(cfg.style, UiStyle::Neutral);
        assert_eq!(cfg.density, Density::Compact);
        assert_eq!(cfg.theme, Theme::Light);
        assert_eq!(cfg.font_size, 15.0);
    }

    #[test]
    fn style_and_density_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        let cfg = AppConfig {
            style: UiStyle::Neon,
            density: Density::Comfortable,
            ..AppConfig::default()
        };
        cfg.save_to(&p).unwrap();
        assert_eq!(AppConfig::load_from(&p), cfg);
    }

    #[test]
    fn ui_font_defaults_to_auto_and_roundtrips_under_its_key() {
        let old =
            parse_config(br#"{"style": "Neon", "style_rev": 1, "density": "Compact"}"#).unwrap();
        assert_eq!(old.ui_font, UiFont::Auto);
        for font in UiFont::ALL {
            let cfg = AppConfig {
                ui_font: font,
                ..AppConfig::default()
            };
            let json = serde_json::to_value(&cfg).unwrap();
            assert_eq!(json["ui_font"], font.key());
            let back = parse_config(json.to_string().as_bytes()).unwrap();
            assert_eq!(back.ui_font, font);
        }
    }

    #[test]
    fn toggle_favorite_twice_restores() {
        let mut cfg = AppConfig::default();
        assert!(cfg.toggle_favorite("a/x"));
        assert!(cfg.toggle_favorite("b/y"));
        assert_eq!(cfg.favorite_models, vec!["a/x", "b/y"]);
        assert!(!cfg.toggle_favorite("a/x"));
        assert_eq!(cfg.favorite_models, vec!["b/y"]);
        assert!(cfg.toggle_favorite("a/x"));
        assert_eq!(cfg.favorite_models, vec!["b/y", "a/x"]);
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_folder_paths_from_old_versions_are_stored_plain() {
        let json = r#"{
            "open_tabs": [{"session_name": "a", "cwd": "\\\\?\\E:\\p\\x", "session_path": null, "title": ""}],
            "folder_scope": "\\\\?\\E:\\p",
            "recent_folders": ["\\\\?\\E:\\p\\x", "E:\\p\\x", "\\\\?\\UNC\\srv\\share\\y"],
            "folder_tabs": {
                "\\\\?\\E:\\p": {"open_tabs": [{"session_name": "b", "cwd": "\\\\?\\E:\\p", "session_path": null, "title": ""}], "active_tab": 3},
                "E:\\p": {"open_tabs": [], "active_tab": 1}
            }
        }"#;
        let cfg = parse_config(json.as_bytes()).expect("parse");
        assert_eq!(cfg.open_tabs[0].cwd, PathBuf::from(r"E:\p\x"));
        assert_eq!(cfg.folder_scope, Some(PathBuf::from(r"E:\p")));
        assert_eq!(
            cfg.recent_folders,
            [PathBuf::from(r"E:\p\x"), PathBuf::from(r"\\srv\share\y")]
        );
        assert_eq!(cfg.folder_tabs.len(), 1);
        assert_eq!(cfg.folder_tabs[Path::new(r"E:\p")].active_tab, Some(1));
    }

    #[test]
    fn recent_folders_dedupe_front_cap() {
        let mut cfg = AppConfig::default();
        for i in 0..12 {
            cfg.push_recent_folder(PathBuf::from(format!("/d/{i}")));
        }
        assert_eq!(cfg.recent_folders.len(), 10);
        assert_eq!(cfg.recent_folders[0], PathBuf::from("/d/11"));
        assert_eq!(cfg.recent_folders[9], PathBuf::from("/d/2"));
        cfg.push_recent_folder(PathBuf::from("/d/5"));
        assert_eq!(cfg.recent_folders.len(), 10);
        assert_eq!(cfg.recent_folders[0], PathBuf::from("/d/5"));
        assert_eq!(
            cfg.recent_folders
                .iter()
                .filter(|p| p.as_path() == Path::new("/d/5"))
                .count(),
            1
        );
    }

    #[test]
    fn folder_tabs_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut cfg = sample();
        cfg.folder_tabs.insert(
            "/home/u/x".into(),
            FolderTabs {
                open_tabs: cfg.open_tabs.clone(),
                active_tab: Some(0),
                split: None,
            },
        );
        cfg.folder_scope = Some("/home/u/x".into());
        cfg.model_provider_filter = Some("openai".into());
        cfg.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path), cfg);
    }

    #[test]
    fn split_state_roundtrips_and_old_state_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        fs::write(
            &p,
            br#"{"open_tabs": [], "folder_tabs": {"/w": {"open_tabs": []}}}"#,
        )
        .unwrap();
        let cfg = AppConfig::load_from(&p);
        assert_eq!(cfg.split, None);
        assert_eq!(cfg.folder_tabs[Path::new("/w")].split, None);

        let state = SplitState {
            layout: SplitLayout::Grid4,
            panes: vec![Some(0), None, Some(2), Some(1)],
            focused: 2,
            ratios: vec![0.4, 0.6],
        };
        let mut cfg = sample();
        cfg.split = Some(state.clone());
        cfg.folder_tabs.insert(
            "/w".into(),
            FolderTabs {
                split: Some(state),
                ..FolderTabs::default()
            },
        );
        cfg.save_to(&p).unwrap();
        assert!(fs::read_to_string(&p).unwrap().contains("\"grid4\""));
        assert_eq!(AppConfig::load_from(&p), cfg);
    }

    #[test]
    fn old_state_without_sync_keys_gets_defaults_and_sync_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        fs::write(&p, br#"{"open_tabs": []}"#).unwrap();
        let cfg = AppConfig::load_from(&p);
        assert!(cfg.project_sync.is_empty());
        assert_eq!(cfg.project_sync_interval, SyncInterval::Every6Hours);

        let mut cfg = sample();
        cfg.project_sync = vec![SyncRow {
            path: "/home/u/x".into(),
            omp: true,
            sessions: false,
            memory: true,
            excluded_sessions: ["a".to_owned(), "b".to_owned()].into(),
        }];
        cfg.project_sync_interval = SyncInterval::Daily;
        cfg.save_to(&p).unwrap();
        assert_eq!(AppConfig::load_from(&p), cfg);
    }

    #[test]
    fn sync_row_without_exclusions_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        fs::write(
            &p,
            br#"{"project_sync": [{"path": "/x", "omp": true, "sessions": true}]}"#,
        )
        .unwrap();
        let cfg = AppConfig::load_from(&p);
        assert!(cfg.project_sync[0].excluded_sessions.is_empty());
        assert!(
            !cfg.project_sync[0].memory,
            "rows saved before Memory existed keep it off"
        );
    }

    #[test]
    fn legacy_drive_auto_backup_migrates_and_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        let load = |json: &str| {
            fs::write(&p, json).unwrap();
            AppConfig::load_from(&p)
        };
        assert_eq!(
            load(r#"{"drive_auto_backup": false}"#).drive_backup_interval,
            SyncInterval::Off
        );
        assert_eq!(
            load(r#"{"drive_auto_backup": true}"#).drive_backup_interval,
            SyncInterval::Every10Minutes
        );
        assert_eq!(
            load("{}").drive_backup_interval,
            SyncInterval::Every10Minutes
        );
        // An explicit new key wins over the legacy one.
        assert_eq!(
            load(r#"{"drive_auto_backup": false, "drive_backup_interval": "Hourly"}"#)
                .drive_backup_interval,
            SyncInterval::Hourly
        );
        let cfg = load(r#"{"drive_auto_backup": false}"#);
        cfg.save_to(&p).unwrap();
        let written = fs::read_to_string(&p).unwrap();
        assert!(!written.contains("drive_auto_backup"));
        assert_eq!(
            AppConfig::load_from(&p).drive_backup_interval,
            SyncInterval::Off
        );
    }

    #[test]
    fn backup_destination_defaults_to_drive_and_round_trips() {
        assert_eq!(load_str("{}").backup_destination, Destination::Drive);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        let cfg = AppConfig {
            backup_destination: Destination::Server,
            ..AppConfig::default()
        };
        cfg.save_to(&p).unwrap();
        assert_eq!(
            AppConfig::load_from(&p).backup_destination,
            Destination::Server
        );
    }

    #[test]
    fn legacy_notify_enabled_loads_as_desktop_and_saves_as_desktop() {
        let c = load_str(r#"{"notify": {"enabled": false, "exited": false}}"#);
        assert!(!c.notify.desktop);
        assert!(!c.notify.exited);
        assert!(c.notify.turn_finished, "unlisted fields keep defaults");
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""desktop":false"#), "{json}");
        assert!(!json.contains(r#""enabled""#), "{json}");
    }

    #[test]
    fn notify_while_viewing_defaults_true_and_round_trips_false() {
        assert!(
            load_str(r#"{"notify": {"desktop": false}}"#)
                .notify
                .while_viewing
        );
        assert!(
            load_str(r#"{"notify": {"enabled": false}}"#)
                .notify
                .while_viewing
        );
        let c = load_str(r#"{"notify": {"while_viewing": false}}"#);
        assert!(!c.notify.while_viewing);
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""while_viewing":false"#), "{json}");
        let back: AppConfig = serde_json::from_str(&json).unwrap();
        assert!(!back.notify.while_viewing);
    }

    fn load_str(json: &str) -> AppConfig {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        fs::write(&p, json).unwrap();
        AppConfig::load_from(&p)
    }

    #[test]
    fn ui_scale_defaults_to_100_percent_and_clamps_on_load() {
        assert_eq!(load_str("{}").ui_scale, 1.0);
        assert_eq!(load_str(r#"{"font_size": 12.0}"#).ui_scale, 1.0);
        assert_eq!(load_str(r#"{"ui_scale": 1.25}"#).ui_scale, 1.25);
        assert_eq!(load_str(r#"{"ui_scale": 3.0}"#).ui_scale, MAX_UI_SCALE);
        assert_eq!(load_str(r#"{"ui_scale": 0.1}"#).ui_scale, MIN_UI_SCALE);
        assert_eq!(clamp_ui_scale(f32::NAN), 1.0);
        assert_eq!(clamp_ui_scale(f32::INFINITY), 1.0);
    }

    #[test]
    fn ui_scale_round_trips() {
        let c = AppConfig {
            ui_scale: 1.15,
            ..AppConfig::default()
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""ui_scale":1.15"#), "{json}");
        assert_eq!(
            serde_json::from_str::<AppConfig>(&json).unwrap().ui_scale,
            1.15
        );
    }

    #[test]
    fn legacy_style_migrates_to_blueprint_only_for_dark_neutral() {
        let c = load_str(r#"{"theme": "Dark", "style": "Neutral"}"#);
        assert_eq!((c.style, c.style_rev), (UiStyle::Blueprint, STYLE_REV));
        let c = load_str("{}");
        assert_eq!((c.style, c.style_rev), (UiStyle::Blueprint, STYLE_REV));
        let c = load_str(r#"{"theme": "Light", "style": "Neutral"}"#);
        assert_eq!((c.style, c.style_rev), (UiStyle::Neutral, STYLE_REV));
        let c = load_str(r#"{"theme": "Light"}"#);
        assert_eq!(c.style, UiStyle::Neutral);
        assert!(!c.dark());
        let c = load_str(r#"{"style": "Neon"}"#);
        assert_eq!(c.style, UiStyle::Neon);
    }

    #[test]
    fn explicit_neutral_with_style_rev_sticks() {
        let c = load_str(r#"{"style": "Neutral", "style_rev": 1}"#);
        assert_eq!(c.style, UiStyle::Neutral);
    }

    #[test]
    fn every_style_roundtrips_and_is_unique() {
        assert_eq!(UiStyle::ALL.len(), 18);
        let mut keys = std::collections::BTreeSet::new();
        for (i, s) in UiStyle::ALL.iter().enumerate() {
            assert!(!UiStyle::ALL[..i].contains(s));
            assert!(keys.insert(s.key()));
            let json = serde_json::to_string(s).unwrap();
            assert_eq!(serde_json::from_str::<UiStyle>(&json).unwrap(), *s);
            let c = load_str(&format!(r#"{{"style": {json}, "style_rev": 1}}"#));
            assert_eq!(c.style, *s);
        }
        assert_eq!(UiStyle::ALL[0], UiStyle::Blueprint);
    }

    #[test]
    fn dark_prefers_dark_only_styles_over_light_theme() {
        let mut c = AppConfig {
            theme: Theme::Light,
            style: UiStyle::Aurora,
            ..AppConfig::default()
        };
        assert!(c.dark());
        c.style = UiStyle::Neutral;
        assert!(!c.dark());
        c.theme = Theme::Dark;
        assert!(c.dark());
    }

    #[test]
    fn old_state_files_with_tmux_name_load_and_are_rewritten_as_session_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        fs::write(
            &p,
            r#"{"open_tabs":[{"tmux_name":"omp-abc","cwd":"/w","session_path":null,"title":"t"}],"active_tab":0}"#,
        )
        .unwrap();
        let cfg = AppConfig::load_from(&p);
        assert_eq!(cfg.open_tabs.len(), 1);
        assert_eq!(cfg.open_tabs[0].session_name, "omp-abc");
        assert_eq!(cfg.active_tab, Some(0));
        cfg.save_to(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("\"session_name\""), "{text}");
        assert!(!text.contains("tmux_name"), "{text}");
        assert_eq!(AppConfig::load_from(&p).open_tabs, cfg.open_tabs);
    }

    #[test]
    fn session_view_is_lowercase_and_old_state_opens_as_terminal() {
        assert_eq!(
            serde_json::to_string(&SessionView::Chat).unwrap(),
            r#""chat""#
        );
        assert_eq!(
            serde_json::from_str::<SessionView>(r#""terminal""#).unwrap(),
            SessionView::Terminal
        );
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        // A state file written before views existed: no view settings, tabs without `view`.
        fs::write(
            &p,
            r#"{"open_tabs":[{"session_name":"omp-1","cwd":"/w","session_path":null,"title":"t"}],
                "folder_tabs":{"/w":{"open_tabs":[{"tmux_name":"omp-2","cwd":"/w","session_path":null,"title":"u"}]}}}"#,
        )
        .unwrap();
        let cfg = AppConfig::load_from(&p);
        assert_eq!(cfg.session_view, SessionView::Chat);
        assert_eq!(cfg.view_rev, VIEW_REV);
        assert_eq!(cfg.open_tabs[0].view, SessionView::Terminal);
        assert_eq!(
            cfg.folder_tabs[Path::new("/w")].open_tabs[0].view,
            SessionView::Terminal
        );
        // Chat survives a save/load, both as the default and on a tab.
        let mut cfg = AppConfig {
            session_view: SessionView::Chat,
            ..AppConfig::default()
        };
        cfg.open_tabs.push(TabState {
            session_name: "omp-3".into(),
            cwd: "/w".into(),
            session_path: None,
            title: "c".into(),
            view: SessionView::Chat,
        });
        cfg.save_to(&p).unwrap();
        let back = AppConfig::load_from(&p);
        assert_eq!(back.session_view, SessionView::Chat);
        assert_eq!(back.open_tabs[0].view, SessionView::Chat);
    }

    #[test]
    fn new_configs_default_to_chat_without_changing_the_legacy_view_default() {
        assert_eq!(SessionView::default(), SessionView::Terminal);
        let cfg = AppConfig::default();
        assert_eq!(cfg.session_view, SessionView::Chat);
        assert_eq!(cfg.view_rev, VIEW_REV);
        let cfg: AppConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.session_view, SessionView::Chat);
        assert_eq!(cfg.view_rev, VIEW_REV);
    }

    #[test]
    fn session_view_migrates_once_and_preserves_subsequent_terminal_choice() {
        let cfg = parse_config(br#"{"session_view":"terminal"}"#).unwrap();
        assert_eq!(cfg.session_view, SessionView::Chat);
        assert_eq!(cfg.view_rev, VIEW_REV);

        let cfg = parse_config(br#"{"session_view":"terminal","view_rev":1}"#).unwrap();
        assert_eq!(cfg.session_view, SessionView::Terminal);
        assert_eq!(cfg.view_rev, VIEW_REV);
        let bytes = serde_json::to_vec(&cfg).unwrap();
        let back = parse_config(&bytes).unwrap();
        assert_eq!(back.session_view, SessionView::Terminal);
        assert_eq!(back.view_rev, VIEW_REV);
    }

    #[test]
    fn session_backend_defaults_for_old_state_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        fs::write(&p, r#"{"sidebar_width": 250.0}"#).unwrap();
        assert_eq!(
            AppConfig::load_from(&p).session_backend,
            BackendChoice::Auto
        );
        for choice in [
            BackendChoice::Auto,
            BackendChoice::Tmux,
            BackendChoice::Host,
        ] {
            let cfg = AppConfig {
                session_backend: choice,
                ..AppConfig::default()
            };
            cfg.save_to(&p).unwrap();
            assert_eq!(AppConfig::load_from(&p).session_backend, choice);
        }
        fs::write(&p, r#"{"session_backend": "Host"}"#).unwrap();
        assert_eq!(
            AppConfig::load_from(&p).session_backend,
            BackendChoice::Host
        );
    }

    #[test]
    fn sync_intervals_have_periods_and_labels() {
        let periods: Vec<_> = SyncInterval::ALL.iter().map(|i| i.period()).collect();
        assert_eq!(
            periods,
            [
                None,
                Some(Duration::from_secs(600)),
                Some(Duration::from_secs(1800)),
                Some(Duration::from_secs(3600)),
                Some(Duration::from_secs(6 * 3600)),
                Some(Duration::from_secs(24 * 3600)),
            ]
        );
        let labels: std::collections::BTreeSet<_> =
            SyncInterval::ALL.iter().map(|i| i.label()).collect();
        assert_eq!(labels.len(), 6);
    }
}
