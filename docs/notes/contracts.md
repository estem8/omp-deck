# OmpDeck cross-crate contracts

Authoritative. Orchestrator-owned; implementers report needed changes instead of editing.
`deck-core` and `deck-term` never depend on each other; `omp-deck` depends on both.
Errors: typed `thiserror` in libs, `anyhow` in the bin. No `unwrap`/`expect` outside tests.

## deck_core (crate `deck-core`, no GUI deps)

### `deck_core::sessions`

```rust
pub struct SessionsRoot { pub path: PathBuf }
impl SessionsRoot {
    /// `$PI_CODING_AGENT_DIR/sessions` if set and non-empty, else `$HOME/$PI_CONFIG_DIR/agent/sessions`
    /// (`PI_CONFIG_DIR` default `.omp`). Never fails; existence checked by `exists()`.
    pub fn resolve() -> SessionsRoot;
    pub fn at(path: impl Into<PathBuf>) -> SessionsRoot;   // --sessions-root override, tests
    pub fn exists(&self) -> bool;                          // is_dir()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: String,                  // header "id"
    pub path: PathBuf,               // absolute .jsonl path
    pub cwd: PathBuf,                // header "cwd"
    pub title: Option<String>,       // title line "title" (trimmed, None if empty)
    pub first_prompt: Option<String>,// first user message text, whitespace-collapsed, ≤200 chars
    pub modified: SystemTime,        // file mtime
}
impl SessionEntry {
    /// title, else first_prompt, else "(untitled)".
    pub fn label(&self) -> &str;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectGroup {
    pub cwd: PathBuf,
    pub display: String,             // `~`-abbreviated cwd
    pub sessions: Vec<SessionEntry>, // newest-first by `modified`
}

/// Groups ordered by their newest session, newest first.
/// Missing root → empty Vec. Unreadable/garbage files are skipped (logged at debug).
/// Hides stubs: title None AND first_prompt None.
pub fn scan(root: &SessionsRoot) -> Vec<ProjectGroup>;

/// Incremental scanner: caches parsed headers keyed by (path, len, mtime) so repeat scans
/// only stat files and re-read changed ones. `scan()` above = `Scanner::new(root).scan()`.
pub struct Scanner { .. }
impl Scanner {
    pub fn new(root: SessionsRoot) -> Scanner;
    pub fn root(&self) -> &SessionsRoot;
    pub fn scan(&mut self) -> Vec<ProjectGroup>;
}

/// Background refresher: one thread, rescans every `interval` (≤5 s) or immediately after
/// `refresh_now()`. After each scan that changed the result it stores the snapshot and calls
/// `on_update` (the UI passes a closure calling `egui::Context::request_repaint`).
/// Drop stops and joins the thread (wake via channel/condvar; never sleeps through a stop).
pub struct SessionWatcher { .. }
impl SessionWatcher {
    pub fn spawn(root: SessionsRoot, interval: Duration, on_update: impl Fn() + Send + 'static) -> SessionWatcher;
    pub fn latest(&self) -> Option<Arc<Vec<ProjectGroup>>>;  // None until first scan finishes
    pub fn generation(&self) -> u64;                          // bumps on each changed snapshot
    pub fn refresh_now(&self);
}

pub fn abbreviate_home(path: &Path) -> String;  // "/home/u/x" -> "~/x"; HOME itself -> "~"
```

### `deck_core::tmux`

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSpec { pub program: String, pub args: Vec<String>, pub cwd: PathBuf, pub env: Vec<(String, String)> }

pub const APP_SOCKET: &str = "omp-deck";

#[derive(Clone, Debug)]
pub struct Tmux { /* program path, socket name, config path */ }
impl Tmux {
    /// tmux on PATH (runs `tmux -V` ok) → Some, socket APP_SOCKET, config `config_dir()/tmux.conf`.
    pub fn detect() -> Option<Tmux>;
    /// Tests: explicit socket (`omp-deck-test-<pid>`) and config path.
    pub fn with_socket(program: impl Into<PathBuf>, socket: impl Into<String>, config: impl Into<PathBuf>) -> Tmux;
    pub fn socket(&self) -> &str;
    /// Writes the generated config (create dirs), and if this socket's server is already running
    /// applies it with `source-file`. Returns the config path.
    pub fn ensure_config(&self) -> io::Result<PathBuf>;
    /// `env -u TMUX <tmux> -L <sock> -f <conf> new-session -A -s <name> -c <cwd'> -- <argv...>`
    /// cwd' = `effective_cwd(cwd)`. env = `terminal_env()`.
    pub fn attach_or_create(&self, name: &str, cwd: &Path, argv: &[String]) -> LaunchSpec;
    /// Names of live sessions on this socket; server not running → empty set (no error).
    pub fn alive_sessions(&self) -> HashSet<String>;
    pub fn has_session(&self, name: &str) -> bool;
    pub fn kill(&self, name: &str) -> Result<(), TmuxError>;
    /// For every live session: the top-level session .jsonl files (direct children of a project
    /// dir under `root`) that any process in its pane process tree holds open
    /// (`#{pane_pid}` → /proc descendants → /proc/<pid>/fd symlinks). omp keeps its current
    /// session file open for writing, so this maps tmux sessions ↔ omp session files even after
    /// omp switches sessions in place (/new, /resume). Linux-only; elsewhere empty map.
    pub fn session_files(&self, root: &Path) -> HashMap<String, Vec<PathBuf>>;
}

pub fn session_name_for(id: &str) -> String;     // "omp-" + id with every char not [A-Za-z0-9_-] → '_'
pub fn new_session_name(now: SystemTime) -> String; // "omp-new-<unix-ms>"
/// Degraded mode (no tmux): run argv directly; same env/cwd rules.
pub fn direct_launch(cwd: &Path, argv: &[String]) -> LaunchSpec;
/// cwd if it is an existing dir, else $HOME (else "/").
pub fn effective_cwd(cwd: &Path) -> PathBuf;
/// [("TERM","xterm-256color"),("COLORTERM","truecolor"),("TERM_PROGRAM","omp-deck"),
///  ("PI_FORCE_IMAGE_PROTOCOL","kitty"),("PI_KITTY_PLACEHOLDERS","1"),("PI_FORCE_HYPERLINKS","1")]
pub fn terminal_env() -> Vec<(String, String)>;
/// The generated tmux.conf text (see docs/notes/tmux.md); ends with `set-environment -g` of the
/// PI_* overrides above.
pub fn config_text() -> String;
```

### `deck_core::config`

```rust
pub fn config_dir() -> PathBuf;   // $XDG_CONFIG_HOME/omp-deck (dirs::config_dir()), fallback ~/.config/omp-deck

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppConfig {
    pub window: Option<WindowGeometry>,
    pub sidebar_width: f32,              // default 300.0
    pub font_size: f32,                  // default 14.0 (points)
    pub theme: Theme,                    // default Dark
    pub open_tabs: Vec<TabState>,
    pub active_tab: Option<usize>,
    pub collapsed_groups: Vec<PathBuf>,  // collapsed project cwds
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowGeometry { pub x: Option<f32>, pub y: Option<f32>, pub width: f32, pub height: f32, pub maximized: bool }
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Theme { #[default] Dark, Light }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TabState {
    pub tmux_name: String,            // tmux session name (stable key)
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,// resumed .jsonl (None for a fresh `omp`)
    pub title: String,                // last known tab label
}
impl AppConfig {
    pub fn load() -> AppConfig;                         // config_dir()/state.json; missing/corrupt → default (corrupt logged)
    pub fn load_from(path: &Path) -> AppConfig;
    pub fn save(&self) -> Result<(), ConfigError>;      // atomic: write tmp + rename
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError>;
}
```

## deck_term (crate `deck-term`: PTY + VT + egui widget; deps egui, epaint[color_fonts], alacritty_terminal 0.26, arboard)

Facts: docs/notes/alacritty-api.md, egui-input.md, egui-fonts.md, tmux.md (read them; do not re-derive).

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnSpec {
    pub program: String, pub args: Vec<String>, pub cwd: PathBuf, pub env: Vec<(String, String)>,
    /// true when `program` is a tmux client (omp-deck's normal mode). Selects key-encoding rules
    /// (see "Key encoding") because tmux, not the inner app, is the outer terminal's peer.
    pub tmux_client: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TermTheme { #[default] Dark, Light }

#[derive(Clone)]
pub struct TermOptions {
    pub scrollback: usize,          // default 10_000 (local scrollback; tmux keeps its own)
    pub theme: TermTheme,
    pub clipboard: Clipboard,       // shared system clipboard (one per app)
}

/// The app's single system clipboard (arboard, CLIPBOARD + PRIMARY). Clone = same instance.
/// Lazily created; if arboard fails (no display) every call is a logged no-op/None.
#[derive(Clone)]
pub struct Clipboard { .. }
impl Clipboard {
    pub fn new() -> Clipboard;
    pub fn get_text(&self) -> Option<String>;
    pub fn set_text(&self, text: &str);
    pub fn get_primary(&self) -> Option<String>;
    pub fn set_primary(&self, text: &str);
}

/// Install terminal font families into egui (call once, from the eframe creator). Starts from
/// `FontDefinitions::default()` (UI fonts unchanged) and adds families `term`, `term-bold`,
/// `term-primary` (primary face only, for has_glyph classification). Primary = `fc-match -f '%{file}' monospace`
/// (bold = `fc-match -f '%{file}' monospace:bold`), fallbacks = existing files among the chain in
/// egui-fonts.md (DejaVu Sans Mono, Source Code Pro, Noto Sans Symbols 2, Noto Sans Symbols [wght],
/// Noto Sans Mono CJK SC (.ttc idx 2), Noto Color Emoji COLRv1, Noto Emoji). Missing files skipped.
pub fn install_fonts(ctx: &egui::Context) -> FontReport;
#[derive(Clone, Debug)] pub struct FontReport { pub primary: Option<PathBuf>, pub loaded: Vec<PathBuf>, pub missing: Vec<String> }

pub struct Terminal { .. }
impl Terminal {
    /// Spawns the PTY child (alacritty tty + EventLoop thread). Initial size 80x24 until first show().
    pub fn spawn(spec: SpawnSpec, opts: TermOptions, ctx: egui::Context) -> std::io::Result<Terminal>;
    /// Drains pending terminal events (answers ColorRequest/TextAreaSizeRequest/ClipboardLoad,
    /// stores ClipboardStore, records title/bell/exit). The app calls this every frame for every
    /// tab (hidden tabs too); TerminalView::show calls it for the visible one. Never blocks on I/O
    /// except a clipboard read for OSC 52 load.
    pub fn poll(&mut self) -> TerminalEvents;
    pub fn title(&self) -> Option<&str>;        // last OSC 0/2 title (None after reset)
    pub fn exit_code(&self) -> Option<i32>;     // Some once the child exited (signal → -1)
    pub fn is_exited(&self) -> bool;
    pub fn set_theme(&mut self, theme: TermTheme);
    /// Ends the child: Msg::Shutdown → Pty drop SIGHUPs the child. For tmux tabs the app instead
    /// kills the tmux session via deck_core (the client then exits on its own).
    pub fn kill(&mut self);
    pub fn write(&self, bytes: &[u8]);          // raw input to the PTY
}
/// Drop: sends Msg::Shutdown without joining (detaches: SIGHUP to the tmux *client* only;
/// tmux server/session survive — verified).

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalEvents {
    pub title_changed: Option<String>,  // Some("") = title reset
    pub exited: Option<i32>,            // reported exactly once
    pub bell: bool,
}

pub struct TerminalView<'a> { .. }
impl<'a> TerminalView<'a> {
    pub fn new(term: &'a mut Terminal) -> Self;
    pub fn font_size(self, points: f32) -> Self;     // default 14.0
    pub fn show(self, ui: &mut egui::Ui) -> TerminalResponse;   // fills available rect
}
pub struct TerminalResponse {
    pub response: egui::Response,
    pub events: TerminalEvents,         // merged poll() result for this frame
    pub open_url: Option<String>,       // Ctrl+click on an OSC 8 hyperlink; app opens it (xdg-open)
}
```

### Input routing contract (egui side)
- App MUST set `NativeOptions.clipboard_shortcuts = false` and `ctx.options_mut(|o| o.zoom_with_keyboard = false)`
  (egui-input.md §0). There is no egui clipboard backend: all system clipboard I/O goes through `Clipboard`.
- App shortcuts are taken first: the app calls `ctx.input_mut(|i| i.consume_key(mods, key))` for its own
  shortcuts (Ctrl+PgUp/PgDn, Ctrl+Shift+T, …) BEFORE showing the terminal; the focused terminal processes
  every remaining `Event::Key`/`Event::Text`/`Event::Ime` and holds focus-lock (tab, arrows, escape).
- Inside the terminal: Ctrl+Shift+C copy selection; Ctrl+Shift+V and Shift+Insert paste CLIPBOARD
  (bracketed when mode set); Ctrl+C/X/V → 0x03/0x18/0x16; Shift+middle-click pastes PRIMARY;
  selection is mirrored to PRIMARY on release.

### Key encoding (input.rs; pure fns, unit-tested)
1. Kitty flags active in TermMode (DISAMBIGUATE_ESC_CODES…; direct mode only, `Config.kitty_keyboard = true`)
   → kitty CSI-u per active flags.
2. else `tmux_client` → tmux always requests modifyOtherKeys 2 from the outer terminal (tmux.md):
   keys legacy can't express (Shift/Ctrl/Alt+Enter, Ctrl+Tab, Ctrl+Shift+letter, Ctrl+digit,
   Ctrl+punctuation without a C0 mapping, Shift+Space/Backspace with mods…) → `CSI 27;<mod>;<code>~`.
   Lone Escape → `ESC O [` (tmux ?7727 application-escape form; avoids tmux's 500 ms ESC delay).
   Alt+char → ESC + char. Everything else xterm legacy.
3. else xterm legacy (Shift+Enter → `\r`, Alt+Enter → `ESC \r`).
xterm legacy: arrows/Home/End honour APP_CURSOR (`ESC O A` vs `ESC [ A`), modified `ESC [1;<mod>A`,
F1-F4 `ESC O P..S` / `ESC[1;<mod>P`, F5-F12 `ESC[15~…`, Insert/Delete/PgUp/PgDn `ESC[2~/3~/5~/6~`,
Backspace 0x7f (Ctrl+Backspace 0x08, Alt+Backspace ESC 0x7f), Tab 0x09, Shift+Tab `ESC[Z`, Enter `\r`.

### Mouse/wheel/paste/focus
- Mouse reporting when `mode & MOUSE_MODE` and Shift not held: SGR (`ESC[<b;x;yM/m`) if SGR_MOUSE, else
  UTF-8 (?1005) or legacy X10 (clamped 223). Click/drag/motion per mode. Wheel → buttons 64/65, one report per line.
- Wheel without mouse mode: ALT_SCREEN && ALTERNATE_SCROLL → arrow keys (3 per notch); else local scrollback.
- Shift+drag always local selection (double-click semantic, triple-click line).
- Paste: BRACKETED_PASTE → `ESC[200~`+text(with any `ESC[201~` removed)+`ESC[201~`; else `\r\n`/`\n` → `\r`.
- Focus in/out → `ESC[I`/`ESC[O` when FOCUS_IN_OUT (on widget focus change AND window focus change).
- Exited child → banner `[process exited: <code>]` painted over the bottom row; tab stays.

### deck_term additions (accepted)
- `pub mod input` — pure encoders (`encode_key`, `encode_mouse`, `encode_paste`, `encode_focus`,
  `encode_alt_scroll`, `encode_text` + their types), public so integration tests can drive them.
- `impl Default for TermOptions` — scrollback 10_000, `TermTheme::Dark`, `Clipboard::new()`.

### Fix-loop contract additions (P6)
- `deck_term::Terminal::set_focused(&mut self, focused: bool)` — records focus state and, on a change,
  writes `ESC[I`/`ESC[O` when FOCUS_IN_OUT is set. TerminalView uses the same state, so the app calls
  `set_focused(false)` on the previously active tab when switching tabs (hidden tabs are never shown).
- `deck_core::sessions::SessionWatcher::root_exists(&self) -> bool` — root availability observed by the
  scanner thread on its last pass (UI never stats the root itself).
- `SessionsRoot::at`/`resolve` absolutize the path lexically (no existence requirement).
- Key encoding rule 2 correction: Alt+Enter and Alt+char are sent as `ESC`+byte (tmux maps `ESC \r` to
  `CSI 13;3u` for omp); modifyOtherKeys `CSI 27;mod;code~` is used for Shift/Ctrl combos legacy can't express.
- Direct mode (no tmux): the terminal tracks the child's `CSI > 4 ; n m` modifyOtherKeys requests itself
  (alacritty ignores them) and encodes per rule 2 while level 2 is active and kitty flags are off.
- `Terminal::poll` never blocks on I/O: OSC 52 loads are answered asynchronously by the clipboard worker.
  `Clipboard` owns arboard on a worker thread: `set_*` enqueue; `get_*` wait ≤ 500 ms then `None`.
- `deck_term::input::encode_key(&KeyInput, TermMode, KeyRules)` with `KeyRules { Legacy, ModifyOtherKeys, Tmux }`
  (Tmux = modifyOtherKeys + Esc as `ESC O [`). Kitty REPORT_EVENT_TYPES follows kitty's encoder: text keys
  report releases as CSI-u; unmodified Enter/Tab/Backspace releases only with REPORT_ALL_KEYS_AS_ESC.
- Known platform limit: winit 0.30 X11 reports non-emulated button 4/5 wheel clicks (XTest) on press and
  release, doubling wheel steps under Xvfb only; real mice use XI2 smooth-scroll axes (one step per notch).

## Wave 2: chrome redesign, omp updates, omp settings, sidebar context menus

Facts: `docs/notes/omp-cli.md` (omp 18.12.3 CLI, verified in a sandbox) and `docs/notes/egui-style.md`
(+ compiled demo `docs/notes/assets/egui-style-demo.rs`, screenshot `assets/egui-style-demo.png`).
Policy: omp performs every write to its own store/config through its CLI; OmpDeck never writes
`~/.omp/**` itself. Every omp invocation is blocking and MUST run on a worker thread.

### `deck_core::config` additions
```rust
pub struct AppConfig { /* existing fields */
    pub native_decorations: bool,   // default false → OmpDeck draws its own title bar (CSD); true → KWin frame
    pub check_updates: bool,        // default true → `omp update --check` at startup and every 6 h
}
```
Old state.json files without these keys load with the defaults (`#[serde(default)]` already on the struct).

### `deck_core::omp` (new module)
```rust
#[derive(Debug, thiserror::Error)]
pub enum OmpError {
    #[error("failed to run omp: {0}")] Spawn(#[from] std::io::Error),
    /// Non-zero exit. `message` = trimmed stderr with a leading "Error: "/"error: " removed
    /// (falls back to stdout when stderr is empty). Never contains a secret value.
    #[error("{message}")] Failed { code: Option<i32>, message: String },
    #[error("unexpected omp output: {0}")] Parse(String),
}

#[derive(Clone, Debug)]
pub struct Omp { /* program path */ }
impl Omp {
    /// First executable `omp` on PATH, else `~/.local/bin/omp`.
    pub fn detect() -> Option<Omp>;
    pub fn at(program: impl Into<PathBuf>) -> Omp;          // tests
    pub fn program(&self) -> &Path;
    /// Every command below runs with `current_dir("/")` (no project `.omp/config.yml` layer),
    /// stdin null, stdout/stderr captured, NO_COLOR=1.
    pub fn version(&self) -> Result<String, OmpError>;      // "omp/18.12.3" → "18.12.3"
    pub fn check_update(&self) -> Result<UpdateStatus, OmpError>;   // `omp update --check`
    /// Merges `config list` (text: tab + enum choices + order), `config list --json` (type, value,
    /// description, redacted) and defaults (`config list --json` with `PI_CODING_AGENT_DIR` = an empty
    /// temp dir, cwd = an empty temp dir, all other `PI_*` vars removed; temp dirs deleted after).
    /// The three commands run concurrently.
    pub fn load_settings(&self) -> Result<SettingsCatalog, OmpError>;
    /// `omp config set <key> --json -- <encoded>` (value is ONE argv element after `--`, so values starting
    /// with '-' are not taken as flags; anything after `--` is value text, so `--json` must precede it).
    /// Returns the saved value from the JSON reply (secrets: `Value::Null`; their stdout is never logged).
    /// A set secret is listed with `value = Some("********")` (`SECRET_PLACEHOLDER`); never edit/echo it.
    /// `Omp::with_env(key, value)` adds env to every command (tests sandbox `PI_CODING_AGENT_DIR`).
    pub fn set_setting(&self, setting: &Setting, value: &serde_json::Value) -> Result<serde_json::Value, OmpError>;
    pub fn reset_setting(&self, key: &str) -> Result<(), OmpError>;   // `omp config reset <key> --json`
    /// Omp-native move: `omp session pack <src> <tmp.tar>` then `omp session unpack <tmp.tar> --cwd <dest>`;
    /// the tar is always removed. omp deletes the original when it is a prefix of the archive
    /// (`Replaced`), so this is copy-then-delete-original performed by omp. Fails with nothing changed
    /// while a live omp process writes the session ("another live omp process…").
    pub fn move_session(&self, src: &Path, dest_cwd: &Path) -> Result<MoveOutcome, OmpError>;
}

/// argv for a terminal tab (same form as `omp --resume`): `["omp","--fork",<src>,"--cwd",<dest>]`.
/// omp writes the copy (new id, artifacts copied, header cwd = dest) as it starts; source untouched.
pub fn fork_argv(src: &Path, dest_cwd: &Path) -> Vec<String>;
/// argv for an update tab: Omp → ["omp","update"], Plugins → ["omp","update","--plugins"],
/// Canary → ["omp","update","--canary"], Stable → ["omp","update","--stable"].
pub fn update_argv(kind: UpdateKind) -> Vec<String>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum UpdateKind { Omp, Plugins, Canary, Stable }
/// argv for the "Install omp" tab (TabRole::InstallOmp, direct, never persisted), offered while
/// `Omp::detect()` is None: `sh -c` fetching `INSTALL_URL` (https://omp.sh/install) with curl, else
/// wget, into a variable (a failed download fails the tab), then `sh -s -- --binary` (prebuilt binary
/// → `$PI_INSTALL_DIR` or `~/.local/bin`, no bun/root). Exit 0 → re-run `Omp::detect()` and adopt it
/// (version, models, usage) without a restart. All omp tab argv (sessions, resume, restart, fork,
/// update) use the detected binary as argv[0], since `~/.local/bin` may not be on PATH yet.
pub fn install_argv() -> Vec<String>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateStatus { pub current: String, pub latest: Option<String> } // latest None = up to date

#[derive(Clone, Debug, PartialEq)]
pub struct MoveOutcome { pub new_path: PathBuf, pub kind: MoveKind }
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveKind { Created, Replaced, Forked { new_id: String } }  // Forked: original left in place

#[derive(Clone, Debug, PartialEq)]
pub struct SettingsCatalog { pub tabs: Vec<SettingsTab> }   // omp's tab order; "internal" last
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsTab { pub id: String, pub label: String, pub settings: Vec<Setting> }
// labels: appearance Appearance, model Model, interaction Interaction, context Context, memory Memory,
// files Files, shell Shell, tools Tools, tasks Tasks, providers Providers, internal Advanced;
// unknown id → capitalized id.
#[derive(Clone, Debug, PartialEq)]
pub struct Setting {
    pub key: String,
    pub kind: SettingKind,
    pub value: Option<serde_json::Value>,    // None = unset (or secret)
    pub default: Option<serde_json::Value>,
    pub description: String,
    pub secret: bool,                        // JSON `redacted` or a known credential key (omp-cli.md A1.5)
}
#[derive(Clone, Debug, PartialEq)]
pub enum SettingKind { Bool, Number, Enum(Vec<String>), String, Array, Record }
impl Setting {
    pub fn label(&self) -> String;     // last key segment humanized: "compactThinkingLevel" → "Compact thinking level"
    pub fn section(&self) -> &str;     // key up to the last '.', "" if none ("statusLine.preset" → "statusLine")
    pub fn is_modified(&self) -> bool; // value differs from default (secret: value set/redacted)
}
/// Value → the single CLI argument: bool "true"/"false", number as JSON number text, enum/string raw
/// text (strings JSON-quoted when they have leading/trailing whitespace), array/record compact JSON.
pub fn encode_value(kind: &SettingKind, value: &serde_json::Value) -> Result<String, OmpError>;
/// Pure parsers (unit-tested against captured output):
pub fn parse_update_check(stdout: &str) -> Result<UpdateStatus, OmpError>;
pub fn parse_unpack(stdout: &str, stderr: &str) -> Result<MoveOutcome, OmpError>;
pub fn parse_settings(list_text: &str, list_json: &str, defaults_json: &str) -> Result<SettingsCatalog, OmpError>;
```

### omp-deck (app) behavior contract
- Chrome: "Midnight neon" theme (deep navy/graphite surfaces, violet→cyan accent gradient) + a matching
  light variant; UI font Adwaita Sans (fallback Cantarell / Noto Sans / egui default) added with
  `ctx.add_font` so emoji/fallback faces stay; icons from `egui_material_icons` (initialize AFTER
  `deck_term::install_fonts`). Terminal grid colors are unchanged.
- Client-side title bar unless `native_decorations`: logo, inline tab strip, update badge, settings
  button, app menu, minimize/maximize/close; drag → `ViewportCommand::StartDrag`, double-click toggles
  maximize, 6 px edge/corner zones → `BeginResize` with matching cursors (not when maximized).
- Settings = an overlay window (see "Settings is a window" below; it was a special non-terminal tab) with omp's
  tabs as a nav rail plus an "OmpDeck" section (font size, theme, title bar style, update checks).
- Sidebar context menus (session row, project header); copy = new tab running `fork_argv` in a fresh
  `omp-new-<ms>` tmux session; move = confirm modal → stop the session if alive (tmux kill, close its tab)
  → `move_session` (retry ≤ 10 × 300 ms on the live-writer error) → rescan → reopen in a tab if it had one.
- Async results (settings writes, update checks, moves) surface as toasts; errors keep omp's message.

### Wave 2 fix-loop additions
```rust
// deck_core::sessions
/// The newest top-level session file under `root` (any project dir) with mtime >= `since` whose
/// header has `"parentSession": <parent_id>`; bounded header reads only (same 16 KiB rule as scan),
/// files older than `since` are skipped by metadata alone. None if not (yet) present.
pub fn find_fork(root: &SessionsRoot, parent_id: &str, since: SystemTime) -> Option<PathBuf>;
```
- Copy flow: the copy tab starts as "copy pending"; the app polls `find_fork` on a worker (≈1 s,
  ≤ 2 min). Found → tab.session_path = path (tab now counts as that session's tab), success toast.
  Process exits first → error toast pointing at the tab output.
- A session row counts as open/running if any tab's `session_path` equals it, in addition to the
  tmux fd mapping. Move's reopen uses the normal open path (`session_name_for(id)` + `--resume`).

## Wave 3: session status bar, model/thinking switcher, session actions

Facts: `docs/notes/omp-status.md` (status-line segments, jsonl schema + parsing pseudo-spec §3.2,
git/PR, breadcrumb, title glyph) and `docs/notes/omp-actions.md` (slash commands, injection bytes §3,
`/switch` + `/effort` §6, per-button recommendations §7). omp is now 18.12.5.
Decision: NO collab guest connection (it posts "joined/left" notices into the user's transcript,
bumps `collab:N`, and re-downloads the whole transcript on join). Live data = jsonl tail + pane
title glyph + git (+ `gh` for PR). Actions = slash-command/hotkey injection into the tab's PTY or a
new tab running an omp CLI.

### deck_term additions
```rust
impl Terminal {
    /// Instant the last 0x03 byte was written to the PTY (from key input or `write`). omp exits on
    /// two Ctrl+C within 500 ms; injectors must keep ≥ 650 ms distance.
    pub fn last_ctrl_c(&self) -> Option<Instant>;
    /// Visible text of the cursor's screen row (wide-char spacers skipped, trailing blanks trimmed);
    /// used to detect an unsent draft in omp's composer.
    pub fn cursor_line(&self) -> String;
}
```

### deck_core additions
```rust
// deck_core::sessions
impl SessionsRoot { pub fn agent_dir(&self) -> PathBuf; }  // parent of the sessions root

// deck_core::tmux
impl Tmux { pub fn pane_tty(&self, session: &str) -> Option<String>; } // `display -p -t <s> '#{pane_tty}'`

// deck_core::omp
// `omp models --json`, enriched with catalog `int`/`tps` from a concurrent headless RPC
// `get_available_models` and measured speed from agent.db `model_perf` (either may be absent).
impl Omp { pub fn models(&self) -> Result<Vec<ModelInfo>, OmpError>; }
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelInfo {
    pub selector: String,            // "provider/id" (what `/switch` takes)
    pub provider: String, pub id: String, pub name: String,
    pub context_window: Option<u64>, pub max_tokens: Option<u64>, pub reasoning: Option<bool>,
    pub thinking: Vec<String>,       // supported effort levels in order; empty = no thinking
    pub input: Vec<String>,          // modalities: "text", "image"
    pub cost_input: Option<f64>, pub cost_output: Option<f64>, pub cost_cache_read: Option<f64>, // $ per M
    pub pricing: Option<Pricing>,    // pricingStatus: Fixed | Free | Included | Variable | Unknown
    pub intelligence: Option<f64>,   // catalog `int`
    pub catalog_tps: Option<f64>,    // catalog `tps`; non-positive = unmeasured → None
    pub measured: Option<MeasuredSpeed>, // { tps, ttft_ms: Option<f64>, samples, tier: Option<String> }
}

// deck_core::status (new)
/// `<agent_dir>/terminal-sessions/<tty with "/dev/" stripped, "/"→"-">` (omp-actions.md §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breadcrumb { pub cwd: PathBuf, pub session: PathBuf, pub fresh: bool }
pub fn read_breadcrumb(agent_dir: &Path, tty: &str) -> Option<Breadcrumb>;

/// Incremental jsonl reader (omp-status.md §2a, §3.2): keyed on (dev, ino); resets on inode change or
/// shrink; line 0 is the 256-byte title slot (re-read each refresh); appended bytes parsed from the
/// last complete line; prefilter lines by substring before JSON parsing.
pub struct SessionStatsReader { .. }
impl SessionStatsReader {
    pub fn new(path: PathBuf) -> Self;
    pub fn path(&self) -> &Path;
    /// Reads what changed since the last call. Ok(true) if the stats changed.
    pub fn refresh(&mut self) -> io::Result<bool>;
    pub fn stats(&self) -> &SessionStats;
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionStats {
    pub title: Option<String>,
    pub model: Option<String>,               // last model_change "provider/id"
    pub thinking: Option<String>,            // effective level (None = model has no thinking)
    pub thinking_configured: Option<String>, // e.g. "auto"
    pub input_tokens: u64, pub output_tokens: u64, pub cache_read: u64, pub cache_write: u64,
    pub cost: f64,                           // Σ assistant usage.cost.total + model_usage (omp's rule)
    pub cost_detail: CostDetail,             // recorded costs and matching token counts, by source/type
    pub context_tokens: Option<u64>,         // last good assistant input+cacheRead+cacheWrite
    pub last_turn_failed: bool,              // last assistant stopReason is error (Retry button)
    pub last_assistant_text: Option<String>, // text parts of the last assistant message (≤ 256 KiB)
}
pub struct CostDetail { pub assistant: CostSource, pub side: CostSource }
pub struct CostPart { pub usd: f64, pub tokens: u64 }
pub struct CostSource {
    pub records: u64, pub total: f64, pub itemized: u64,
    pub input: CostPart, pub output: CostPart, pub cache_read: CostPart, pub cache_write: CostPart,
    pub unitemized_records: u64, pub unitemized: CostPart,
    pub uncosted: u64, pub uncosted_tokens: u64,
} // Components include only records with all four usable components and a usable total.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitInfo { pub branch: String, pub ahead: u32, pub behind: u32, pub staged: u32, pub unstaged: u32, pub untracked: u32, pub pr: Option<PrInfo> }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrInfo { pub number: u64, pub url: String }
/// `git -C <cwd> status --porcelain=v2 --branch -unormal` (5 s timeout); None if not a repo / no git.
pub fn git_info(cwd: &Path) -> Option<GitInfo>;   // pr filled by the watcher only
pub fn parse_git_status_v2(out: &str) -> Option<GitInfo>;   // pure, tested

/// One background thread tracking the ACTIVE tab only.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusTarget {
    pub tmux: Option<(Tmux, String)>,   // tab's tmux client + session name (None in direct mode)
    pub session: Option<PathBuf>,        // tab's known session file (fallback when no breadcrumb)
    pub cwd: PathBuf,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionStatus {
    pub session: Option<PathBuf>,        // current file (breadcrumb wins; follows /new, /fork, /move)
    pub fresh: bool,                     // breadcrumb says not yet persisted
    pub cwd: PathBuf,
    pub stats: SessionStats,
    pub git: Option<GitInfo>,
}
pub struct StatusWatcher { .. }
impl StatusWatcher {
    /// jsonl/breadcrumb every 1 s, git every 3 s, `gh pr view --json number,url` once per (cwd, branch)
    /// when `gh` exists and branch isn't the default/detached; `on_update` after a changed snapshot.
    pub fn spawn(agent_dir: PathBuf, on_update: impl Fn() + Send + 'static) -> StatusWatcher;
    pub fn set_target(&self, target: Option<StatusTarget>);   // immediate refresh on change
    pub fn refresh_now(&self);
    pub fn latest(&self) -> Option<Arc<SessionStatus>>;        // for the current target only
}   // Drop stops + joins.
```

### omp-deck behavior
- Bottom status bar (only for terminal tabs running omp): busy indicator (from the tab title glyph:
  `π > …` idle, braille spinner busy, `!` attention), model chip + thinking glyph (dropdown), path,
  git branch + `*N +N ?N` + PR, context-usage meter (tokens / window from `ModelInfo`), ↑in ↓out
  tokens, cost; right side action buttons. Repaint only on data change (spinner animates only while busy).
- Cost hover: recorded input/output/cache-read/cache-write dollars and token counts, separately for
  assistant replies and `model_usage` calls. Missing components retain the total as unitemized;
  missing costs retain tokens as uncosted. Gaps between components and totals are shown explicitly.
  No historical tokens are repriced with today's catalog. Scope: all branches, no compaction reset;
  task/subagent runs and spend not yet written to the file are excluded.
- Model dropdown: models from `Omp::models()` (cached; refresh button), searchable, grouped by provider,
  current marked; thinking-level selector limited to `off`, `auto` + the model's levels.
  Apply with `/switch <selector>[:level]` and `/effort <level>` injection.
- Injection (omp-actions.md §3.7/§3.10): per-tab serialized queue driven by frame time (never sleeps on
  the UI thread): if the cursor line holds a draft → confirm modal (it will be cleared) → Ctrl+C
  (respect `last_ctrl_c` ≥ 650 ms) → wait 650 ms → `ESC[200~<cmd>ESC[201~` → 150 ms →
  (idle only) Esc → 150 ms → Enter. Idle-only actions are disabled while busy. Every key (Ctrl+C,
  Esc, Enter, hotkeys) goes through `Terminal::send_key`, never as raw bytes (ConPTY merges a raw
  trailing `ESC` with the next write once win32-input-mode is in use); only the paste is raw.
- Actions: Handoff (idle, optional focus modal), Compact (idle; soft/snapcompact/focus variants),
  Fork to new tab (`fork_argv(path, cwd)` + copy-pending linking), Rename (`/rename <title>`; auto-name),
  Plan mode toggle (Alt+Shift+P), Agent hub (Alt+A), Context (`/context`), Retry (F5, shown when
  `last_turn_failed`), Stop turn (Esc, busy only), Copy last response (clipboard), Export HTML
  (`omp --export <jsonl>` with cwd = `$XDG_CACHE_HOME/omp-deck/exports`, then xdg-open).
- The tab's `session_path` follows `SessionStatus.session` (keeps sidebar mapping right after /new, /fork).

### Wave 3 fix-loop additions
```rust
// deck_term — replaces `cursor_line` (removed).
pub struct ScreenText { pub lines: Vec<String>, pub cursor_row: usize, pub cursor_col: usize }
impl Terminal {
    /// Visible rows of the active screen (viewport at the bottom, i.e. ignoring local scrollback),
    /// wide spacers skipped, trailing blanks trimmed. Called only on user actions.
    pub fn screen_text(&self) -> ScreenText;
    /// While `Instant::now() < until`, ALL user keyboard/paste/IME input to this terminal is dropped
    /// (mouse wheel/selection unaffected; focus reports unaffected). `write()` is never held.
    /// Used by injectors so the user cannot interleave Enter / Ctrl+C into an injection.
    pub fn hold_input_until(&self, until: Instant);
    /// One synthetic key stroke (press + release) encoded like the user's own key under the
    /// child's current protocol (`input::encode_stroke`; win32 modifier records restored to what
    /// the child last saw). Never held; counts as Ctrl+C for `last_ctrl_c`.
    pub fn send_key(&self, key: input::Key, mods: input::Mods);
}
```
- Focus reports: a click on app chrome (status bar, menus, dialogs) while a terminal tab is active must
  NOT produce `ESC[O`/`ESC[I` pairs adjacent to injected bytes. An injected lone `ESC` must be followed
  by ≥ 100 ms without further PTY bytes from the app (omp merges `ESC` + `ESC[I` into one key → Stop fails).
- Injection: idle-only jobs require `Activity::Idle` exactly (not Attention/Unknown); a job aborts
  without sending more bytes if activity leaves Idle before its final Enter; user input is held from the
  first injected byte until 150 ms after the final byte; a tab close is deferred until its injector
  is at a job boundary (≤ ~1.5 s, then close).
- Draft detection reads the composer region (rows around the cursor matching the composer frame of
  the configured `composer.shape`), not arbitrary rows; if the cursor is not in a recognizable
  composer (an omp overlay/picker is open), slash jobs are refused with a toast ("Close omp's open
  dialog first"), hotkey jobs still run.
- Stop: `ESC`, then after 650 ms, if the title still shows busy, one more `ESC` (never two within 500 ms).
- Opening a sidebar session never attaches a tmux session that now runs another session file
  (after `/new`, `/fork`, `/resume`): consult `Tmux::session_files`; attach to whichever tmux session
  holds this file, else create `omp-<id>-<unix-ms>`.
- deck_core breadcrumb trust: if the pane start time is unknown, trust only a breadcrumb naming the
  target's known session.
- deck_term `Terminal::set_visible(&self, bool)`: output-driven repaints are coalesced (visible ≤ one
  per 16 ms, hidden ≤ one per 250 ms); the app marks only the active tab visible. Window title updates
  from omp's spinner are throttled to 1 Hz by the app.
- deck_term `pub fn wait_detached(timeout: Duration)`: joins in-flight detach-flush workers (from
  dropping a `Terminal` with unwritten input); omp-deck calls it after `eframe::run_native` returns.

## Wave 4: model picker favorites, native Agent Hub, per-folder mode

### deck_core::config additions (serde default, backward compatible)
```rust
pub struct AppConfig {
    // …existing…
    pub favorite_models: Vec<String>,          // `ModelInfo.selector`s, insertion order
    pub model_provider_filter: Option<String>, // None = "All providers"
    pub folder_scope: Option<PathBuf>,         // last sidebar scope (None = global view)
    pub recent_folders: Vec<PathBuf>,          // most-recent first, deduped, ≤ 10
    pub folder_tabs: BTreeMap<PathBuf, FolderTabs>, // per-folder open tabs (folder mode launch/restore)
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FolderTabs { pub open_tabs: Vec<TabState>, pub active_tab: Option<usize> }
impl AppConfig {
    pub fn toggle_favorite(&mut self, selector: &str) -> bool;  // returns new state
    pub fn push_recent_folder(&mut self, dir: PathBuf);         // dedupe, front, cap 10
}
```

### Model picker (omp-deck)
- Below the search field: provider dropdown "All providers (N)" + each provider with its model count
  (selection persisted in `model_provider_filter`; unknown provider → All).
- "★ Favorites" section first (favorites within the current provider filter and search), then the
  provider groups (All) or the single provider's list. Every row has a star toggle (☆ empty / ★ filled,
  accent) that does NOT select the model; favorites persisted in `favorite_models`.
- Order inside a provider: newest first per `docs/notes/model-order.md` (series rank interleave,
  version tuple desc, alias before dated, date desc, id, name), implemented as a pure tested function
  using that note's 38-row test table.

### Per-folder mode (omp-deck)
- Enter: CLI `omp-deck [FOLDER]` (positional) or `--folder <dir>`; sidebar scope menu "Open folder…"
  (kdialog/zenity picker via the existing `transfer::picker_command`, plus a typed-path field) and recent
  folders; project-group context menu "Focus this folder". Leave: scope menu "All projects".
- Scope = sessions whose header cwd equals the folder or is inside it (path-component prefix,
  canonicalized/`~`-insensitive); grouping unchanged. Filter box works within the scope. Empty scope →
  empty state with "New session in <folder>". A "+ New session" for the folder is always visible.
- Scope persists in `folder_scope`; CLI folder overrides it for that run. Window title
  "OmpDeck — <folder name>" in folder mode. Running indicators / tabs of out-of-scope sessions stay
  (tabs are never closed by a scope change).
- Tabs: when launched with a CLI folder, restore `folder_tabs[folder]` instead of the global
  `open_tabs`; on exit save tabs to the scope the app was launched with (CLI folder → `folder_tabs`,
  else global `open_tabs`).

### Native Agent Hub
Facts: `docs/notes/agent-hub.md` (file layout §2, parent↔child mapping + status inference §3, live
tail rules §4, render shapes §6). Read-only from files; no collab guest; kill/chat/revive are NOT done
natively (only "Open in omp's Agent Hub" = `ESC a` injection via the existing injector).
UI dependency: `egui_commonmark` 0.26 (workspace dep, `default-features = false, features = ["pulldown_cmark"]`)
for assistant text / output markdown.

```rust
// deck_core::agents (new)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentStatus { Running, Done, Failed, Aborted, Interrupted }
#[derive(Clone, Debug, PartialEq)]
pub struct AgentSummary {
    pub id: String,                 // file stem, e.g. "CoreStatus" or "M2cCore.M2cSaveService"
    pub parent_id: Option<String>,  // None = spawned by the main session
    pub depth: usize,
    pub path: PathBuf,              // transcript .jsonl
    pub advisor: bool,
    pub agent: Option<String>,      // session_init.agent ("task", "scout", …)
    pub model: Option<String>,      // provider/id (latest model_change, else resolvedModel)
    pub thinking: Option<String>,
    pub task_gist: String,          // preamble stripped, `# Target` section preferred, ≤ 1000 chars
    pub task: String,               // full assignment text (≤ 64 KiB)
    pub status: AgentStatus,
    pub error: Option<String>,      // last errorMessage for Failed/Aborted
    pub current: Option<String>,    // running: pending tool ("bash · cargo test", intent preferred)
    pub created: Option<SystemTime>, pub last_activity: Option<SystemTime>,
    pub duration: Option<Duration>, // last entry − header timestamp
    pub requests: u32, pub tool_calls: u32, pub tokens: u64, pub cost: f64,
    pub context_tokens: Option<u64>,
    pub output_path: Option<PathBuf>, // `<id>.md` when size > 0
}
#[derive(Clone, Debug, PartialEq)]
pub struct AgentsTarget { pub session: PathBuf, pub owner_alive: bool } // owner_alive: the tab's omp runs
#[derive(Clone, Debug, Default)]
pub struct AgentsSnapshot { pub session: PathBuf, pub agents: Vec<AgentSummary> } // parent-before-child, newest first among siblings
pub struct AgentsWatcher { .. }
impl AgentsWatcher {
    /// Rescans `<stem>/` (recursively for nested dirs) + incrementally tails each transcript
    /// ((dev,ino) + offset + partial-line; reset on inode change/shrink); 1 s while any agent is
    /// Running, else 3 s; publishes only on change.
    pub fn spawn(on_update: impl Fn() + Send + 'static) -> AgentsWatcher;
    pub fn set_target(&self, target: Option<AgentsTarget>);
    pub fn latest(&self) -> Option<Arc<AgentsSnapshot>>;
}   // Drop stops + joins.

#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptItem {
    User { text: String, from_agent: bool, at: Option<SystemTime> },
    Assistant { text: String, model: Option<String>, at: Option<SystemTime> },
    Thinking { text: String },
    ToolCall { call_id: String, name: String, intent: Option<String>, summary: String, args: String },
    ToolResult { call_id: String, name: String, text: String, is_error: bool, truncated: bool },
    Notice { kind: String, text: String },       // custom_message display:true, async-result, irc
    Compaction { tokens_before: Option<u64>, tokens_after: Option<u64>, summary: String },
    ModelChange { model: String },
    Error { message: String },                   // assistant stopReason error/aborted
    Exit { reason: String },
}
/// Long text fields capped (tool result 16 KiB, thinking 64 KiB, args 4 KiB) with `truncated`.
pub struct TranscriptWatcher { .. }
impl TranscriptWatcher {
    pub fn spawn(path: PathBuf, on_update: impl Fn() + Send + 'static) -> TranscriptWatcher; // 500 ms tail
    pub fn latest(&self) -> Arc<Vec<TranscriptItem>>;  // full list so far (append-only growth)
    pub fn revision(&self) -> u64;                     // bumps on change
}   // Drop stops + joins.
```

UI (omp-deck): status bar "Agents" chip with live counts (running spinner only while any Running) toggles a
right-side Agent Hub panel for the active tab's session: aggregate header, search, tree/flat,
cards (status, id, agent type, model, task gist, cost/duration/requests/tools/tokens, current tool,
age). Clicking a card opens a resizable transcript popup (header facts + Transcript / Output / Task
tabs; markdown via egui_commonmark; collapsible thinking & tool results; live follow while Running;
copy output; prev/next agent). The old "Agent hub" action becomes "Open omp's Agent Hub (TUI)" in ⋯.

## Wave 5: live subagent control, title strip, larger status bar

Facts: `docs/notes/omp-control-ext.md` (proven in-process extension: `omp -e <file.ts>` reaches
`AgentRegistry` / `AgentLifecycleManager`; kill/chat/revive semantics = omp's TUI hub; NDJSON unix
socket protocol; survives /new /fork /resume /restart; no visible side effects; internal module
specifiers may change between omp versions → extension stays inert, OmpDeck falls back to read-only).

### deck_core::ctl (new)
```rust
/// The extension source (crates/deck-core/assets/ompdeck-ctl.ts, `include_str!`): the note's final
/// version + `off` for the progress listener on stop + protocol `v:1`; inert unless OMPDECK_CTL_DIR set.
pub const EXTENSION_SOURCE: &str;
/// Writes the extension to `<dir>/ompdeck-ctl.ts` if missing/different (0600); returns the path.
pub fn install_extension(dir: &Path) -> io::Result<PathBuf>;
/// `$XDG_RUNTIME_DIR/omp-deck/ctl/<key>` (fallback `<temp>/omp-deck-<uid>/ctl/<key>`); key = tmux
/// session name (or a direct-mode tab key). Socket = `<dir>/ctl.sock`.
pub fn ctl_dir(key: &str) -> PathBuf;
/// `["env", "OMPDECK_CTL_DIR=<dir>", <omp>, "-e", <ext>, <rest of omp args…>]` — prefixes an omp argv.
pub fn with_ctl(omp_argv: &[String], ext: &Path, dir: &Path) -> Vec<String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveStatus { Running, Idle, Parked, Aborted }
#[derive(Clone, Debug, PartialEq)]
pub struct LiveAgent { pub id: String, pub name: String, pub parent: Option<String>, pub status: LiveStatus,
    pub activity: Option<String>, pub live: bool, pub streaming: Option<bool>, pub session_file: Option<PathBuf>,
    pub model: Option<String> }
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveSnapshot { pub pid: u32, pub session_file: Option<PathBuf>, pub main_idle: Option<bool>, pub agents: Vec<LiveAgent> }
#[derive(Clone, Debug, PartialEq)]
pub enum CtlCommand { Kill { id: String }, Revive { id: String }, Chat { id: String, text: String } }
#[derive(Clone, Debug, PartialEq)]
pub struct CtlReply { pub ok: bool, pub error: Option<String> }
/// One thread per client: connects to `<dir>/ctl.sock`, reconnects with backoff (≤ 2 s) when the
/// socket is absent/closed (/restart, omp not yet started); never blocks the caller.
pub struct CtlClient { .. }
impl CtlClient {
    pub fn spawn(dir: PathBuf, on_update: impl Fn() + Send + 'static) -> CtlClient;
    pub fn connected(&self) -> bool;
    pub fn latest(&self) -> Option<Arc<LiveSnapshot>>;     // None until first snapshot / after disconnect
    /// Queued; the reply arrives via `take_replies` (and `on_update` fires). Returns a request id.
    pub fn send(&self, cmd: CtlCommand) -> u64;
    pub fn take_replies(&self) -> Vec<(u64, CtlReply)>;
}   // Drop closes + joins.
```

### omp-deck
- Every omp tab OmpDeck launches (open/resume, new, fork/copy, move-reopen, restore when it creates a
  session) runs `with_ctl(argv, ext, ctl_dir(tmux_name))`. Extension installed at startup into
  `$XDG_DATA_HOME/omp-deck/` (before the window; small write). `omp update` tabs: no extension.
- The Agent Hub keeps one `CtlClient` for the active session tab. When connected: live status
  (incl. Parked) and activity override file inference for matching ids; popup + cards gain Kill
  (confirm), Revive (parked), and a Chat composer (steers running, prompts idle, revives parked).
  Not connected: read-only as today with a hint; for tabs not launched with the extension, a
  "Restart session with live control" action (idle only, confirm): kill that tmux session, relaunch
  `--resume <current session file>` with the extension in the same tab.
- UI: model picker popup ~50 % wider; a centered title strip at the top of the CSD chrome showing the
  active tab's full title (omp `π <glyph> ` prefix stripped; tooltip with cwd/session path); native
  decorations mode sets the window title to "<tab title> — OmpDeck" instead. Status bar fonts, icons
  and height ~50 % larger.

### Wave 5 fix-loop additions
- `ctl_dir(key) -> PathBuf` stays pure but bounded: leaf = `<16 hex of a stable 64-bit hash of key>`;
  root = `$XDG_RUNTIME_DIR/omp-deck/ctl` when set, absolute, owned by the effective uid and not
  group/other-writable; otherwise `$XDG_CACHE_HOME/omp-deck/ctl` (never a predictable `/tmp` path).
- New `prepare_ctl_dir(dir: &Path) -> io::Result<()>`: creates every missing component 0700, then
  verifies with no-follow lstat that root and leaf are real dirs owned by the euid with no group/other
  write (leaf exactly 0700), and that `<dir>/ctl.sock` fits the 108-byte sun_path limit; error →
  caller launches WITHOUT control (logged). omp-deck calls it on a worker before every `with_ctl`.
- New `prune_ctl_dirs(alive: &HashSet<String>)`: removes leaf dirs (and a stale `ctl.sock` inside)
  whose key's hash isn't among `ctl_dir` leaves of `alive` keys; non-recursive beyond that, no-follow.
- `install_extension` enforces regular non-symlink file owned by euid with mode 0600 even when the
  content matches.
- Extension: commands authorized only for agents whose sessionFile belongs to the current root;
  `chat` replies once the prompt is accepted (not when the turn ends); per-dir ownership via an
  exclusive lock file (`ctl.lock`, `flock`-style or O_EXCL pid file with liveness check) — never
  unlink a socket owned by a live process; on close unlink only if the path still has our inode;
  setup failure after listen stops the server and unlinks its socket.
- Round 2: `ctl_dir(namespace, key)` / `prune_ctl_dirs(namespace, alive)` — namespace = the tmux
  socket name (`omp-deck`, `omp-deck-test-…`) or `direct`; leaves live under `<root>/<namespace>/`, so
  instances on different sockets never see each other's dirs. Prune refuses a symlinked root, and
  skips any leaf whose lock owner is live (lock records `pid starttime` from `/proc/<pid>/stat` field
  22; the extension writes the same and treats a lock as stale only if pid is dead or its start time
  differs) or whose socket accepts a connection. Every `CtlCommand` carries the expected root session
  file; the extension rejects commands whose root ≠ `rootFile()`; omp-deck ignores live snapshots whose
  `session_file` ≠ the tab's current session file.

## Wave 6: session tree view with fork-from-node; chrome/status-bar tweaks

Facts: `docs/notes/session-tree.md` (tree model, omp `/tree` visibility + labeling rules, navigate /
fork semantics, extension ops proven in a sandbox, perf numbers). Chrome/status-bar tweaks (two-row
CSD chrome, status bar ≈1.2×, wide model chip, picker popup back to 430 px) already landed.

### deck_core::tree (new; file-based, works for any session)
```rust
pub enum NodeKind {                     // mirrors omp entry types (session-tree.md §1 row-text table)
    User, Assistant, ToolResult, Developer, BashExecution, OtherRole(String),   // type == "message"
    CustomMessage(String), Compaction, BranchSummary, ModelChange, ModelUsage, ThinkingChange,
    Label, Custom(String), Setting(String) /* service_tier_change, title_change, mode_change, credential_pin, session_init, ttsr_injection, reset_boundary, other */,
}
pub struct RecordedCost { pub total: f64, pub incomplete: bool } // USD; default 0 / false
pub struct TreeNode {
    pub id: String, pub parent: Option<String>, pub kind: NodeKind,
    pub timestamp: Option<String>,     // ISO as written
    pub text: String,                  // omp row text WITHOUT the role prefix, ≤200 chars, \n\t→space, trimmed
                                       // (toolResult → "[read: path]" etc. from the matching toolCall; assistant w/o text → error/“(aborted)”/“(no content)”)
    pub has_text: bool,                // assistant visibility rule input (non-whitespace text block)
    pub stop_reason: Option<String>,
    pub active_leaf: Option<Option<String>>,  // `activeLeafId` if present (Some(None) = explicit null)
    pub cost_before: RecordedCost,     // exclusive journal-order prefix, independent of tree layout
}
pub struct SessionTree {
    pub nodes: Vec<TreeNode>,                 // file order
    pub index: HashMap<String, usize>,
    pub children: Vec<Vec<usize>>,            // sorted by timestamp asc
    pub roots: Vec<usize>,                    // parentId null/missing/self
    pub labels: HashMap<String, String>,      // target id → latest label text (cleared labels removed)
    pub file_leaf: Option<usize>,             // omp load rule (activeLeafId fallback, session-tree.md §1)
}
impl SessionTree {
    pub fn active_path(&self, leaf: Option<usize>) -> Vec<bool>;   // per node: on root→leaf path
    pub fn is_forkable(&self, i: usize) -> bool;                  // kind is a `message` (User needs a parent)
    pub fn fork_target(&self, i: usize) -> Option<&str>;          // User → parent id; other message kinds → own id
}
/// Streams the file on its own thread: header/title lines skipped, prefix-only parse for hidden types,
/// bodies parsed only for visible ones; truncated last line tolerated; tails appends (≤1 s poll or
/// notify) and rebuilds incrementally; full reload when the file is replaced/shrinks (/move).
pub struct TreeWatcher;
impl TreeWatcher {
    pub fn spawn(path: PathBuf, on_update: impl Fn() + Send + 'static) -> TreeWatcher;
    pub fn latest(&self) -> Option<Arc<SessionTree>>;
    pub fn revision(&self) -> u64;
    pub fn error(&self) -> Option<String>;
}  // Drop stops + joins promptly.
```

`cost_before` sums recorded `usage.cost.total` from assistant messages, hidden `model_usage` entries,
and legacy `task` tool results with `details.usage`. It includes earlier spend on other branches,
does not reset at compaction, and excludes the current entry. Live subagent spend absent from the
journal cannot be reconstructed. Missing/invalid costs, malformed skipped lines, and oversized
potential usage records mark later prefixes incomplete while retaining the known subtotal; the
view displays `Before ≥$…` and explains the limitation in the row tooltip. Appends preserve old
node snapshots; replacing or shrinking the journal resets the prefix. Unlike this legacy-task-aware
tree prefix, the status bar's cost breakdown covers assistant and side-model usage only.

### deck_core::ctl additions
- `LiveSnapshot.leaf: Option<String>` (main session's in-memory leaf, `ctx.sessionManager.getLeafId()`).
- `CtlCommand::ForkAt { root, entry_id }` → `CtlReply.new_file: Option<PathBuf>`: extension opens a second
  `SessionManager` on the file (`flush()` first) and `createBranchedSession(entryId, {copyArtifacts:true})`;
  rejects non-`message` entries; when the target is an assistant message with toolCalls, extends the cut
  to cover the matching toolResults (like omp's `fork(entryId)`), never leaving unpaired calls. Live tab
  is untouched.
- `CtlCommand::Navigate { root, entry_id }` → `CtlReply.leaf: Option<String>`: idle only (`busy` error
  otherwise); `Main.session.extensionRunner.createCommandContext().navigateTree(id, {summarize:false})`
  (TUI re-renders; user target → leaf = parent, text goes to the editor — omp semantics).
- Every op keeps the root check.

### deck_core::omp addition (sessions without live control)
`Omp::fork_at(&self, session_file: &Path, entry_id: &str) -> Result<PathBuf, OmpError>`: headless
`omp --mode rpc --resume <file>` (cwd = header cwd or $HOME), sends `{"type":"fork","entryId"}` then
`get_state` for the new `sessionFile`, closes stdin; 30 s timeout; error text surfaced (e.g. lease held
by a live omp). Blocking — callers run it on a worker.

### omp-deck: Session tree view
- Opens from: status-bar tree button (active tab), ⋯ menu "Session tree", Ctrl+Shift+B (same gating as
  Ctrl+Shift+A), sidebar row context menu "Session tree…" (any session, file-only).
- Large centered window (like the agent popup): header (session title, node counts, filter chips
  Conversation [user + assistant text, default] / Default [omp's /tree default] / All / Labeled,
  search), body = virtualized git-graph style rows: lanes with smooth branch curves, active path in
  accent, other branches muted, leaf marked "you are here", role glyph + text, label pills, compaction
  / branch-summary rows styled distinctly, relative time on the right. Hidden nodes lift visible
  descendants (omp rule); linear runs stay compact. Hover: full text tooltip + timestamp.
- Per row actions (on hover, plus right-click menu): **Fork from here** (enabled iff `is_forkable`;
  tooltip "Fork after this reply" / "Fork before this prompt"): live control → `ForkAt`; otherwise
  `Omp::fork_at` on a worker; result opens a new tab `--resume <new file>` with control. **Go here**
  (live control + idle only): `Navigate`. Copy text / copy id in the context menu.
- Live tab: leaf from `LiveSnapshot.leaf`, else `file_leaf`. Tree follows appends live (keeps scroll
  unless pinned to the leaf). Keyboard: ↑/↓ select, Enter = Go here, F = fork, Esc closes.

## Wave 7: sidebar Restart / Delete session; packaging

Facts: `docs/notes/omp-delete.md` (no omp CLI/RPC delete; omp's own deletion =
`FileSessionStorage.deleteSessionWithArtifacts` — removes `.jsonl`, sibling `<stem>/`, `.bak` leftovers;
lease probe `tryAcquireSessionLease(id)`; both exported from `@oh-my-pi/pi-coding-agent/session/session-storage`;
`/session delete` over RPC leaves a stub — never use; `/delete` injection leaves an empty replacement — not used).

### deck_core::omp addition
```rust
/// Embedded `assets/ompdeck-del.ts`; run as
/// `env OMPDECK_DEL_TARGET=<abs jsonl> OMPDECK_DEL_OUT=<tmp result file> omp --mode rpc --no-session --no-extensions -e <del.ts> </dev/null`
/// (cwd = $HOME). The extension reads the header id, takes the lease (null → `live`), calls
/// `deleteSessionWithArtifacts`, releases, writes one JSON result line. Missing result line = failure.
/// 20 s timeout; child killed on timeout/drop. Blocking — call from a worker.
pub fn delete_session(&self, file: &Path) -> Result<(), DeleteError>;
#[derive(Debug, thiserror::Error)]
pub enum DeleteError { Live, NotASession, Omp(String), Io(io::Error), Timeout }
```
Never touches anything outside what omp's own delete removes. Extension installed next to
`ompdeck-ctl.ts` (same hardening as `install_extension`).

### omp-deck
- Sidebar row menu: **Restart session** (running only; in place for a tab, else new tab; confirm, busy
  warning) and **Delete session…** (destructive, red; confirm modal naming title, project, file size,
  "subagent transcripts and artifacts are deleted too; this cannot be undone"). Running in a tab/tmux →
  the confirm says it stops the session first: close the tab, kill the tmux session (app socket), then
  retry `delete_session` while it reports `Live` for up to ~5 s (lease release after exit). `Live` after
  that → toast "open in another omp process". Success → toast + immediate sidebar rescan; recently-closed
  (Ctrl+Shift+T) and persisted tab lists drop the deleted path.

## Wave 8: "Neutral" modern look (default), Compact density, lucide icons, sidebar auto-expand

User request: their-style modern look (oh-my-pi-gui: neutral near-black, hairline borders, Geist, lucide
icons, flat, compact) added **and made the default**; lucide icons everywhere; sidebar expands only the
active tab's folder, the rest collapsed by default.

### deck_core::config
```rust
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum UiStyle { #[default] Neutral, Neon }
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Density { #[default] Compact, Comfortable }
// AppConfig gains (both #[serde(default)], so existing config files switch to Neutral + Compact):
pub style: UiStyle,
pub density: Density,
// AppConfig loses `collapsed_groups` (expansion is no longer persisted); old files carrying the key
// still load.
```
`Theme { Dark, Light }` unchanged (light/dark axis; style is the second axis).

### deck_term
`TermTheme` gains `NeutralDark` and `NeutralLight` (existing `Dark`/`Light` = Breeze stay for Neon).
NeutralDark: background `0x0b0b0d` (must equal the chrome `bg`), foreground `0xe4e4e7`, cursor `0xfafafa`;
ANSI 0–15: `18181b f87171 4ade80 facc15 60a5fa c084fc 22d3ee d4d4d8 / 52525b fca5a5 86efac fde047 93c5fd
d8b4fe 67e8f9 fafafa`. NeutralLight: background `0xffffff`, foreground `0x18181b`, cursor `0x18181b`, ANSI
tuned for contrast on white (zinc/tailwind-600 family). OSC 10/11/4 replies follow the palette.

### omp-deck
- Neutral palettes (theme.rs): dark bg `0b0b0d`, panel/bar `0f0f11`, surface `17171a`, raised `1f1f23`,
  stroke `232327`, text `d4d4d8`, text_strong `fafafa`, weak `71717a`, accent teal `2dd4bf` (accent_a =
  accent_b = accent: no gradients), ok `4ade80`, warn `fbbf24`, danger `f87171`. Light: bg `ffffff`, panel/bar
  `fafafa`, surface `f4f4f5`, raised/stroke `e4e4e7`, text `3f3f46`, strong `09090b`, weak `71717a`, accent
  `0d9488`, ok `16a34a`, warn `ca8a04`, danger `dc2626`. Neutral rules: flat (no gradient/glow), 1 px hairline
  borders, radius 8 (popups 10), hover = subtle surface fill, selection = surface fill + strong text with a
  thin accent indicator, accent only for focus/active/primary/activity.
- Fonts: Geist (bundled, OFL; license file shipped) is the UI font in Neutral; Neon keeps Adwaita Sans.
- Icons: lucide icon font (ISC; license shipped) replaces Material Symbols everywhere (both styles);
  `egui_material_icons` dependency removed.
- Density Compact: tighter sidebar rows/headers, tab row, title row, menus, tree/picker rows, paddings.
  The status bar keeps its user-tuned size (41 px bar, model chip 1.3 × 213 px in every session, at most 52 % of
  the bar; it does not follow the path chip, so a long worktree path cannot push usage out).
- Settings → Appearance: Theme (Dark/Light), Style (Neutral/Midnight neon), Density (Compact/Comfortable);
  live switch, persisted.
- Sidebar expansion: open(group) = filtering || override(group) ?? (group == active tab's project group).
  User toggles set an in-memory override; activating a tab clears the override of its group (it opens);
  other groups without an override collapse. Collapse-all / Expand-all set overrides for every group. Not
  persisted (startup: everything collapsed except the restored active tab's group).

## Wave 9: native Usage view, detailed context tooltip

Facts: `docs/notes/omp-usage-context.md` (omp 18.12.6 `/usage` + `/context` internals, `omp usage --json`
shape, stats.db schema, extension imports, timings).

### deck-core

- `deck_core::usage` (new module; workspace dep `rusqlite` 0.40 `bundled`, read-only opens only):
  - Serde mirror of `omp usage --json` (`raw` ignored, unknown fields ignored, every optional field
    `Option`/default): `UsageSnapshot { generated_at_ms: i64, reports: Vec<UsageReport>,
    accounts_without_usage: Vec<UnavailableAccount>, disabled_credentials: Vec<UnavailableAccount> }`,
    `UsageReport { provider, fetched_at_ms, limits: Vec<UsageLimit>, reset_credits: Option<ResetCredits>,
    notes: Vec<String>, metadata: UsageMetadata { email, account_id, org_name, plan_type, tier, project_id } }`,
    `UsageLimit { id, label, scope: UsageScope { account_id, model_id, tier, window_id, shared }, window:
    Option<UsageWindow { id, label, duration_ms, resets_at_ms, reset_label }>, amount: UsageAmount { used,
    limit, remaining, used_fraction, remaining_fraction, unit: String }, status: LimitStatus
    { Ok, Warning, Exhausted, Unknown }, notes }`, `ResetCredits { available_count, redeemable_count,
    cooldown_until, credits: Vec<ResetCredit { title, remaining_count, usable, expires_at }> }`,
    `UnavailableAccount { provider, label: Option<String>, reason: Option<String> }`.
  - `fetch_usage(omp: &Omp, timeout: Duration) -> Result<UsageSnapshot, UsageError>`: runs
    `omp usage --json --no-extensions` through omp's 5-minute report cache (like omp's own `/usage`), kills on
    timeout (default 30 s), typed errors (spawn / timeout / exit status + stderr tail / parse). Never runs
    `usage reset` or `usage invalidate` (0.21.1): after an invalidation omp drops a failing provider's last good
    report (`retainLastGood = !invalidated`), and Anthropic's IP-limited `/usage` endpoint then left the account
    "unavailable" after most manual refreshes. omp computes `policy` from the current config on every run, so a
    plain re-read after saving rules shows them.
  - `provider_cards(&UsageSnapshot, now_ms) -> Vec<ProviderCard>` — pure port of omp's
    `buildProviderCards`: group windows by `label|window.id`, mean fraction across accounts,
    `resets_in_ms` from the fullest window, window tag (`5h`/`7d`/`mo`…) only when labels collide,
    effective status (explicit, else used ≥1 exhausted / ≥0.9 warning / ok), `idle` when every window
    < 0.5 % used and no credits/notes, sort active by max fraction desc then name, display names
    (`anthropic` → `Anthropic`, `openai-codex` → `Codex`, …), accounts count + unavailable account labels,
    reset-credit summary text. A provider with several reports also carries `per_account:
    Vec<AccountCard { label, card: ProviderCard, identity: ReportIdentity }>` — the same card built from each
    report alone (label = `email` / `orgName` / `accountId`, else `account N`, made unique); the Usage view pages
    through these instead of showing the mean. `tightest(&[ProviderCard]) -> Option<Tightest { provider, account,
    window, used, status }>` reads windows per account for such providers (`account` = the page label, else `None`).
  - `ReportIdentity::of(&UsageReport)` (lowercased `metadata.email`/`orgId`, account/project ids from metadata and
    every limit's `scope`) `.matches(&OAuthIdentity { email, account_id, project_id, org_id })`: port of omp's
    `limitMatchesActiveAccount` — org ids must agree (an org-only identity then matches), two emails decide alone,
    else an account id, else a project id, must appear; a report without limits matches nobody.
    `UsageMetadata.org_id` and `UsageScope.project_id` parsed for it.
  - `stats_db_path() -> PathBuf` (`~/$PI_CONFIG_DIR/stats.db`, `PI_CONFIG_DIR` default `.omp`).
  - `activity(db, days) -> Result<Activity, UsageError>`: read-only open (`SQLITE_OPEN_READ_ONLY`, WAL
    tolerated, busy timeout); per local day `cost, requests, tokens` (omp's query) and per model
    `{provider, model, cost, requests, tokens}` for the last 30 days. Missing db → empty `Activity`.
  - `Heatmap::build(&Activity, today, weeks = 53)`: 7×weeks cells `Option<u8>` 0–4 (`ceil(sqrt(v/max)·4)`,
    value = cost if any cost else requests), month labels, totals (past year cost/requests/tokens),
    last-30-days totals + daily series, busiest day, active days, longest streak.
- `deck_core::ctl`: `LiveSnapshot.context: Option<ContextBreakdown>` (absent from older extensions →
  `None`). `ContextBreakdown { model: Option<String> /* display name */, context_window: u64,
  used_tokens: u64, anchored: bool, categories: Vec<ContextCategory { id, label, tokens }>,
  auto_compact_buffer: u64, free_tokens: u64, threshold_tokens: Option<u64>, snapcompact_saved: Option<u64>,
  tools: Vec<(String, u64)> /* per-tool schema tokens, desc */, skills: u32, computed_at_ms: i64 }`.
  Extension (`ompdeck-ctl.ts`) computes it with `computeSessionContextBreakdown(session,
  {snapcompactSavings:true})` (+ per-tool schema tokens) on `session_start`, `session_switch`,
  `agent_end`, `turn_end`, model change, and caches it; snapshots carry the cached value (no model blob).
  Any failure → field omitted.
- `deck_core::ctl`: `LiveSnapshot.account: Option<LiveAccount { provider, identity: OAuthIdentity, pinned: bool,
  credentials: Vec<LiveCredential { id, identity }> }>` — the OAuth account the main session uses for its current
  model's provider. Extension: `mainAccount()` =
  `session.modelRegistry.authStorage.oauth.identity(model.provider, session.sessionId)` (what omp's own status line
  shows: the session's sticky credential once a request went out, before that omp's default pick); `pinned` =
  `authStorage.sessions.get(provider, sessionId)` is an explicit oauth record (`/session pin`); `credentials` =
  `authStorage.oauth.accounts(provider, sessionId)` (omp's numeric `credentialId` sent as a string). Wire
  `account: {provider, email, accountId, projectId, orgId, pinned, credentials: [{credentialId, email, accountId,
  projectId, orgId}]} | null`. Null for api-key providers; an identity naming nobody, a missing provider or a
  malformed value → `None`; credentials without an id or identity are skipped. Additive, no protocol bump. The
  extension pushes a snapshot every second while a client is connected, so a `/session pin` typed in the terminal
  shows within ~1 s.

### omp-deck

- Usage state owned by the app: background worker runs `fetch_usage` ~3 s after startup, every 5 min,
  when the Usage view opens and data is > 60 s old, and on manual refresh (`Mutation::UsageRefresh`, no fields); and
  `activity` when the view opens (≥ 10 min apart). UI thread never blocks.
- Status bar **usage chip** (gauge icon + tightest `NN% left`, coloured by status; dropped first when
  narrow; hidden when there are no reports). Hover: compact per-provider cards (bars = remaining,
  `resets in …`). Click, ⋯ menu "Usage…", app menu, Ctrl+Shift+U → **Usage view**. A multi-account provider shows
  one account in the chip and in the hover, both from `usage_view::shown_page`: the page whose report matches the
  focused tab's `LiveSnapshot.account` (`CardView::in_use`, same provider only; labelled `email · in use`, or
  `email · pinned` when the session is pinned), else the page last picked in the Usage view's pager (labelled
  `email · i / N`), else the first.
  `BarView::set_account` bumps `rev`; the segment-width cache also keys on the shown page.
- **Usage view** (modal like the session tree): header "Usage" + "Updated Xs ago" + refresh;
  tabs **Overview** (stats strip: Tightest quota, Past year, Last 30 days + sparkline, Busiest day;
  provider cards with remaining bars, `NN% left`, `resets in`, resets banked, unavailable accounts,
  "untouched: …" footnote; activity heatmap 53×7 with month labels + "N active days · longest streak M
  days"), **Details** (per provider table Limit | Account | Left | Resets; top models last 30 days) and
  **Accounts** (see below). Empty states: no accounts (hint `omp /login`), fetch error inline with Retry, omp missing.
- **Context tooltip**: rich hover on the status-bar context chip: model + window header,
  `used / window (pct)` (+ "estimated" when not anchored), stacked category meter with the
  auto-compact threshold mark, legend rows (swatch, label, tokens, % of window) for categories, free
  space, autocompact buffer, "N left before auto-compaction at P %" / "past auto-compaction", snapcompact
  savings, top tool schemas (≤ 8 + "N more"), skills count. Without a breakdown (no live control /
  old extension): current numbers + note that the full breakdown needs live control.
- Both styles (Neutral default, Neon) and Compact/Comfortable density; lucide icons; no per-frame allocation.

### Accounts tab: omp's account order (`auth.accountPolicies`) and the session pin

Facts (omp 18.14.0): a policy entry is `{provider, account: {email?, accountId?, projectId?, orgId?}, priority?,
reservePct?, reclaimAbovePct?}`; `account` needs one of email/accountId/projectId and matches by exact equality of
every field it gives; two entries matching one account, or reserve/reclaim on a provider without usage reports,
make omp throw when it resolves that provider's credentials; an entry matching nobody is ignored;
`reclaimAbovePct` must be above the account's reserve (own, else `retry.usageReservePct`). omp's ranking puts
blocked, spent, inside-reserve and ≥ 85 %-of-5h accounts last before `priority` decides; a session keeps its
account until it is spent or in reserve, or a higher-priority account is above its reclaim level. `omp config set`
does not validate the shape; running sessions pick up the change. `omp usage --json` adds `policy: {priority,
reservePct, reserveSource: "global"|"override", reclaimAbovePct?, state: "eligible"|"inside-reserve"|"exhausted"|
"unknown", remainingPct?}` to reports and `accountsWithoutUsage` entries of providers with policies.

- `deck_core::usage`:
  - `OAuthIdentity` (camelCase serde, `None` fields skipped) `::new(email, account_id, project_id, org_id)` (empty
    strings dropped, `None` when nothing is left), `.same_account(&other)` (case-insensitive: two emails decide, org
    ids too when both have one; else account ids, else project ids).
  - `policy_key(target, accounts) -> Option<OAuthIdentity>`: the fewest fields of `target` that select it alone among
    `accounts` by omp's matcher — {email}, {email, org}, {accountId}, {email, accountId}, {projectId},
    {email, projectId}, all; `None` when even all fields match another account.
  - `AccountPolicy { provider, account, priority, reserve_pct, reclaim_above_pct }` (omp's entry, `None` skipped);
    `replace_account_policies(current: &Value, provider, &[AccountPolicy]) -> Value`: other entries (other providers,
    non-objects, unknown fields) kept verbatim in place, `provider`'s dropped, the new ones appended.
  - `UsagePolicy` (`policy` above; `own_reserve()` = `reservePct` when `override`), `PolicyState` (unknown →
    `Unknown`); `UsageReport.policy`, `UnavailableAccount.{project_id, org_id, org_name, kind ("type"), policy}`,
    `UsageReport::account()` / `UnavailableAccount::account()` (api keys → `None`).
  - `policy_providers(snap, now_ms) -> Vec<PolicyProvider { provider, name, accounts: Vec<PolicyAccount { label,
    identity, key, card: Option<ProviderCard>, reason, policy }>, usage_capable, has_policies }>`: OAuth accounts from
    reports and `accountsWithoutUsage` (disabled credentials take no part in routing), `priority` descending then
    snapshot order; a label shared by two accounts gets ` · orgName` (else orgId).
- `Omp::set_account_policies(provider, &[AccountPolicy]) -> Value`: `omp config get auth.accountPolicies`, then
  `replace_account_policies`, then `omp config set auth.accountPolicies --json -- <array>` (the same private
  `set_encoded` path as `set_setting`); returns the saved array.
- Gateway: `Mutation::AccountPolicies { provider, policies }` (label `account-policies`, not allowed in maintenance)
  → job `jobs::account_policies` → `Done::Setting(saved array)`. Empty `policies` = back to omp's automatic order.
- ctl: `CtlCommand::PinAccount { root: Option<PathBuf>, provider, credential: Option<String> }`, wire
  `{t:"pin_account", root, provider, credentialId}`. Extension: root checked only when both sides have one; refused
  when `provider` is not the main session's model provider, while streaming, or for an unknown credential;
  `credentialId` mapped back to omp's own id, then `session.pinCurrentProviderOAuthAccount(id)`; `null` →
  `authStorage.sessions.clear(provider, sessionId)`. Reply `{ok}`; a snapshot follows at once.
- omp-deck: `usage_state::AccountGroup` (providers with ≥ 2 OAuth accounts or with policies; rows = label, identity,
  key, quota pills, reason, policy state, own reserve, reclaim) in `Summary.accounts`; `merged_draft(group, draft)`
  keeps a draft's order/values across refreshes (accounts gone drop out, new ones append; same-identity accounts by
  count); `account_policies(group, rows, global_reserve)`: priority `n − i` over the rows that have a key (rows
  without one get no entry: omp ranks them below every rule), reserve/reclaim only when `usage_capable`, a reclaim at
  or under the effective reserve dropped. `usage_state::KeyGroup { provider, name, rows: Vec<KeyRow { label, pills,
  note }> }` in `Summary.keys`: providers with ≥ 2 API keys (reports naming no OAuth account + `api_key` entries
  without usage), labelled as the Overview pager labels them. omp's `auth.accountPolicies` and `/session pin` cover
  OAuth credentials only (API keys: per-request pick, used-up/blocked skipped, usage-ranked where measured), so these
  are listed read-only.
- Accounts tab (`usage_view::accounts`): routing note; one card per group: "your order" / "omp's automatic order",
  "Reset to automatic" (only with policies), the focused tab's account and whether it is pinned; per row ↑/↓,
  rank, label (weak with a tooltip when it has no key), `inside reserve`/`used up` badge, no-usage reason, quota
  pills, and Reserve (stepper, "default" = `retry.usageReservePct`, ✕ back to default) / Reclaim above (off, or a
  stepper kept above the reserve, ✕ off) where omp measures quotas. For the focused tab's provider: `this tab`
  (+ lock when pinned), Pin (the matching `LiveCredential`) / Unpin. A dirty draft (`ViewState.drafts`) shows
  Discard / Save order; Save → `UsageAction::SaveAccounts` → `Mutation::AccountPolicies`, toast, `UsageRefresh`;
  the draft stays until a newer summary arrives. Pin/Unpin → `UsageAction::Pin` →
  `ctl_command(PinAccount)`, toast with omp's refusal reason. `retry.usageReservePct` is shown, never written
  (it also drives omp's model fallback); read from the settings catalog (`usage_reserve_from_catalog`, 10 until read).
- Key groups follow the OAuth groups as read-only cards ("N API keys · omp picks", how omp picks, per key label,
  no-usage reason, quota pills); the "One account per provider" empty state shows only when neither kind exists.

## Tab activity indicators, "Open tabs" sidebar section

- `tabs::TabStatus::of(exit_code, Activity::from_title(title), bell && !active)`: precedence
  Exited > Attention (`π !`) > Busy (spinner glyph) > Bell > Idle. `tabs::paint_status` draws it (check/x,
  amber `!` badge, `widgets::Spin`, warn dot; Idle → caller's own icon/LED) for the tab strip, the
  "Open tabs" rows and session rows; `tabs::paint_attention_pill` tints Attention pills/rows. No pulse:
  idle frames stay free (the spinner repaints at 10 fps only while visible).
- Tab labels use `Tab::label` (omp's `π <glyph>` prefix stripped, also from stored fallback titles);
  `Tab::title` stays raw for persistence/kill labels.
- Background tabs: `Terminal::poll` runs every frame for every tab; title events repaint through the
  hidden-tab gate (≤ 1 per 250 ms), so indicators update without input.
- Sidebar: "Open tabs" (collapsible, count + amber badge when any tab waits, ≤ 6 rows then scrolls) pinned
  above the scope chip; click → `SidebarAction::ActivateTab(i)`, ✕/middle-click → `CloseTab(i)` (app routes
  both through `apply_tab_action`, i.e. `request_close`). Session rows whose file is open in a tab show
  that tab's Busy/Attention (`SidebarInput::tab_activity`, built per frame) instead of the running LED.

## Wave 10: project sync (per-project `.omp` + sessions to Google Drive)

Facts: `docs/notes/project-sync.md` (omp pack/unpack format + conflict rules, project `.omp`, Drive resumable
upload, ureq traps). A **separate workflow** from the config backup (`google-drive-backup.md`): the config
backup's files, archive, list query (`omp-backup-` prefix), retention, card and auto timer stay unchanged.
Sessions are restored only through `omp session unpack` (omp performs the store writes).

### deck_core::config (additions, `#[serde(default)]`, old state files load)

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRow { pub path: PathBuf, pub omp: bool, pub sessions: bool }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncInterval { Off, Hourly, #[default] Every6Hours, Daily }
impl SyncInterval { pub fn period(self) -> Option<Duration>; pub const ALL: [SyncInterval; 4]; pub fn label(self) -> &'static str }
// AppConfig gains: pub project_sync: Vec<SyncRow> (default empty), pub project_sync_interval: SyncInterval.
```

### deck_core::backup::project (new; re-exported from `backup`)

```rust
pub const PROJECT_PREFIX: &str = "omp-project-";
pub const KEEP_PROJECT_BACKUPS: usize = 3;          // per (host, path_id)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parts { pub omp: bool, pub sessions: bool }
impl Parts { pub fn any(self) -> bool }

pub enum OmpDir { Missing, Global, Present { files: u64, bytes: u64 } }   // Global: folder is $HOME
pub struct ProjectSizes { pub omp: OmpDir, pub sessions: u64, pub session_bytes: u64 } // jsonl + artifacts
/// Blocking (worker). Sessions = `sessions::scan(root)` group whose `cwd` equals `project`.
pub fn measure(root: &SessionsRoot, project: &Path) -> std::io::Result<ProjectSizes>;
/// Lexical identity of a project path: sha256(path bytes) → first 16 hex.
pub fn path_id(project: &Path) -> String;

pub enum Stage { Scanning, Packing, Uploading, Downloading, Verifying, RestoringOmp, RestoringSessions }
pub struct Progress { pub stage: Stage, pub done: u64, pub total: u64 }  // bytes or items per stage

/// Per-path local sync state, `config_dir()/project-sync.json` (0600, atomic write); separate from
/// google-drive.json so the two Drive lanes never race on one file.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectSyncState { pub projects: BTreeMap<PathBuf, ProjectSynced> }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProjectSynced { pub fingerprint: String, pub parts: Parts, pub last: LastBackup }

pub struct ProjectBackup {            // one Drive file, parsed from appProperties (no download)
    pub file: BackupFile, pub host: String, pub path_id: String, pub path: String, pub name: String,
    pub omp_bytes: Option<u64>, pub sessions: Option<u64>, pub session_bytes: Option<u64>,
}
pub struct ProjectBackupOutcome { pub backup: ProjectBackup, pub synced: ProjectSynced, pub pruned: usize }

pub struct SessionPlan { pub id: String, pub title: String, pub modified_unix: i64, pub bytes: u64, pub new_id: bool }
pub struct ProjectRestorePlan {
    pub target: PathBuf, pub create_target: bool,        // target missing → created on apply
    pub omp_exists: bool, pub omp_overwritten: Vec<String>, pub omp_added: Vec<String>, // rel to .omp
    pub sessions: Vec<SessionPlan>,
}
pub struct PreparedProjectRestore { pub backup: ProjectBackup, pub plan: ProjectRestorePlan, /* staging */ }
pub struct ProjectRestoreReport {
    pub omp_written: usize, pub omp_overwritten: usize, pub safety_dir: Option<PathBuf>,
    pub sessions_added: Vec<PathBuf>, pub sessions_failed: Vec<(String /*title or id*/, String /*why*/)>,
}

impl Service {
    pub fn project_state(&self) -> ProjectSyncState;   // small file; missing/corrupt → default (+warn)
    /// None = unchanged since the last backup of this path with these parts (skipped unless `force`).
    /// `!parts.any()` or nothing present → Err(NothingToBackUp).
    pub fn backup_project(&self, root: &SessionsRoot, project: &Path, parts: Parts, force: bool,
        cancel: &AtomicBool, progress: &(dyn Fn(Progress) + Sync)) -> Result<Option<ProjectBackupOutcome>>;
    pub fn list_projects(&self) -> Result<Vec<ProjectBackup>>;   // newest first, kind=project only
    /// Downloads + verifies into staging; touches nothing outside the cache dir.
    pub fn prepare_project_restore(&self, backup: &ProjectBackup, target: &Path, parts: Parts,
        root: &SessionsRoot, cancel: &AtomicBool, progress: &(dyn Fn(Progress) + Sync)) -> Result<PreparedProjectRestore>;
    /// `omp: None` (omp not installed) allows a `.omp`-only restore; sessions then fail with `Error::Io`.
    pub fn apply_project_restore(&self, prepared: PreparedProjectRestore, omp: Option<&Omp>,
        cancel: &AtomicBool, progress: &(dyn Fn(Progress) + Sync)) -> Result<ProjectRestoreReport>;
}
```

- **Service concurrency**: the project lane and the config lane may run at once; each job builds its own
  `Drive` (own access token). Only the project lane writes `project-sync.json`.
- **Snapshot**: `.omp` = regular files under `<project>/.omp` (skip `node_modules`, `.git`, symlinks,
  non-regular; depth ≤ 16; not offered when `project == $HOME`). Sessions = scan group with `cwd == project`;
  each jsonl cut at its last `\n` at snapshot time, its `<stem>/` artifacts, and every blob referenced (regex in
  facts) from `<agent>/blobs/<hex>` (missing → skipped + logged). **Fingerprint** = sha256 over parts flags and
  each included file's (part, rel path, len, mtime ns) — metadata only, no content reads.
- **Archive** `omp-project-<YYYYMMDDTHHMMSSZ>-<host>-<slug>.tar.gz` (slug = sanitized folder name ≤ 40),
  streamed to `$XDG_CACHE_HOME/omp-deck/project-sync/<random>/` (never /tmp; removed after; stale > 24 h
  dirs swept at job start). Members: `omp/<rel>`, `sessions/<id>/session.jsonl`, `sessions/<id>/artifacts/<rel>`,
  `blobs/<hex>` (deduped), then **`manifest.json` last**: `{format:1, kind:"project", created, host,
  app_version, omp_version, project (abs path), name, fingerprint, omp: Option<{files:[{path,size,sha256,mode}]}>,
  sessions: Option<{items:[{id, file_name, title, modified_unix, size, sha256, artifacts:[ManifestFile],
  blobs:[hex]}], blobs:[{sha256,size}]}>}`.
- **Upload**: resumable, 8 MiB chunks (256 KiB multiple), 308 handled (`max_redirects(0)`), resume via
  `bytes */total` on network/5xx (≤ 5 attempts, backoff), 404/410 → one fresh session; no global timeout over
  the transfer (stall timeout ≥ 60 s). appProperties (each ≤ 124 B): `kind=project`, `host`, `pid` (path_id),
  `fp` (fingerprint), `name`, `path` (if too long: `…` + tail), `omp` (bytes), `sess` (count), `sessb` (bytes).
  After upload: keep newest `KEEP_PROJECT_BACKUPS` per (host, pid); prune failure only logged. List query
  `appProperties has { key='kind' and value='project' } and trashed = false`, `spaces=appDataFolder`, paginated.
- **Prepare restore**: stream-download to the cache dir, extract to staging with validation (regular files only,
  safe relative paths under the four prefixes, manifest present exactly once, every member listed with matching
  size + sha256, no extras, `format==1`, `kind=="project"`). Plan: `.omp` overwritten/added relative paths
  (when `parts.omp`); sessions with their local copies (`SessionPlan.local`, see "Project restore: session times
  and id conflicts"). `parts` not present in the backup → `InvalidBackup`. `.omp` into `$HOME` → error.
- **Apply**: create target if missing. `.omp`: safety copy of every overwritten file to
  `$XDG_DATA_HOME/omp-deck/restore-safety/project-<stamp>/` (newest 5 project copies kept), then atomic writes
  (exec bit kept); **never deletes** local files. Sessions: per item build a pack-format tar in staging
  (`manifest.json {version:1,id,cwd:<original project>,omp:<omp_version or "unknown">}`, `session.jsonl` with the
  header id rewritten to a fresh UUIDv7-shaped id when `new_id`, `artifacts/**`, its `blobs/<hex>`), then
  `omp session unpack <tar> --cwd <target>` (stdin null, terminal env vars removed per facts, 120 s timeout,
  process killed on timeout). stdout path → `sessions_added`; non-zero exit or unexpected stderr mode →
  `sessions_failed` entry (continue with the rest). Cancel between items.

### omp-deck

- Settings nav gains **Sync** (cloud icon) right under OmpDeck; omp catalog tabs shift down. Page:
  account strip (email / Sign in / "not available in this build", reusing `DriveAction`), **Projects** table
  (rows from `cfg.project_sync`): folder (`~`-abbreviated, tooltip full path, Change…), `.omp` switch + size
  (disabled with reason when Missing/Global), Sessions switch + `N sessions · size`, last backup (relative age,
  Never, or error with tooltip), row actions Back up now (progress while running) and Remove. "Add folder…"
  (native picker when kdialog/zenity exists, else typed path). Interval selector (`SyncInterval`). Short note:
  stored in the private Drive app folder, counts toward Drive storage, `.omp` may contain secrets.
  **Restore…** opens the restore dialog.
- **Restore dialog** (modal): cloud projects grouped by (host, pid): name, path, host ("this PC" when equal),
  newest date, parts + sizes, older versions selectable. Target folder (preset to the original path when it
  exists here, else empty; Browse…/typed; missing → "will be created"), `.omp` / Sessions checkboxes (only parts
  in the backup). Continue → prepare (progress, Cancel) → confirm with the plan: warning (danger) when the
  target `.omp` exists and N files will be overwritten (first few listed); "K sessions will be added alongside
  existing ones (J get a new id)". Restore → progress → toast summary; sessions failures listed; sidebar
  refresh (`SessionWatcher::refresh_now`).
- **Sidebar**: project header menu gains "Back up to Google Drive…" and "Restore from Google Drive…"; the
  shared session menu (rows + tabs) gains a "Google Drive" submenu with both for `entry.cwd`. Hidden when the
  build has no credentials; disabled with a reason when signed out or the project lane is busy. Backup dialog:
  folder, both switches with sizes (measured on a worker), "Keep this folder in Sync" (default on: adds/updates
  the row), "Back up now" (forced). Restore from the sidebar = restore dialog with the target fixed.
- **Lane**: its own job phase, one project job at a time, independent of the config lane; progress throttled
  (≤ 10 repaints/s). Auto: first pass 2 min after startup/sign-in, then every `SyncInterval::period()`, rows in
  order, unchanged rows skipped by fingerprint; first automatic failure per pass toasts, the rest log. Sizes are
  measured on a worker when the page/dialog opens, after row changes and after jobs — never per frame.

### Wave 10 fix-loop additions

- `OmpDir::Symlink`: a symlinked `<folder>/.omp` is never followed or backed up (switch disabled with a reason);
  symlinked artifact dirs and skipped `.omp` entries (symlinks, special files, non-UTF-8 or `\` names, too deep,
  unreadable) are listed in `ProjectBackupOutcome.skipped: Vec<String>` (`"<what>: <reason>"`).
- `ProjectSynced.unresolved_blobs: BTreeMap<String, String>` (`#[serde(default)]`): missing/damaged blob state
  observed while packing; any later change makes the row count as changed.
- Sessions with a jsonl/artifact/blob member > 2 GiB (omp's pack limit) are excluded at backup (listed in
  `skipped`) and flagged at prepare in `SessionPlan.problem: Option<String>` (reported failed, omp not run).
- Restore never writes through symlinks: `<target>/.omp` that is a symlink or file, or a target that is a file,
  fails at prepare; a symlinked file/intermediate dir or a file/folder clash inside `.omp` is listed in
  `ProjectRestorePlan.omp_skipped` / `ProjectRestoreReport.omp_skipped` and not written (re-checked at apply).
  A target folder that is itself a symlinked path is allowed (user-chosen). Concurrent symlink swaps between
  check and write are out of the threat model (needs write access to the user's own folders).
- Prepare checks each archived session header id equals its item id. Apply runs omp with
  `PI_CODING_AGENT_DIR = <prepared root>.agent_dir()` and `TMPDIR`/`TMP`/`TEMP` inside the staging dir; omp
  failure text is reduced to its `error:` line (≤ 240 chars).
- Cancel after the first write returns `Ok(ProjectRestoreReport { cancelled: true, .. })` with what was done;
  `Err(Cancelled)` (displays "cancelled") only when nothing was written.
- Transfers: initiation, chunks and downloads retry network/429/5xx with bounded backoff; a 308 that does not
  advance, or a download that does not pass its high-water mark, counts as a failure. The access token is
  fetched before packing (offline fails at once). `Service.cache_root` (pub) = `$XDG_CACHE_HOME/omp-deck/project-sync`.

## Wave 11: cloud backup management, deliberate add, session opt-out, config backup interval

### deck-core
- `config::SyncInterval` gains `Every10Minutes` ("Every 10 minutes") and `Every30Minutes` ("Every 30 minutes");
  `ALL` = `[Off, Every10Minutes, Every30Minutes, Hourly, Every6Hours, Daily]`. Existing serialized names unchanged.
  `#[default]` stays `Every6Hours` (project sync).
- `AppConfig.drive_auto_backup: bool` is replaced by `AppConfig.drive_backup_interval: SyncInterval`
  (config backup lane; default `Every10Minutes`). Migration on load: a stored `"drive_auto_backup": false` with no
  `drive_backup_interval` key → `Off`; `true`/absent → `Every10Minutes`. The old key is never written again.
- `config::SyncRow` gains `#[serde(default)] excluded_sessions: BTreeSet<String>` (session ids opted out; every
  other session, including ones created later, is backed up).
- `backup::ProjectSizes` (now `Clone`, not `Copy`) gains `session_list: Vec<SessionSize>`, newest first, for every
  session of the project (exclusions not applied; `sessions`/`session_bytes` stay totals over all of them).
  `SessionSize { id: String, title: String /* title, else first-prompt snippet, else "(untitled)" */,
  modified: SystemTime, bytes: u64 /* jsonl + artifacts */ }`.
- `Service::backup_project(root, project, parts, excluded: &BTreeSet<String>, force, cancel, progress)`: sessions
  whose id is in `excluded` are left out silently (not in `skipped`); the fingerprint covers the excluded set, so
  changing it counts as a change. Selected parts with nothing left (all sessions excluded, no `.omp`) →
  `Error::NothingToBackUp`.
- `Service::delete_project_backup(&self, backup: &ProjectBackup) -> Result<()>`: Drive `files.delete` (permanent,
  bypasses trash; appDataFolder files cannot be trashed anyway); 404 = already gone = `Ok`. Refuses (typed error)
  a file whose appProperties `kind` is not `project`. If the deleted file's name equals the `last.name` of a
  `ProjectSyncState` entry, that entry is removed (state saved) so the next pass uploads that project again.

### omp-deck
- Add folder / Change folder only adds/changes the row: new rows start with `.omp` and Sessions **off** and no
  exclusions; nothing is packed or uploaded until the user clicks Back up now (or the row is later due in an
  automatic pass with something selected). Adding a row never makes a pass due immediately (if `next_auto` is in
  the past, push it to now + period). Sizes are still measured on a worker.
- Sessions cell: count/size of the **included** sessions (`9 of 11 sessions · 80 MB` when some are excluded,
  computed on measure/exclusion change, never per frame) + a "Choose…" button → dialog "Sessions to back up":
  filter box, All / None, checkbox list (checked = backed up; title, age, size), note "New sessions are backed
  up automatically", Save / Cancel. Save stores the unchecked ids (only ids in the current list).
- Every backup of a row (manual, automatic, sidebar dialog for a folder in the table) passes the row's
  exclusions; the sidebar backup dialog shows "N sessions excluded on the Sync page" when nonzero.
- Delete cloud backups (project lane, one job at a time, never while another project job runs):
  - Restore dialog: each version has a trash button → confirm "Delete permanently" (project, PC, date, size,
    "This can't be undone"), then the list refreshes.
  - Sync page: "Manage cloud backups…" next to Restore… → dialog listing every project backup grouped by project
    → PC ("this PC" for ours) → versions (date, size, parts), version checkboxes + group select-all, Refresh,
    footer "N selected · size", "Delete permanently…" → confirm (count, total size, can't be undone) → worker
    deletes sequentially with progress "Deleting i of n", stops on cancel, reports deleted/failed, refreshes.
- Settings → OmpDeck → Google Drive backup: the "Back up automatically" switch becomes a dropdown over
  `SyncInterval::ALL` ("Off" disables; others = period between checks; unchanged config is still skipped). The
  first automatic check after startup/sign-in/turning it on stays 45 s.

## Wave 12: native Windows + OmpDeck host (tmux replacement)

Facts: `docs/notes/windows-host.md`. Goal: OmpDeck builds, tests and runs natively on Windows 10/11 x64
(arm64 best effort) with sessions that survive closing a tab or the app, via our own per-session host daemon.
Linux keeps tmux as the default backend; the host is the Linux fallback when tmux is missing (replacing the
no-persistence "direct" mode there) and can be chosen explicitly. Every crate compiles for
`x86_64-pc-windows-msvc` with clippy `-D warnings` (cargo-xwin locally, CI natively).

### New crate `crates/deck-host` (lib + bin `omp-deck-host`)
Depends on nothing of ours. Deps: alacritty_terminal (headless Term), serde/serde_json, thiserror, log,
getrandom, sha2, dirs; `libc` (unix); `windows-sys` 0.61 (windows). No portable-pty, no tokio.

**Namespaces and endpoints.** `Namespace::new(ns)` with `ns` = `omp-deck` (app) or `omp-deck-test-<unique>`
(tests; `[A-Za-z0-9_-]{1,40}`). Session names `[A-Za-z0-9_-]{1,64}` (`session_name_for` already conforms).
- Unix: socket `<base>/<ns>/<name>.sock`, `<base>` = `$XDG_RUNTIME_DIR/omp-deck-host` else
  `<temp_dir>/omp-deck-host-<uid>`; base and ns dirs 0700 and owned by us (refuse otherwise); sun_path length
  checked (typed error).
- Windows: named pipe `\\.\pipe\omp-deck-host.<user>.<ns>.<name>`, `<user>` = 16 hex of sha256(current user
  SID string). Pipe DACL = current user SID only (+SYSTEM), `PIPE_REJECT_REMOTE_CLIENTS`, first instance created
  with `FILE_FLAG_FIRST_PIPE_INSTANCE` (a second daemon for the same name fails). Listing = enumerate
  `\\.\pipe\` entries with the `omp-deck-host.<user>.<ns>.` prefix.

**Wire protocol** (version `PROTOCOL = 1`; both sides send `Hello{protocol, version}` first; a major mismatch is
a typed error, the GUI can still `Kill`). Frames: `u32 LE length` (≤ 1 MiB) + `u8 kind` + payload. Byte
payloads raw (`Input`, `Output`, `Snapshot` chunks), control payloads JSON (unknown fields ignored).
- client -> daemon: `Hello`, `Attach{cols, rows, cell_width, cell_height, history_lines, palette}`, `Input(bytes)`,
  `Resize{cols, rows, cell_width, cell_height}`, `Palette(Palette)`, `Detach`, `Kill`, `Info`, `Ping`.
- daemon -> client: `Hello`, `Info(HostInfo)`, `Snapshot(bytes)`… `SnapshotEnd`, `Output(bytes)`,
  `Exited{code: Option<i32>}`, `Displaced` (another client attached), `Error(String)`, `Pong`.
- `Palette`: 16 ANSI + fg + bg + cursor as `[u8;3]` (answers OSC 4/10/11/12 queries).
- `HostInfo { name, protocol, host_version, daemon_pid, child_pid, child_started_unix_ms, cwd, argv,
  tty: Option<String> /* unix slave path */, term_session_id: Option<String> /* windows */,
  attached: bool, cols, rows }`.
- One attached client: a new `Attach` sends `Displaced` to the old one and closes it. Non-attach connections
  (Info/Kill/Ping) are served concurrently.

**Client API** (`deck_host::Client`, blocking; callers run it on workers):
- `Namespace::list(&self) -> Vec<String>` (alive = endpoint accepts + Hello within 500 ms), `info(name) ->
  Result<HostInfo>`, `has(name) -> bool`, `kill(name) -> Result<()>` (returns after the daemon confirmed or the
  endpoint vanished, ≤ 5 s).
- `create_or_attach(&Namespace, &SessionSpec, AttachParams) -> Result<Attached>`: attach if alive (never
  respawn), else spawn the daemon detached (`daemon_exe()` = `omp-deck-host[.exe]` next to `current_exe()`,
  env override `OMP_DECK_HOST_EXE`) with the spec passed through a 0600 file the daemon deletes, wait ≤ 5 s
  for the endpoint (daemon startup errors are reported from its log), then attach.
- `SessionSpec { name, program, args: Vec<String>, cwd, env_set: Vec<(String,String)>, env_remove: Vec<String> }`.
- `Attached` splits into `AttachedRx` (blocking `recv() -> Result<HostEvent>`; `HostEvent::{Snapshot(Vec<u8>)
  /* whole replay */, Output(Vec<u8>), Exited(Option<i32>), Displaced, Closed}`) and a cloneable
  `AttachedTx` (`input(&[u8])`, `resize(..)`, `palette(..)`, `kill()`, `detach()`); drop = detach.

**Daemon** (`omp-deck-host serve --ns <ns> --name <name> --spec <file>`; also `ls`, `info <name>`,
`kill <name>`, `dump <name>` (attach, print the snapshot as plain text, detach) for debugging and CI):
- Detached from the GUI: Unix `setsid` + stdio to null; Windows `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP |
  CREATE_NO_WINDOW` (+ `CREATE_BREAKAWAY_FROM_JOB`, retried without it on access denied). Release builds use the
  Windows GUI subsystem (no console window). Logs to `<cache_dir>/omp-deck/host/<ns>/<name>.log` (≤ 1 MiB).
- Child PTY, own module `deck_host::pty`: Unix `openpty` + `Command` (`setsid`, `TIOCSCTTY`, slave as stdio);
  Windows ConPTY via `conpty.dll` sideload (exe dir) with kernel32 fallback, flags chosen by CI byte probes
  (start with `RESIZE_QUIRK | WIN32_INPUT_MODE`; PASSTHROUGH only with the sideloaded DLL on build ≥ 22621 if
  the probe proves it helps), child created `CREATE_SUSPENDED` with `EXTENDED_STARTUPINFO_PRESENT`, assigned to a
  `KILL_ON_JOB_CLOSE` job, then resumed; MSVCRT argv quoting; `.cmd`/`.bat` via `cmd.exe /d /c`. No panics on
  ConPTY errors (typed errors).
- Output loop: one reader thread; each chunk goes to `vte::ansi::Processor` -> headless `Term`
  (`scrolling_history = 50_000`), to a side-state scanner, and to the attached client as `Output`. The daemon's
  `Term` answers every query (DA/DSR/CPR/kitty `CSI ? u`/OSC 4/10/11/12 with the last client palette) by writing
  to the PTY, attached or not. Windows Term config: `kitty_keyboard = false`; Unix: `true`.
- Side-state the snapshot needs beyond `Term`: title (from `Event::Title`), `CSI > 4;n m`, `?9001`, `?2031`,
  `?2026` state, scroll region (DECSTBM). Images (APC) are not retained (same as tmux today).
- Snapshot on attach (taken under the Term lock between reads, then live `Output` follows): VT replay that a fresh
  alacritty `Term` turns into the same state: reset; up to `history_lines` scrollback rows then screen rows of the
  primary grid (SGR, zero-width, OSC 8, WRAPLINE joins); if alt screen active: `?1049h` + alt grid; cursors,
  DECSC saved cursor, DECSCUSR, every `TermMode` bit, kitty flags (`CSI = flags ; 1 u`), modifyOtherKeys,
  `?9001`, `?2031`, OSC 4/10/11 overrides, title, scroll region. Pure function
  `snapshot::encode(&Term, &SideState, history_lines) -> Vec<u8>`, tested by round-tripping through a fresh Term.
- Resize: PTY + Term resize (cell pixel size kept for `CSI 14t`). Child exit: `Exited{code}` to the client,
  endpoint removed, daemon exits. `Kill`: Unix `kill(-pgid, SIGHUP)`, `SIGKILL` after 2 s; Windows
  `TerminateJobObject`; then exit.

### deck-core
- Depends on deck-host. New `deck_core::mux`:
  - `BackendChoice { Auto (default), Tmux, Host }` persisted as `AppConfig.session_backend` (`#[serde(default)]`);
    Windows always resolves to Host.
  - `enum Mux { Tmux(Tmux), Host(HostMux) }`; `Mux::detect(choice) -> Option<Mux>` (Auto: tmux if installed else
    host if `daemon_exe()` exists; env `OMP_DECK_TMUX_SOCKET` / `OMP_DECK_HOST_NS` select test namespaces).
  - `kind()`, `namespace() -> &str`, `launch(name, cwd, argv) -> Launch` with `Launch::Pty(LaunchSpec)` (tmux,
    unchanged command) | `Launch::Host(deck_host::SessionSpec)` (env per windows-host.md "Env for omp
    children", Windows adds `TERM_SESSION_ID=<16 hex>` derived from the session name),
    `alive_sessions()`, `has_session()`, `kill()`, `session_files(root)` (host: breadcrumb of each session via
    `HostInfo.tty` / `term_session_id`), `pane(name) -> Option<Pane { breadcrumb_key: String, started:
    SystemTime }>` (replaces `pane_tty` + `pane_started`; tmux keeps its `/proc` path on Linux).
  - `tmux::direct_launch` stays only as the last-resort Unix fallback when neither backend is available.
- `deck_core::platform` (one home for OS differences): `hostname()`, `process_start(pid)`, `find_on_path(name)`
  (PATHEXT on Windows), `is_executable`, `create_private_dir`, `write_private`, `open_private_file`,
  `file_identity(&fs::Metadata/&File) -> Option<FileId>` (Unix dev/ino, Windows volume serial + file index),
  `KillableCommand`/`spawn_killable` (Unix process group, Windows job object; `kill_tree()`), `open_path(path)`
  (xdg-open / ShellExecuteW), `reveal(file)` (file manager selecting the file; `explorer /select,` on Windows),
  `user_dirs` fallbacks via `dirs` only (no `~/.config`-style hardcoding). Every `std::os::unix` use outside
  `platform` and `cfg(unix)` tests is gone.
- ctl on Windows: pipe `\\.\pipe\ompdeck-ctl.<user>.<key>` passed as `OMPDECK_CTL_PIPE` (Rust computes, TS never
  derives); `ompdeck-ctl.ts` uses `node:net` on win32, no lock file, no mode check, no socket unlink; Unix path
  unchanged. `Command::env` replaces the `env` wrapper argv on both OSes.
- omp on Windows: `Omp::detect` searches PATH (PATHEXT), `%LOCALAPPDATA%\omp\omp.exe`,
  `%USERPROFILE%\.bun\bin\omp.exe`; `.cmd` runs through `cmd.exe /d /c`; install argv = PowerShell
  `& ([scriptblock]::Create((irm https://omp.sh/install.ps1))) -Binary` (`-NoProfile -ExecutionPolicy Bypass`).
- backup on Windows: no POSIX modes (stored as 0o644/0o755 by origin), `path_id` over `as_encoded_bytes()`,
  restore skips (and lists in `omp_skipped`) names invalid on Windows (`<>:"|?*`, control chars, reserved
  CON/PRN/AUX/NUL/COM1-9/LPT1-9, trailing dot/space) and never writes through reparse points.
- `status.rs` breadcrumb trust uses `Mux::pane`; a bare `omp` title is noise on Windows.

### deck-term
- Compiles and works on Windows: clipboard without PRIMARY there; local-PTY path (`TrackedPty`, alacritty
  `EventLoop`) is `cfg(unix)`; Windows uses only remote terminals. Windows font discovery (Cascadia Mono ->
  Consolas -> Courier New; fallbacks Segoe UI Symbol, Segoe UI Emoji, Segoe UI) with `fc-match` kept on Unix.
- `TermOptions.kitty_keyboard: bool` (default true).
- Remote terminals: `pub trait Remote: Send` with `fn split(self: Box<Self>) -> (Box<dyn RemoteRx>,
  Box<dyn RemoteTx>)`; `RemoteRx::recv(&mut self) -> io::Result<RemoteEvent>` (`Snapshot(Vec<u8>)`,
  `Output(Vec<u8>)`, `Exited(Option<i32>)`, `Displaced`, `Closed`); `RemoteTx: Send + Sync` with `input`,
  `resize(cols, rows, cell_w, cell_h)`, `palette([[u8;3]; 19])`, `kill`, `detach`.
  `Terminal::connect(Box<dyn Remote>, TermOptions, egui::Context) -> Terminal` (same `TerminalView`, events,
  title, bell, exit, screen_text). In remote mode the local `Term` never answers queries (the host does) except
  OSC 52 clipboard loads; a `Snapshot` resets the local Term and replays; `Displaced` shows
  `[attached elsewhere]` and stops input; drop = detach.
- win32-input-mode: track `CSI ? 9001 h/l` in output (local and remote, like modifyOtherKeys). While on, keys are
  encoded as `CSI Vk;Sc;Uc;Kd;Cs;Rc _` (key down and up; `Uc` from the text event; `Cs` = Windows control-key
  state; `Sc` from `MapVirtualKeyW` on Windows, 0 elsewhere); mouse stays VT; paste per Windows Terminal's
  behavior. Pure encoders, exact-byte tests.

### omp-deck
- Uses `Mux` everywhere (no direct `Tmux` outside deck-core): launch -> `Terminal::spawn` for `Launch::Pty`,
  worker `create_or_attach` -> `Terminal::connect` for `Launch::Host` (tab shows "Starting…" meanwhile; errors in
  the tab); running indicators, Stop, Restart, Delete/Move kill, tab restore, status-bar breadcrumb via `Mux`.
- Settings → OmpDeck (Unix only): "Session backend" Auto / tmux / OmpDeck host (applies to new tabs; live
  sessions keep their backend until closed). No-tmux banner replaced: Auto with host available needs none;
  direct fallback keeps the banner.
- Windows app: `windows_subsystem = "windows"` (release), embedded icon + version info, client-side title bar
  (unchanged look), folder picker via `rfd` (Windows only; kdialog/zenity stay on Linux), `platform::open_path`
  / `reveal`, UI fonts fallback (Segoe UI Variable / Segoe UI), config in `dirs::config_dir()/omp-deck`.

### Packaging / CI
- CI: add `windows-latest` job (clippy `-D warnings`, `cargo test --workspace`, Windows-only host tests incl.
  ConPTY byte probes with a test child that echoes what it reads), keep Linux job (+ host backend tests).
- Release: `omp-deck-<v>-x86_64-windows.zip` (omp-deck.exe, omp-deck-host.exe, conpty.dll, OpenConsole.exe,
  licenses incl. ConPTY MIT notice) + sha256; arm64 zip if its runner builds green. `packaging/install.ps1`
  (latest release, sha256 check, `%LOCALAPPDATA%\Programs\OmpDeck`, Start Menu shortcut, rename-aside of locked
  exes, installs omp via omp.sh/install.ps1 unless `-NoOmp`, `-Uninstall`). Linux tarball/AppImage/deb/rpm also
  ship `omp-deck-host`.

### Wave 12 as built (Wave A; supersedes the sketch above where they differ)
- deck-host: no `Client` struct. `Namespace::{new, as_str, log_path, list, has, info, kill}`; free fns
  `daemon_exe() -> Option<PathBuf>` (env `OMP_DECK_HOST_EXE`), `start(ns, spec, cols, rows)` (create, no attach),
  `create_or_attach(ns, spec, AttachParams) -> Result<Attached>`, `attach(ns, name, params)`,
  `peek(ns, name, history_lines) -> (HostInfo, Vec<u8>)` (frame `Peek` 0x0a: snapshot without displacing/resizing).
  `Attached::split() -> (AttachedRx, AttachedTx)`; `AttachedRx::recv() -> Result<HostEvent>`; `AttachedTx: Clone`
  `input/resize/palette(&Palette)/kill/detach -> Result<()>`. AttachParams cols/rows 0 = keep size.
  `snapshot::encode(&mut Term, ..)`; kitty flags replayed as `CSI > f u`; title tracked by the output scanner.
  Env `OMP_DECK_CONPTY_FLAGS` (default 0x6 = RESIZE_QUIRK | WIN32_INPUT_MODE). CLI `new|ls|info|dump|kill`,
  `--ns` default `omp-deck`.
- deck-core: `platform::{hostname, process_start, home_or_root, root_dir, no_window, path_extensions,
  is_executable, find_in, find_on_path, create_private_dir(_all), open_private_file, write_private, FileId,
  file_identity, file_mode, set_file_mode, open_regular_file, is_link, path_bytes, tree_open_files,
  KillableCommand, spawn_killable, KillableChild::kill_tree, open_path, reveal}`; `mux::{BackendChoice, MuxKind,
  MuxError, Pane, Launch, LaunchSpec, Mux, HostMux, host_env, term_session_id, is_noise_title, title_is_noise,
  session_name_for, new_session_name, direct_launch, effective_cwd, terminal_env, HOST_NAMESPACE,
  TMUX_SOCKET_ENV, HOST_NS_ENV, HOST_ENV_REMOVE}` (moved from `tmux`, no re-exports; the env overrides accept only
  `omp-deck-test-…` names that are valid host namespaces, ≤ 40 bytes); `Mux::{detect, kind, namespace, launch,
  launch_with_env, alive_sessions, has_session, kill, session_files, pane}`; `ctl::with_ctl(omp_argv, ext)` (no
  dir/env wrapper) + `ctl_env(dir) -> io::Result<Vec<(&'static str, OsString)>>` (`OMPDECK_CTL_DIR`; Windows
  `OMPDECK_CTL_PIPE` + `OMPDECK_CTL_TOKEN` = sha256(secret ‖ pipe), secret in `default_key_file()`);
  `ctl_env_with_key(dir, key_file)` / `CtlClient::spawn_with_key` inject the key file (tests).
  `TabState.session_name` (serde alias `tmux_name`).
  `StatusTarget.mux: Option<(Mux, String)>`; `Tmux::pane`, `Tmux::detect_on`; `AppConfig.session_backend`.
- deck-term: `Remote`/`RemoteRx`/`RemoteTx` (`&self` methods returning `io::Result<()>`; `palette([[u8;3];19])`),
  `RemoteEvent`, `Terminal::connect(Box<dyn Remote>, TermOptions, Context) -> Terminal`, `Terminal::is_displaced`,
  `TermOptions.kitty_keyboard`, `KeyRules::Win32`, `encode_win32_key`, `encode_win32_modifiers`,
  `encode_paste_win32`; `Terminal::spawn`/`SpawnSpec` are `cfg(unix)`. After `detach()`/`kill()` the Remote's
  `recv` must return `Closed`/error promptly.

## Wave 13: git worktree sessions

Facts + user decisions: `docs/notes/worktrees.md` (authoritative for behavior). omp never gets a worktree flag;
OmpDeck composes `omp worktree add` + plain `omp`, omp's `/wt`, and git. OmpDeck never writes `~/.omp/**` itself
(`omp worktree add` creates the dir under the managed base).

### deck-core
- New module `worktree` (pub; no GUI deps; all git/omp runs blocking — callers use workers):
  - `pub struct Worktree { pub path: PathBuf /* worktree root */, pub primary: PathBuf /* main checkout root */,
    pub branch: Option<String> /* None = detached */ }` (Clone, Debug, PartialEq, Eq).
  - `pub fn resolve(dir: &Path) -> Option<Worktree>`: filesystem only, no process (cheap enough for the scanner):
    walk up from `dir` (bounded) to the first `.git`; a **file** `gitdir: <x>` (relative to the file's dir allowed)
    → read `<x>/commondir` (relative to `<x>`) → common dir; primary = parent of a common dir named `.git`
    (bare/other layouts → None); branch from `<x>/HEAD` (`ref: refs/heads/<b>`). A `.git` directory → None.
  - `pub fn managed_base(setting: Option<&str>) -> PathBuf`: `OMP_WORKTREE_DIR` (absolute or `~`) > `setting`
    (absolute or `~`) > `<home>/.omp/wt` (same precedence as omp's `getWorktreesDir`; relative values ignored).
  - `pub fn slug(branch: &str) -> String` (path-safe, lowercase-free-preserving: `[A-Za-z0-9._-]`, others → `-`,
    collapsed, trimmed, ≤ 60 bytes, never empty/`.`/`..`).
  - `pub fn default_location(base: &Path, primary: &Path, branch: &str) -> PathBuf`:
    `<base>/<slug(branch)>-<slug(primary file name)>`, then `-2`, `-3`, … while the path exists.
  - `pub struct RepoInfo { pub root: PathBuf /* toplevel of dir */, pub primary: PathBuf, pub current_branch:
    Option<String>, pub branches: Vec<String> /* local first, then remote-tracking `origin/x`; no HEAD aliases */,
    pub has_commits: bool }`; `pub fn repo_info(dir: &Path) -> Result<RepoInfo, WorktreeError>`
    (`NotARepo` typed error when outside git).
  - `pub fn check_branch_name(repo: &Path, name: &str) -> Result<(), WorktreeError>` (`git check-ref-format
    --branch`, plus "already exists" check).
  - `pub fn create(omp: &Omp, repo: &Path, branch: &str, base_ref: &str, dest: &Path) -> Result<(), WorktreeError>`:
    `omp worktree add -q -C <repo> -b <branch> <dest> <base_ref>` via the bounded/tree-killing runner (120 s),
    with `PI_CONFIG_FILES=<overlay>` where the overlay (written under OmpDeck's data dir, not `~/.omp`) sets
    `worktree.clone: false` so ignored files are never copied (user decision: clean checkout). Verify the overlay
    key format and that `omp worktree add` honours it (v18.12.6 source + sandbox: the "clone fell back" warning
    must disappear). Errors carry omp's stderr.
  - `pub struct WorktreeStatus { pub branch: Option<String>, pub dirty: bool /* staged|unstaged|untracked,
    ignored excluded */, pub merged: bool /* branch tip is an ancestor of base */, pub ahead: u32 /* commits not
    in base */ }`; `pub fn status(wt: &Worktree, base: &str) -> Result<WorktreeStatus, WorktreeError>`.
  - `pub fn infer_base(wt: &Worktree) -> Option<String>`: the primary checkout's current branch, else the remote
    default branch (`origin/HEAD` target), else None.
  - `pub enum Unsafe { Dirty, NotMerged { ahead: u32 }, Detached, Missing }`;
    `pub fn remove(wt: &Worktree, base: &str, delete_branch: bool) -> Result<(), WorktreeError>`: re-checks
    status first and returns `WorktreeError::Unsafe(Vec<Unsafe>)` without touching anything when not clean+merged;
    then `git -C <primary> worktree remove <path>` (never `--force`) and, if asked, `git -C <primary> branch -d
    <branch>` (never `-D`). `remove_unchecked` must not exist.
  - `WorktreeError` (thiserror): `NotARepo`, `Git { command, stderr }`, `Omp { stderr }`, `Unsafe(Vec<Unsafe>)`,
    `Io`, `Timeout`.
- `sessions::ProjectGroup` gains `pub worktree: Option<worktree::Worktree>` (from `resolve(cwd)` at scan time;
  every constructor updated).
- `status::GitInfo` gains `pub worktree_of: Option<PathBuf>` (primary root when the cwd is a linked worktree).
- `config::AppConfig` gains `pub worktree_bases: BTreeMap<PathBuf, String>` (worktree path → base branch
  recorded by OmpDeck at create / move time; serde default; pruned on removal).

### omp-deck
- Sidebar: groups with `worktree: Some(w)` nest under the group whose cwd == `w.primary` (a synthetic, session-less
  parent header is shown when the primary has no sessions); child header shows the branch (fallback: folder
  name); parent sort key = newest of itself and children; expanding follows the active tab (parent + child);
  filter matches children; per-folder scope includes worktrees whose primary is in scope.
- Project header menu (non-worktree groups): "New session in worktree…" → dialog (branch name prefilled
  `wt/<yyyymmdd-hhmm>`, base ref dropdown from `repo_info` defaulting to the current branch, location defaulting
  to `default_location(managed_base(omp worktree.base setting), primary, branch)` + Browse/edit, inline
  validation via `check_branch_name` on a worker) → worker `worktree::create` → record `worktree_bases` → open a
  tab running plain `omp` in the worktree (same path as "New session here").
- Running-tab / session-row menu: "Move to new worktree…" (tab running, not already a linked worktree): branch
  dialog → record base (tab's current branch) → `Job::slash("Move to worktree", "/wt <branch>",
  Requires::Idle)`; the tab follows omp's relocation.
- Worktree group header menu: "Finish worktree" and "Remove worktree…" (+ existing items).
  - Finish: target = the worktree's running tab (else open its newest session in a tab first); inject
    `Job::slash("Finish worktree", <finish prompt>, Requires::Idle)`. Prompt (one line) tells the agent: commit
    its work on `<branch>`, merge it into `<base>` in the main checkout at `<primary>` (e.g. `git -C <primary>
    merge <branch>`), follow this project's rules on committing/merging and ask the user first if they don't
    allow doing it unprompted, don't delete the worktree or branch (OmpDeck does), report the result.
    Mark the worktree "finishing" (chip/badge on its header + cancel). On every busy→idle transition of that tab
    while finishing: worker `status`; clean+merged → stop **only** the tabs whose cwd is inside the worktree
    (`Backends::stop` per tab), `worktree::remove(.., delete_branch: true)`, toast; otherwise keep waiting
    (one info toast with the reason the first time). Finishing state is in-memory only.
  - Remove…: confirm dialog listing branch/base/status; refuses with the reasons from `Unsafe`; stops only that
    worktree's tabs first.
  - After any removal: drop `worktree_bases`, `folder_tabs`, `recent_folders` entries for the path; if a Drive
    project-sync row exists for it, ask whether to drop the row (cloud backups untouched).
- Status bar git chip: worktree icon + tooltip "Worktree of <primary>" when `worktree_of` is set.
- Housekeeping: whenever a tab's `session_path` changes (breadcrumb/`/proc` follow), set `tab.cwd` to the scanned
  entry's header cwd (or the breadcrumb cwd) so restore/reopen/sync use the relocated folder.
- (Wave 13 follow-up) `config::AppConfig.removed_worktrees: BTreeMap<PathBuf, RemovedWorktree>` +
  `RemovedWorktree { primary: PathBuf, branch: Option<String> }`: recorded by omp-deck on every worktree it removes;
  the sidebar nests groups whose cwd is a key and no longer exists under the primary as "<branch> (removed)", and
  resuming such a session launches with the primary as cwd (omp relocates it there). Entries whose primary is gone
  are pruned.

### Wave 13 as built (supersedes the sketch above where they differ)
- `worktree::remove(wt, base, delete_branch) -> Result<Removed, WorktreeError>`, `Removed { branch_deleted: bool,
  branch_note: Option<String> }`: verifies clean + tip ancestor of `base`, `git worktree remove -- <path>` (no
  force), then `git update-ref -d refs/heads/<b> <verified tip>` (+ best-effort `branch.<b>` config removal) —
  works when `base` isn't checked out (`branch -d` would refuse); a moved branch is kept with a note (partial
  success, not an error). A clean detached HEAD already in base is removable; missing dir → `Unsafe([Missing])`;
  `status` rejects a base that is empty, starts with `-`, or doesn't resolve. Paths are lexically normalized.
- `create` reads `PI_CONFIG_FILES` and the data dir from the `Omp` command env first (sandboxable), rejects an
  option-like base and a relative dest before running omp.
- `Tmux::attach_or_create` returns `LaunchSpec.cwd = platform::root_dir()`; the pane dir only travels as `-c`
  (the tmux server inherits its first client's cwd; a removed worktree stranded it).
- omp-deck removal: a worker classifies tabs (`worktree_ui::classify`) by live cwd (breadcrumb when fresh for the
  pane / matching session, else launch cwd for direct tabs; canonical paths, Windows case-insensitive, component
  prefix); the UI stops exactly those, a second worker re-classifies, refuses if one still runs inside, then
  removes. A direct tab with unknown live cwd launched in the primary blocks removal (named in the message).
- Finish: merge target computed on a worker from `git worktree list --porcelain` (primary on base → merge there;
  base in another worktree → merge there; base checked out nowhere → merge base into the worktree and
  `push . HEAD:refs/heads/<base>`); all values single-quoted + code-spanned; injection retried every 1.5 s until
  `FINISH_TAB_TIMEOUT`; exited tabs are replaced by a resumed session.

## Wave 14: notifications, changes panel, command palette, OmpDeck self-update

Facts + user decisions: `docs/notes/wave14.md`. New workspace deps (root `Cargo.toml`): `zip` (deflate-flate2),
`zbus` 5 (async-io), `tauri-winrt-notification` 0.8, `wayland-client` 0.31 (system), `wayland-protocols` 0.32
(client, staging). Platform-gate them per target in the crate manifests.

### deck-core
- `config::AppConfig` gains (serde default; old configs load):
  - `notify: NotifyConfig { enabled: bool = true, turn_finished: bool = true, attention: bool = true,
    exited: bool = true, only_when_unfocused: bool = true }`
    (`only_when_unfocused = false` also notifies for background tabs while the window has focus).
  - `check_app_updates: bool = true`; `skipped_app_version: Option<String>` ("Skip this version").
  - `palette_recent: Vec<String>` (command ids, newest first, ≤ 20).
  - `changes_panel_width: f32` (0 = default).
- New `pub(crate) mod git`: the shared git runner moved out of `worktree.rs` (`git_command`, raw-bytes and string
  variants, exit code + stderr, timeouts, `LC_ALL=C`, `GIT_TERMINAL_PROMPT=0`, `no_window`, tree kill);
  `worktree.rs` uses it (behavior unchanged).
- New `pub mod changes` (blocking; callers use workers):
  - `pub enum Change { Modified, Added, Deleted, Renamed, Copied, TypeChanged }`
  - `pub struct ChangedFile { pub path: String /* display, repo-relative, lossy */, pub os_path: OsString /* exact,
    for commands */, pub orig_path: Option<String>, pub staged: Option<Change>, pub unstaged: Option<Change>,
    pub untracked: bool, pub conflicted: bool, pub is_dir: bool }`
  - `pub struct Changes { pub root: PathBuf, pub branch: Option<String>, pub files: Vec<ChangedFile> }`
  - `pub fn list(cwd: &Path) -> Result<Option<Changes>, ChangesError>` (`None` = not inside a work tree).
  - `pub enum DiffSide { Staged, Unstaged, Untracked }`;
    `pub struct FileDiff { pub text: String, pub binary: bool, pub truncated: bool, pub added: u32, pub removed: u32 }`;
    `pub fn diff(root: &Path, file: &ChangedFile, side: DiffSide) -> Result<FileDiff, ChangesError>` (cap 2 MiB /
    20 000 lines → `truncated`; untracked synthesized in Rust; empty text for mode/EOL-only changes is valid).
  - `pub fn patch(root: &Path) -> Result<String, ChangesError>`: staged + unstaged unified diff (for Copy diff).
  - Writes, each re-verifying `rev-parse --show-toplevel == root` first (else `ChangesError::RootChanged`):
    `stage(root, &[&ChangedFile])` (`add --`), `unstage` (`restore --staged --`; unborn HEAD → `rm --cached --`),
    `discard(root, &[&ChangedFile])` (tracked unstaged changes only: `restore --`; refuses conflicted),
    `delete_untracked(root, &[&ChangedFile])` (`clean -f[d] --` per path after a `clean -n` dry run lists exactly
    that path; never `-x`, never without a pathspec). Every path operand after `--` with `GIT_LITERAL_PATHSPECS=1`.
  - `ChangesError` (thiserror): `Git { command, stderr }`, `RootChanged`, `Conflicted`, `Io`, `Timeout`.
- New `pub mod selfupdate` (blocking; callers use workers):
  - `pub enum InstallKind { UserDir { dir: PathBuf } /* install.sh / tarball, writable */, AppImage { file: PathBuf },
    Deb, Rpm, WindowsDir { dir: PathBuf }, Dev /* cargo target dir */, Unsupported { reason: String } }`;
    `pub fn detect() -> InstallKind` (`$APPIMAGE`, `current_exe` location, `dpkg -S`/`rpm -qf` for `/usr`, dir
    writability probe; read-only/unknown → Unsupported).
  - `pub struct Release { pub version: String /* "0.9.0" */, pub tag: String, pub page_url: String }`;
    `pub fn latest() -> Result<Release, UpdateError>` (redirect of `<base>/releases/latest`, no API; base =
    `OMP_DECK_UPDATE_BASE` env or `https://github.com/lavinhoque33/omp-deck`, so tests/sandboxes can serve a fake
    release over local HTTP); `pub fn is_newer(latest: &str, current: &str) -> bool` (numeric semver, pre-release
    tags never newer than their release).
  - `pub fn asset_for(kind: &InstallKind, version: &str) -> Option<String>` (arch from `std::env::consts::ARCH`).
  - `pub fn download(release: &Release, asset: &str, dir: &Path, progress: &dyn Fn(u64, Option<u64>),
    cancel: &AtomicBool) -> Result<PathBuf, UpdateError>`: streams to `dir`, verifies against `<asset>.sha256`
    (mismatch → error, file deleted).
  - `pub fn apply(kind: &InstallKind, downloaded: &Path) -> Result<Applied, UpdateError>`:
    UserDir: unpack, write `omp-deck-host` then `omp-deck` as temp files in the same dir (0755), `rename` over;
    AppImage: chmod + `rename` over `$APPIMAGE`; WindowsDir: unzip, for each of `omp-deck-host.exe`, `conpty.dll`,
    `OpenConsole.exe`, `omp-deck.exe` rename the existing file to `<name>.old-<yyyyMMddHHmmss>` then write the new
    one (roll back renamed files on failure); Deb/Rpm: `pkexec dnf|apt-get install -y <file>` (or `rpm -U`/
    `dpkg -i` when dnf/apt are missing) with a 15 min timeout; pkexec missing or auth dismissed →
    `UpdateError::NeedsManual { command: String }`. `Applied { restart: (PathBuf, Vec<OsString>) }` = what to exec
    after exit (AppImage → `$APPIMAGE`; others → the installed `omp-deck` path) with the original args.
  - `pub fn sweep_leftovers()`: Windows: delete `*.old-*` next to the exe (best effort, ignores locked files).
  - `UpdateError` (thiserror): `Http`, `Checksum`, `NoAsset`, `Unsupported(String)`, `NeedsManual { command }`,
    `Io`, `Timeout`, `Cancelled`.

### omp-deck
- **Notifications** (`notify.rs` + platform files): a notifier thread (Linux: one zbus session connection,
  `Notify` with `actions ["default","Open"]`, hints `desktop-entry=omp-deck`, app icon `omp-deck`, replaces_id per
  tab; listens to `ActivationToken` + `ActionInvoked` + `NotificationClosed`; Windows: WinRT toast with AUMID
  `io.github.lavinhoque33.OmpDeck` + `SetCurrentProcessExplicitAppUserModelID` at startup, PowerShell AUMID
  fallback when the Start-menu shortcut lacks it). Triggers from `tabs::TabStatus` transitions of a tab that is not
  (active and window focused) — or only when the window is unfocused, per `notify.only_when_unfocused`: Busy→Idle
  (turn finished; only after ≥ 3 s busy), →Attention, →Exited (`[code]`), each gated by its toggle. Only omp's own
  `π …` title moves a live session's state (`Sighting::omp_title`): tmux restores the pre-omp title (`CSI 23 t`) one
  tick before a pane's exit is seen, so a busy session that ends is Busy→Exited ("Session ended"), never a finished
  turn (fixed after 0.22.1: it notified with "Session ended" off). Title = tab
  title; body = event + project. Activating the tab closes its notification. Click → activate that tab and raise
  the window (Wayland: xdg-activation with the received token; else/fallback `Focus` + `RequestUserAttention`;
  Windows `Minimized(false)` + `Focus`). No D-Bus/notification server → silently disabled (log once). Settings →
  OmpDeck → Notifications (toggles + "Send test notification"). `packaging/install.ps1` sets
  `System.AppUserModel.ID` on the `OmpDeck.lnk` shortcut (Wave 14 owner of that file: the notifications agent).
- **Command palette** (`palette.rs`): Ctrl+Shift+K = commands; Ctrl+Shift+E = quick switcher (open tabs, sessions,
  projects; newest first on empty query). One modal overlay (search box, ranked rows with highlighted matches,
  shortcut hints, disabled rows greyed with the reason as tooltip, ↑↓/Enter/Esc, mouse). In-house fuzzy scorer
  (case-folded subsequence, bonuses for consecutive/word-start/path-segment matches, length penalty) unit-tested.
  Commands cover every action in the title-bar app menu, ⋯ menu, status-bar actions, active tab / session / project
  context menus (target = active tab's session / project), settings pages, Usage, Agent hub, Session tree,
  worktree actions, Drive actions (open dialogs only), plus the new `App::toggle_changes_panel()`,
  `App::check_app_update_manually()`, `App::start_app_update()`, notifications toggles. Dispatch through the same
  functions as the menus; availability via `session_actions::availability` and the existing gates.
  `palette_recent` boosts recent commands.
- **Changes panel** (`changes_view.rs`, `app/changes.rs`): right panel `changes` (exclusive with the agent hub),
  toggled by Ctrl+Shift+G (verify unbound in omp v18.12.6; else report), the status-bar git chip ("Review
  changes"), palette. Shows the active tab's live cwd repo (`SessionStatus.cwd`, fallback `tab.cwd`): branch, root,
  sections Conflicted / Staged / Changes / Untracked with counts, per-file + section actions Stage/Unstage, Discard
  (confirm, danger, lists paths), Delete untracked (confirm), Copy diff (whole patch), open file's folder. Selecting
  a file shows its diff (virtualised rows, hunk headers, old/new line numbers, add/remove colours from the theme,
  binary/too-large/EOL-only placeholders). Refresh on open, tab switch, after actions, every 3 s while open (worker,
  cancellable). Not a repo → empty state. Width persisted in `changes_panel_width`.
- **Self-update** (`app_update.rs`, `app/app_update.rs`): own tracker (startup + every 6 h when
  `check_app_updates`, manual "Check for OmpDeck updates"), independent of omp being installed. Title-bar pill
  "OmpDeck X.Y.Z" (distinct from the omp pill) with popover: release page link, **Update and restart**
  (progress, cancel; download → verify → apply), deb/rpm "Install (asks for your password)"; `NeedsManual` →
  copyable command + release page; Dev/Unsupported → explanation, no button; "Skip this version". Restart =
  flush state (same path as `on_exit`), spawn the successor after the flush, exit; tabs re-attach. Startup calls
  `selfupdate::sweep_leftovers()` on a worker. Settings → OmpDeck: toggle + "Check now" + current version.

### Wave 14 as built (supersedes the sketch above where they differ)
- `selfupdate::UpdateError::{Checksum, NoAsset}` carry a `String`. `latest()` has its own 15 s deadline, no redirect
  following; downloads keep the stall-based client. `detect()` honours the verification override
  `OMP_DECK_UPDATE_KIND=deb|rpm|appimage:<file>|dir:<dir>|dev` (overridden deb/rpm restart the current exe);
  under `/usr`, only dpkg/rpm-owned exes are Deb/Rpm, others fall through to the writability probe.
  `download` fetches `<asset>.sha256` first and refuses a release without one; the caller owns the temp dir
  (kept while a NeedsManual command is shown). Startup sweeps `omp-deck-update-*` temp dirs older than a day.
- Update UI: checks wait for install detection and never run on Dev; a later failed check keeps a known release's
  Update button; a window close during download cancels and waits, during apply defers ("Finishing the OmpDeck
  update…") and then closes without relaunching; a successful Update and restart flushes `state.json` before
  spawning the successor (observed ~30 ms after the flush, the old GUI exits ~90 ms later).
- `changes`: binary diffs have empty text; untracked directories share one byte budget; `discard` skips untracked,
  unchanged and intent-to-add entries; `stage` on a conflicted file marks it resolved; unstaging a staged rename also
  unstages its source; `delete_untracked` accepts only `Would remove` lines for the requested path and refuses when
  git would keep something that still exists (nested repo). Panel: `ChangesUi::retarget` clears the listing on every
  tab/cwd change; destructive confirms re-check the repo at confirm time; file list rendered with `show_rows`.
- Notifications: zbus connection with a 5 s method timeout; the notifier keeps only the newest pending `Show` per tab.
  Palette: per-model (`Model: <provider> / <name>`, favourites first) and `Thinking: <level>` rows through
  `queue_change`; "Tab: Kill session process" disabled for session-backed tabs (Stop with confirm instead);
  worktree rows gate on the sidebar group or `GitInfo.worktree_of` and resolve the real root at run time.
  `install.ps1` stamps `System.AppUserModel.ID` on `OmpDeck.lnk`; the release workflow's installer round trip
  asserts it.

## Wave 15: split view, agents dashboard, review comments

User decisions (2026-10-24): panes show existing tabs (tab = session stays true); the dashboard covers open tabs
plus every running session without a tab; it is a full view over the terminal area that replaces the right-hand
Agent Hub panel (no side panel any more); review comments offer both **Insert into omp** and **Send**.
Recon facts with file:line evidence: `docs/notes/wave15.md`.

### deck-host (owned by the deck-core implementer this wave)
- `protocol::HostInfo` gains `title: Option<String>` (`#[serde(default)]`; old daemons → `None`), filled by the
  daemon from its headless snapshot's current OSC 0/2 title. No protocol version bump (additive field).

### deck-core
- `Mux::pane_titles(&self) -> HashMap<String, String>`: session name → current pane title for every live session
  of that backend in one pass (tmux: a single `list-panes -a -F '#{session_name}<TAB>#{pane_title}'` on the
  dedicated socket, active pane only; host: `info` per live session, `title` field). Never blocks longer than the
  existing alive query timeouts; errors → empty map.
- `pub mod fleet` (new; pattern of `status::StatusWatcher` / `agents::AgentsWatcher`: one background thread,
  generation counter, Condvar wake, publish-on-change `Arc`, `on_update` callback):
  - `FleetTarget { session: PathBuf, cwd: PathBuf, owner_alive: bool }`.
  - `FleetWatcher::spawn(on_update) -> FleetWatcher`; `set_targets(Vec<FleetTarget>)` (empty = idle, drops all
    readers); `latest() -> Option<Arc<FleetSnapshot>>`.
  - Per target it keeps one incremental `status::SessionStatsReader` and one `agents::AgentsScanner`
    (dropped when the target goes); per distinct cwd one `git_info` (dedup by cwd). Cadence: stats + agents every
    1 s while any agent of that session runs, else 3 s; git every 5 s. First pass of a huge file must not delay
    other sessions' updates by more than one cycle (process targets round-robin; publish after each target).
  - `FleetSnapshot { sessions: Vec<FleetSession> }` in target order; `FleetSession { session: PathBuf, model:
    Option<String>, thinking: Option<String>, context_tokens: Option<u64>, input_tokens: u64, output_tokens: u64,
    cost: f64, last_reply: Option<String> /* ≤ 400 chars, whitespace collapsed */, last_turn_failed: bool,
    agents: Vec<AgentSummary>, git: Option<GitInfo> }` (reuse the existing types; field names may follow them).
- No writes anywhere; real sessions root read-only as always.

### deck-term
- `TerminalView::id_salt(impl Hash) -> Self`: the widget's interaction id derives from the salt (stable per tab
  uid), so several views in one frame keep their own focus/drag state regardless of draw order.
- `Terminal::paste_bytes(&self, text: &str) -> Option<Vec<u8>>`: the exact bytes the widget's own paste would
  send for `text` under the current modes (bracketed `encode_paste` or win32-input-mode `encode_paste_win32`),
  `None` when the application has not enabled bracketed paste (?2004). Pure apart from reading modes.

### omp-deck — split view
- Layouts: Single, Two columns, Two rows, Three columns, Grid 2×2. `App.active` stays "the tab in the focused
  pane", so everything already bound to the active tab (status bar, Changes panel, ctl link, model picker,
  palette, session tree) follows pane focus. New state: `panes: Vec<Option<u64 /*tab uid*/>>` (one per slot),
  `focused_pane`, divider ratios. Invariant: a tab is shown in at most one pane; `panes[focused_pane]` is the
  active tab whenever the active tab is a terminal tab.
- Each visible pane renders its tab via `TerminalView::id_salt(uid)` in its own rect (with the same
  `surrender_focus_on` override), `set_visible(true)`; every non-drawn terminal tab is polled by the hidden loop.
  Focus reclaim / `focus_pending` target the focused pane. Click in a pane focuses it. Thin pane header when
  split: status glyph, title, close-pane (×, removes the pane, the tab stays open); focused pane gets the accent
  border. Draggable dividers.
- Tab strip: clicking a tab visible in a pane focuses that pane; otherwise the tab replaces the focused pane's tab.
  Tabs visible in a pane get a subtle marker. Tab/"Open tabs"/session context menus: "Open in split right",
  "Open in split down" (adds a pane; at 4 panes replaces the least recently focused non-focused pane).
  Palette: Split right/down, Grid, Unsplit (keep focused pane), Focus next/previous pane. Keyboard: focus pane
  left/right/up/down with Ctrl+Alt+Arrow — implementer verifies in omp's source (`~/PersProject/oh-my-pi`) that
  omp does not bind these and reports; Ctrl+PgUp/PgDn keep cycling tabs (into the focused pane).
- Closing/detaching a tab shown in a pane removes that pane (layout shrinks; 1 left → Single). Settings/update/
  install tabs never enter a pane; activating one shows it full width and the split comes back on the next
  terminal tab activation.
- Per-tab rules switch from "active" to "visible": bell suppression, notification `Sighting.active` and
  `take_seen` (a visible pane counts as looked at), the modal input hold (all visible panes).
- Persistence: `AppConfig.split: Option<SplitState { layout, panes: Vec<Option<usize> /*index into the persisted
  open_tabs*/>, focused: usize, ratios: Vec<f32> }>` and the same in `FolderTabs` (`serde(default)`); restored
  after tab restore; dangling indices dropped.

### omp-deck — agents dashboard (replaces the right-hand Agent Hub panel)
- Ctrl+Shift+A (no longer needs an active session tab), status-bar Agents chip (opens with the active session's
  card expanded), palette, ⋯ menu: toggle a full view over the terminal area (panes not drawn while it is open;
  Esc / Ctrl+Shift+A closes). Changes panel hidden while it is shown.
- Cards = open session tabs + sessions held by a live backend session without a tab (`RunningMonitor`), driven
  by `FleetWatcher` only while the dashboard is open. Card: title, project (`~`-abbreviated, worktree branch),
  status (open tab: `TabStatus`; no tab: `Activity::from_title` of `Mux::pane_titles`, else "Running"),
  model + thinking, context meter % (window from `ModelsCache`; live ctl context for the active tab), cost,
  ↑in ↓out tokens, changed files (staged/unstaged/untracked from `GitInfo`), last reply snippet (3 lines),
  subagents summary (running/total) expandable into the existing subagent card rows (tree/flat). Responsive grid;
  search (title, project, id prefix, subagent id/task) and status filter (All / Needs attention / Busy / Idle);
  order: attention, busy, then most recent activity.
- Click card → focus/open the session (existing `open_session`/`activate`, into the focused pane) and close the
  dashboard. Click a subagent → transcript popup (existing). Kill/Revive/Chat stay available when that session's
  tab is the active one with live control (activate it first if it is open); for a session without a tab the
  popup is read-only with "Open session for live control".
- The old right-panel hub UI is removed (dead code deleted); its subagent card renderer and view model are reused.

### omp-deck — review comments (Changes panel)
- Diff rows become clickable: hover shows a "+" in the gutter; click opens an inline multi-line comment editor
  under the row; Shift+click extends to a line range in the same file and side. Comments: `{ path (repo-relative),
  side (new for added/context, old for deleted), lines start..=end, quoted lines (≤ 12, "…" beyond), body }`, kept
  in memory per repo root (not persisted), survive refreshes, listed in a "Review (N)" section with edit/delete
  and inline markers on matching rows. A general comment (no line) is allowed.
- Prompt text: a short header, then numbered items `path:start[-end]` + quoted lines (`> `) + body, paths relative
  to the repository root (header says so). Cap 32 KiB → refuse with a clear message.
- `inject::Payload::Text { text, submit: bool }`: text keeps `\n`/`\t`, strips every other C0/C1 control (incl.
  ESC) and is written with `Terminal::paste_bytes` (refused when `None`). `submit=false` (**Insert into omp**):
  Requires::Any except exited, no draft confirm, no clear, no Esc/Enter — pasted at omp's cursor. `submit=true`
  (**Send**): Requires::Idle, existing draft confirm + Ctrl+C clear, paste, Esc, Enter; abort rules as Slash.
  Target = the panel's tab uid, re-resolved at click time. Success clears the review (toast); refusal keeps it.

### Wave 15 as built (supersedes the sketch above where they differ)
- deck-core: `SplitLayout { Single, Columns2, Rows2, Columns3, Grid4 }` + `SplitState` on `AppConfig.split` and
  `FolderTabs.split`. `Mux::pane_titles`: tmux `list-panes -a` with the title as the last field (tmux/alacritty
  strip tabs from titles); host queries daemons concurrently. `fleet`: `FleetSnapshot { sessions:
  Vec<Arc<FleetSession>> }` (an unchanged session is the same `Arc` across snapshots), `FleetSession.agents:
  Arc<Vec<AgentSummary>>`; git runs on its own thread (a slow git never delays session updates) with a cancel flag
  set by `set_targets`/`Drop`; snapshots are built only on a visible change; a session appears after its first read.
- deck-term: salted views take their id from `make_persistent_id(("deck_term::TerminalView", hash))`; salts must be
  unique within the parent scope. `paste_bytes` returns `Some(vec![])` for empty text; the widget's own paste paths
  share the same encoder selection.
- Split view: panes hold tab uids (no index shifting); a split without a spare tab leaves an empty pane that the next
  opened session fills; focus never moves to an empty or reserved pane (clicking one releases egui focus).
  Launches into a pane carry a reservation ticket (`PaneClaim`, keyed by session path / mux name) so they fill
  exactly their pane; a full grid replaces the least recently focused non-reserved pane. Closing a tab also drops
  empty panes. Ctrl+Alt+Arrow verified unbound in omp (`pi-natives/src/keys.rs` exact modifier match; omp binds only
  `alt+arrow` and `ctrl+alt+]`); desktops may still grab it.
- Dashboard: `dashboard.rs` + `dashboard_model.rs`, glue in `app/agents.rs`; `agent_hub.rs` is now only the
  embeddable subagent rows. Tab-less sessions map to files via the backend snapshot or their canonical `omp-<id>`
  name. Rebuilds are driven by an allocation-free signature (age bucket, scan generation, running/fleet `Arc`
  pointers, models generation, live context, per-tab facts). Bells are cleared only when a terminal is drawn.
  Entry points: Ctrl+Shift+A, status-bar Agents chip (expands the active card), palette, title-bar app menu.
- Review comments: `review.rs` (model + prompt builder), injector `Payload::Text` gets its bytes at write time via
  `Env::paste` (the injector stays pure; `None` → refused, `Poll::no_paste`); completions reported in
  `Poll::completed`; pending hand-offs tracked per tab uid. Editor: Ctrl+Enter saves, Esc cancels; while it takes
  input it cancels `focus_pending`. The Review section shows for every ready repository (also a clean tree); the
  file list is sized to its rows (≤ 45 %), the file/diff divider is fixed.

## Wave 16: theme gallery (16 dark styles, Blueprint default)

Design + decisions: `docs/notes/wave16.md`; exact values: `docs/notes/theme-mockups.html`.

### deck-core (`config.rs`)

```rust
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum UiStyle {
    Neutral, Neon,
    ZincContrast, LayeredDepth, TealSignal, SlateInk, MonoWire, GraphiteIslands,
    Aurora, ObsidianGlass, #[default] Blueprint, Ember, IndigoNight, Phosphor,
    NeoBrutal, RoseNoir, Spotlight, TwoToneFrame,
}
impl UiStyle {
    /// Gallery / palette order: Blueprint first, then Neutral, Neon, then mockups 1–16 minus Blueprint.
    pub const ALL: [UiStyle; 18];
    pub fn label(self) -> &'static str;   // "Blueprint", "Zinc Contrast", "Rosé Noir", …
    pub fn key(self) -> &'static str;     // "blueprint", "zinc_contrast", … (palette ids `style.<key>`)
    pub fn dark_only(self) -> bool;       // every style except Neutral and Neon
}
pub const STYLE_REV: u32 = 1;
pub struct AppConfig { …, pub style_rev: u32 /* default STYLE_REV */, … }
impl AppConfig {
    /// Effective dark mode: `theme == Dark || style.dark_only()`.
    pub fn dark(&self) -> bool;
}
```

Serde names are the variant names (`"Blueprint"`, `"RoseNoir"`, …). `parse_config` migration: when the JSON has
no `style_rev`, a `style` of `"Neutral"` or absent becomes `"Blueprint"` unless `theme` is `"Light"`; then
`style_rev` = `STYLE_REV`. Unknown style strings keep failing the parse as today (no silent fallback beyond the
existing behavior).

### deck-term (`theme.rs`, re-exported from the crate root)

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermColors { pub foreground: [u8; 3], pub background: [u8; 3], pub cursor: [u8; 3], pub selection_bg: [u8; 3] }
pub enum TermTheme { Dark, Light, NeutralDark, NeutralLight, Custom(TermColors) }
```

`Palette::for_theme(Custom(c))` = those four colors + Neutral dark's 16 ANSI colors. Everything that already takes
a `TermTheme` (spawn options, `set_theme`, host color push, OSC 10/11/4 answers) works unchanged with `Custom`.

### omp-deck

- `theme::apply(ctx, style, density, dark)` builds per-style palettes + a per-style structural look; `dark` is
  `AppConfig::dark()`. `theme::neutral()` is replaced by `theme::flat()` (true for every style but Neon).
- `term_theme(theme, style)`: Neutral/Neon unchanged; new styles → `TermTheme::Custom` (canvas bg, text, text_strong,
  mix(bg, accent, 0.35)).
- Settings → Appearance: Style gallery (18 tiles); Light disabled with "<Style> is dark-only" for dark-only styles;
  same gating in the title-bar menu and the command palette (`theme.light`). Palette: `style.<key>` per style
  (replaces `style.neutral` / `style.neon`).

### Wave 16 as built

- deck-core: as specified. Extra migration rule: a legacy file with `theme: "Light"` and no `style` gets
  `style: "Neutral"` written explicitly (otherwise serde's new default Blueprint, dark-only, would silently turn a
  light-mode user dark).
- deck-term: as specified; the NeutralDark ANSI table is one private `NEUTRAL_DARK_ANSI` shared by `NeutralDark`
  and `Custom`.
- omp-deck: `theme.rs` keeps the egui style build + shared helpers; new submodules `theme/palettes.rs` (one palette
  per style), `theme/look.rs` (`Look`: per-style structure knobs: regions, card frame, hover/active effects,
  canvas, tabs, selection, accordion kind/chevron/rail/segment bar, fonts), `theme/skin.rs` (window regions,
  canvas decorations with cached meshes, card/tab/row painters) and `theme/acc.rs` (accordion geometry +
  painting). Accessors: `theme::style()`, `look()`, `look_of(style)`, `palette(style, dark)`, `is_gallery()`
  (one of the 16 new styles), `flat()`, `label_font(size)` (mono for Blueprint labels), `ui_rad(base)`.
- Central panels go through `skin::central()` (all 5 call sites: terminal, split, dashboard, Settings, empty
  state) so region decorations paint everywhere; it turns egui's scroll fade off only inside gallery styles'
  filled central panel (Neutral/Neon unchanged, pixel-identical to v0.10.1). Cached meshes are keyed per style.
- Known differences from the mockups: Graphite Islands' title bar is flush (not inset into the backdrop frame);
  egui has one weight per font, so Neo-Brutal's heavy titles are approximated; glows are mesh gradients, not CSS
  blur.

## Wave 17: sharpshooter project memory in project sync

Facts: `docs/notes/sharpshooter-memory.md` (layout, bank id = Wyhash/base36, XDG rule, consolidation lock). A third
project-sync part next to `.omp` and Sessions; the config backup is unchanged. **User-approved store exception**:
restore writes the three memory files directly into omp's memories dir (no omp command can import memory).

### deck_core::sharpshooter (new module, no GUI deps)

```rust
pub const MEMORY_FILES: [&str; 3] = ["architecture.md", "product.md", "style.md"];
/// Zig std.hash.Wyhash, seed 0 (= Bun.hash). Pinned by vectors generated with bun.
pub fn wyhash(bytes: &[u8]) -> u64;
/// omp's `sharpshooterBankId(cwd)`: lexical path.resolve of `project`, sanitize/limit, `<name>-<base36 hash>`.
/// Hashes the platform path string (UTF-8; non-UTF-8 → lossy). `project` is absolute.
pub fn bank_id(project: &Path) -> String;
/// `<memories dir>/sharpshooter` for the agent dir that holds `root` (XDG state rule from the facts).
pub fn root_dir(root: &SessionsRoot) -> PathBuf;
pub fn bank_dir(root: &SessionsRoot, project: &Path) -> PathBuf;   // root_dir / bank_id
/// omp's consolidation lock for a bank (`<bank>/consolidate.lock`; Linux abstract socket, Windows Global mutex,
/// other Unix flock). Retries every 100 ms up to `wait`; `Ok(None)` = still held by omp. Released on drop.
pub fn lock_bank(bank: &Path, wait: Duration) -> io::Result<Option<BankLock>>;
pub struct BankLock { /* platform handle */ }
```

### deck_core::config
- `SyncRow` gains `#[serde(default)] pub memory: bool` (new rows: off, per Wave 11).

### deck_core::backup::project
- `Parts` gains `#[serde(default)] pub memory: bool`; `any()` includes it.
- `pub enum MemoryBank { Missing, Present { files: u64, bytes: u64 } }` (Missing = bank dir absent or all three
  files missing/empty/non-regular). `ProjectSizes` gains `pub memory: MemoryBank` and `pub memory_dir: PathBuf`
  (the bank path, for tooltips).
- Snapshot: when `parts.memory`, the non-empty regular files among `MEMORY_FILES` in `bank_dir(root, project)`
  (symlinks never followed: a symlinked bank dir or file → `skipped` entry `memory/<file>: <reason>`). Members
  `memory/<file>`; manifest `memory: Option<MemoryManifest { files: Vec<ManifestFile> }>` (`#[serde(default)]`,
  old backups = None). Fingerprint: the memory flag and memory files are hashed **only when `parts.memory`**, so
  fingerprints of rows without memory are unchanged. Selected parts with nothing present → `NothingToBackUp` as
  today. appProperties `mem` = memory bytes; `ProjectBackup.memory_bytes: Option<u64>`.
- Prepare: archive validation accepts the `memory/` prefix (only the three names); `parts.memory` without a memory
  part in the backup → `InvalidBackup`. Plan gains `memory_dir: PathBuf` (bank of `target`), `memory_exists: bool`
  (bank has any non-empty memory file), `memory_overwritten: Vec<String>`, `memory_added: Vec<String>` (file names),
  `memory_skipped: Vec<String>` (symlinked bank dir/file, `<name>: <reason>`).
- Apply (order: `.omp`, memory, sessions): `Stage::RestoringMemory` (items = files). Take `lock_bank(memory_dir,
  10 s)`; still held → memory not written, `report.memory_error = Some("omp is updating this folder's memory right
  now; try again in a moment")`, other parts continue. Else create the bank dir (parents too), safety-copy each
  overwritten file into the same project safety dir under `memory/<name>`, write atomically (temp in the bank dir
  + rename), never through symlinks (re-checked), never delete, never touch `state.json`/`queue/`. Report gains
  `memory_written: usize`, `memory_overwritten: usize`, `memory_skipped: Vec<String>`, `memory_error: Option<String>`.
- Backups containing `memory/` members are not restorable by older OmpDeck builds (their member check rejects the
  prefix); backups without the part stay readable by them.

### omp-deck
- Sync page row: third switch **Memory** + `N files · size` (tooltip: bank path); disabled with "No omp memory for
  this folder yet" when `MemoryBank::Missing`. Sidebar backup dialog: same third switch. Auto pass / "nothing
  selected" logic uses `Parts::any()`.
- Restore dialog: **Memory** checkbox when the backup has it (`memory_bytes.is_some()`). Confirm step: when
  `memory_exists && !memory_overwritten.is_empty()` a danger warning "This folder already has omp memory: N files
  will be overwritten (copies saved first)" listing the names; else "N memory files will be added". Skipped entries
  listed like `.omp`'s.
- Result toast/summary includes memory written/overwritten, skipped, and `memory_error`. Manage-cloud-backups and
  restore version rows show the memory part/size where parts are shown.

### Wave 17 as built

- `deck_core::platform::{NamedLock, try_named_lock}` hold the OS lock (Linux abstract socket, Windows `Global\`
  mutex, other Unix flock, which also creates the bank folder). Lock interop proven on Linux both ways against omp's
  installed `pi_natives` `FileLock.tryAcquire`.
- `resolve` on Windows strips `\\?\`/`\\?\UNC\` first, resolves drive-relative inputs with `std::path::absolute`,
  keeps drive-letter case; a UNC share root is `\\server\share\` with basename `share` (bun vectors under
  `cfg(windows)`). The XDG default-agent-dir comparison normalizes both sides.
- The memory restore waits for the lock in 100 ms zero-wait retries (10 s) and checks cancel between them; a cancel
  there yields `Ok(report { cancelled: true })`. A failure writing memory other than the lock is also routed to
  `memory_error` so `.omp`/sessions still finish. An empty existing file counts as "added" (no safety copy);
  identical non-empty content is listed as overwritten (mirrors `.omp`).
- Symlinks are checked from the bank dir down (bank, files); omp's own memories root may be a symlink (e.g. `~/.omp`
  on another disk) and is followed, as omp does.
- UI: `sync_view::part_cell` lays each switch/label/size cell out once (galleys reused) and wraps only between
  cells; the Projects row and the sidebar backup dialog share it.

## Wave 18: OmpDeck owns notifications (omp's silenced) + notification inbox

Facts (omp 18.12.6, `~/PersProject/oh-my-pi/packages`): all three omp notifications — turn complete
(`coding-agent/src/modes/controllers/event-controller.ts:2735`, `completion.notify`), ask "Waiting for input"
(`tools/ask.ts:589`, `ask.notify`), "Stopped with error" (`event-controller.ts:2710`, `error.notify`) — go through
`TERMINAL.sendNotification` (`tui/src/terminal-capabilities.ts:235`), whose first line returns when
`PI_NOTIFICATIONS` is `off|0|false`. Inside OmpDeck omp detects a generic terminal (`TERM_PROGRAM=tmux`, empty
`#{client_termtype}` → `trueColor`/`base`, `NotifyProtocol.Bell`), so today it writes BEL **and** on Linux runs
`notify-send --app-name omp` (`tui/src/desktop-notify.ts`). Suppression removes both; omp emits no other BEL.
The title run-state glyph (`!` = waiting on the user) is unaffected, so `TabStatus` inputs are unchanged.

### deck-core
- `mux::OMP_ENV` gains `("PI_NOTIFICATIONS", "off")` (always, independent of OmpDeck's own settings). It reaches
  tmux tabs via the generated `set-environment -g` (update `docs/notes/tmux.conf` text + `tests/tmux.rs`), host and
  direct tabs via `terminal_env()`. Sessions already running keep omp's notifications until restarted.
- `NotifyConfig`: field `enabled` renamed `desktop` (`#[serde(alias = "enabled")]`; old configs load unchanged);
  meaning narrows to "desktop pop-ups". Events `turn_finished`/`attention`/`exited` and `only_when_unfocused` keep
  names. Defaults unchanged (all `true`).
- New `status::last_turn_outcome(path: &Path) -> TurnOutcome { Completed, Error(Option<String>), Aborted, Unknown }`:
  bounded tail read (≤ 256 KiB from EOF, truncated last line tolerated) of the session jsonl; the last
  `"type":"message"` with `role:"assistant"` decides (`stopReason` `error` → `Error(errorMessage ≤ 200 chars)`,
  `aborted` → `Aborted`, anything else → `Completed`; none found → `Unknown`). Unit-tested with fixtures.

### omp-deck
- **Event routing** (`notify/tracker.rs` stays the single state machine; ≥ 3 s busy rule unchanged):
  - Per-event toggles gate both the inbox and desktop pop-ups.
  - Inbox: an event is recorded when the tab is not being viewed (`!(window focused && tab visible in a pane)`).
  - Desktop pop-up: additionally needs `desktop` on and, if `only_when_unfocused`, an unfocused window (today's rule).
  - Turn finished is classified off the UI thread via `last_turn_outcome(<tab's current session file>)` before it is
    shown: `Error` → title "Stopped with error" (+ message line), `Aborted` → dropped entirely (matches omp),
    `Completed`/`Unknown`/no session file → "Finished its turn". Classification must not delay the event more than
    ~1 s (omp appends the assistant message before the title flips idle; retry once after 300 ms if the tail's
    last assistant predates the busy start, then fall back to "Finished its turn").
- **Bell marker parity**: when a turn-finished (or error) event is recorded for a tab not being viewed, set that tab's
  `bell` flag (the same flag a terminal BEL sets; existing clearing on view stays). Real terminal BELs keep working.
- **Inbox** (`notify/inbox.rs` model + `notify_panel.rs` view): one entry per tab (newest event replaces the tab's
  entry and moves it to the top), fields: tab uid, event kind (finished / error / attention / exited{code}), title
  (tab label), project (cwd basename), body, time. Entry removed when the user views that tab (same moment the
  desktop notification is withdrawn), when the tab closes, by its own ✕, or by "Clear all". Not persisted.
  Click on an entry (or on its desktop pop-up) → activate the tab (focus its pane when it is in a split), raise the
  window, remove the entry.
- **Bell button**: lucide bell icon in the title bar's right cluster immediately left of the Settings gear (both the
  integrated title bar and the native-title-bar tab row), tooltip "Notifications". Badge with the entry count (1–9,
  then "9+"), hidden at 0; badge uses the style's warn color when any entry is attention/error, else accent; follows
  the active style (Wave 16) like the update badge. Click toggles a popover (≈ 360 px wide, max ≈ 60 % window height,
  scrolls): header "Notifications" + "Clear all", rows with kind icon/color, bold tab title, project · relative age,
  body, hover outline per the hover rule, ✕ on hover; empty state "You're all caught up". Esc / click outside closes.
  Palette command "Notifications: Open panel" (no new keyboard shortcut).
- **Settings card** (`notify_view.rs`): sections "Notify me about" (A turn finished — "…or stopped with an error",
  Needs attention, Session ended) and "Desktop pop-ups" (toggle; Only when OmpDeck is not focused; Test). Card note:
  "omp's own notifications are turned off inside OmpDeck; these settings decide everything." Palette notify commands
  renamed to match (`notify.toggle` → "Desktop pop-ups: Turn on/off").

### Acceptance
- Real omp in a tmux tab, a host tab and a direct tab: `cat /proc/<omp pid>/environ` shows `PI_NOTIFICATIONS=off`;
  a completed turn produces no BEL byte on the PTY and no `notify-send` child (proc watch), while OmpDeck still
  records the event, sets the bell dot on a background tab, and (unfocused) shows its pop-up.
- Inbox: two background tabs finishing → badge 2; viewing one → 1; Clear all → hidden; attention entry → warn badge.
- Old config with `"enabled": false` loads as `desktop: false` and saves as `desktop`.

### Wave 18 as built
- `status::last_turn(&Path) -> LastTurn { outcome, assistant_at }`; `assistant_at` is the reply's **finish** time
  (`message.completedAt`, else the entry's ISO `timestamp`, else `message.timestamp` — omp stamps the latter at
  request start, so it must not be compared with the busy start). Tail window keeps a line starting exactly at the
  cut; `agents::tail::from_epoch_ms` rejects non-finite/negative/after-year-9999 values.
- omp-deck: `notify::tracker::Ticket { uid, generation, session }` — a classifier result is applied only while the
  tab's transition generation and session path are unchanged (a later Attention/Exited/next turn wins).
  `classify::predates` allows 2 s slack against the busy start. `notify::inbox`, `notify_panel` (row galleys cached
  in egui temp data, viewport-only layout), `NotifyAction::Desktop`, `BarAction::Inbox`.
- Verified with real omp in tmux, host and direct tabs: `PI_NOTIFICATIONS=off` in omp's environ, 0 BEL outside OSC
  terminators, no `notify-send`/`gdbus` exec; without the variable omp emits both (control run). D-Bus `Notify`
  gating checked on a private session bus.
- **Withdrawal parity** (follow-up): every inbox removal withdraws the matching system notification — view, row
  click, tab close (already) **and** ✕ (`PanelAction::Dismiss`) and **Clear all** (withdraw every tab's notification).
  Windows: toasts are built with WinRT directly (`windows` 0.62, features `UI_Notifications`, `Data_Xml_Dom`,
  `Foundation`; `tauri-winrt-notification` removed): `ToastNotification` with `Tag = "tab-<uid>"` (test toast
  `"test"`) and `Group = "omp-deck"`, shown via `CreateToastNotifierWithId(app_id)` (same app-id rule as before), so a
  newer toast for a tab replaces the older one; withdraw = `ToastNotificationManager::History()
  .RemoveGroupedTagWithId(tag, "omp-deck", app_id)`, Clear all = `RemoveGroupWithId("omp-deck", app_id)`.
  Activated/Dismissed handlers keep today's signals (click → `Signal::Clicked`, dismiss → `Signal::Gone`).

## Wave 19: session manager hub (inventory, sizes, bulk delete / copy / move)

Facts (omp 18.12.6, `~/PersProject/oh-my-pi/packages`; real store measured 2026-10-24):
- `deleteSessionWithArtifacts` (`coding-agent/src/session/session-storage.ts:1057-1093`) removes the `.jsonl`, the
  same-stem sibling dir and `<stem>.jsonl.*.bak`. Blobs (`~/.omp/agent/blobs`, content-addressed, shared) are only
  reclaimed by `omp gc --blobs`. So "space freed by delete" = jsonl + sibling dir + `.bak` files.
- Usage/cost live inline on each assistant `message` entry (`message.usage {input,output,cacheRead,cacheWrite,
  totalTokens}`, `message.cost.total` — may be absent; `message.model`, `message.provider`); calls outside the
  transcript are `model_usage` entries with the same `usage`/`model` fields. Header (`type:"session"`) carries
  `id, timestamp, cwd, parentSession? (abs path of the parent jsonl), previousSessionFiles?`. Subagent transcripts
  are `<stem>/**/<AgentId>.jsonl` (same entry format, `parentSession` → the main file).
- `~/.omp/stats.db` has per-file rows but is only synced while `omp stats`/`/usage` run (stale rows for deleted
  files observed) → **not used**; OmpDeck parses the jsonl itself.
- Store here: ~114 top-level sessions, ~12.7 k files / 3.2 GiB under the sessions root; a metadata-only walk of
  everything takes ~0.4 s warm. Main jsonl total ~0.8 GB, subagent transcripts ~1.4 GB (largest single file 196 MiB).
- omp has no session-listing CLI; `omp session` = pack | unpack | moved.

### deck-core (new module `deck_core::inventory`; `sessions.rs` scanner unchanged for the sidebar)
- `pub fn scan(root: &SessionsRoot) -> Vec<InventoryEntry>`: every top-level `<root>/<dir>/*.jsonl` that has a
  session header — **including the prompt-less stubs the sidebar hides** — metadata + bounded header reads (reuse
  the sessions parser rules: 16 KiB header window, ≤ 2 MiB for the first prompt) plus a metadata-only recursive walk
  of the sibling dir (no symlink following, depth ≤ 16). Never lists subagent transcripts as sessions.
  ```rust
  pub struct InventoryEntry {
      pub id: String, pub path: PathBuf, pub cwd: PathBuf,          // header cwd
      pub title: Option<String>, pub first_prompt: Option<String>,
      pub created: Option<SystemTime>,                               // header timestamp
      pub modified: SystemTime,                                      // jsonl mtime
      pub jsonl_bytes: u64,
      pub artifact_bytes: u64, pub artifact_files: u32,              // sibling dir (recursive) + .bak files
      pub subagents: u32,                                            // *.jsonl transcripts in the sibling dir (no .bak)
      pub parent: Option<PathBuf>,                                   // header parentSession
      pub cwd_exists: bool,
  }
  impl InventoryEntry { pub fn total_bytes(&self) -> u64; pub fn is_stub(&self) -> bool /* no title, no prompt */; }
  ```
- `pub struct SessionStats { user_messages: u32, assistant_turns: u32, input_tokens: u64, output_tokens: u64,
  cache_read_tokens: u64, cache_write_tokens: u64, cost: f64, subagent_cost: f64, models: Vec<String> /* by use count
  desc */, compactions: u32, last_activity: Option<SystemTime> /* last entry timestamp */, last_outcome: TurnOutcome }`
  (all `pub`; `TurnOutcome` = `status::TurnOutcome`). Main-file numbers come from the main jsonl only; `subagent_cost`
  sums `cost.total` over the sibling dir's transcripts.
- `pub struct StatsCache` (`new()`, `get(&self, path) -> Option<&SessionStats>`, `compute(&mut self, entry:
  &InventoryEntry) -> io::Result<SessionStats>`): keyed by file (path, len, mtime) for the main file and each
  transcript; an appended file (same path, larger len, same prefix) is parsed **incrementally from the cached offset**;
  truncated last line tolerated; parse cost proportional to bytes (prefilter lines by type before full JSON decode;
  never deserialize message content). In-memory only (no disk cache). Blocking; callers run it on a worker.
- `Omp::delete_sessions(&self, files: &[PathBuf], on_result: impl FnMut(&Path, Result<(), DeleteError>))`: **one**
  headless omp process for the whole batch. `ompdeck-del.ts` gains batch mode: `OMPDECK_DEL_TARGETS=<file with one
  abs path per line>` (the single `OMPDECK_DEL_TARGET` mode is removed), one NDJSON result line per target
  `{"target":…,"ok":true,"id":…}` / `{"target":…,"ok":false,"error":"live"|"no session header"|msg}`, each target under
  its own lease. Targets answering `live` are retried in a fresh batch up to 5 rounds ~250 ms apart (the retry moves
  from the app into deck-core). Timeout scales with the batch (20 s + 2 s per target). Results are reported per target
  as they complete. `delete_session` is removed; the sidebar's single delete calls `delete_sessions` with one file.
- `Omp::copy_session(&self, src: &Path, dest_cwd: &Path) -> Result<PathBuf, OmpError>`: headless whole-session copy
  with the same semantics as the tab Copy (`omp --fork <src> --cwd <dest>`: fresh id, `parentSession` = src, artifacts
  copied, src untouched) via a headless omp (`--mode rpc` + stdin EOF, like `fork_at`); returns the new file. Fact-check
  the route with real omp and record it in `docs/notes/` facts of the result.
- `Omp::move_session` unchanged.

### omp-deck (`sessions_view.rs` rendering + `app/session_manager.rs` glue)
- **Hub**: large modal window like Usage (scrim, resizable `egui::Window`, palette frame, Esc closes when no popup is
  open). Opened by **Ctrl+Shift+S**, ⋯ app menu "Session manager…", palette "View: Session manager", a sidebar header
  icon button, and project context menu "Manage sessions…" (opens scoped to that project). Registered in the `ui()`
  focus block so the terminal does not take keys while it is open.
- **Left pane — projects**: "All projects" + one row per project cwd (display = `~`-abbreviated, worktrees by their
  branch label like the sidebar): session count + total size, sorted by size desc; "folder missing" marker when
  `!cwd_exists`. Selecting a row scopes the table.
- **Table** (hand-rolled rows; visible borders, hover pops the outline, theme palette): checkbox, title (fallback
  first prompt, else `(empty)` stub badge), project (when All), size (total; tooltip jsonl / artifacts / files),
  messages (user prompts · assistant turns), tokens (in/out), cost (main + subagents), model (most used), subagents,
  created, last activity (relative), status badges: Running, Open in tab, Busy, Error/Aborted last turn, Fork of …,
  Folder missing, Stub. Sort by clicking any column header (default: last activity desc). Stats columns fill in
  progressively from a background worker (spinner/“…” until computed); the table never blocks.
- **Filters**: text (title, prompt, cwd, id prefix; case-insensitive) + quick chips that combine: "Older than"
  (7/30/90 days, by last activity), "Larger than" (10/100 MB), "Empty stubs", "Folder missing", "Not running".
  Header shows totals for the scope: N sessions · size, and the selection: N selected · frees X.
- **Selection**: checkbox per row, Shift+click range, header checkbox = all visible, Esc-safe; selection drops paths
  that disappear after a refresh.
- **Detail pane** (row click): full title, id (copy), path (copy, show in file manager), cwd (open folder), created,
  last activity, size breakdown (jsonl / artifacts / files / subagents), tokens (in/out/cache r/w), cost split, models
  list, compactions, parent (click → select it) and forks of this session (sessions whose `parent` = this path), and
  buttons Open (tab; closes the hub), Copy to…, Move to…, Delete…, Stop process (running only).
- **Bulk actions** on the selection: **Delete…** (confirm: count, projects, total freed size, running count — running
  ones are stopped first, busy ones flagged as interrupted; then one `delete_sessions` batch on a worker with a
  per-row progress state; tabs for deleted sessions are closed without "remember", paths dropped from recently-closed
  / persisted tabs — the same cleanup the sidebar delete does), **Copy to…** / **Move to…** (destination = transfer
  targets list + "Choose folder…" `FolderDialog`; copy = sequential `copy_session` on one worker, no tabs opened; move
  = existing confirm text + stop-first + `retry_while_live(move_session)` per session sequentially, with tabs reopened
  on the new path like the single move), **Stop** (running ones; only those sessions' processes). Progress shows
  in the hub footer ("Deleting 3 / 12…"), results as toasts + per-row error text; the inventory refreshes after.
- The sidebar's single delete / copy / move keep their current UX but route through the same deck-core calls
  (`delete_sessions` with one file).
- Inventory scan runs on a worker when the hub opens and after every hub operation (plus a Refresh button); it is not
  polled while the hub is closed. StatsCache lives in the app for the process lifetime (re-open is instant for
  unchanged files).

### Wave 19 as built
- Facts verified with omp 18.12.6 (CoreW19): cost lives at `message.usage.cost.total` (stats read it first, fall back
  to `message.cost.total`). `parentSession` is **either** the parent's session id (`omp --fork` writes the id) **or**
  an absolute path (subagents, older forks); `inventory::scan` resolves an id to the parent's file when it is in the
  store (else `parent = None`) and keeps absolute paths as written. `model_usage` entries count into tokens/cost/models
  but not `assistant_turns`.
- `Omp::copy_session` = headless `omp --mode rpc --fork <abs src> --cwd <abs dest>` + `get_state` (`sessionFile`) +
  stdin EOF; fresh id, header cwd = dest, sibling dir copied, src untouched; the source path must be absolute.
  `fork_at` and `copy_session` share one private rpc helper (surfaces omp's stderr on an early exit / broken pipe).
- Extra API: `InventoryEntry::artifact_dir()`, `StatsCache::compute_many(entries, threads, cancel, on_done)` (slice
  order, `on_done` on the caller's thread), `StatsCache::prune(entries)`. `Omp::delete_session` removed.
- Real store (113 sessions, 2.6 GiB parsed, warm page cache): scan 35–340 ms; stats cold 4.3 s on 1 thread, 1.3–2.4 s
  on 4–8 threads; warm (cached) 25–90 ms.
- omp-deck: pure model `sessions_model.rs` (filters, chips, sort, Shift-range, pruning, badges), view
  `sessions_view.rs`, glue `app/session_manager.rs`; `transfer::{Subject, FolderDialog::many, check_dest_dir}`;
  move split into `prepare_move` + `MovePrep::run` shared by sidebar and hub; app-side delete retry removed.

### Wave 19b: running sessions — RAM + kill (user follow-up)
User ask: "in the session manager, give an option to kill running sessions (tmux processes and how much ram they are
taking)".

deck-core:
- `platform::tree_memory(roots: &[u32]) -> Vec<Option<TreeMemory>>`, `pub struct TreeMemory { bytes: u64, processes:
  u32 }` (all `pub`): memory of each root's whole process tree (root + descendants). Linux: `/proc/<pid>/smaps_rollup`
  `Pss:` (proportional, so shared pages are not double counted), falling back to `VmRSS` from `/proc/<pid>/status`
  when unreadable; one `/proc` pass for all roots (build the ppid map once). Windows: Toolhelp32 snapshot for the tree,
  `GetProcessMemoryInfo` → `PrivateUsage` (fallback `WorkingSetSize`). Vanished processes skipped; root gone → `None`.
  Unit-test the tree aggregation with an injected process table (pure function), plus one Linux test that spawns a
  child tree and sees `processes ≥ 2` and `bytes > 0`.
- `Mux::running(&self, root: &Path) -> Vec<RunningSession>` (blocking, ≤ ~200 ms typical; called on a worker):
  ```rust
  pub struct RunningSession {
      pub name: String,                 // mux session name (what Mux::kill takes)
      pub backend: BackendKind,         // Tmux | Host (existing enum if there is one, else new)
      pub files: Vec<PathBuf>,          // same mapping as session_files
      pub title: Option<String>,        // pane title (as pane_titles)
      pub cwd: Option<PathBuf>,         // pane current path / host cwd
      pub started: Option<SystemTime>,  // tmux session_created / host child_started_unix_ms
      pub pid: Option<u32>,             // pane pid / host child pid
      pub memory: Option<TreeMemory>,   // tmux: pane process tree; host: daemon tree (daemon + child + descendants)
  }
  ```
  plus `Tmux::server_memory(&self) -> Option<TreeMemory>` (the server process alone, `#{pid}`), exposed through
  `Mux::server_memory()` (`None` for Host). One tmux `list-panes -a` call for all panes; one `/proc` pass.
- Kill uses the existing `Mux::kill` (tmux detaches clients first — the tmux segfault rule stays).

omp-deck (hub):
- Hub gets a top-level switch **Sessions | Running** (Running label shows `N · <total RAM>`). The Running view lists
  every live session of every polled backend — including ones not mapped to a session file (e.g. a new session before
  its first message, an omp that `/resume`d elsewhere) — columns: checkbox, title (session title via file → inventory,
  else pane title, else mux name), project, backend, **RAM** (tree total, tooltip: processes count, pid), uptime,
  badges (Open in tab, Busy, Attention). Sort by RAM desc default; any column sortable. Footer: total RAM of the
  listed sessions + tmux server line ("tmux server: X") when tmux is in use.
- Polled on a worker every 2 s **only while the Running view is visible**.
- **Kill…** on the selection (and per-row button): confirm with count, RAM to be freed, and warnings for busy ones
  ("will be interrupted") and ones open in tabs (those tabs show `[process exited: …]`, same as the existing "Stop
  session process"). Kills **only the selected sessions** via the same path the existing stop action uses; other tabs'
  processes untouched. Results toast; list refreshes immediately.
- Sessions table: a **RAM** column (blank for not-running) and a "Running" filter chip; the detail pane shows RAM and
  a "Kill process" button for running sessions (the existing Stop wording becomes "Kill process" in the hub).

### Wave 19b as built + fix loop
- deck-core reuses `mux::MuxKind { Tmux, Host }` (no `BackendKind`); `platform::process_memory(pid)` for the tmux
  server; `Tmux::running` = one `list-panes -a`; tmux memory/files cover all panes of a session.
- Review fixes: `Omp::delete_sessions(&[DeleteTarget { path, id }], …)` — the extension leases the **expected id** and
  re-reads the header under the lease; mismatch/missing → `DeleteError::Changed` (never retried); a `.jsonl` without
  a readable header also reports `Changed`. Inventory and `sessions::Scanner`/`find_fork` skip symlinked project dirs
  and symlinked `.jsonl` files. rpc helper shutdown is bounded (5 s, then kill + reap) on every path. StatsCache
  append check = SipHash over the whole parsed prefix after the title line (+ first-line length).
- `copy_session`: when omp fails after writing the fork (e.g. "Could not restore model …" without credentials) the
  fork is found via `sessions::find_copy(root, parents, dest_cwd, since)` and returned as success; omp error text is
  trimmed to its `error:` line everywhere (full stderr logged).
- UI: selection/stats/errors survive a rescan only when path **and** id match; cell galleys cached (`paint_cache.rs`);
  hub releases terminal focus on open; Project column = folder name + elided parent; short badges (`Fork`,
  `No folder`, `In tab`) with full tooltips; Subs column dropped (count in the Msgs tooltip); detail buttons pinned;
  copy toast reports skipped sessions already in the destination. Sessions view also polls running/RAM every 5 s
  while a session runs (hub open only).
- Known, unchanged: an omp started outside OmpDeck that has not written yet holds no lease, so it is neither shown as
  running nor protected from delete (omp's own lease behaviour). Sizes use binary units labelled KB/MB (app-wide).
- Column widths: `col_alloc::allocate` (pure) sizes fixed columns to their widest cached cell, title gets the rest (min 160), drop order Created → Model → idle RAM → empty Status → …; Size and Cost never dropped. Model shown compact (`compact_model`, full name in tooltip).
- **Hidden-window fix** (post-0.14.0): notification observation no longer depends on painting. `App::logic` (eframe 0.37,
  runs on every tick incl. the logic-only ticks while the window is minimized or on another desktop) polls all terminal
  tabs (`poll_tabs`) and runs `drive_notifications`; `App::ui` does neither. Before: a turn that ended while the window was
  hidden was first seen on return with the window focused and the tab shown, so `tracker::wanted` dropped (and consumed) it.
  The focused-and-shown gate itself is unchanged (by design: no inbox entry or pop-up for a tab the user is looking at).

## Wave 20: bell while viewing + settings subpages

### Bell even when the tab is in view (setting, default on)
- deck-core `NotifyConfig` gains `while_viewing: bool` (default `true`; absent in old configs ⇒ `true`; serialized).
  Meaning: the in-app inbox (bell) also records events of a tab the user is looking at (window focused **and** tab
  shown in a pane). Per-event toggles still gate everything. The tab's bell dot is never set for a tab in view. A desktop
  pop-up for it follows the normal rule `desktop_wanted(cfg, focused)`: default (`only_when_unfocused` on) none; with
  "Only when OmpDeck is not focused" off it pops up too and is held until its inbox entry goes (look away and back, next
  turn, dismiss, Clear all).
- `notify::tracker::wanted(cfg, event, window_focused, visible) = toggle && (cfg.while_viewing || !(window_focused &&
  visible))`. `app/notifications.rs`: `record_notification` learns whether the tab is being looked at (`looking =
  focused && visible`): `looking` ⇒ inbox entry only (no `t.bell`, no `notifier.show`); not looking ⇒ as today. The
  classifier's "only if the user still is not looking" check becomes `!cfg.while_viewing && looking`.
- Inbox lifetime of an entry recorded while looking (`Entry.armed = false`; entries recorded while not looking are
  `armed = true`, today's behaviour): it stays while the user keeps looking; each frame `Inbox::sync_viewing(visible,
  focused)` sets `armed = true` for entries whose tab is not looked at, and removes entries that are looked at **and**
  armed (i.e. the user looked away and came back) and withdraws their desktop notification as today.
  `Inbox::turn_started(tab)` removes the tab's entry when it is not armed (the next turn began, the entry is stale);
  call it when a tab's status becomes Busy. Manual ✕ / click / Clear all / tab close work as before.
  Unit tests: record-while-looking survives `sync_viewing(looking)`, is removed after a look-away + look-back, and by
  `turn_started`; armed entries behave exactly as before.
- Settings → OmpDeck → Notifications: new section caption "Bell" with toggle **"Also when I'm looking at the tab"**,
  sub "Adds the bell entry even for the tab in view in a focused window. It stays until you look away and back, the
  next turn starts, or you dismiss it. Desktop pop-ups for it follow "Only when OmpDeck is not focused"." `NotifyAction::WhileViewing(bool)`.
  Palette command "Notifications: Also when viewing: Turn on/off" next to the existing notify commands.
- Bell popover (`notify_panel.rs`): a gear icon button (lucide settings icon already used by the title bar, tooltip
  "Notification settings") in the header, left of "Clear all" ⇒ `PanelAction::Settings`: closes the popover and opens the
  settings page on OmpDeck → Notifications.

### Settings: OmpDeck is a group of subpages
- The single long "OmpDeck" page (`settings_page.rs::ompdeck_section`) is split into subpages shown one at a time, each
  with its own title + one-line subtitle, scroll reset on page change (existing `view_key` mechanism):
  **General** (Sessions backend + Window), **Appearance** (font size, theme, style gallery, density),
  **Notifications**, **Updates** (OmpDeck self-update card + omp update card), **Backup** (Google Drive).
  Left nav: an "OmpDeck" group header with these five items (selected item highlighted like today's nav items), then the
  existing "Project sync" item, then omp's catalog tabs as today. Replace the `usize` selection encoding by an enum
  (`OmpDeck(OmpDeckPage)`, `Sync`, `Omp(i)`); `SettingsState::show_ompdeck()` keeps meaning "OmpDeck → General",
  new `show_ompdeck_page(OmpDeckPage)`. Deep links: bell gear → Notifications; title-bar OmpDeck/omp update badge and
  any "update" entry → Updates; Google Drive entry points (palette/menus/toasts) → Backup; palette gains one
  "Settings: <page>" command per subpage. Searching omp settings behaves as today.
- Nothing else about settings content changes (labels, actions, `SettingsAction`s stay).

### Acceptance
- Unit: tracker `wanted` table incl. `while_viewing`; inbox semantics above; deck-core: old config without the field
  loads `while_viewing == true` and round-trips.
- Smoke (fake-omp harness `~/.cache/nf`, X11 + openbox is enough): focused window + shown tab, turn ends ⇒ badge `1`,
  no desktop Notify; staying in the tab keeps it; switching to another tab and back (or defocus/refocus) removes it;
  next turn starting removes it; setting off ⇒ previous behaviour (no entry). Bell gear opens Settings → Notifications.
  Screenshots of every settings subpage; each shows only its own cards.

### Wave 20 as built
- Settings: selection is `Entry { OmpDeck(OmpDeckPage), Sync, Omp(i) }`; `OmpDeckPage` = General, Appearance, Notifications,
  Updates, Backup (label, subtitle, icon, palette slug). `SettingsState::show_ompdeck_page(page)` replaces `show_ompdeck()`;
  `App::open_settings_page(page)` opens the Settings window on a page. `view_key` = `Option<(Option<Entry>, String)>`
  (`None` while searching). Palette: `settings.ompdeck.<slug>` ("Settings: OmpDeck — <page>") and `notify.while_viewing`.
  No update/Drive entry point opened Settings before (badges are popovers, Drive commands act directly), so only the bell
  gear and the palette navigate to a subpage.
- Bell: `Entry.armed` (`false` via `Entry::while_looking()`), `Inbox::sync_viewing(visible, focused)` per frame (replaces
  `remove_viewed`; window defocus counts as looking away), `Inbox::turn_started(tab)` on the Idle→Busy transition
  (`Tracker::is_busy`), `record_notification(uid, kind, looking)`: looking ⇒ inbox only (no bell dot, no pop-up);
  `PanelAction::Settings` = gear in the popover header. Smoke (X11 + openbox + fake omp + private D-Bus): focused+shown turn
  end ⇒ badge 1 and no Notify; stays while looking; removed after switching tab and back or when the next turn starts;
  toggle off ⇒ old behaviour.

### Wave 20b: pop-up for the tab in view
- `record_notification(uid, kind, looking)`: `looking` ⇒ inbox entry (no bell dot) and a desktop pop-up exactly when
  `desktop_wanted(cfg, focused)` (i.e. only with "Only when OmpDeck is not focused" off). `Tracker.held` = pop-ups sent
  while looking; `take_seen` skips them, so looking does not withdraw them. Any inbox removal through
  `Inbox::sync_viewing` (returns the removed tabs) or `turn_started` also closes the tab's notification and calls
  `tracker.gone`, so entry and pop-up go together. Smoke (X11 + openbox, fake notification server): only_when_unfocused
  off ⇒ Notify, no Close while looking, `CLOSE` after switching tab and back and when the next turn starts; defaults ⇒ no
  Notify; unfocused window ⇒ Notify; Desktop pop-ups off ⇒ none. Not driven: dismissing a held pop-up from the bell panel
  (same `close` + `gone` path as before).

## Mobile companion step 1: shared application service

Implemented (2026-10-25): the first numbered slice of `docs/notes/mobile-companion.md`.
No gateway daemon, public listener, relay or PWA is enabled by this slice.

- New workspace crate `deck-service` depends on `deck-core`, `deck-host` and `deck-term`; it has no
  direct `egui`/`eframe` dependency. It owns shared execution policy, not native presentation.
  `deck-core` and `deck-term` remain independent of each other.
- Existing composer recognition, injection protocol and action construction live in this crate with
  one implementation; GUI copies were removed. Do not replace slash commands by individual direct
  ctl operations: runtime-command parity must be established for the complete set before a migration.
- A `SessionController` owns the terminal writer and serialized command execution. Stable process/session
  targeting plus a generation protects pending requests and draft confirmations against `/new`, `/fork`,
  `/resume`, move and restart. A request accepted previously must never be blindly resubmitted.
  Delivery of bytes is not proof of successful runtime execution.
- The desktop widget borrows the controller's terminal; it does not attach another writer. User input
  obeys the same input-hold boundary as injected jobs. Preserve idle/overlay checks, draft confirmation,
  mode-aware paste, control-key spacing and close-after-input-drain behavior.
- Host `Observe` is read-only: atomic snapshot followed by live output and authoritative geometry, no
  controller displacement or input/resize/palette/focus/kill rights. Each observer has byte-bounded
  backpressure and a slow/disconnected observer cannot stall the controller or PTY.
- `deck-term` provides headless constructors and a duration-bearing wake callback, independent of an
  egui frame loop. Read-only renderers follow host geometry, not their widget's layout. Observation
  cannot silently promote to `Attach`, even when talking to an older daemon.
- Shared settings have one transaction owner per service and a cross-process file lock. Clients submit
  field-level compare-and-set edits; disjoint changes merge, conflicting edits fail atomically.
  Window/view fields remain device-scoped. Existing state files retain their migration behavior.
  `TabLayout` groups open tabs, active index and split as one CAS unit. `Field::FolderTabs(path)`
  compares one folder's complete layout; disjoint folder edits merge, including additions/removals.
- Notification transition detection, turn classification and inbox policy have one canonical service
  owner. Native OS delivery/window activation stay in the desktop client. Preserve focused-view inbox
  notifications, per-event toggles, aborted-turn suppression and the desktop-unfocused popup policy.
  Every omp launch retains `PI_NOTIFICATIONS=off`.
- Session-store mutations still go through omp; no controller, observer or service writes session JSONL
  or artifacts directly. Real session/blob roots remain read-only during implementation and verification.

### Executable service API

- `controller::{SessionKey, SessionTarget, RequestKey, Submission, SessionController}`:
  `SessionKey::{backend,tmux,host,direct}` distinguishes backend kind and namespace (including when tmux
  and host both use `omp-deck`). A `SessionTarget` is `{ key, generation }`; pending actions, draft answers,
  notification tickets and activation targets are fenced by this identity.
- `SessionController::submit` accepts `Command::{Action,SwitchModel,SetEffort,Review,WorktreeMove,WorktreeFinish}`.
  The caller drives `tick` (poll + drive) or `drive` and consumes `ControllerEvent::{Confirm,Ended}`.
  `answer` applies only to the original request. `observe_model` supplies actual model/effort facts.
  Per-client accepted-sequence watermarks survive settled-history eviction: old requests are expired,
  not re-executed. A duplicate key cannot be reused for different work. Pending/confirmation/runtime
  verdict requests are retained.
- `SessionController::write_input(&SessionTarget, &[u8])` accepts already-encoded, ephemeral user input:
  stale/exited/read-only/detached/held input is refused before writing; held keystrokes are not queued.
  `Terminal::input_held` is public for trusted native widgets. Raw `Terminal::{write,send_key}` bypass
  that gate intentionally for injection; protocol clients must use the controller's checked route.
- `subscribe_view(WakeCallback)` returns a latest-only `ViewReceiver`; `take_changed` / `latest` yield
  `Arc<SessionSnapshot>` containing target, path, title, activity, exit and `ScreenText`. This is state/text,
  not a styled VT transport. It neither creates another mux attachment nor changes geometry. Adding a
  subscriber must not advance the publication baseline past updates owed to existing subscribers.
- `deck-term` adds `Terminal::{spawn_headless,connect_headless,observe,observe_headless}` and
  `WakeCallback = Arc<dyn Fn(Duration) + Send + Sync>`. Headless activity, including decoded graphics,
  wakes the owner without an eframe event loop. `Terminal::resize` serves the controlling headless owner;
  observed terminals ignore local geometry/input/focus/palette/clipboard effects.
- `deck-host::observe(ns,name,history_lines)` returns `(HostInfo, Observed)`.
  `Observed::split` gives `ObserverRx` / `ObserverTx`; events are `Snapshot { bytes, grid }`,
  `Output`, `Resized`, `Exited` and `Closed`. Protocol v1 gains `Observe` (0x0b) / `Resized` (0x28);
  normal `Attach` remains unchanged. Snapshot and registration are atomic under the PTY/core lock.
  Per-observer bounds include in-flight output and per-item cost (4 MiB plus the initial snapshot).
  Forbidden requests abort immediately, including blocked Windows pipe I/O, without draining a reply
  behind an unread snapshot. Detach unblocks readers and never reports a fabricated child exit.
- Generic tmux/direct state subscriptions are supported; raw tmux/direct VT mirroring is not.
  A later mobile terminal transport must resolve that fidelity boundary before full terminal parity.
  Existing native terminal rendering and command encoding stay unchanged.
- `settings::{SettingsService,Edit,PendingEdit,SettingsError,Field}`: open, snapshot, submit, reload,
  flush and change subscriptions. `Edit::between(base,edited)` is a field CAS, not a replacement save.
  The transaction worker locks a sibling file, strictly re-reads once using `deck_core::config::parse_config`,
  merges disjoint edits and commits, or returns `Conflict { fields,current }` with no partial edit.
  Reload/decode errors retain the last-good snapshot; corrupt files are never replaced by defaults.
  A capacity-one notifier channel isolates arbitrary change callbacks from the transaction worker.
  The desktop periodically reloads off-thread, rebases pending local changes, reconciles style/backend
  and backup-schedule effects, and surfaces conflicts. Unchanged native view projections cannot
  overwrite another window's committed layout; explicit folder-map edits are retained. Shutdown
  retries conflict-rebased survivors within a changed-setting-derived bound, but does not retry
  I/O failures, then flushes before exit.
- `notifications::NotificationCenter` owns transition detection, asynchronous turn classification,
  generation-bound tickets, inbox and `Effect::{Show,Withdraw,WithdrawAll,Bell}`. `Observation` supplies
  native visibility/focus and actual status facts; only the native adapter performs OS delivery/raising.
  Unknown/old turn outcomes retry up to four times at 300 ms before fallback; aborted turns remain
  suppressed. A late status/path/generation result cannot notify a different session.
- `backend::Backends`, `host_remote::{HostRemote,HostObserver}`, `session_actions::export_html` and
  `lifecycle::{stop_then_move,stop_then_delete,retry_while_live,kill_all,surviving_session}` are executable
  shared helpers. Existing core ctl/tree/git/worktree/Drive/update executors remain the implementation
  for their action families, not empty service-command wrappers. The complete route matrix is in
  `mobile-companion.md`; native dialogs/clipboard/folder/window effects remain client-local.
- Delete and move stop every live session that runs a target (gateway jobs `delete_sessions`/`move_session`):
  the caller's `stop` (sessions holding the file open, tabs) plus, via `jobs::with_ctl_holders`, each live
  session with a control endpoint and no held session file whose ctl snapshot reports a target as its
  `session_file` (≤ 2 s, on the worker). Reason: omp drops its jsonl writer handle on every atomic rewrite (the
  title after the first turn) until its next append, so an idle session is often invisible to the open-file map;
  it was left running and omp's session lease refused the delete ("it is open in another omp process").

### Delivery, close and later IPC

`Delivery::Delivered` acknowledges local PTY writes or successful `RemoteTx::input`, not omp execution
or the host daemon's child PTY. Model/effort requests retain a separate runtime verdict.
`Writing` has no ordinary acknowledgement timeout while those bytes could still be delivered.

An explicit native close preserves the old injector `can_close` boundary (no half-written/queued
command, then the 150 ms tail). If the transport still has not acknowledged, `Delivery::Unobserved`
settles that request before detach: bytes may yet arrive. This is not success or a retry invitation.
Keep review comments; do not resend worktree prompts; preserve an already observed `Runtime::Confirmed`.
The desktop drains and dispatches controller events before destroying a tab. Failed output rejects
new input and does not wait for the ordinary two-second drain of unreachable bytes.

The Rust service API is not an IPC serializer, paired-device permission model or persistent request
ledger. Later gateway/relay slices must bind process-local generations to a service-instance epoch,
preserve refusal/outcome distinctions and recover accepted work without blind replay. They must also
implement long-job and explicit native-effect transport; no mobile capability is implied by merely
having a shared executor.

## Mobile companion step 2: local gateway and OS lifecycle

### Ownership and process boundary

`deck-service` contains the `omp-deck-gateway` binary and `gateway::{protocol,client,server,jobs}`.
There is one gateway owner per logged-in user and application namespace. The native application
is an asynchronous local client: it does not create another `SessionController`, settings writer,
notification classifier, session launch owner or independent execution worker.

The gateway owns backend detection, terminal/controller lifetime, ctl links, bounded session scans,
running-process/RAM state, status/model observations, usage/update schedules, notifications/inbox,
Drive/config-backup and project-sync schedules, and accepted work. Expensive view-only watchers
(inventory statistics, agents dashboard, trees and diffs) are interest-driven, not always-on scans.
Native workers may wait for RPC results; their execution takes place in the gateway.

All existing action families in `mobile-companion.md` retain their real core/service executors:
session launch/restart/kill/fork/copy/move/delete/bulk; injected commands/model/effort/review;
tree/subagent control; git changes; worktrees; usage; omp config; Drive/project sync/memory;
omp/plugin install/update/channel; OmpDeck update. The wire carries typed operations and actual
results, not Rust closures or a second command implementation. Navigation, tabs/splits, rendering,
dialogs, clipboard/download presentation, opening URLs/folders and window activation remain
client effects. A request never changes another client's navigation or implicitly raises its GUI.

### Private IPC, identities and outcomes

The gateway reuses `deck_host::transport::{Endpoint,Listener,Stream,connect,prepare}` rather than
duplicating the socket/pipe security implementation. It uses a gateway-specific derived namespace,
separate from the session-host namespace, and one fixed endpoint. Unix directories remain real,
owner-only 0700 directories, with the listener holding the exclusive instance lock. Windows pipes
retain current-user + SYSTEM DACLs, first-instance exclusion and remote-client rejection.
`deck_host::launch_detached` reuses the host's restricted Windows inherited-handle list.
Unix daemon readiness uses the existing single-threaded detach/readiness primitive.

`gateway::protocol` defines a versioned handshake, bounded control envelopes and binary terminal
frames. Terminal bytes are not JSON integer arrays. Large snapshots/results are chunked or paged.
Slow clients cannot create an unbounded queue or block another session. Unsupported protocol,
stale epoch, closed transport, rejected input, execution refusal and uncertain outcome remain
distinct. Disconnect does not fabricate a process exit.

`controller::ServiceEpoch` is a random, process-instance identity. `SessionTarget` contains the epoch
as well as its existing key and generation. The handshake exposes that same epoch, and non-session
mutations also bind it. Reconnecting to a new instance cannot accidentally validate an old target.
`RequestKey { client,seq }` remains the logical mutation identity; registration binds client identity
to the connection. Keys are never reused for different work.

Accepted mutation records are journaled privately before execution. Outcome lookup does not
execute the operation. Duplicate requests recover the accepted record; evicted accepted sequences
remain expired. After an owner crash, unfinished records are uncertain, never replayed automatically.
Outcome lookup and `CancelJob` are limited to the caller's own client id (mobile R4): another client's key answers
`Refused` (`FOREIGN_KEY`), so no client reads another's results (e.g. a pairing invite) or cancels its jobs. Client ids
starting `d-` are reserved for paired devices: a local hello claiming one is refused (mobile R5). Paired
devices are not local clients: they speak `deck_remote::api` (`docs/notes/mobile-relay.md` "R4 contract").
Raw typing/paste is ephemeral: while disconnected or held, it is refused/dropped, never queued for
a later connection. Reconnection restores subscriptions and desired view state only.

`gateway::client::{GatewayClient,GatewayOptions}` supplies off-thread connect/start, a latest-state
cache, asynchronous requests/events and per-session frontend connections. Native submission UI
distinguishes local pending, server acceptance, confirmation, delivery and runtime observation.
It must not manufacture an accepted `RequestId` before the server acknowledgement.

### Full-fidelity native terminal transport

The existing `deck_host::snapshot::encode<T>` remains the VT grid serializer. `deck-term` adds
`OutputSubscription` and `Terminal::subscribe_output`: parsing, side-state advancement, output
publication, and snapshot-plus-registration share one ordered critical section. A raw read tap
followed by a later wake callback is not an atomic publication boundary. The local PTY engine must
provide that boundary without wrapping tmux/direct sessions in another host/emulator.

`deck_term::{Frontend,FrontendTx,InputMeta,FrontendState}` is the native-client link, separate from
the raw backend `Remote` link. `Frontend::split` returns a `RemoteRx` and `FrontendTx`.
`FrontendTx` routes input with metadata, clipboard replies, focus, resize, palette, selected-process
kill and detach through the gateway. `Terminal::frontend` creates the native renderer for that link.
`InputMeta` carries semantic Ctrl+C and win32 modifier state; kitty/win32 Ctrl+C must update the
owner's quiet-time guard even when its encoding contains no raw 0x03.
`SessionController::write_input(target,bytes,meta)` checks the full target and authoritative hold
before accepting user input. Protocol replies are not user input and cannot bypass the hold as
arbitrary bytes: forwarded replies are restricted to the outstanding clipboard query.

The owner alone answers DA/DSR/color/keyboard queries. A frontend does not duplicate those replies.
Clipboard reads/stores are routed to one designated native frontend, not every observer.
`FrontendState` communicates tmux key rules, input hold, win32 modifiers and clipboard ownership.
Focus and geometry are explicit frontend state; only the designated controlling view changes
geometry. Observing never acquires input rights, displaces an owner or resizes the child.

Fresh frontend attachment replays styled primary/alternate grids, scrollback, cursor/modes,
hyperlinks and the owner's retained kitty image data/placements. Pending image decode completion
must also reach a frontend that joined after the upload. Image storage and output queues stay
bounded. Local selection/scrollback and all existing key/mouse/paste encoders remain native.
Closing a frontend only detaches that view; it does not close the gateway's injector or abandon an
accepted write. A disconnected renderer is stale/read-only until a fresh, identity-checked attach.

### Modes, supervision and startup context

`deck_core::config::GatewayMode` has `Off` (default), `WhileOpen`, and `Background`.
`AppConfig.gateway_mode` is a settings CAS field. Enabling without an explicit lifetime selects
Background. Off disables mobile access/autostart but still permits the native client's private
local gateway; no network listener, relay, pairing or PWA is introduced in this step.

Off/WhileOpen stop after the last GUI and accepted work have drained. Multiple GUI connections
count independently. Losing a view is not a reason to interrupt a written command/destructive job.
Unwritten confirmations belonging to a cleanly closed client may be cancelled, never auto-approved.
An active direct-PTY program cannot be killed merely to complete a lifetime change: it remains a
draining job until it exits or the user explicitly stops it. Persistent tmux/host processes survive
gateway detachment.

`deck_core::platform::gateway` owns `Startup`, `Registration`, `AutostartError` and register/start/
inspect/remove operations. Linux uses a user systemd unit; Windows uses an interactive-user logon
scheduled task, least privilege, no before-login execution, no battery/network execution cutoff,
a logon trigger plus a five-minute repeating time trigger (the Task Scheduler does not rerun a task
that exits non-zero or is killed, measured on Windows 11; `IgnoreNew` makes the repeat a no-op while
a supervisor runs). Registration is idempotent and does not enable access during installation.
Unavailability/failure is surfaced, not reported as successful background operation.

The OS registration starts `omp-deck-gateway supervise`. The supervisor monitors the singleton
owner and starts a replacement after an owner crash; it can supervise an already running,
GUI-launched owner without replacing it or interrupting accepted work. The OS also restarts a
crashed supervisor (systemd `Restart=`; on Windows the repeating trigger, within five minutes).
Supervisors are not GUI clients and hold no session controller/input rights.
They exit when Background is disabled and honor update maintenance barriers.

Linux uses `KillMode=process`: stopping the supervisor must not kill host/omp descendants that
share its cgroup. Windows registration removal never uses task-tree termination. Mode changes
register/start/remove supervision through the gateway and report desired/effective state;
failed registration cannot leave a falsely enabled mode or duplicate autostart entries.

Startup context preserves absolute executable/root/profile paths and non-secret omp configuration
environment in a private file. Shell-only credential values are not copied to a plaintext autostart
file; use omp's normal credential store for unattended fresh-login operation. An initially launched
owner still inherits the native launch environment. Background means a logged-in, awake user
session, not before login, after logout or while the machine is asleep/offline.

### Notifications, packaging and update

The gateway runs the one canonical notification tracker/inbox while the GUI is closed. Native OS
delivery/activation is an adapter, never another classifier. GUI visibility/focus is reported as
client state; focused-tab inbox behavior and desktop-unfocused popup policy remain independent.
Activation is fenced by the full target. With no GUI, an explicit notification activation may
launch the native client with a private, generation-bound activation ticket. `PI_NOTIFICATIONS=off`
remains in every OmpDeck-launched omp environment.

Every tarball/AppImage/deb/rpm/Windows zip ships the gateway alongside GUI and host. Installers
preserve the opt-in setting, repair an existing enabled registration for the new location, and do
not opt a new installation into background operation. AppImage registration uses the stable
AppImage path and `AppRun --gateway`, not its ephemeral mount directory.

Updates and uninstall use a private maintenance barrier: reject new mutations, drain accepted work,
flush settings, preserve persistent sessions, and prevent GUI/supervisor auto-start during binary
replacement. Native clients flush/acknowledge before an update restart. Include the gateway in
atomic/rename-aside replacement, rollback and stale-file cleanup lists. Resume supervision and
clients only after a consistent installed bundle is available. Uninstall removes only OmpDeck's
own registration and stops the gateway after draining; it never kills unrelated session processes.

### Mobile companion step 2: as built

Observed on Linux (isolated HOME/XDG/PI roots and `omp-deck-test-*` namespaces; real omp, no provider request).
Windows: the first CI run hung in `cargo test` on the pipe `Listener` (accept parked forever after `remove`; fixed in
`e33ea0a`, with a bounded test). Since then `windows-latest` runs the workspace tests, host release tests, the WinRT
toast withdraw, real-omp live control over the named pipe, the ConPTY host smoke and a GUI screenshot step that starts
`omp-deck-gateway.exe` and types into a restored omp tab through the frontend link. `gateway_e2e` skips in that job
(omp is installed after `cargo test`); commit `14ae775` made it runnable on Windows (host backend) and measured the
task's repeating-trigger need on Windows 11. Not covered by CI: `install.ps1` drain/resume, enabling Always on from Settings on Windows, and
toast delivery from the gateway.

- **Identity.** `GatewayId = gw-<24 hex sha256(host namespace)>`. Settings/sessions paths are compared in the handshake
  (`ProfileMismatch`), never served silently. Concurrent `serve` calls yield one owner; losers exit 0 after signalling readiness.
- **CLI** (`omp-deck-gateway`, all accept `--ns --settings --sessions-root --startup`): `serve [--foreground]`, `supervise`,
  `status [--json]`, `request '<json Request>' [--timeout-secs N]` (stamps `key.client` with its connection's client id),
  `watch`, `drain --purpose update|uninstall [--timeout-secs N]`, `resume`, `uninstall`, `--version`.
  Exit codes: 0 ok, 3 not running, 4 drain timeout, 5 maintenance barrier, 6 direct program still running.
- **Mode/supervision.** Config default Off; enabling from Settings picks Background. Background is *effective* only after
  the supervisor's own hello. Off/WhileOpen owner exits ~10 s (`LINGER`) after the last GUI with no jobs or direct programs.
  A mode change goes only through `Mutation::SetGatewayMode`; a settings edit touching `gateway_mode` is refused.
  Registration failure leaves the mode unchanged and the card shows the reason (e.g. no systemd user manager).
- **Protocol additions beyond step 1.** `Query::RunningDetail`, `Mutation::KillNames{names,wait}` → `Done::Killed`,
  `ControllerInfo.live_cwd`, `UpdatePhase::NeedsManual{command}`, `GatewayError::Cancelled`; large payloads are boxed
  (`Outcome::Done(Box<Done>)`, `Answer::Topic(Box<..>)`); wire JSON is unchanged.
- **Bounds.** Control-link event queue: 1024 events and 8 MiB, then one `Resync`; view side items: 256. Replies are bounded
  only by outstanding requests (not byte-capped; known limit).
- **Maintenance barrier** is a file at `<app_root>/gateway/maintenance`: `drain` raises it, `resume` clears it and repairs
  registrations for the new install location. Installers (`install.sh`, `install.ps1`, self-update) drain before replacing
  binaries and resume after.
- **Supervisor self-retirement.** deb/rpm removal cannot reach per-user registrations and ships no maintainer scripts. A
  supervisor whose program has been missing for ≥ 30 s drains the owner (no barrier, so a reinstall works), removes its own
  registration and exits 0; a replaced file exits 75 so the OS restarts it on the new binary. A removed package therefore
  leaves a registration for at most ~30 s of an *already running* supervisor; a registration that never ran again is
  removed the first time the OS starts it.
- **AppImage.** Registration stores `$APPIMAGE --gateway …`; `AppRun --gateway` execs `usr/bin/omp-deck-gateway` (dispatch
  checked; mount lifetime under a real AppImage run is not exercised here).
- **Install without `omp-deck-gateway` (0.15.1).** Self-updaters of 0.14.6 and older replace only the files they know
  (`omp-deck`, `omp-deck-host`; Windows also ConPTY), so an install.sh/tarball/Windows-dir install they move to 0.15.x has
  no gateway executable and the 0.15.0 GUI showed nothing. The gateway CLI lives in `deck_service::gateway::cli`; the
  `omp-deck-gateway` bin and `omp-deck --gateway <command>` (dispatched first in `main`, before logging or any thread)
  both run it. `gateway_program()` resolves `$OMP_DECK_GATEWAY_EXE` → AppImage `--gateway` → the gateway itself → its
  sibling → the `omp-deck` app (itself or sibling) with `--gateway`; startup contexts, systemd units and Windows tasks
  then record that program. The next self-update (0.15.x updater) installs the missing executable. Verified on Windows:
  a dir with only `omp-deck.exe` + host served, registered `omp-deck.exe --gateway supervise`, uninstalled cleanly.
- **`BackupFile.size` on the wire (0.15.2).** Drive reports `size` as a decimal string; serde serializes the `u64` as a
  number. Before the gateway nothing re-decoded our own output, so `size_string` accepted strings only and every gateway
  reply carrying a `BackupFile` (Drive list/backup, project list/backup) failed with "undecodable gateway reply". It now
  accepts both; a gateway JSON round trip is unit-tested.
- **Outdated owner replacement (0.15.3).** An update replaces the executables under a running owner, and nothing restarted
  it: the in-app updater runs inside the owner, deb/rpm have no maintainer scripts, and `PROTOCOL` is unchanged across
  patch releases. So a 0.15.2 GUI kept talking to a 0.15.1 owner. Now an autostarting GUI client whose `GATEWAY_VERSION`
  `selfupdate::is_newer` than `Welcome.version` sends `MaintenanceOp::Poll` (no barrier) first on the link: `Drained` →
  `MaintenanceOp::Exit` (barrier in memory only, no marker file, so the client's autostart or the supervisor launches the
  new owner); `Draining` → asked again every 15 s while that epoch's link lasts. Once per client: an owner that comes back
  still older (another installation) is kept. Only newer clients replace, so an older GUI never downgrades the owner and
  two installs never fight. A gateway with no desktop GUI stays old until one connects. Verified: an isolated real 0.15.2
  owner was replaced by the dev 0.15.3 build within ~140 ms (one `Down` blip), no marker left.
- **Replacement before the link is published (0.16.1).** 0.15.3 published `Link::Up` to the outdated owner first, so after
  "Update and restart" the new GUI restored its tabs (`Mutation::Launch`) and loaded omp's settings/models against the
  owner it was about to retire: launches after the `Exit` failed with `Maintenance`, accepted ones and the one-shot
  `Query::OmpSettings` died with it (`Disconnected` → settings "Could not load"), and the reduced tab list was then
  persisted. Now `greet` replaces it on the fresh stream before `Attempt::Up`: synchronous `Poll` (events skipped) every
  1 s for up to 10 s; `Drained` → `Exit`, `Attempt::Replaced`, link stays `Connecting` (its barrier/shutdown refusals for
  10 s too); still busy → the link comes up on it for this client's lifetime (no mid-run retirement, which would end
  every tab's view) and the GUI shows a one-time toast to restart when idle. The GUI also re-runs the models + settings
  catalog loads on every `Link::Up`. Tests `a_drained_older_owner_is_replaced_before_the_link_comes_up`,
  `a_busy_older_owner_is_kept_and_never_told_to_exit`. Verified: real 0.15.3 owner → dev client: one `Up 0.16.0` at
  128 ms, no `Up` to 0.15.3, old process gone.
- **GUI client.** `omp-deck` owns no controller/settings/notification/worker state: `gw.rs` (`Gw` facade, jobs as typed
  requests), `activate.rs` (`--activate <ticket>`), frontend terminals via `deck_term::Frontend`. Tab close sends
  `Mutation::Close` (view detach); kill is explicit. Drive login cancel is typed `GatewayError::Cancelled` in the protocol;
  the GUI compares `GatewayError::Cancelled.to_string()` because `Gw` carries `String` errors.
- **Known GUI gaps.** Changes "Refresh" re-reads the cached topic (no force refresh endpoint); one Drive lane (busy → refusal
  toast); the post-failure move check reads the running snapshot, which can lag.
- **Evidence.** `cargo test -p deck-service --test gateway_e2e` (real omp: singleton, Rename delivered, owner `kill -9` →
  re-adoption, kill); manual user-systemd Background run (owner and supervisor SIGKILL, drain/resume/uninstall); Xvfb GUI
  smoke: autostart, terminal render + typing through the frontend link, tab close/reopen on the same backend PID, restart of
  GUI and gateway re-adopting the live session, two GUIs on one gateway, Mobile access card Off→Always on (systemd
  unit active, supervisor connected)→While open (unit removed), GUI closed with Background (owner stays; `Query::Screen`
  readable) and WhileOpen (owner exits, tmux session survives); `install.sh` fake bundle install/reinstall/upgrade-with-owner
  (drained)/uninstall. Not exercised: desktop notification delivery and `--activate` in a GUI run (unit-tested only), host
  backend through the gateway, draft-confirm origin routing and reconnect-without-replay (unit-tested).

## Desktop notification sounds (Linux)

Cause of "pop-ups but no sound" (verified against Plasma 6.7 with `gdbus` + `pactl subscribe`): the notifier sent only
`desktop-entry`; Plasma plays a sound for a freedesktop notification only when it carries `sound-name`/`sound-file`, and
it plays none for a notification that *replaces* a still-listed one (updated in place). Fix: `NotifyConfig.sound`
(default `true`, Settings → Notifications "Play a sound", palette "Desktop pop-ups: … sound", Linux only; Windows toasts
stay `<audio silent>`). `notifications::Sound { Finished, Attention, Error, Ended, Test }` is chosen from the inbox
`Kind` when the effect is produced (`Effect::Show.sound`, `desktop::Request.sound`, `None` when off) and mapped to the
sound-theme names `complete`, `dialog-warning`, `dialog-error`, `message-new-instant`. `desktop/linux.rs`: `hints()`
adds `sound-name` (or `suppress-sound: true` when off); a sounded notification first withdraws the session's previous
pop-up (`close` forgets the id before asking, so the resulting `NotificationClosed` is ignored) so the new one is not a
silent replacement. Smoke: the real `Notifier` showing Finished, Finished again, Attention, Error, Ended opened an audio
sink-input each; the silent variants none.

## Metric badges and the provider quota chip

`badge.rs` (`Badge`, `Hue`) paints every numeric fact that used to be one run of grey text as its own tinted
badge (fill 16 % + 1 px outline of the hue, radius from `theme::ui_rad`). Hues are fixed per meaning and
independent of the style (most styles have one accent, so neighbouring accent-coloured badges would blur):
cost green, time amber, requests blue, tools orange, tokens teal, model violet, thinking rose; token types in the
cost tooltip: input blue, output teal, cache read violet, cache write amber. Users: subagent cards
(`agent_hub::card`: model badge top right, `agent_model::metrics` → one badge per `Metric`, dropped from the end
when narrow), session cards (`dashboard`: model, thinking, cost, `↑`/`↓`), the agent popup header, the session
tree (`before $x` badge per row and in the row tooltip, amber `≥` when `RecordedCost.incomplete`) and the status
bar. The cost tooltip is `cost_tip::CostTip` (formatted once in `BarView::build`, painted by `cost_tip::show`):
total badge, one section per source (assistant replies / side-model calls) with a per-token-type table and amber
notes for gaps; the data rules of the earlier text version are unchanged.

Status-bar usage chip (not hovered) shows the **selected model's provider**, not the tightest quota overall:
`Summary::card_of(provider)` (provider id = the selector's `provider/` prefix) → the shown page's
`CardPage.pills` (the account the session uses, see the usage chip above), the fullest
window of ~5 h length (`5h`, blue tag) and of ~a week (`wk`, violet tag), lengths from `CardWindow.window_ms`
(`durationMs`, else the id: `5h`/`7d`/`1w`). A warning/exhausted window turns its pill amber/red. A provider with
neither window shows its two shortest measured windows (`1d`, `mo`, …, teal); a provider with no measurable window
(or no model selected) falls back to the old tightest-overall chip, prefixed with that provider's name. The hover
card lists all providers.

## Settings-backup manager, dialog padding, UI font

User request (four items): status-bar contents sat low; modals/hover popups had no inner padding; the Drive
**settings** (config) backup needs a manager like project sync's "Manage cloud backups"; new default UI font
chosen by the user from 10 rendered mockups.

- **Status bar**: `statusbar::chip` used `ui.horizontal`, which reserves `interact_size.y` (24 px Compact) and
  let the 29 px chip grow downward (~2.5 px low). It now allocates `CHIP_H` with `Layout::*(Align::Center)`.
  Pinned by `centering_tests::bar_segments_are_vertically_centered` (every painted text within 0.5 px of the bar
  centre in Blueprint/Compact, Neutral/Compact, Neon/Comfortable).
- **Padding**: `confirm::show_dialog*` used `theme::rad(20, 22)` (a radius helper, 3 px in Blueprint) as the inner
  margin. Now `theme::dialog_margin()` = 18 px flat Compact, 20 px flat Comfortable, 22 px Neon. Rule: never derive
  spacing from `rad`/`ui_rad`/`popup_radius`. `spacing.menu_margin` (all `Frame::popup`: tooltips, hover cards)
  = 10×8 Compact / 12×10 Comfortable; `theme::menu_style` keeps 6/8 for context menus, dropdowns, notification and
  scope popovers.
- **Settings-backup manager**:
  - `deck_core::backup::Service::delete_backup(&self, &BackupFile) -> Result<()>`: permanent `files.delete`;
    re-reads the file's metadata and refuses anything that is not a config backup (`remote::is_config_backup`,
    the same rule as `list()`: `omp-backup-` name, no `kind` property) or whose name changed; 404 = deleted.
    Deleting the recorded `last_backup` clears it and `synced_fingerprint`; deleting a file with the synced
    fingerprint clears that; the next automatic pass then uploads again.
  - Gateway: `Mutation::DriveDeleteBackup(BackupFile)`, journal key `drive-delete-backup`, Drive lane, `Done::Unit`,
    not allowed in maintenance, never replayed when uncertain (like `ProjectDeleteBackup`);
    `jobs::drive_delete_backup`.
  - UI (`drive_view::{BackupManager, manager_dialog}`, `app/drive.rs`): "Manage backups…" on the Backup card and
    palette `drive.manage` "Drive: Manage settings backups…" (disabled while signed out / a Drive job runs);
    host → versions newest first (this PC first, date, age, size, "this PC"/"latest"), row + group checkboxes,
    totals, confirm, sequential delete with progress and per-file errors, Cancel stops after the file in flight,
    reload afterwards; `RestorePick::Delete` trash per row in the restore picker.
- **UI font**: bundled `assets/fonts/JetBrainsMono-VariableFont.ttf` (OFL, `JetBrainsMono-OFL.txt` shipped in every
  package) is the primary face of `ui_fonts::NEUTRAL_FAMILY` (all flat styles); Geist and its license are removed.
  Neon keeps Adwaita Sans; `mono_labels`/`mono_ui` keep egui's Monospace family.

## Signal-edge chrome, Drafted buttons, UI font picker, app font size

User request; directions picked from six rendered mockups (`~/omp-deck-style-mockups/{A..F}.png`, source `mock.html`):
tabs **E "Signal edge"**, buttons **B "Drafted"**, modals + hover cards **E "Gradient rule"**. Applies to all 18
styles through the palette; per-style character kept where it fits.

- **Primitives** (`deco.rs`): `accent_pair(p)` = `accent_a → accent_b`, or for single-accent styles the accent and a
  softer partner (hue +45°, saturation ×0.6, value ≥ 0.95: Blueprint cyan → lavender); `tone_pair` for status tones;
  `rounded_polygon`/`rounded_rect`/`slanted` outlines; gradient outlines via epaint `ColorMode::UV`; gradient rules,
  glows, drop shadows (`*_shape` variants for the placeholder-shape pattern).
- **Tabs**: row `tab_hit_h()` = `dens(42, 46)`, pill inset 5 (32 / 36 px tabs). One painter for every style
  (`skin::paint_tab`): parallelogram slanted by tan 12°, radius `tab_r.clamp(2, 6)`; plain = outline only; hovered =
  `hover_bg` + `mix(stroke, hover_line, .35)` (also while over ✕); active = dark plate, 1 px left→right accent-pair
  outline, two-layer glow (α .18 blur 22 + α .22 blur 8; half on light) over a drop shadow (dy 6, blur 14); split
  "shown" tabs = 45 % outline. Brutal: 2 px + hard (3,3) accent shadow; Glass, Tint, Filled, Outlined, Folder keep
  their fills (`tab_joins_canvas` removed). Pinned by `titlebar::tests::tab_row_content_is_vertically_centered`.
- **Drafted button** (`widgets::DraftButton`, `DraftKind::{Secondary, Primary, Danger}`): uppercase tracked label,
  1 px border + outer rule 2 px outside + hard 3 px base shadow; Primary/Danger = tinted gradient fill, inner
  highlight, deep base, glow; pressed sinks 2 px; Enter/Space activate when focused. Used for every text button in
  modals, popovers and the large windows; icon-only buttons, list rows and inline row pills stay as they were.
- **Cards** (`card.rs`): `chrome` (gradient border 135° accent_a → stroke → stroke → partner, glow + drop shadow),
  `Header { icon, title, subtitle, tone }` with gradient icon tile + tone wash + gradient rule, `kv` ("▸ KEY" column +
  coloured value/badges), `section`, `kbd`, `footer_hints`, `footer`; entry points `show`, `modal`, `hover` (built on
  `egui::Tooltip::for_enabled` with a frameless popup). `confirm::Dialog` gains `icon`, `subtitle`, `tone` (all
  `Default`); every dialog sets them. `tips.rs` holds shared hover pieces (status badge/tone, `~` paths, id badges
  showing the last 8 chars). Plain one-line tooltips keep egui's frame with accent-tinted stroke, card radius and an
  8/24 shadow.
- **UI font**: `deck_core::config::UiFont` (`AppConfig.ui_font`, kebab keys, default `Auto`; settings row `UiFont`,
  Shared): Auto (today's per-style choice), System, and 18 bundled faces (JetBrains Mono variable + 17 static
  Regular instances in `assets/fonts/ui/`: Geist, Inter, IBM Plex Sans, Manrope, Space Grotesk, Plus Jakarta Sans,
  Onest, Outfit, Sora, Bricolage Grotesque, Lexend, Figtree, Urbanist, Geist Mono, Red Hat Mono, Martian Mono,
  Victor Mono). Licenses in one `assets/fonts/UI-FONTS-OFL.txt` shipped by every package. `ui_fonts::finalize` builds
  one family per face once (face + full fallback chain); switching only selects a family (`ctx.set_fonts` at runtime
  garbled cached galleys), and cached-galley keys include the font. Explicit fonts apply to every style; Blueprint and
  Phosphor labels stay monospace.
- **App font size**: `AppConfig.ui_scale` (0.8–1.6, default 1.0, clamped at load and by the settings owner; row
  `UiScale`, Shared) → `ctx.set_zoom_factor`. Terminals get `font_size / zoom` so their on-screen size depends only on
  "Terminal font size". Window geometry is persisted in native logical points (zoom-independent).
  Settings → Appearance "App font size" + "Terminal font size" use `widgets::stepper` (− / whole-number field / unit
  / +, no slider): buttons step to the previous/next multiple of 5 % / 1 pt, the field takes any whole number
  (Enter or focus loss; Escape reverts; clamped); a value is reported only after `STEPPER_SETTLE` (0.4 s) without
  further change (typed values at once), so a click burst zooms once. Palette Larger/Smaller/Reset (10 % steps).
- **Sidebar**: session rows are one step below their project header (flat 12 vs 13 px, Neon 13 vs 14 px).
- **Title strip** (`titlebar::STRIP_H`) is 40 px (was 32); all its content is centred, so the extra is equal top and
  bottom padding.
- **Subagent rows** (agents hub / dashboard accordion) show the thinking level (Rose brain badge, omp's short names)
  right of the model chip: `agent_model::agent_thinking` = latest `thinking_level_change`, else the model selector's
  `:level` suffix; `off` is hidden; dropped before the model chip when the row is narrow.
- **Sidebar sections** (`sidebar.rs` `section_header`, `sidebar/running.rs`): "Open tabs" (collapsed by default) and
  **"Running sessions"** (expanded by default; egui memory is not persisted, so defaults apply every launch) are
  pinned above the scope chip, headers in the UI font (`theme::font`, never `label_font`). Running sessions = every
  name in `RunningSnapshot.alive` (hidden when empty or `blind`): status (tab status, else `activity_of`), title via
  the session file's entry, else the mux name, folder, tab marker. Order (`running::order`, tested): tab-strip order,
  then newest session file, then unmapped by name; per-folder scope applies. Click: activate the tab, else
  `Open(entry)`, else `SidebarAction::AttachRunning { name, title }` (attaches the existing mux session, never starts
  omp). Menus: `tab_menu` / `session_menu`; unmapped rows Open + Stop (`StopRunning` → the usual confirm).
- **Close buttons**: every modal/window header ✕ is `card::close_button` (26 / 14) or `card::close_button_sized`
  (30 / 18 for Sessions, Usage, Session tree) — hover fill + outline. Dialog bodies must not widen past the dialog
  width (unwrapped labels in `horizontal` rows → `.truncate()`), else the header ✕ ends short of the corner.

## Project restore: session times and id conflicts

User request (2026-10-26): restored sessions keep their relative age, and a backed-up session whose id already
exists locally is **overwritten or kept** by the user's choice instead of always becoming a duplicate. Overrides the
earlier "always alongside, never replace" rule for that case only; the default stays non-destructive.

- **Session time** (deck-core): after every unpack that reports `MoveKind::Created`, the new `.jsonl`'s mtime is set
  to `UNIX_EPOCH + ItemManifest.modified_unix` s (the jsonl mtime at backup time, already in every backup) via
  `File::set_modified` (metadata only, no bytes; `modified_unix <= 0` → left alone; failure → `log::warn`, not a
  session failure). Both OmpDeck's sidebar and omp's own picker order/age sessions by file mtime
  (`session-listing.ts` `modified: mtime`, newest-mtime-first). User-approved exception to "OmpDeck never writes
  `~/.omp/**`". A rolled-back local copy (below) gets its own previous mtime back the same way.
- `deck_core::backup` (re-exported like the other project types):
  ```rust
  #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
  pub enum ConflictChoice { #[default] KeepLocal, Overwrite, KeepBoth }
  #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
  pub struct LocalCopy {
      pub path: PathBuf,             // the local `<ts>_<id>.jsonl`
      pub cwd: Option<PathBuf>,      // its header cwd (bounded header read)
      pub modified_unix: i64,        // its mtime, s
      pub bytes: u64,                // jsonl + `<stem>/` artifacts
  }
  ```
  `SessionPlan.new_id` is **removed**; `SessionPlan.local: Vec<LocalCopy>` = every `*_<id>.jsonl` in a project dir of
  the sessions root whose header id is the session id (empty = no conflict). `ProjectRestoreReport` gains
  `sessions_replaced: usize` (of `sessions_added`, how many replaced local copies) and `sessions_kept: Vec<String>`
  (title or id of conflicting sessions left as they were, nothing written).
  `Service::apply_project_restore(prepared, conflicts: &BTreeMap<String, ConflictChoice>, omp, cancel, progress)`;
  key = session id, a conflicting id missing from the map = `KeepLocal`.
- **Apply per session** (local copies re-discovered at apply time; none left → plain unpack with the original id):
  `KeepLocal` → nothing written, `sessions_kept`. `KeepBoth` → today's fresh-id copy. `Overwrite` → (1) every local
  copy is saved with `Omp::pack_session(<copy>, <safety>/sessions/<id>[-k].tar)` (new; `omp session pack`, stdin null,
  terminal env removed, bounded like `unpack_session`; retried ≤ ~3 s while omp reports a live writer), the safety
  dir being the restore's `restore-safety/project-<stamp>/` (`report.safety_dir`); a failed pack → session failed,
  nothing deleted; (2) `Omp::delete_sessions` (headless omp, lease-guarded) removes every copy; (3) the backup is
  unpacked with the **original id**, must be `Created`. Any failure after a copy was deleted → each deleted copy is
  unpacked back from its safety tar with `--cwd` = its header cwd when that folder exists, else the restore target,
  and its mtime restored; the failure text says the local copy was put back (or names the safety tar if that failed).
  A live omp holding a copy makes the delete fail (`Live`) → session failed, local copy untouched.
- `deck-service`: `Mutation::ProjectApplyRestore { handle: u64, conflicts: BTreeMap<String, ConflictChoice>, excluded:
  BTreeSet<String>, stop: Vec<String> }`; the job calls `PreparedProjectRestore::leave_out_sessions(&excluded)`
  (drops those ids from `plan.sessions` and the manifest's items together, they stay index-aligned; unknown ids
  ignored), runs `lifecycle::kill_all(&ctx.backends, &stop, "restore")` and then the apply.
- `omp-deck` confirm step: a **Sessions** row ("N sessions" / "K of N sessions" / "none selected") with
  **Choose…**, which opens the shared session chooser as `SessionsTarget::Restore` ("Sessions to restore", rows
  from the plan's `SessionPlan`s with the backed-up age; Save sets `PlanView.excluded`). Left-out sessions are
  dropped from the conflicts, the problems list, the danger check and the apply's `conflicts`/`stop`; Restore is
  disabled when nothing would be written. A **"Sessions already here"** section when any restored plan session has
  `local`: an "All:" choice
  (Keep current / Overwrite / Keep both) and one row per conflict (title, backup age vs local age with a Newer
  badge on the newer side, the local copy's folder `~`-abbreviated when it differs from the target, Running badge)
  with its own three-way choice; default Keep current. Any Overwrite → danger box "N local session(s) will be
  replaced by the backup" + "Copies are saved to OmpDeck's restore-safety folder first; running ones are stopped
  and their tabs closed." On Confirm, `stop` = `session_kill_names(entry, true)` for each overwritten local copy.
  The result summary reports added / replaced / kept counts.

## Worktree sessions belong to their parent folder

User request (2026-10-26): a "New session in worktree…" session should live in the parent (primary repo) folder
while the work happens in the worktree; after the work is merged no worktree folder or dangling session/group
may remain. Facts (omp 18.13.1, sandbox-verified; `docs/notes/worktrees.md` "Relocation"): omp binds the tool cwd
to the session header `cwd`, so the session must run with cwd = worktree while working (no omp mode stores it as
folder A while tools run in B; `--session-dir` only moves the file, header cwd stays the worktree). omp's own
`/move <abs dir>` (live TUI, or `{"type":"prompt","message":"/move <dir>"}` over `--mode rpc --resume <path>`)
moves the jsonl + artifacts into `<dir>`'s bucket, rewrites header `cwd`, chdirs the process; no model call, no
message appended. `omp --resume <path>` never relocates a removed-worktree session by itself (only id-prefix
resumes of omp-named worktrees do), so OmpDeck relocates explicitly. Replaces the Wave 13 "worktree groups nest
under their primary repo" presentation and its "(removed)" groups.

- **deck-core** `Omp::relocate_session(&self, file: &Path, dest: &Path) -> Result<PathBuf, OmpError>`: headless
  `omp --mode rpc --resume <abs file>` with process cwd `dest` (stdin piped, terminal env removed like the other
  headless runs, `--no-extensions`), sends `/move <abs dest>` as a prompt, waits for the success response, returns
  the session's new path (`get_state`'s session file, else `<dest bucket>/<same file name>` verified to exist).
  `dest` must be an existing absolute directory without control characters. Header cwd already `dest` → `Ok(file)`
  without running omp. Live-writer refusal retried ≤ ~3 s; 60 s timeout, process tree killed. Blocking.
- **deck-service**
  - `controller::Command::MoveTo { dest: PathBuf }` (`CommandKind::MoveTo`) → `Job::slash("Move to folder",
    "/move <dest>", Requires::Idle)`; rejected while busy, and when `dest` is relative or has control characters.
  - `Mutation::WorktreeRemove` gains `moved: Vec<SessionTarget>` (running sessions the client moved out with
    `MoveTo`; never stopped, skipped by the "another session still runs in this worktree" check, which otherwise
    uses each managed session's live cwd, else its launch cwd). The job kills `stop`, re-checks clean + merged
    (refused → nothing moved), relocates every session whose header cwd is inside the worktree
    (`deck_core::sessions::sessions_under`, fresh scan, stubs included) into `worktree.primary`; the first
    failure fails the job ("could not move a session of this worktree into <primary>: <why>; the worktree was
    kept"; sessions already moved stay in the primary), then `git worktree remove`.
  - `Mutation::RelocateSessions { files: Vec<PathBuf>, dest: PathBuf }` → `Done::Relocated(Vec<(PathBuf,
    Result<PathBuf, String>)>)` (old path → new path or error), one job, sequential.
- **omp-deck**
  - **Sidebar**: a worktree's sessions are ordinary rows of the primary repo's group (newest-first among the
    repo's own), each with a branch chip (git-branch icon + branch / folder name; "Finishing…" state); no nested
    worktree groups. A repo with only worktree sessions still gets its (synthetic) group. Worktree actions (Finish
    worktree, Cancel finishing, Remove worktree…, Open worktree folder, Copy worktree path) are in the session
    row / tab / "Open tabs" context menus of sessions in that worktree; the row hover shows Branch + Worktree path.
    Per-folder scope, filter (matches branch), active-tab expansion and "Move to new worktree…" gating keep working.
  - **Finish / Remove**: once removal is allowed (clean + merged, unchanged checks), every live omp tab whose live
    cwd is in the worktree gets `Command::MoveTo { dest: primary }` when idle; OmpDeck waits (≤ 30 s) until its
    live cwd / session file left the worktree (`track_relocations` then updates `tab.cwd`); those tabs keep running
    in the parent, are excluded from `stop` and sent as `moved`. Tabs that did not move (e.g. busy for 30 s) are
    stopped as before. Remove confirmation and toasts say the sessions move into the parent folder.
  - **Leftovers**: sessions whose header cwd is a removed worktree (`cfg.removed_worktrees`) and that are not
    running are relocated into the recorded primary with `RelocateSessions` once per run; the record is dropped when
    no session of it remains. Opening such a session first relocates it, then opens the new path in the primary.

### "Merged" covers squash and rebase merges (2026-10-26)

User: some repos squash-merge, some merge-commit; Finish/Remove must treat both equally.
- `deck_core::worktree::status(wt, base)`: `merged` = the worktree's work is contained in `base` **or** in base's
  upstream (`<base>@{upstream}`, e.g. `origin/main`, refreshed first by a best-effort `git fetch --quiet <remote>
  <branch>`: `GIT_TERMINAL_PROMPT=0`, ssh `BatchMode=yes`, ≤ 20 s, failure ignored). Contained = `HEAD` is an
  ancestor (regular merge / fast-forward), else `git merge-tree --write-tree <target> HEAD` exits 0 with a tree
  equal to `<target>^{tree}` (merging again changes nothing: squash / rebase / cherry-pick merges; later commits on
  the target are fine). Conflicts, errors or git < 2.38 (no `--write-tree`) → not contained (safe side). `ahead` =
  commits not in the target as before. `remove`'s final re-check before `git worktree remove` uses the same rule;
  branch deletion stays the CAS `update-ref -d <ref> <verified tip>` (works for squash-merged branches).

### Park a worktree before its PR is merged (2026-10-26)

User decisions: "Remove worktree…" on a worktree that is clean and **pushed** but not merged **parks** it: sessions
move into the parent, the folder is removed, the local branch is kept and OmpDeck **never deletes a parked
branch** (the user deletes local branches by hand). Both "Continue in worktree" and "Continue in main checkout"
exist. Unpushed or dirty work is still refused. Merged worktrees keep the Finish/Remove behavior (merged branch
deleted).

- **deck-core** (`worktree.rs`, `config.rs`)
  - `WorktreeStatus` gains `pushed: bool` (HEAD is an ancestor of the branch's upstream `<branch>@{upstream}`,
    else of `<remote>/<branch>` for the only/`origin` remote, after the same best-effort fetch as the merged rule;
    no upstream → false) and `upstream: Option<String>` (e.g. `origin/wt/x`).
  - `pub enum Removal { Merged { delete_branch: bool }, Park }`; `remove(wt, base, how: Removal)`: `Merged` as
    today; `Park` requires clean + `pushed` (new `Unsafe::NotPushed`; detached → `Unsafe::Detached`) and never
    touches the branch. `park_blockers(st) -> Vec<String>` beside `blockers`.
  - `add_existing(omp, repo, branch, dest) -> Result<(), WorktreeError>`: `omp worktree add -q -C <repo> <dest>
    <branch>` with the same clean-checkout overlay as `create` (branch must exist; git refuses when it is
    checked out elsewhere — surface its message).
  - `switch_branch(dir, branch) -> Result<(), WorktreeError>`: refuses a dirty checkout (`Unsafe::Dirty`, nothing
    run), then `git -C <dir> switch --no-guess -q <branch>` (never `--force`/`--discard-changes`).
  - `branch_state(primary, branch, base) -> BranchState { exists: bool, checked_out: Option<PathBuf>, merged: bool }`
    (`checked_out` from `git worktree list --porcelain`; `merged` = the squash-aware rule for the branch tip vs base /
    base upstream; `false` when it does not exist).
  - `config::ParkedBranch { primary: PathBuf, branch: String, base: String, sessions: Vec<String> /* ids */,
    parked_ms: u64 }`; `AppConfig.parked_branches: Vec<ParkedBranch>` (serde default, key = primary + branch).
- **deck-service**
  - `settings::Field::ParkedBranches(parked_branches) = Device`.
  - `Mutation::WorktreeRemove { worktree, base, how: Removal, stop, moved }` (replaces `delete_branch`); the job's
    pre-relocation check uses `blockers` for `Merged` and `park_blockers` for `Park`.
  - `Mutation::WorktreeAddExisting { repo, branch, dest }` → `Done::Path(dest)`; `Mutation::SwitchBranch { dir,
    branch }` → `Done::Unit`; `Query::BranchState { primary, branch, base }` → `Answer::BranchState(BranchState)`.
  - `Done::Removed { removed: Removed, sessions: Vec<String> }`: ids of the sessions the job relocated
    (`sessions_under` now returns `(path, id)`); the park record = these ∪ the moved tabs' session ids. `moved`
    targets are matched by session key only (the `/move` path fix starts a new controller generation); tabs are
    matched to sessions by session file name (omp's `/move` keeps it) and get `SetSessionPath` once they moved.
- **omp-deck**
  - **Remove worktree…**: merged → as today. Not merged but clean + pushed → "Park" confirm (branch, upstream,
    "the PR is not merged yet; the local branch is kept and never deleted by OmpDeck; sessions move into <parent>")
    → same relocation flow with `Removal::Park`; on success a `ParkedBranch` with the ids of every session of the
    worktree (scan before removal) is recorded. Otherwise refused with the reasons (push first / commit first).
  - Rows of parked sessions (id in a record): chip `parked · <branch>` (own hue); hover Branch / Base / "Parked —
    local branch kept"; when checked out in the primary: "checked out in the main checkout".
  - Row/tab menu on parked sessions: **Continue in worktree** (disabled with a reason while the branch is checked
    out somewhere): default location for the branch → `WorktreeAddExisting` → open tabs of the record's sessions get
    `MoveTo { dest: worktree }`, closed ones `RelocateSessions`, then the clicked session is opened/focused; records
    `worktree_bases[worktree] = base`, drops the park record. **Continue in main checkout**: `Query::BranchState`,
    confirm that lists the other running sessions in the main checkout ("they will see branch <b>"), `SwitchBranch
    (primary, branch)` (dirty → refused with the reason), then opens/focuses the session. **Switch main checkout
    back to <base>** when the branch is checked out in the primary (same confirm/refusal).
  - Record pruning (startup + every 10 min, worker): drop when the branch no longer exists, or when it is merged and
    not checked out anywhere. Never deletes a branch.
  - **Row hover buttons** (user, 2026-10-26): every worktree/park action of a row is also an icon button shown on
    hover of the eligible row (header-button style: size, outline pop, tooltip = menu label, disabled + reason):
    worktree rows → Finish worktree (Cancel finishing while finishing) + Remove worktree…; parked rows → Continue in
    worktree + Continue in main checkout (Switch main checkout back to <base> when checked out there). Same code
    path as the menu items; the age column yields to the buttons while hovered.

## Terminal file drops and separated panels (0.18.1+)

- **File drops** (`deck_term::input::encode_dropped_paths`, `TerminalView`): `RawInput::dropped_files` paste into the
  view under the pointer (the focused one when no pointer position is known) as space-separated paths, Unix
  backslash-escaped over omp's unescape set (whitespace, quotes, backslash, backtick and `()[]{}&;<>|?*!$`), Windows `"`-wrapped when needed;
  through the terminal's bracketed paste, blocked while input is held. `hovered_files` draw an accent outline.
  Wayland: `omp-deck` `file_drop.rs` binds `wl_data_device` (v≤3) on winit's display (separate queue + thread), accepts
  `text/uri-list` over the main surface, reads the list off-thread, and injects `PointerMoved` (surface px / zoom),
  `PointerGone` and `dropped_files` in `App::raw_input_hook`. X11/Windows come from winit.
- **Separated panels** (user, 2026-10-27; VS Code reference): `AppConfig.separated_panels` (default true,
  `Setting::SeparatedPanels`, Shared). `theme::set_separated` before `theme::apply`; `look()`/`look_of()` return
  `look::separated(look)`: Lines/Fade regions → `Region::Islands` with `island_gap` 4, `island_r` 8, `separated`
  (no drop shadow, no inner highlight, no Layered Depth cast, backdrop `pal.bar`); Islands/Frame styles unchanged.
  Neutral/Neon get the framed central panel too. Changes panel uses `skin::right_panel_frame` (Islands panel in every
  Islands look). Tab row stays full width (user choice). Settings → Appearance toggle + palette
  `layout.separated_panels`.

## Legacy TUI shortcuts removed (user, 2026-10-27)

- `session_actions::{Kind, Action}` lose `AgentHub` (Alt+A) and `Context` (`/context`): the status-bar ⋯ menu and the
  palette no longer open omp's TUI agent hub or context overlay (the agents dashboard and the context tooltip replace
  them). Supersedes the "Open omp's Agent Hub (TUI)" line of Wave 5 and the Agent hub / Context actions of the status bar.
- App menu (title bar ⋯): no "Usage…" (status-bar chip, status-bar ⋯ and Ctrl+Shift+U remain) and no top-level
  "Check for updates"; the omp submenu's check is labelled "Check for updates" ("Checking for updates…" while running).
- `confirm::Dialog.ctrl_enter_confirms`: Ctrl+Enter (consumed before the body, so the multiline editor never sees it)
  confirms; footer hint "Ctrl+Enter confirms". Used by the Handoff dialog.

## Custom agent configuration (Settings → Agents)

- `deck_core::agent_defs` reads and edits the task agents omp discovers (`task/discovery.ts`): the user dir
  `<agent dir>/agents/*.md` (`agent dir` = the sessions root's parent), per project the nearest
  `<ancestor>/.omp/agents/*.md` walking up from its cwd, and omp's built-ins. Precedence (omp's): project > user >
  plugins > bundled, so a file of a built-in's name replaces it. Extension/plugin agents are not listed.
  `discover(agent_dir, cwds)` → `Catalog{dirs, bare_roots, settings}`: the user `AgentDir` first (even when
  missing), then project dirs deduped and sorted; `bare_roots` = project cwds with no `.omp/agents` above them
  (where a new project agent would go: `<cwd>/.omp/agents`).
- Built-ins: `Omp::bundled_agent_sources()` runs `omp agents unpack --dir <temp dir> --json` and reads back only
  the `written` files directly in that temp dir (removed afterwards); `bundled(&[(file, content)])` parses them
  like files into an `AgentDir{scope: Scope::Bundled}` under pseudo paths `omp:bundled/<file>` (`BUNDLED_PREFIX`,
  `is_bundled`), sorted. They are never written: every writer refuses those paths.
- Reading mirrors omp's `parseAgent`: CRLF normalized, leading HTML comments skipped, frontmatter up to the first
  `\n---`, trimmed body. `AgentFile.issue` says why omp skips a file (unreadable, no frontmatter, missing name or
  description, reserved `main`/`sub`, name already declared by an earlier file of the same dir). `revision` = first
  16 bytes of the content's SHA-256, hex.
- Edits are textual (`Edit::Model(Option<String>)`, `Edit::Body(String)`): a model change rewrites only the `model`
  entry (inserted after `description`, written JSON-quoted; `None` removes it), a body change replaces only the
  trimmed body. Every other byte, comments, key order and line endings included, is kept. `save(path, agent_dir,
  revision, edit)` refuses anything but an existing `*.md` directly in an agents dir (`is_agents_dir`: the user
  dir or any absolute `…/.omp/agents`; `NotAgentFile`), re-reads and refuses a moved revision (`Changed`, text
  `CHANGED_ON_DISK`, tested with `is_changed_error` by clients that only see the message), then replaces the file
  atomically (through a symlink to its target, keeping its permissions).
- `create(dir, agent_dir, name, content)` writes `<dir>/<name>.md` with `create_new`: `dir` must be an agents dir
  (`NotAgentsDir`) whose project root exists (the user dir is created if missing), `name` passes `check_name`
  (`[a-z0-9]` words joined by single hyphens, ≤ `NAME_MAX` = 64, not `main`/`sub`; `BadName`), and no file in
  `dir` may already declare it (`Exists`). `NewAgent{name, description, model, body}.content()` is omp's own
  `saveAgent` layout: JSON-quoted `name`/`description`/`model` frontmatter, a blank line, the trimmed body
  (frontmatter only when empty; empty description → `NoDescription`). `delete(path, agent_dir, revision)`: same
  path and revision checks as `save`.
- `Omp::agent_settings()` reads `task.agentModelOverrides`, `task.disabledAgents` and `modelRoles` (`omp config get
  --json`, side by side) into `AgentSettings`; override entries are kept whole, malformed ones included, so a write
  changes only the agent it names: `overrides_with(name, Option<&str>)` / `disabled_with(name, bool)` return the
  full new value for `OVERRIDES_KEY` / `DISABLED_KEY` (written with `Mutation::OmpSet`, as omp's `/agents` hub
  does). `role_model(alias)` resolves `@role`, `pi/role` and `@role:level` through `modelRoles` (`ROLES_KEY`).
  omp's model precedence: explicit request > config override > the agent's `model` > the spawning session's
  model, so an override hides the file's model; an explicit `:level` suffix beats `thinkingLevel`, which beats the
  role's level.
- Gateway (`PROTOCOL` unchanged): `Query::AgentDefs{projects}` → `Answer::AgentDefs(Catalog)` (discovery, the three
  config reads and the unpack in parallel; a failed unpack is the built-in dir's `error`). Mutations, serialized
  by one lock: `AgentEdit{path, revision, edit}` ("agent-edit") and `AgentCreate{dir, agent: NewAgent}`
  ("agent-create") → `Done::AgentFile(Box<AgentFile>)`; `AgentCustomize{dir, name}` ("agent-customize") unpacks
  the built-ins, copies the one omp loads as `name` verbatim through `create` → `Done::AgentFile` (no such
  built-in: `Refused`); `AgentDelete{path, revision}` ("agent-delete") → `Done::Unit`. `NotAgentFile` and
  `NotAgentsDir` are `Refused`, every other agent error `Failed`.
- UI (`agent_config.rs`, app glue `app/agent_config.rs`): Settings nav entry "Agents" at the top of the omp section
  (badge = files with unsaved prompt edits) and the palette command `settings.agents`. Rendered full height outside
  the settings scroll area (max 1600 px), without the omp search field. Projects = the sidebar groups' cwds. The
  page rereads the files whenever it is opened (the page or the Settings window on it) and on its refresh button.
  - Header: "New agent" opens the create dialog. List: filter box (name, description, model, file name,
    "built-in"); a "User" group, one per project folder (with their path) and "Built-in" last; a "+" on the user
    and project groups opens the dialog for that location. Rows show the model (or the overriding one; roles as
    `@smol · <model>`) and glyphs for issue, disabled, override, replaced built-in and unsaved edits. A selection
    made by the page (a new file, what a delete gives way to, "Show it") is scrolled into view.
  - Enabled switch (every agent): writes `task.disabledAgents` with or without its name.
  - Model: a chip opening `model_picker::show` (the status bar's dropdown; `PickerInput.unset` is its "no current
    model" text). Custom agents: a pick writes `selector[:level]` to the file, keeping the agent's level when the
    new model supports it; effort pills re-level the current model; × removes `model`. Built-ins: a pick writes
    `task.agentModelOverrides[name]`, ↶ removes it (omp's default applies). Fallback lists show as written and
    mark no model current. Reload, stars and provider filter are the app's shared ones.
  - Override notice on custom agents: "Remove override" writes `task.agentModelOverrides` without this agent.
    Settings writes update the omp settings row, toast, and reread.
  - Built-in pane: read-only prompt, "replaced by" banner with "Show it" when a user (everywhere) or project file
    (there) of its name exists, and "Customize ▾": one entry per location (user, each project), disabled where a
    file already declares the name; the copy is selected once listed.
  - Create dialog (`agent_config/create.rs`): name (live `check_name`, clash with the location's files, note when
    it replaces a user agent or a built-in), location (user, every project folder, those without `.omp/agents`
    included), description (required), optional prompt; Ctrl+Enter creates. A refusal stays in the dialog; the
    new file is selected once listed and its model is picked in the editor.
  - Delete (custom agents): asks first, saying what applies afterwards (the user agent of that name in the
    project, else the built-in, else nothing) and selects it after the reread.
  - System prompt: monospace editor per file, Save / Revert, Ctrl+S. Drafts live per file for the app's lifetime;
    a reread keeps dirty drafts and, when the file moved on under one, shows "The file changed on disk…" with
    "Keep my edits" (rebase onto the disk revision; saving then replaces only the prompt) / "Use the file". A save
    that loses the race rereads and shows that choice (info toast); a lost model pick is an error notice.
  - Changes apply to the next task an agent runs, in running sessions too (omp rediscovers agents per spawn and
    watches its config).
- `omp_settings::split_level` is the shared `provider/id:level` splitter (`auto` and omp's levels);
  `widgets::card_frame` is `card`'s surface without a caption.

## Settings is a window (user, 2026-10-27)

- `Tab::Settings` is gone: Settings opens as an overlay window like the session tree, Usage and Sessions windows
  (scrim + `egui::Window`, title bar off, `card::chrome`, resizable/movable, default 94 % × 92 % of the screen
  clamped to 860–1800 × 520–1300, min 760 × 440). It covers the sidebar and the tab strip; the tabs and the active
  tab are untouched, so closing it returns to the tab that was active.
- `settings_page::window(ctx, state, input, blocked) -> Vec<SettingsAction>` (replaces `show(ui, …)`): header row
  (gear tile, "Settings", ✕ → `SettingsAction::Close`) over the nav rail and the content, both rounded panes.
  `SettingsState::opened()` releases keyboard focus held outside the window (the terminal) on its next frame and
  rereads the agents when the Agents page is selected. Agents page width cap 1200 → 1600 px.
- Close: ✕, a click on the scrim, or Esc when, at the start of the frame, no widget had focus (a text field being
  edited lets go first), no menu/popup and no egui modal was open, and the key is still unconsumed after the page and
  its dialogs were drawn. `blocked` (app dialogs, palette, Sessions/Usage/tree windows, agent popup) disables Esc and
  the scrim click.
- App: `settings_open: bool`; `open_settings()` (no-op while open) / `close_settings()` (returns focus to the
  terminal). `modal_open()` includes it (no palette, tab switching or split shortcuts behind it); the terminal does not
  take focus back and its input is held while it is open (`hold_input_during_modals`). Drawn right after the central
  panel, so the other windows and dialogs stack above it. `SettingsAction::InstallOmp` closes it before opening the
  installer tab. The title-bar gear shows active while it is open; the quick switcher no longer lists a Settings tab.

## Backup destinations: Google Drive or an FTP/FTPS/SFTP server (deck-core)

- Module `deck_core::gdrive` is now `deck_core::backup`: `archive`, `project` and the shared `Service` logic at the
  root; `backup::drive` (`Account`, `AccountStore`, `Credentials`, `Endpoints`, `LoginFlow`, `LOGIN_TIMEOUT`,
  `SCOPES`; private `client` = Drive v3, `oauth`); `backup::server` (FTP/FTPS/SFTP); private `remote` (the
  `Remote` trait every operation goes through, `BackupFile`, `is_config_backup`/`is_project_backup`, `to_delete`).
- `pub enum Destination { #[default] Drive, Server }` (serde: `"Drive"`, `"Server"`);
  `AppConfig.backup_destination: Destination` (default Drive). One destination is active at a time.
- `Service { creds: Option<Credentials> /* None = Drive unavailable */, endpoints, store: AccountStore,
  server_store: ServerStore, destination, agent_dir, safety_root, cache_root }`; `Service::new(Option<Credentials>,
  agent_dir)` (Drive active); `with_destination(Destination) -> Service` (clone). `backup`, `list`,
  `prepare_restore`, `apply_restore`, `delete_backup`, `backup_project`, `list_projects`, `prepare_project_restore`,
  `apply_project_restore`, `delete_project_backup`, `project_state` act on `destination`. Without a linked
  destination: Drive → `NotSignedIn` (`DriveUnavailable` when `creds` is `None`), Server → `NoServer`.
- State per destination, all under `config_dir()`, mode 0600: Drive `google-drive.json` + `project-sync.json`
  (unchanged); Server `backup-server.json` (`ServerAccount { config, last_backup, synced_fingerprint }`, holds the
  password) + `project-sync-server.json`. Switching destinations never reads the other's sync state, so the first
  pass after a switch uploads. `server_connect` keeps `last_backup`/`synced_fingerprint` and the project state only
  when protocol, host, port, user and folder are unchanged; otherwise both reset. `server_forget` deletes both server
  files and nothing on the server.
- `server::{Protocol { Ftp, Ftps /* explicit AUTH TLS */, Sftp }, ServerAuth { Anonymous /* FTP/FTPS */,
  Password { user, password }, Key { user, key_path, passphrase } /* SFTP */ } /* serde tag "kind" */,
  ServerConfig { protocol, host, port /* 0 = 21/22 */, auth, folder /* "" = DEFAULT_FOLDER "omp-deck-backups",
  relative to the login dir unless it starts with "/" */, pinned: Option<String> }, ServerAccount, ServerStore}`.
  `ServerConfig::normalized()` → `InvalidServer` for impossible combinations; `label()` = `scheme://host[:port]`.
  `Debug` of `ServerAuth` redacts secrets.
- `Service::server_connect(ServerConfig) -> Result<ServerAccount>`: normalize, connect, log in, create the folder,
  write + rename + delete a probe file, then save. Nothing is saved on failure. `server_account()` reads the file.
- Trust on first use: `Error::UntrustedServer { fingerprint }` when nothing is pinned (SFTP: any host key; FTPS: a
  certificate the WebPKI roots reject) — the UI shows the fingerprint and retries with `pinned = Some(fingerprint)`.
  `Error::ServerChanged { pinned, presented }` when the server shows another key/certificate than the pinned one.
  SFTP fingerprint = OpenSSH `SHA256:<base64>` (`ssh-keygen -l`); FTPS = `SHA256:` + uppercase colon hex of the leaf
  DER (`openssl x509 -fingerprint -sha256`). A pinned FTPS certificate is accepted on an exact match only (no
  WebPKI, name or expiry check). Other server failures are `Error::Server(String)`, worded for the user.
- Server folder layout: each backup = `<name>.tar.gz` + sidecar `<name>.tar.gz.json`
  (`{"created": RFC 3339, "size", "props"}` = Drive's `createdTime`/`size`/`appProperties`). Both are uploaded as
  `.part` and renamed, archive first; a failed or cancelled upload removes its parts. `BackupFile.id` = the archive
  name. Listing needs the sidecar; only `omp-backup-*`/`omp-project-*` `.tar.gz` names with no `/`, `\`, `..` or
  control characters are valid ids; other files in the folder are never listed, pruned or deleted. Delete removes
  the sidecar, then the archive; a missing file counts as deleted. Retention (30 config, 3 per host+project) and
  the delete safety checks are the Drive rules.
- Transports (blocking, on the caller's worker thread; connect 15 s, stall 60 s): FTP/FTPS via `suppaftp` 12.2
  (`rustls-ring`), passive mode with the data connection dialled to the control address, binary type; SFTP via
  `russh` 0.65 (`ring`) + `russh-sftp` 3 on a private current-thread tokio runtime. No OpenSSL / aws-lc.
- Accepted advisory (`deny.toml`): RUSTSEC-2023-0071 (Marvin, `rsa` timing side channel; no fixed release). It comes
  in through russh's `rsa` feature, which RSA user keys and host keys need. The only private-key operation is the
  client's single user-auth signature per login, sent to a server the user configured whose host key is pinned on first
  use. Host-key verification uses public-key operations only. Marvin needs an attacker timing many private-key
  operations, which this one-signature-per-login pattern does not offer. Dropping the feature would break RSA-key
  logins.
- Wired into the gateway and the GUI in 0.20.0 (next section). The gateway's `Service` reads `google-drive.json` and
  `backup-server.json` from the served profile's `app_root()`. That is `config_dir()` for the default profile; an
  explicit `--settings` profile keeps its own on every platform, since Windows' config dir ignores the environment.

## Backup destinations in the gateway and the GUI (0.20.0)

- Protocol (`PROTOCOL` stays 1; the 0.20.0 gateway version makes a newer GUI replace an older gateway):
  - `Topic::Drive`/`TopicValue::Drive(DriveView)` → `Topic::Backup`/`TopicValue::Backup(BackupView)`;
    `StateCache.backup` (always on). `BackupView { destination, drive_available, drive_signed_in, drive_email,
    server: Option<ServerView>, last_backup /* active destination */, busy /* lane label */, last_error
    /* automatic backup at the active destination */, last_auto_unix_ms }`, `linked()`, `place()`.
  - `ServerView { protocol, host, port /* effective */, user /* None = anonymous */, key_path, folder, pinned:
    bool, label }` = `ServerView::of(&ServerAccount)`: never the password or passphrase.
  - Renamed (they act on the active destination): queries `BackupList`, `ProjectBackups`, `ProjectSyncState`,
    `ProjectMeasure { project }` (same `Answer` names); mutations `Backup { force }`, `PrepareRestore(BackupFile)`,
    `ApplyRestore(u64)`, `DiscardRestore(u64)`, `DeleteBackup(BackupFile)` (journal keys `backup`,
    `prepare-restore`, `restore`, `discard-restore`, `delete-backup`); `Done::{Backup, Prepared { handle, plan },
    Restored}`. Google-only ops keep their names: `DriveLoginBegin/Finish/Cancel`, `DriveSignOut`,
    `Done::{DriveLogin, DriveAccount}`.
  - `Mutation::ServerConnect { config: ServerConfig, keep_secret: bool }` (`server-connect`) →
    `Done::ServerConnect(ServerConnectOutcome::{Connected(ServerView), Untrusted { fingerprint }, Changed { pinned,
    presented }})`: trust answers are results, not errors. Before connecting, `jobs::inherit` fills from the saved
    server when protocol, host (case-insensitive) and effective port match: a missing `pinned`, and with
    `keep_secret` an empty password / missing passphrase for the same trimmed user. Remove + add again drops both.
  - `Mutation::ServerForget` (`server-forget`): deletes the local server files only.
- Engine: one backup lane for every destination (`backup_lane`; login finish, sign-out, backup, restores, deletes,
  project jobs, `ServerConnect`, `ServerForget`); refusal "Another backup job is running (…); try again.".
  `Service` is always built (`Credentials::detect()` may be `None`); jobs use `service.with_destination(
  cfg.backup_destination)`. Schedules arm per `(destination, linked)` and re-arm on a change; a destination switch
  clears the shown error; an automatic failure is recorded only while its destination is still active.
- Settings: `BackupDestination(backup_destination: Destination)`, `Shared`; the GUI switches it through the
  settings path like any other shared field.
- GUI: `backup_view.rs` (was `drive_view.rs`) — `BackupCard { destination, drive: DriveAccount, server, place,
  last, interval, busy, picker }`, `BackupAction`, `ServerForm` (kept in `SettingsState.server_form`); the card
  shows a "Back up to" picker (Drive disabled with a reason when unavailable), the Drive rows or the server
  summary/form (protocol FTP/FTPS/SFTP, plain-FTP warning, host, port, anonymous, user, password or key file with
  Browse… = `transfer::pick_file`, passphrase, folder), the trust dialog with the fingerprint, then the shared
  schedule/backup/restore/manage rows. `app/backup.rs` (was `app/drive.rs`, `BackupUi`). Dialogs, toasts, the
  sidebar `BackupMenu { place, off }` and the palette (`project.backup`, `project.restore`, `backup.now`,
  `backup.restore`, `backup.manage`; `sync.*` unchanged) name the active destination; the Sync page's strip shows
  the server or a "Backup settings…" button.

## Native chat view (TSP) step 1: protocol core + host terminal (user, 2026-11-01)

Facts: `docs/notes/tsp.md` (read first). User decisions: build omp's native (TSP) view in OmpDeck in three steps
(1 protocol core + host, 2 egui renderer, 3 detach/reattach replay); the chat view exists **only on the OmpDeck
session host** (tmux cannot carry TSP); keep omp's own composer (drive it with `edit`/`send` events, never a private
text box). Step 1 ships no GUI.

### New crate `crates/deck-tsp` (lib; depends on nothing of ours)
Deps: serde, serde_json, thiserror, log, sha2, base64 (workspace). Pure, no threads, no I/O, no clocks of its own
(callers pass `Instant`). Wire types follow omp **v18.15.0** `packages/wire/src/tsp.ts` (fetch the tag; cite it in a
module comment). Unknown verbs/fields/kinds/ops are tolerated, never fatal.
- `wire`: `Node { id: String, k: String, p: serde_json::Map<String, Value> /* default empty */, c: Vec<Node> }`,
  `Op` (all ops of the tag + `Unknown(Value)`), `Frame { sf, s: u64, ops }`, `Open`, `Close`, hello `Query`/`Reply`,
  `Event` (every event kind of the tag; serialize-only is fine for kinds the terminal never sends).
- `stream::Splitter`: incremental scanner over PTY **output** bytes; `feed(&mut self, bytes, now: Instant) ->
  Vec<Message>` where `Message { verb: Verb, params: Vec<(String, String)>, body: Vec<u8> }` (body reassembled from
  `c`/`m` chunks). Passive: never alters or consumes the stream (callers still feed alacritty). Handles sequences
  split at any byte, ignores non-`tsp;` APC (kitty `G`), drops a chunk sequence on interruption or after 1 s idle,
  caps one APC at 262 144 bytes and a reassembled body at 16 MiB + base64 overhead (oversize ⇒ dropped, logged).
- `document::Document`: per-surface trees from `o`/`f`/`x`/`s`/`b` messages; `apply(&mut self, &Message) ->
  Applied { acks: Vec<(String /*sf*/, u64 /*s*/)>, errors: Vec<Event> }` (one `error` event per rejected op with its
  index; the rest of the frame still applies). `splice` uses UTF-16 offsets. Blobs verified (sha256 id) and kept
  under a byte cap (LRU, 64 MiB). `Serialize` for the whole document (blobs as `{id, mime, len}` only).
- `terminal::Responder`: the terminal side. `Responder::new(TermInfo { cols, dark, reduce_motion, hour12, input:
  InputEncoding::{Apc, Osc877} })`; `on_output(&mut self, bytes, now) -> Vec<u8>` = scan + apply + the bytes to
  write to the child's input, in order: hello reply (to the `hello` query; kinds = the tag's `TSP_KINDS` minus
  `json`, features limited to what this crate honours — decide from the spec and document), `blobs` query replies,
  one coalesced `ack` per surface per call, error events. `resize(cols)` / `theme(dark)` return the matching event
  bytes only once hello was answered (nothing before). `encode_event(&Event, InputEncoding) -> Vec<u8>` for the GUI
  later. `document(&self) -> &Document`, `live(&self) -> bool` (hello answered).

### deck-host
- `SessionSpec` gains `#[serde(default)] pub tsp: bool` (old specs ⇒ classic). Only `tsp: true` sessions answer
  hello; classic sessions are byte-for-byte unchanged.
- `Core` gains `tsp: Option<deck_tsp::Responder>` (Unix `InputEncoding::Apc`, Windows `Osc877`). `Core::feed`:
  `processor.advance` → `scanner.advance` → `tsp.on_output` appended to `replies` → `answer_queries` (so the hello
  reply precedes the DA1 reply of the same chunk). Acks are sent attached or not. PTY resize ⇒ `resize` event;
  palette change ⇒ `theme` event when darkness flips (dark = client palette bg luminance < 0.5; default dark).
- Protocol (additive, `PROTOCOL` stays 1): `Kind::TspPeek = 0x0c` (client → daemon, no payload), answered on the
  same connection with `Snapshot`… `SnapshotEnd` whose concatenated bytes are the document JSON, or `Error("not a
  TSP session")`. Client fn `tsp_peek(ns, name) -> Result<serde_json::Value>`.
- CLI `omp-deck-host tsp <name> [--follow]`: prints the document as an indented outline (surface, region, `k`,
  `p.role`, ≤ 80 chars of text) — the step-1 debug view; `--follow` re-peeks every 500 ms and reprints on change.

### Acceptance (step 1)
- Unit (deck-tsp): splitter at every split offset, interrupted/idle chunk drop, caps, kitty APC ignored; every op incl.
  UTF-16 `splice` on surrogate pairs, `set` null delete, `move`, rejected op ⇒ `error` with index while later ops
  apply; responder emits hello reply then acks, nothing before hello; fixtures recorded from real omp 18.15.0 via
  `PI_TUI_TSP_RECORD` (committed under `crates/deck-tsp/tests/fixtures/`, scrubbed of home/user/host names) replay
  into a document whose final tree contains the composer `editor` and the streamed assistant text.
- Host (no omp): a test child writes hello APC + `ESC[c` and echoes its input: the hello reply arrives before the
  DA1 reply; a `tsp: false` session gets DA1 only.
- Live (real omp 18.15.0 + the scripted fake model, sandbox agent dir, `omp-deck-test-*` namespace; lives in
  `crates/deck-service/tests/` to reuse `support::fake_model`): a `tsp: true` host session goes native; a `send`
  event submits a prompt; the reply streams (≥ 2 frames grow the assistant text); `tsp_peek` shows it; then with no
  client attached a second prompt (sent via a short attach + detach) completes and appears in the peeked document
  (proves detached acking, no "stopped acknowledging" stall in omp's log).

### Step 1 as built (2026-11-02)
- deck-tsp: `stream::{Splitter::{new, feed, feed_into}, Message, Verb, encode_message, encode_chunked}`,
  `wire::{Node, Op, Frame, Open, Close, Event, Query, Reply, KINDS}` (45 kinds at v18.15.0),
  `document::{Document::{apply, surfaces, surface, inline, blobs_have}, Surface, NodeRef, OpError}`,
  `terminal::{Responder::{new, on_output, resize, theme, motion, document, live, program, info}, TermInfo,
  InputEncoding::{Apc, Osc877, native}, encode_event, encode_reply, hello_reply, FEATURES}`.
- Hello reply: `term:"omp-deck"`, kinds = KINDS minus `json` and `media` (0.25.1: omp then sends tool-result images as
  blob-backed `image` nodes instead of path-only `media`, which also works for remote hosts), features
  `["blobs","settle","adopt","reduce-motion"]`
  (only what the crate honours; `dock`/`aside`/`scroll`/`styles`/`flow` are renderer decisions for step 2),
  `apc:65536`, `credits:2`.
- deck-host: `SessionSpec.tsp`, `Kind::TspPeek = 0x0c`, `tsp_peek`, CLI `new … --tsp`, `tsp <name> [--follow]`.
  deck-core `mux` and deck-service `host_remote` pass `tsp: false` (classic sessions unchanged).
- omp only probes outside tmux: TSP sessions need `host_env` (removes `TMUX`/`TMUX_PANE`/…).
- Proof: `crates/deck-tsp/tests/replay.rs` (real omp 18.15.0 recording, any chunking ⇒ identical document and the
  recorded acks), `crates/deck-host/tests/tsp_host.rs` (hello before DA1, detached acks, classic untouched),
  `crates/deck-service/tests/tsp_live.rs` (real omp + fake model: native, `send`, streaming, detached turns, tool turn).
  Windows: xwin clippy only.

## Native chat view (TSP) step 2: chat view (user, 2026-11-02)

User: "move onto next step, the chat view must be elegant and beautiful". The visual direction is chosen by the
user from mockups (`~/omp-deck-chat-mockups/`); this section fixes data, input, launch and persistence. Research:
`/tmp/omp-chat/tsp-catalog.md` (what omp emits per kind/role), plumbing map below. The late-join replay originally
planned as step 3 is a prerequisite here (the GUI frontend always joins after the gateway owner, the owner may join
the host after omp's first frames), so it lands in step 2; step 3 is reduced to polish found in use.

### Data: mirror + replay
- **Single answerer stays the host.** Nothing outside deck-host may reply/ack (`Responder` is host-only); every other
  copy of the document is a passive mirror.
- deck-tsp:
  - `Splitter::is_idle(&self) -> bool` (no APC in progress and no chunk sequence pending).
  - `Document::apply` takes the receive time (`apply(&mut self, &Message, now: Instant)`; `Responder` passes its
    `now`) and stamps each node with the time its props/text last changed: `NodeRef::changed_at() -> Instant`.
  - `Document::revision() -> u64`, bumped by every applied message that changed anything.
  - Program hello: the document remembers the program's last `hello` query (`Document::program() ->
    Option<&Program>`; `Responder::program` reads it from there).
  - `Document::replay(&self, now: Instant) -> Vec<u8>`: program→terminal APC bytes (`encode_chunked`, apc 65 536)
    that rebuild an equal document in a fresh `Mirror`: the program's hello query, then per open surface `o`, `s`
    stylesheets, one `f` of `add` ops (regions + subtrees in order, `seq` = the surface's last seq), `settle`s,
    `focus`; retained blobs as `b`. Timers stay truthful: `age` props of unstopped `elapsed` nodes and of running
    `tool`/`agent` nodes are advanced by `now - changed_at` in the replayed props.
  - `mirror::Mirror { feed(&mut self, bytes, now) -> bool /* revision changed */, document(), is_idle(), live() /*
    an open inline surface with role omp.session */, reset() }` = `Splitter` + `Document`, never produces bytes.
  - Equality test: for the 18.15.0 fixture at every message boundary, `Mirror` fed `replay()` serializes equal to the
    source document (except timer props, compared separately).
- deck-host: `Core::snapshot` appends `tsp.document().replay(now)` after the VT replay when the session is TSP and
  live (covers attach, observe, Peek and exit-linger). vte ignores APC, so the grid is unchanged.
- deck-term (`OutputScan`): a `Mirror` beside the kitty scanner, fed in `OutputScan::feed`, rebuilt in `reset`;
  `Engine::snapshot` appends `mirror.document().replay(now)` after the grid and before the unfinished-sequence
  prefix; `Engine::ready` also waits while the mirror is not idle (a join mid-chunk-sequence would lose a frame).
  Shared with the UI without the engine lock: `Terminal::tsp(&self) -> TspHandle` (cloneable) with
  `revision() -> u64` (atomic), `live() -> bool`, `with<R>(&self, f: impl FnOnce(&Document) -> R) -> R` (short
  mutex; the output thread holds it only while applying one message). Repaint rides the existing `Wakeup`.
- deck-term input: `Terminal::send_tsp(&self, &deck_tsp::wire::Event) -> Result<(), TspRefused>` encodes with
  `encode_event(.., InputEncoding::native())` and goes through the same checked path as user input; refused (typed:
  `NotLive`, `Held`, `ReadOnly`, `Exited`, `Detached`) instead of silently dropped, so the UI can show it. Key
  presses for omp's composer use the existing key encoders/`Terminal::write` (omp owns the editor text; it answers
  with `text`/`splice`/`set`).

### Launch and persistence
- deck-core `AppConfig.session_view: SessionView { Terminal (default), Chat }` (`#[serde(default)]`): how new tabs
  open. Shared settings field through `SettingsService` like other shared fields.
- `TabState.view: SessionView` (`#[serde(default)]`): written from the tab, used when a restore has to create the
  session again (an alive session keeps whatever mode it was created with).
- deck-service gateway `protocol::LaunchSpec.view: SessionView` (`#[serde(default)]`). In `prepare`, `Chat` launches
  on the **host** backend whenever it is available, regardless of `BackendChoice` (tmux cannot carry TSP), and sets
  `SessionSpec.tsp = true`; host unavailable ⇒ launched as Terminal and the outcome says so
  (`LaunchOutcome`/`Done` carries the effective view). Every other `LaunchSpec` constructor passes the view it means
  (restart/restore/worktree/mobile keep the tab's current view; mobile launches use `AppConfig.session_view`).
- Switching an open tab between views = restart of the omp process in the other mode (`omp --resume <file>`), idle
  only, busy warning, same path as Restart session; the tab keeps its place.
- `ControllerInfo.view: SessionView` (effective; from the host spec) so GUI/phone can tell.

### GUI (omp-deck)
- One dispatch in `show_terminal` and `show_split`: a tab whose `Terminal::tsp().live()` shows `chat_view::ChatView`,
  else `TerminalView`. Chat view keeps the wrapper duties: `Terminal::resize` (cols from the chat column width in
  the terminal font, rows from the pane) and `set_focused`, `focus_pending`/`terminal_focused` bookkeeping, split
  focus border, visible-for-notifications.
- Renders the inline surface: `main` as a scrolling transcript (stick-to-bottom while at bottom, "jump to latest"
  pill otherwise), `dock` as the composer area, `layer` as overlays/pickers/toasts. Follows the active OmpDeck style
  (all 18; colors from the theme, not omp's palette); omp `tone`s map onto theme colors. Markdown via the existing
  styled markdown renderer where it fits; code/diff/ansi blocks in the terminal font. No allocation per frame per
  node beyond egui layout; cache laid-out jobs by (node id, revision).
- Composer = omp's `editor` node rendered natively (text, cursor, selection, placeholder, ghost); keys go to omp as
  PTY bytes; paste through `paste_bytes`; send button/Enter per omp (`submit`); every clickable node sends the
  event its `actions` declare (`toggle`/`select`/`activate`/`action`/`change`); `copy`/`open`/`zoom` local.
- Menus: session row + tab menus "Open in chat view"/"Open in terminal" (+ "Switch to chat/terminal view" on open
  tabs), Settings → General "Open sessions in", palette commands; a hover button on session rows for the
  non-default view (project rule: context actions also as hover buttons).

### Mirror as built (TspMirror, 2026-11-02)
- Beyond the sketch: `Splitter::resume_bytes` / `Responder::resume_bytes` (the host appends them after the replay so a
  client attaching mid chunk sequence still gets that frame); `Document::set_program`; `HostInfo.tsp: bool`
  (`#[serde(default)]`); `Engine::join_split` (a join waiting on a non-idle mirror splits at the next ESC);
  `TspHandle::revision` stays monotonic across snapshot resets. `deck_tsp::Program` lives in `document`.
- `send_tsp` checks only the local terminal; an owner's later refusal of a frontend's bytes is asynchronous
  (debug-logged). A stalled chunk sequence expires only when more bytes arrive.
- Proof: `deck-tsp/tests/mirror.rs` (replay equality at every fixture boundary), `deck-host/tests/tsp_host.rs`,
  deck-term frontend/remote tests, `deck-service/tests/tsp_mirror_live.rs` (real omp late joiner equal + send streams).

### Visual direction (user, 2026-11-02): "F with A conversation style"
Mockups `~/omp-deck-chat-mockups/F-signal-edge.png` + `A-conversation.png` (round 3; HTML beside them holds exact
CSS). Structure from **A**: user turns right-aligned bubbles (max ~70% width) with avatar initials and time;
assistant prose unboxed on the left under a small `omp · <model>` byline; tools inline in document order as
cards; composer floats detached from the bottom edge with a shadow. Visual language from **F**: slanted
(parallelogram) edges on user bubbles' and tool cards' headers, cyan→violet accent-pair gradient hairlines,
the running tool glows (animated, respects reduce-motion), glass-like elevated composer with a gradient outline.
Type: prose 15 px / 1.6 (proportional UI face), mono 12.5–13 px (terminal font), labels ≥ 11 px; measure ≤ ~880 px
centered; Blueprint by default but colors come from the active style (all 18 styles, light included).
Default view stays **Terminal**; chat is opt-in (user).

### Step 2 as built (2026-11-02)
- **Run state** (smoke found omp drops the `π` title under TSP; its state channel is OSC 7501, `run-status.ts`):
  `deck_host::{RunState, RunPhase, BlockedKind, TspFacts, run_state::{RunStateScanner, Report, parse_osc,
  encode_report}}` (re-exported by deck-term); held report replayed in host + engine snapshots; `HostInfo.run_state`,
  `HostInfo.tsp_facts`; `Terminal::run_state()`; `Mux::pane_status() -> PaneStatus { title, run_state, tsp_facts }`
  replaces `pane_titles`; `RunningSession.{run_state, tsp_facts}`. One precedence everywhere:
  `Activity::derive(run_state, title, tsp_facts)` / `Activity::of_terminal(&Terminal)` = OSC 7501 report > `π` title
  > TSP document (dock `omp.working` ⇒ Busy, `omp.editor.ask*` ⇒ Attention, sendable composer ⇒ Idle) > Unknown;
  Working ⇒ Busy, Blocked ⇒ Attention, Idle/Done/Error ⇒ Idle. tmux sessions keep titles (tmux drops unknown OSC).
  Gate: `deck-service/tests/tsp_run_state_live.rs`.
- **Launch**: `Backends::for_launch`; `Mutation::Restart { target, view: Option<SessionView> }`, `Done::Session(target,
  view)`; restart placement via `LaunchJob.owner` (`Owner`), a restart that changes backend (tmux → host for Chat)
  changes the session key; `ControllerInfo.view`, `ManifestEntry.view`; mobile/worktree opens use
  `AppConfig.session_view`. Gate: `deck-service/tests/chat_launch.rs`.
- **GUI**: `omp-deck/src/chat_view/` (`ChatView::{new, id_salt, font_size, show} -> ChatResponse { response,
  open_url, refused }`), dispatch in `show_terminal` and `show_split` on `tsp().live()`; non-default
  open/new menu entries + row hover button, "Switch to chat/terminal view" (idle only, confirm), Settings → General
  "Open sessions in", palette "View: Switch to …", toast when a Chat launch fell back to Terminal.
  Chat links: `file://` hrefs (tool path targets) decode to a local path opened with the platform opener on the chat
  path only (`is_openable_url` unchanged). TSP-live tabs/panes show a chat glyph. Running sessions without a tab offer
  plain **Open** (attach keeps the session's real mode). Keyboard: `Terminal::route_keyboard` is the one routine both
  `TerminalView` and `ChatView` use (win32 modifier sync, repeats/releases, IME preedit via `Terminal::preedit`).
  `TspHandle::{epoch, with_stamp}`: the view re-syncs on (epoch, revision), never on the document's own revision.
  Exited sessions show `[process exited: <code>]` in place of the composer; caret blink only while composer + window
  are focused (≤ 20 s after the last input); `ChatState` swept after 15 min unseen.
- **Activity publish**: the gateway republishes `Topic::Controllers` when `ctl.activity()` changes
  (`Managed.published_activity`), not only on title change (gate `tsp_run_state_gateway_live.rs`). Child exit and RIS
  clear the held run state; an exited terminal's `Activity::of_terminal` is `Unknown`.
- **Injection under TSP** (`deck_service::inject`): when `terminal.tsp().live()`, composer state comes from the mirrored
  document (`composer::TspComposer::of`: dock `editor` with `sendable` + surface focus; ask dialog/no focus ⇒ `Missing`;
  draft text, chips, autocomplete open, `can_send` = hello lists `send`), passed as `Env.tsp`. Slash commands and idle
  Send go as one TSP `send` event (`Write::Send`, `Terminal::inject_tsp` = `send_tsp` minus the input-hold check);
  a confirmed draft is displaced into omp's recall history (no Ctrl+C). Insert/Steer/FollowUp/attachments/Stop/hotkeys
  keep the PTY path and observe the document. No `send` in hello ⇒ PTY path for everything. A refused `Write::Send`
  ends `Aborted`, never delivered. Gate `tsp_inject_live.rs`.
- **Replay**: `Document::replay_with(now, ReplayBlobs::{Referenced, None})`; attach/observe replay only blobs a live node
  references, Peek none (a blob sent before an attach and referenced only later is missing in that client — accepted).
  `Splitter::resume_bytes(now, out)` carries skipping + sequence age (`x-resume-skip`, `x-resume-age`); a join waiting
  on an abandoned chunk sequence completes after `CHUNK_IDLE` (`tsp-expiry` wake). Queued restores keep their real
  backend in the manifest (`Owner::Restored`). `PROTOCOL` stays 1: gateway and clients are replaced together.

## Native chat view (TSP) step 3: polish (user, 2026-11-02: "fix those limits elegantly")
Removes the step-2 known limits. Shared interfaces:
- **Toast ring** (`deck-tsp`): omp appends a `toast` to `main` and deletes it in the next frame, so a GUI can miss it.
  `Document::apply` records every created `toast` node into a bounded ring (≤ 8):
  `Document::toasts(now) -> impl Iterator<Item = &ToastRecord>`, `ToastRecord { seq: u64, text: String, sub:
  Option<String>, ttl: Duration, at: Instant }`, entries expire at `at + ttl` (default 2400 ms when omp sends none).
  The ring is part of `replay` only while unexpired. The chat view draws toasts only from the ring, keyed by `seq`.
- **Blob fetch** (`deck-host` + `deck-term`): a mirror node may reference a blob the client never received (sent before
  attach, referenced later). deck-term's remote mirror asks the host for missing blob ids referenced by live nodes
  (new request kind, one in flight per id, bounded, no retry storm); the host answers from its document's blob store
  or "absent". The mirror inserts the blob and bumps its revision. Invisible to the view; local (non-host) terminals
  have the whole stream and never fetch.
- **Draft clear under TSP** (`deck-service::inject`): "Clear and run" clears the editor exactly like classic (Ctrl+C,
  observed as editor `text` empty in the document) before the `send`, so the draft is not pushed into omp's recall
  history. Classic behavior is unchanged.
- **Syntax highlighting** (`omp-deck` `chat_view`): `syntect` 5.3 (`default-features = false`, features
  `default-syntaxes`, `regex-fancy`; pure Rust, no onig) for `code`, fenced `md` code and `diff` lines; scopes map to
  the active style's terminal palette (the same 16 ANSI colors the terminal uses), never to syntect themes. Syntax set
  loads lazily off the UI thread; highlighting runs on a worker, results cached by (node id, generation); plain text is
  shown until ready. Unknown language ⇒ plain.
- **Diff split mode**: `diff.mode` `split` ⇒ side-by-side old/new columns with aligned hunks; `auto` ⇒ split when the
  card is wide enough for two ≥ 60-column panes, else unified; `unified` ⇒ unified.
- **md `marks`**: chip tokens in sent prompts styled as inline chips.
- **Selection**: transcript text is selectable and copyable (drag + Ctrl+C/Ctrl+Shift+C, context menu Copy); the
  composer supports drag selection mapped onto omp's editor (`edit`/`anchor` when the program advertises `edit`, else
  a local selection that only copies).
- **Tool actions**: every tool-card action omp advertises (e.g. rerun) is offered as a hover button + context menu.
- **IME**: preedit drawn at the caret, commit sent as text; covered by an egui-level test with synthetic `Event::Ime`.
- **Thinking**: live thinking body clamped to its tail (last lines, fade on top); idle todo pill right-aligned.

### Step 3 as built (2026-11-02)
- **Highlighting**: `syntect` 5.3 (`parsing`, `regex-fancy`) + `two-face` 0.5 (`syntect-fancy`; bat's syntax set incl.
  TypeScript/TSX/TOML, ≈ +0.6 MB; no onig). One `chat-highlight` worker (set loaded there), LRU 192 resumable at
  32-line checkpoints; scopes → `Tok` classes colored at paint time from the style palette with OKLab hue steering
  (keyword/string/number/function/type kept apart and off the text color on all 20 style classes). Diffs: header
  `+N −M` + Unified | Split switch (per card, local); absent `mode` = unified (omp's edit tool), `auto` splits at
  ≥ 2 × 60 columns; old/new sides highlighted as separate streams. `md.marks` chips on plain text.
- **Transcript selection** (`chat_view/tsel.rs`; egui label selection is off in the chat, `selectable_labels = false`):
  OmpDeck-owned, independent of painting, so it spans virtualized items. Position = node id (resolved to the per-revision
  pre-order `ord`) + run `seq` (reserved for every run a renderer visits, painted or not) + char. Deleted endpoint ⇒
  start of what followed; epoch reset clears. Painted runs register `(key, galley, rect)` each frame for hit-testing
  and highlight; copy re-renders the selected items invisibly and joins their runs in document order (so copied text
  = painted text; blank line between items/md blocks, tab between table cells, list/diff markers only when the
  selection starts at the run's first char; unified diffs keep `+`/`-`/space, split diffs copy one side). Gestures:
  drag, wheel-or-edge auto-scroll while dragging, click sets the anchor, Shift+click extends, double-click word,
  triple-click line; links/fold toggles never start one; a split-diff drag past a column's last row stays in that
  column; dock/composer chrome is never selectable. Menu: Copy, Select all (from the first conversation item, welcome
  card excluded). Copy keeps the selection; a plain click or Esc clears it — Esc is also sent to omp while a turn runs
  (`Snap::busy`), swallowed when idle. Not selectable: card/tool/agent heads, badges, kbd, icons, live status lines,
  meters, picker/list rows, gutters, code headers, kv keys, table heads.
- **Composer selection** (`select.rs` `ComposerSel`) is local (omp's `anchor` is vim-visual only): release sends a
  caret `edit`; typing/delete/cut/paste over it send a replacing `edit`; without `edit` in hello it is copy-only. The
  composer context menu has no hover buttons (standard text-field actions, all on shortcuts).
- **Tool actions**: omp 18.15.0 has no tool rerun (`tools[]` reserved, never filled). Cards offer `tools[]`,
  `actions.menu`, `href`, and local Copy command/output/task + Open as hover buttons and a context menu.
- **Toasts** only from `Document::toasts(now)` (ring ≤ 8, default ttl 2.4 s, cap 60 s, replayed while unexpired).
- **Blob fetch**: `Kind::{BlobGet, BlobHead, BlobData}`, `HostInfo.blob_fetch` (attach asks `Info` first so an old
  daemon never sees the new kind); the owning connection fetches blobs its mirror lacks and forwards them to
  subscribers between sequences; observers/frontends never ask.
- **Draft clear**: TSP "Clear and run" = Ctrl+C, wait for editor `text` empty, then `send` (never sends over a draft).
  omp's `composer.recallClearedDrafts` (default true) puts a Ctrl+C-cleared draft into recall in both views — omp's
  behavior, identical to classic.
- **Fake model**: `fm::task_script(req, hold)`; prompts `TASK-MARK` (plain subagent) / `TASK-YIELD-MARK` (needs
  `task.structured: true`). Gate `tsp_task_live.rs`.
- **Clipboard bridge**: egui copies a label selection in its end-of-pass step, after the app ran, so
  `clipboard_bridge::forward_copies` missed it; replaced by `clipboard_bridge::install` (an egui output-hook plugin)
  that forwards every `OutputCommand::CopyText` to the app clipboard. Chat menu Copy is requested at the start of the
  press frame (egui clears label selections on a press outside text).
- **Chords**: `clipboard_shortcuts: false` delivers Ctrl+C/X/V as `Event::Key` while a terminal/chat has focus, so
  `select::plan` takes them directly: with a selection Ctrl+C copies, Ctrl+X cuts (composer, needs `edit`), Ctrl+V
  replaces the composer selection then pastes; press and release are swallowed. No selection ⇒ the chord reaches omp.
  `clipboard_bridge::install` runs once in `App::new`.
- **Highlight worker**: generation checked every 32 lines (stale parses abandoned, done lines kept), 200 ms budget per
  request, lines > 1000 bytes stay plain; eviction keeps every slot asked for in the current frame; word operators
  (`in`, `as`, `and`, `not`, `is`, `instanceof`) are keywords. Wrapped code/diff rows hang at the line's own indent
  (≤ ⅓ pane). `\ No newline at end of file` is `NoEol` (no banner/reset/pairing break; dropped in split).
- **Blobs**: `Blob.bytes: Arc<[u8]>`; host caps unwritten blob answers at 64 MiB per connection (over ⇒ "absent"),
  a full client queue closes the connection; clients ask only when `blob_fetch`, at most 3 times per blob per
  snapshot (15 s loss timeout), only on a revision change; absent/forged answers make the mirror forget the blob; the
  wanted set is recomputed from live nodes on delete.
- **Toast age**: replay adds `age` (ms) to a toast node omp has not deleted yet; the ring backdates it, so an expired
  toast never reappears on reopen. omp 18.15.0 reuses one toast node while idle (first toast `add`, later ones `set`
  on the same id, recorded live): `set`/`text`/`splice` on a `toast` node record a fresh toast. Gate `tsp_toast_live.rs`.
- **Attachments**: a TSP composer with chips and no text is `Draft("[n attachment(s)]")` ⇒ confirmation before `send`.
- **Fullscreen pages** (0.25.1 fix; user bug: clicking the git segment froze the chat until the session was killed):
  omp's fullscreen overlays (`status.git` git app, `/models`, agents hub, `/settings`) open a second surface
  `o {"mode":"screen","role":"omp.session"}` and freeze the inline one; all input goes to the overlay until `x`. The
  view keeps a second `Snap` + `Local` for the newest open non-inline surface (`snap::Follow::Page`), draws it instead
  of the transcript/dock (`chat_view/page.rs`: native `button`, two-pane `omp.app.git`, scrolling sheets) and
  addresses its events to that `sf`. An always-drawn "Back to chat" strip sends omp Esc, so even an undrawable page can
  be left. deck-service `TspComposer::of` returns `Composer::Missing` while any non-inline surface is open ⇒ injected
  commands are refused as "dialog open" and never typed into the overlay.

### 0.25.1 (2026-11-03, user requests after 0.25.0)
- **Session status bar in the tab area**: the bar is a bottom strip of the area right of the sidebar (panel order
  sidebar → status bar → changes panel → central; the sidebar keeps the full window height), one bar across split
  panes following the focused one. Restyled to the chat composer's row: `BAR_H` 40, `CHIP_H` 26, 13/12 px text,
  15 px icons, chip plates (chat chip fill + hairline, `theme::rad(8,10)`, pop ring on hover), no dividers (6 px gaps,
  measured-width dropping unchanged, usage first), glass gradient strip with the accent-pair hairline, model chip
  `MODEL_CHIP_W` 260 / floor `MODEL_CHIP_MIN` 176 / share 0.42, actions in one outlined box, Retry/Stop amber pills.
  Test `bar_is_a_bottom_strip_of_the_tab_area_and_leaves_the_sidebar_alone`.
- **Model picker** restyled to the chat language (accent header band "Model · N models" + reload, 32 px search and
  provider plates, current row wash + left bar + ✓, chip-fill thinking pills); behavior unchanged.
- **Favorites** (user: lost after an Anthropic logout/login): `favorite_models` was always persisted; `build_rows`
  hid selectors missing from omp's auth-filtered catalog and the catalog only reloaded at startup. Now unavailable
  favorites stay in ★ as grey "Not available now" rows (star removable, not selectable), favorites show while the
  catalog loads/fails, and every picker opening refreshes the catalog (cached rows kept meanwhile). Selectors are
  `provider/id` independent of auth mode (omp `models-cli.ts`). Gate `deck-service/tests/model_preferences.rs`.
- **No duplicates in single-pane chat**: `ChatView::hide_duplicates(true)` for a single session pane skips omp's
  composer pieces OmpDeck's bar shows (roles `omp.composer.{model,effort,model.effort,context,usage}`, facts `model`,
  `git`, `path`, `context_pct`, `context_total`, `cost`, `token_*`, `cache_*`) and their separators; empty growing
  extras stay as the spacer so Send/Stop remain right-aligned. Split panes keep omp's chips.
- **IRC cards** (`omp.irc.*` and IRC `write`/`wait` tool cards): whole-card amber outline from the style's warn tone
  (contrast-floored via `Ink::legible`), tinted head, message icon.
- **Wrapping rows**: `row{wrap:true}` with container children uses greedy flex-wrap with bounded child widths
  (`horizontal_wrapped` only for leaf children); `omp.job.tiles` stacks one tile per line; card bodies are clamped and
  clipped to the card width.
- **Images**: `image` crate gains `webp` (omp re-encodes pasted images to WebP/JPEG); `image.max` accepts
  `{"w":"12ch","h":"4lines"}` objects (and px/numbers); `w`/`h` reserve space before the blob; failed decodes are cached;
  click opens a lightbox (Esc/outside closes); "Open" menu works on image nodes. Hello `kinds` omit `media`.
