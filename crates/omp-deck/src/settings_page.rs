//! The Settings window: an overlay over the whole app (like the session tree and Usage windows)
//! with a nav rail on the left (OmpDeck's own preferences, then omp's settings tabs) and the
//! selected section on the right, with a search field that filters across every omp tab.

use std::path::Path;

use crate::advisor_page::{self, AdvisorAction, AdvisorInput, AdvisorState};
use crate::agent_config::{self, AgentsAction, AgentsInput, AgentsState};
use crate::icons::Icon;
use crate::icons::{
    ICON_BELL, ICON_BOT, ICON_BRAIN, ICON_CHEVRONS_UP_DOWN, ICON_CIRCLE_ALERT,
    ICON_CIRCLE_ARROW_UP, ICON_CLOUD, ICON_CLOUD_UPLOAD, ICON_CPU, ICON_FOLD_VERTICAL, ICON_FOLDER,
    ICON_KEYBOARD, ICON_LIST_CHECKS, ICON_LIST_TREE, ICON_MOON, ICON_NEWSPAPER, ICON_PALETTE,
    ICON_REFRESH_CW, ICON_SEARCH, ICON_SEARCH_X, ICON_SETTINGS, ICON_SETTINGS_2,
    ICON_SLIDERS_HORIZONTAL, ICON_SPARKLES, ICON_SUN, ICON_TERMINAL, ICON_TRIANGLE_ALERT,
    ICON_WRENCH,
};
use crate::icons::{ICON_FILE_PEN_LINE, ICON_PUZZLE};
use crate::mobile_view::{self, MobileAction, MobileInput};
use deck_core::config::{
    Density, GatewayMode, MAX_UI_SCALE, MIN_UI_SCALE, Theme, UiFont, UiFontKind, UiStyle,
};
use deck_core::mux::BackendChoice;
use deck_core::omp::{Setting, SettingsCatalog};
use deck_service::gateway::protocol::{Effective, GatewayStatus, MobileView, Supervision};
use egui::{
    Align2, Color32, CursorIcon, FontId, Frame, Id, Key, Margin, Modifiers, Order, Rect, RichText,
    Sense, Sides, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use serde_json::Value;

use crate::backup_view::{self, BackupAction, BackupCard, ServerForm};
use crate::notify_view::{self, NotifyAction, NotifyInput};
use crate::omp_rows::{
    JsonModal, ModalOutcome, RowEvent, SecretModal, note_banner, show_json_modal, show_row,
    show_secret_modal, skeleton,
};
use crate::omp_settings::{Model, TabHits, TabModel};
use crate::plugins_page::{self, PluginsAction, PluginsInput, PluginsState};
use crate::sync_view::{self, SyncAction, SyncPage};
use crate::theme::{self, Palette};
use crate::update::{UpdateState, status_line};
use crate::widgets::{
    card, empty_state, icon_button, icon_text, paint_icon, segmented, setting_row, stepper, toggle,
};

/// One page of the settings area: an OmpDeck subpage, project sync, the custom agents, or omp's
/// catalog tab `i`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    OmpDeck(OmpDeckPage),
    Sync,
    Agents,
    Omp(usize),
    /// omp's "Installed" page: the plugins `omp plugin list` reports.
    Plugins,
    /// `/advisor configure`: a `WATCHDOG.yml`.
    Advisor,
}

impl Default for Entry {
    fn default() -> Self {
        Self::OmpDeck(OmpDeckPage::General)
    }
}

/// The subpages of Settings → OmpDeck, in nav order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OmpDeckPage {
    General,
    Appearance,
    Notifications,
    Updates,
    Backup,
}

impl OmpDeckPage {
    pub const ALL: [Self; 5] = [
        Self::General,
        Self::Appearance,
        Self::Notifications,
        Self::Updates,
        Self::Backup,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Notifications => "Notifications",
            Self::Updates => "Updates",
            Self::Backup => "Backup",
        }
    }

    /// Stable id part of the palette command.
    pub fn slug(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Appearance => "appearance",
            Self::Notifications => "notifications",
            Self::Updates => "updates",
            Self::Backup => "backup",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::General => "Where sessions run and how the window behaves",
            Self::Appearance => {
                "App and terminal font size, theme, style, UI font and density of this app"
            }
            Self::Notifications => "What the bell and desktop pop-ups tell you about your sessions",
            Self::Updates => "Keep OmpDeck and omp up to date",
            Self::Backup => "Back up your omp configuration to Google Drive or your own server",
        }
    }

    pub fn icon(self) -> Icon {
        match self {
            Self::General => ICON_SLIDERS_HORIZONTAL,
            Self::Appearance => ICON_PALETTE,
            Self::Notifications => ICON_BELL,
            Self::Updates => ICON_CIRCLE_ARROW_UP,
            Self::Backup => ICON_CLOUD_UPLOAD,
        }
    }
}

const CONTENT_MAX_W: f32 = 760.0;
/// The agents page lays out a list and an editor side by side: the editor takes the width.
const AGENTS_MAX_W: f32 = 1600.0;

enum Modal {
    Json(JsonModal),
    Secret(SecretModal),
}

/// State of the omp half of the page.
#[derive(Default)]
struct OmpPane {
    model: Option<Model>,
    loading: bool,
    load_error: Option<String>,
    /// omp's page that was asked for before the catalog was loaded.
    pending_page: Option<String>,
    query: String,
    /// Search results for the query they were computed for.
    hits: Option<(String, Vec<TabHits>)>,
    modal: Option<Modal>,
    /// The project's own settings file (JSON) of the active tab, which supplies some values.
    layer: Option<Value>,
}

#[derive(Default)]
pub struct SettingsState {
    selected: Entry,
    /// (selected page or `None` while searching, search query) last shown; used to reset scroll.
    view_key: Option<(Option<Entry>, String)>,
    omp: OmpPane,
    pub agents: AgentsState,
    pub plugins: PluginsState,
    pub advisor: AdvisorState,
    /// The project whose settings file `omp.layer` holds (`Some(None)`: no project).
    layer_for: Option<Option<std::path::PathBuf>>,
    /// The backup server form (kept while switching pages).
    pub server_form: ServerForm,
    /// Drop keyboard focus held outside the window (the terminal) on its next frame.
    release_focus: bool,
    /// A widget had the keyboard at the end of the last frame (egui drops it on Esc before the
    /// next frame runs, so this is how Esc tells "stop editing" from "close").
    focus_held: bool,
}

impl SettingsState {
    /// The window opens: it takes the keyboard, and the agents page rereads the files.
    pub fn opened(&mut self) {
        // The project's file may have changed since: read it again.
        self.layer_for = None;
        self.release_focus = true;
        self.focus_held = false;
        match self.selected {
            Entry::Agents => self.agents.opened(),
            Entry::Plugins => self.plugins.opened(),
            Entry::Advisor => self.advisor.opened(),
            _ => {}
        }
    }

    /// A catalog load finished (success or failure).
    pub fn catalog_loaded(&mut self, result: Result<SettingsCatalog, String>, config_dir: String) {
        let pane = &mut self.omp;
        pane.loading = false;
        pane.hits = None;
        match result {
            Ok(catalog) => {
                let mut model = Model::from_catalog(catalog, config_dir);
                model.set_project_layer(pane.layer.as_ref());
                pane.model = Some(model);
                pane.load_error = None;
                // A page omp has but OmpDeck's catalog lacks leaves the first one shown.
                if let Some(id) = pane.pending_page.take() {
                    self.show_omp_page(&id);
                }
            }
            Err(e) => {
                pane.pending_page = None;
                pane.load_error = Some(e);
            }
        }
    }

    /// Show omp's settings (what omp's own `/settings` screen is): its category `page` (omp's
    /// page ids are the catalog's tab ids), or the first. Applied when the catalog arrives if it
    /// has not yet.
    pub fn open_omp_page(&mut self, page: Option<&str>) {
        self.selected = Entry::Omp(0);
        self.omp.query.clear();
        self.omp.pending_page = None;
        let Some(id) = page else { return };
        // OmpDeck's own pages (omp's "Installed" page is `plugins`) need no catalog.
        if self.show_own_page(id) {
            return;
        }
        if self.omp.model.is_none() {
            self.omp.pending_page = Some(id.to_owned());
        } else {
            self.show_omp_page(id);
        }
    }

    /// Show one OmpDeck subpage.
    pub fn show_ompdeck_page(&mut self, page: OmpDeckPage) {
        self.selected = Entry::OmpDeck(page);
        self.omp.query.clear();
    }

    /// Show the project sync page.
    pub fn show_sync(&mut self) {
        self.selected = Entry::Sync;
        self.omp.query.clear();
    }

    /// Show the custom agents page (rereading the agent files).
    pub fn show_agents(&mut self) {
        self.selected = Entry::Agents;
        self.omp.query.clear();
        self.agents.opened();
    }

    /// Show the Plugins page (rereading the plugin list).
    pub fn show_plugins(&mut self) {
        self.selected = Entry::Plugins;
        self.omp.query.clear();
        self.plugins.opened();
    }

    /// Show the Advisor page (rereading the file).
    pub fn show_advisor(&mut self) {
        self.selected = Entry::Advisor;
        self.omp.query.clear();
        self.advisor.opened();
    }

    /// The pages OmpDeck draws itself for what omp's `/settings` shows as its own pages: omp's
    /// "Installed" page (`plugins`; `installed` and `plugin` are tolerated) and the advisor.
    fn show_own_page(&mut self, id: &str) -> bool {
        match id {
            "plugins" | "plugin" | "installed" => self.show_plugins(),
            "advisor" => self.show_advisor(),
            _ => return false,
        }
        true
    }

    /// Show the search results for one omp setting key (a link to that setting).
    pub fn show_omp_setting(&mut self, key: &str) {
        self.selected = Entry::Omp(0);
        self.omp.query = key.to_owned();
    }
    /// The effective value (saved, else default) of omp setting `key` as the loaded catalog has it.
    pub fn omp_effective(&self, key: &str) -> Option<&Value> {
        self.omp.model.as_ref()?.find(key)?.effective()
    }

    /// The project whose settings file the badges and hidden editors follow is `project`: the
    /// file to read, when it is not the one already read (or being read). With no project there
    /// is no file, and the marks are cleared at once.
    pub fn layer_request(&mut self, project: Option<&Path>) -> Option<std::path::PathBuf> {
        if self.layer_for.as_ref().map(|p| p.as_deref()) == Some(project) {
            return None;
        }
        self.layer_for = Some(project.map(Path::to_path_buf));
        match project {
            Some(p) => Some(p.to_path_buf()),
            None => {
                self.apply_layer(None);
                None
            }
        }
    }

    /// The settings file of `project` was read (`None`: it has none, or not a JSON object): the
    /// rows it supplies get an "overridden by" badge, and omp's three editors that it hides go.
    /// A reply for a project no longer followed is dropped.
    pub fn project_layer(&mut self, project: &Path, layer: Option<Value>) {
        if self.layer_for.as_ref().and_then(Option::as_deref) == Some(project) {
            self.apply_layer(layer);
        }
    }

    fn apply_layer(&mut self, layer: Option<Value>) {
        let pane = &mut self.omp;
        pane.layer = layer;
        pane.hits = None;
        if let Some(model) = &mut pane.model {
            model.set_project_layer(pane.layer.as_ref());
        }
    }

    /// `(id, label)` of omp's settings categories once the catalog is loaded, then the pages
    /// OmpDeck draws itself.
    pub fn omp_pages(&self) -> Vec<(String, String)> {
        let mut pages: Vec<(String, String)> = self.omp.model.as_ref().map_or_else(Vec::new, |m| {
            m.tabs
                .iter()
                .map(|t| (t.id.clone(), t.label.clone()))
                .collect()
        });
        pages.push(("plugins".to_owned(), "Plugins".to_owned()));
        pages.push(("advisor".to_owned(), "Advisor".to_owned()));
        pages
    }

    /// Show omp's settings category `id`; false when the catalog has none by that id.
    pub fn show_omp_page(&mut self, id: &str) -> bool {
        if self.show_own_page(id) {
            return true;
        }
        let Some(i) = self
            .omp
            .model
            .as_ref()
            .and_then(|m| m.tabs.iter().position(|t| t.id == id))
        else {
            return false;
        };
        self.selected = Entry::Omp(i);
        self.omp.query.clear();
        true
    }

    /// A write finished; returns the row's label for the toast (the key if the row is gone).
    pub fn write_done(&mut self, key: &str, outcome: Result<Option<Value>, String>) -> String {
        match self.omp.model.as_mut().and_then(|m| m.find_mut(key)) {
            Some(row) => {
                row.finish(outcome);
                row.label.clone()
            }
            None => key.to_owned(),
        }
    }
}

pub struct SettingsInput<'a> {
    pub font_size: f32,
    pub session_view: deck_core::config::SessionView,
    pub font_range: (f32, f32),
    pub font_default: f32,
    /// App-wide scale (egui zoom, 1.0 = 100 %).
    pub ui_scale: f32,
    pub theme: Theme,
    pub style: UiStyle,
    pub density: Density,
    pub separated_panels: bool,
    pub ui_font: UiFont,
    pub native_decorations: bool,
    pub omp_found: bool,
    pub omp_version: Option<&'a str>,
    pub check_updates: bool,
    pub update: &'a UpdateState,
    pub backend: BackendCard,
    pub backup: BackupCard,
    pub sync: SyncPage<'a>,
    pub notify: NotifyInput<'a>,
    pub app_update: crate::app_update::SettingsCard<'a>,
    pub mobile: MobileCard<'a>,
    pub agents: AgentsInput<'a>,
    /// The project of the active session tab.
    pub project: Option<&'a Path>,
    /// An omp session is open in the active tab (plugin changes and saved advisors reach it).
    pub live_tab: bool,
}

/// The session-backend card: the stored choice and which backends exist on this machine.
#[derive(Clone, Copy)]
pub struct BackendCard {
    pub choice: BackendChoice,
    pub tmux: bool,
    pub host: bool,
}

#[derive(Debug, PartialEq)]
pub enum SettingsAction {
    /// Close the window (✕, Esc, a click outside it).
    Close,
    FontSize(f32),
    /// App-wide zoom factor (1.0 = 100 %).
    UiScale(f32),
    Theme(Theme),
    Style(UiStyle),
    Density(Density),
    SeparatedPanels(bool),
    UiFont(UiFont),
    NativeDecorations(bool),
    SetCheckUpdates(bool),
    CheckUpdates,
    /// Load (or reload) omp's settings catalog on a worker.
    LoadCatalog,
    WriteSetting {
        setting: Box<Setting>,
        value: Value,
    },
    ResetSetting {
        key: String,
    },
    CopyKey(String),
    /// Copy an omp error message to the clipboard.
    CopyDetails(String),
    /// Run omp's official installer in a tab (omp was not found).
    InstallOmp,
    /// The Backup card (destination, account or server, schedule).
    Backup(BackupAction),
    /// Where new sessions run (Unix: tmux or the OmpDeck host).
    SessionBackend(BackendChoice),
    SessionView(deck_core::config::SessionView),
    /// The project sync page.
    Sync(SyncAction),
    Notify(NotifyAction),
    /// The gateway lifetime mode (Settings → Mobile access).
    GatewayMode(GatewayMode),
    /// The Mobile access card's relay, devices and pairing.
    Mobile(MobileAction),
    AppUpdate(crate::app_update::SettingsAction),
    /// Settings → Agents.
    Agents(AgentsAction),
    /// Settings → Plugins.
    Plugins(PluginsAction),
    /// Settings → Advisor.
    Advisor(AdvisorAction),
}

fn tab_icon(id: &str) -> Icon {
    match id {
        "appearance" => ICON_PALETTE,
        "model" => ICON_BRAIN,
        "interaction" => ICON_KEYBOARD,
        "context" => ICON_NEWSPAPER,
        "memory" => ICON_CPU,
        "files" => ICON_FOLDER,
        "shell" => ICON_TERMINAL,
        "tools" => ICON_WRENCH,
        "tasks" => ICON_LIST_CHECKS,
        "providers" => ICON_CLOUD,
        "internal" => ICON_SETTINGS_2,
        _ => ICON_LIST_TREE,
    }
}

/// Draws the window (scrim + resizable window) while it is open. `blocked`: another dialog is in
/// front of it, so Esc and clicks outside are not the window's.
pub fn window(
    ctx: &egui::Context,
    state: &mut SettingsState,
    input: &SettingsInput<'_>,
    blocked: bool,
) -> Vec<SettingsAction> {
    let mut out = Vec::new();
    let pal = Palette::of_ctx(ctx);
    let screen = ctx.content_rect();

    egui::Area::new(Id::unique("settings-scrim"))
        .order(Order::Background)
        .fixed_pos(screen.min)
        .interactable(true)
        .show(ctx, |ui| {
            let (rect, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
            let dark = ctx.global_style().visuals.dark_mode;
            ui.painter().rect_filled(rect, 0, theme::backdrop(dark));
            if resp.clicked() && !blocked {
                out.push(SettingsAction::Close);
            }
        });

    if std::mem::take(&mut state.release_focus)
        && let Some(id) = ctx.memory(|m| m.focused())
    {
        ctx.memory_mut(|m| m.surrender_focus(id));
    }
    // Esc belongs to whatever holds it first: a text field being edited (it lets go of the
    // focus), an open menu, or a dialog of the page (it consumes the key while drawn below).
    let esc_free = !blocked
        && !state.focus_held
        && ctx.memory(|m| m.top_modal_layer().is_none())
        && !egui::Popup::is_any_open(ctx);

    let size = vec2(
        (screen.width() * 0.94)
            .clamp(860.0, 1800.0)
            .min(screen.width() - 24.0),
        (screen.height() * 0.92)
            .clamp(520.0, 1300.0)
            .min(screen.height() - 24.0),
    );
    let margin = Margin::symmetric(16, 14);
    egui::Window::new("settings-window")
        .id(Id::unique("settings-window"))
        .title_bar(false)
        .frame(Frame::NONE.inner_margin(margin))
        .resizable(true)
        .movable(true)
        .collapsible(false)
        .constrain_to(screen)
        .min_size(vec2(760.0, 440.0))
        .default_size(size)
        .default_pos(screen.center() - size / 2.0)
        .show(ctx, |ui| {
            crate::card::chrome(ui, margin, pal.bar, |ui| {
                if header(ui) {
                    out.push(SettingsAction::Close);
                }
                ui.add_space(10.0);
                out.extend(body(ui, state, input));
            });
        });
    if esc_free && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        out.push(SettingsAction::Close);
    }
    state.focus_held = ctx.memory(|m| m.focused().is_some());
    out
}

/// Title row and rule; true when its ✕ was clicked.
fn header(ui: &mut Ui) -> bool {
    let pal = Palette::of_ui(ui);
    let mut close = false;
    Sides::new().height(32.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            crate::card::icon_tile(ui, 30.0, ICON_SETTINGS, Color32::TRANSPARENT, true);
            ui.label(theme::strong("Settings").size(19.0).color(pal.text_strong));
        },
        |ui| close = crate::card::close_button_sized(ui, 30.0, 18.0).clicked(),
    );
    let (left, right, y) = (
        ui.min_rect().left(),
        ui.min_rect().left() + ui.available_width(),
        ui.min_rect().bottom() + 3.0,
    );
    crate::card::header_rule(ui, left, right, y, Color32::TRANSPARENT);
    close
}

fn body(ui: &mut Ui, state: &mut SettingsState, input: &SettingsInput<'_>) -> Vec<SettingsAction> {
    let pal = Palette::of_ui(ui);
    let mut actions = Vec::new();

    // Lazy first load.
    let pane = &mut state.omp;
    if input.omp_found && pane.model.is_none() && !pane.loading && pane.load_error.is_none() {
        pane.loading = true;
        actions.push(SettingsAction::LoadCatalog);
    }
    let searching =
        input.omp_found && state.selected != Entry::Agents && !pane.query.trim().is_empty();
    if searching {
        ensure_hits(pane);
    }

    let pane_frame = || {
        Frame::NONE
            .stroke(Stroke::new(1.0, pal.stroke))
            .corner_radius(theme::rad(8, 12))
    };
    egui::Panel::left("settings-nav")
        .exact_size(230.0)
        .resizable(false)
        .frame(
            pane_frame()
                .fill(pal.panel)
                .inner_margin(Margin::symmetric(12, 14)),
        )
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| nav(ui, state, input, searching));
        });

    let mut jump: Option<&'static str> = None;
    let content = pane_frame().fill(pal.bg).outer_margin(Margin {
        left: 10,
        ..Margin::ZERO
    });
    egui::CentralPanel::default_margins()
        .frame(content)
        .show(ui, |ui| {
            theme::skin::paint_canvas(ui.painter(), ui.max_rect(), pal);
            Frame::NONE
                .inner_margin(Margin::symmetric(32, 24))
                .show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    if state.selected == Entry::Agents {
                        // Full height: the list and the prompt editor scroll on their own.
                        ui.set_max_width(ui.available_width().min(AGENTS_MAX_W));
                        actions.extend(
                            agent_config::show(ui, &mut state.agents, &input.agents)
                                .into_iter()
                                .map(SettingsAction::Agents),
                        );
                        return;
                    }
                    if input.omp_found {
                        search_field(ui, &mut state.omp.query);
                        ui.add_space(14.0);
                    }
                    // Scroll resets to the top whenever the category or the search query changes.
                    let view_key = if searching {
                        (None, state.omp.query.trim().to_owned())
                    } else {
                        (Some(state.selected), String::new())
                    };
                    let mut scroll = egui::ScrollArea::vertical()
                        .id_salt("settings-content")
                        .auto_shrink([false, false]);
                    if state.view_key.as_ref() != Some(&view_key) {
                        state.view_key = Some(view_key);
                        scroll = scroll.vertical_scroll_offset(0.0);
                    }
                    scroll.show(ui, |ui| {
                        ui.set_max_width(ui.available_width().min(CONTENT_MAX_W));
                        if searching {
                            omp_view(ui, &mut state.omp, View::Search, &mut actions);
                        } else {
                            match state.selected {
                                Entry::OmpDeck(page) => ompdeck_page(
                                    ui,
                                    page,
                                    input,
                                    &mut state.server_form,
                                    &mut actions,
                                ),
                                Entry::Sync => actions.extend(
                                    sync_view::page(ui, &input.sync)
                                        .into_iter()
                                        .map(SettingsAction::Sync),
                                ),
                                Entry::Agents => {}
                                Entry::Plugins | Entry::Advisor if !input.omp_found => {
                                    not_found_card(ui, &mut actions)
                                }
                                Entry::Plugins => actions.extend(
                                    plugins_page::show(
                                        ui,
                                        &mut state.plugins,
                                        &PluginsInput {
                                            project: input.project,
                                            reload_tab: input.live_tab,
                                        },
                                    )
                                    .into_iter()
                                    .map(SettingsAction::Plugins),
                                ),
                                Entry::Advisor => {
                                    let enabled = state
                                        .omp
                                        .model
                                        .as_ref()
                                        .and_then(|m| m.find(advisor_page::ENABLE_KEY))
                                        .and_then(|r| r.effective())
                                        .and_then(Value::as_bool);
                                    let mut open = false;
                                    actions.extend(
                                        advisor_page::show(
                                            ui,
                                            &mut state.advisor,
                                            &AdvisorInput {
                                                project: input.project,
                                                models: input.agents.models,
                                                favorites: input.agents.favorites,
                                                provider: input.agents.provider,
                                                enabled,
                                                live_tab: input.live_tab,
                                            },
                                            &mut open,
                                        )
                                        .into_iter()
                                        .map(SettingsAction::Advisor),
                                    );
                                    if open {
                                        jump = Some(advisor_page::ENABLE_KEY);
                                    }
                                }
                                Entry::Omp(_) if !input.omp_found => {
                                    not_found_card(ui, &mut actions)
                                }
                                Entry::Omp(tab) => {
                                    omp_view(ui, &mut state.omp, View::Tab(tab), &mut actions);
                                }
                            }
                        }
                    });
                });
        });
    if let Some(key) = jump {
        state.show_omp_setting(key);
    }
    show_modals(ui.ctx(), &mut state.omp, &mut actions);
    actions
}

fn ensure_hits(pane: &mut OmpPane) {
    let q = pane.query.trim();
    if pane.hits.as_ref().is_some_and(|(k, _)| k == q) {
        return;
    }
    pane.hits = pane.model.as_ref().map(|m| (q.to_owned(), m.search(q)));
}

// ------------------------------------------------------------------ nav

fn nav(ui: &mut Ui, state: &mut SettingsState, input: &SettingsInput<'_>, searching: bool) {
    let pal = Palette::of_ui(ui);
    ui.label(theme::section_caption("OmpDeck", pal.weak));
    ui.add_space(4.0);
    for page in OmpDeckPage::ALL {
        if nav_item(
            ui,
            page.icon(),
            page.label(),
            state.selected == Entry::OmpDeck(page) && !searching,
            None,
        )
        .clicked()
        {
            state.show_ompdeck_page(page);
        }
    }
    ui.add_space(8.0);
    if nav_item(
        ui,
        ICON_CLOUD,
        "Sync",
        state.selected == Entry::Sync && !searching,
        None,
    )
    .clicked()
    {
        state.show_sync();
    }
    ui.add_space(12.0);
    ui.label(theme::section_caption("omp", pal.weak));
    ui.add_space(4.0);
    let unsaved = state.agents.unsaved();
    let unsaved_text = unsaved.to_string();
    if nav_item(
        ui,
        ICON_BOT,
        "Agents",
        state.selected == Entry::Agents && !searching,
        (unsaved > 0).then_some((unsaved_text.as_str(), true)),
    )
    .on_hover_text(if unsaved > 0 {
        "Custom task agents · unsaved prompt edits"
    } else {
        "Custom task agents: model and system prompt"
    })
    .clicked()
    {
        state.show_agents();
    }
    if nav_item(
        ui,
        ICON_PUZZLE,
        "Plugins",
        state.selected == Entry::Plugins && !searching,
        None,
    )
    .on_hover_text("Installed omp plugins: enable, features and settings")
    .clicked()
    {
        state.show_plugins();
    }
    let advisor_unsaved = state.advisor.unsaved();
    if nav_item(
        ui,
        ICON_FILE_PEN_LINE,
        "Advisor",
        state.selected == Entry::Advisor && !searching,
        advisor_unsaved.then_some(("unsaved", true)),
    )
    .on_hover_text("Advisors that review the session in the background (WATCHDOG.yml)")
    .clicked()
    {
        state.show_advisor();
    }
    match (&state.omp.model, input.omp_found) {
        (Some(model), true) => {
            let hits = state.omp.hits.as_ref().filter(|_| searching);
            for (i, tab) in model.tabs.iter().enumerate() {
                let badge = match hits {
                    Some((_, hits)) => {
                        let n = hits.iter().find(|h| h.tab == i).map_or(0, |h| h.rows.len());
                        (n > 0).then(|| (n.to_string(), true))
                    }
                    None => Some((tab.visible().to_string(), false)),
                };
                let selected = !searching && state.selected == Entry::Omp(i);
                if nav_item(
                    ui,
                    tab_icon(&tab.id),
                    &tab.label,
                    selected,
                    badge.as_ref().map(|(t, hot)| (t.as_str(), *hot)),
                )
                .clicked()
                {
                    state.selected = Entry::Omp(i);
                    state.omp.query.clear();
                }
            }
        }
        _ => {
            let label = if input.omp_found {
                "omp settings"
            } else {
                "omp not found"
            };
            let icon = if input.omp_found {
                ICON_SETTINGS_2
            } else {
                ICON_TRIANGLE_ALERT
            };
            if nav_item(
                ui,
                icon,
                label,
                matches!(state.selected, Entry::Omp(_)) && !searching,
                None,
            )
            .clicked()
            {
                state.selected = Entry::Omp(0);
                state.omp.query.clear();
            }
        }
    }
}

fn nav_item(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    selected: bool,
    badge: Option<(&str, bool)>,
) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let neutral = theme::flat();
    let (rect, resp) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::dens(30.0, 36.0)),
        Sense::click(),
    );
    let ctx = ui.ctx();
    let hov = ctx.animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.12);
    let sel = ctx.animate_bool_with_time(resp.id.with("sel"), selected, 0.18);
    let p = ui.painter();
    p.rect_filled(
        rect,
        if neutral { 6 } else { 10 },
        theme::hover_fill(pal).gamma_multiply(0.7 * hov * (1.0 - sel)),
    );
    if sel > 0.01 && neutral {
        p.rect_filled(rect, 6, pal.surface.gamma_multiply(sel));
        let bar = egui::Rect::from_center_size(
            pos2(rect.left() + 1.0, rect.center().y),
            vec2(2.0, 14.0 * sel),
        );
        p.rect_filled(bar, 1, pal.accent);
    } else if sel > 0.01 {
        p.rect(
            rect,
            10,
            theme::mix(pal.surface, pal.accent, 0.20).gamma_multiply(sel),
            Stroke::new(1.0, pal.accent.gamma_multiply(0.5 * sel)),
            StrokeKind::Inside,
        );
        let bar = egui::Rect::from_center_size(
            pos2(rect.left() + 2.0, rect.center().y),
            vec2(3.0, 18.0 * sel),
        );
        theme::gradient_rect(p, bar, pal.accent_a, pal.accent_b, false);
    }
    paint_icon(
        p,
        pos2(
            rect.left() + if neutral { 20.0 } else { 22.0 },
            rect.center().y,
        ),
        Align2::CENTER_CENTER,
        icon,
        if neutral { 16.0 } else { 19.0 },
        if neutral {
            theme::mix(pal.glyph, pal.text_strong, sel.max(hov * 0.6))
        } else {
            theme::mix(pal.weak, pal.accent, sel.max(hov * 0.6))
        },
    );
    p.text(
        pos2(
            rect.left() + if neutral { 36.0 } else { 40.0 },
            rect.center().y,
        ),
        Align2::LEFT_CENTER,
        label,
        crate::theme::font(if neutral { 13.0 } else { 14.0 }),
        theme::mix(pal.text, pal.text_strong, sel),
    );
    if let Some((text, hot)) = badge {
        let color = if hot { pal.accent } else { pal.weak };
        let g = p.layout_no_wrap(text.to_owned(), crate::theme::font(11.5), color);
        let chip = egui::Rect::from_center_size(
            pos2(
                rect.right() - 10.0 - (g.size().x + 12.0) / 2.0,
                rect.center().y,
            ),
            vec2(g.size().x + 12.0, 18.0),
        );
        let fill = if hot {
            pal.accent.gamma_multiply(0.18)
        } else if neutral {
            Color32::TRANSPARENT
        } else {
            pal.surface.gamma_multiply(0.8)
        };
        p.rect_filled(chip, 9, fill);
        p.galley(chip.center() - g.size() / 2.0, g, color);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

// ------------------------------------------------------------------ search + omp views

fn search_field(ui: &mut Ui, query: &mut String) {
    let pal = Palette::of_ui(ui);
    let id = egui::Id::unique("omp-settings-search");
    let focused = ui.memory(|m| m.has_focus(id));
    ui.scope(|ui| {
        ui.set_max_width(ui.available_width().min(CONTENT_MAX_W));
        ui.add(
            egui::TextEdit::singleline(query)
                .id(id)
                .prefix(icon_text(ICON_SEARCH, 17.0, pal.weak))
                .hint_text("Search all omp settings…")
                .align(Align2::LEFT_CENTER)
                .min_size(vec2(0.0, 38.0))
                .desired_width(ui.available_width())
                .margin(Margin::symmetric(12, 9))
                .frame(
                    Frame::new()
                        .fill(pal.surface)
                        .corner_radius(12)
                        .stroke(Stroke::new(
                            1.0,
                            if focused { pal.accent } else { pal.stroke },
                        )),
                ),
        );
    });
}

#[derive(Clone, Copy)]
enum View {
    Tab(usize),
    Search,
}

fn omp_view(ui: &mut Ui, pane: &mut OmpPane, view: View, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    let OmpPane {
        model,
        loading,
        load_error,
        pending_page: _,
        query,
        hits,
        modal,
        layer: _,
    } = pane;
    let Some(model) = model.as_mut() else {
        match (load_error.as_deref(), *loading) {
            (Some(err), false) => load_error_card(ui, err, actions, loading),
            _ => {
                ui.label(
                    theme::strong("omp settings")
                        .size(22.0)
                        .color(pal.text_strong),
                );
                ui.add_space(16.0);
                skeleton(ui);
            }
        }
        return;
    };

    let (title, sub) = match view {
        View::Tab(t) => match model.tabs.get(t) {
            Some(tab) => (tab.label.clone(), format!("{} settings", tab.visible())),
            None => return,
        },
        View::Search => {
            let n: usize = hits
                .as_ref()
                .map_or(0, |(_, h)| h.iter().map(|g| g.rows.len()).sum());
            (
                format!("{n} result{}", if n == 1 { "" } else { "s" }),
                format!("for “{}”", query.trim()),
            )
        }
    };
    egui::Sides::new().height(40.0).show(
        ui,
        |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::strong(title).size(22.0).color(pal.text_strong));
                ui.label(RichText::new(sub).color(pal.weak));
            });
        },
        |ui| {
            if *loading {
                ui.add_space(8.0);
                ui.add(crate::widgets::Spin::new(18.0, pal.accent_b));
            } else if icon_button(ui, ICON_REFRESH_CW, "Reload settings from omp", false).clicked()
            {
                *loading = true;
                actions.push(SettingsAction::LoadCatalog);
            }
        },
    );
    ui.add_space(8.0);
    let banner = format!(
        "Changes are written to {}/config.yml via `omp config`; running omp sessions reload them automatically.",
        model.config_dir
    );
    note_banner(ui, &banner);

    let mut events: Vec<(usize, usize, RowEvent)> = Vec::new();
    match view {
        View::Tab(t) => {
            let TabModel { rows, sections, .. } = &mut model.tabs[t];
            for sec in sections.iter() {
                if sec.rows.iter().all(|&ri| rows[ri].hidden()) {
                    continue;
                }
                card(ui, &sec.title, |ui| {
                    let mut n = 0;
                    for &ri in &sec.rows {
                        if rows[ri].hidden() {
                            continue;
                        }
                        if let Some(ev) = show_row(ui, &mut rows[ri], n == 0) {
                            events.push((t, ri, ev));
                        }
                        n += 1;
                    }
                });
            }
        }
        View::Search => {
            let groups: &[TabHits] = hits.as_ref().map_or(&[], |(_, h)| h.as_slice());
            if groups.is_empty() {
                empty_state(
                    ui,
                    ICON_SEARCH_X,
                    pal.weak,
                    "No matching settings",
                    "Try other words — every word must match the key, name or description.",
                );
            }
            for g in groups {
                let tab = &mut model.tabs[g.tab];
                let label = tab.label.clone();
                card(ui, &label, |ui| {
                    for (n, &ri) in g.rows.iter().enumerate() {
                        if let Some(ev) = show_row(ui, &mut tab.rows[ri], n == 0) {
                            events.push((g.tab, ri, ev));
                        }
                    }
                });
            }
        }
    }

    for (t, r, ev) in events {
        let row = &mut model.tabs[t].rows[r];
        match ev {
            RowEvent::Set(value) => {
                row.begin(Some(value.clone()));
                actions.push(SettingsAction::WriteSetting {
                    setting: Box::new(row.setting.clone()),
                    value,
                });
            }
            RowEvent::Reset => {
                row.begin(None);
                actions.push(SettingsAction::ResetSetting {
                    key: row.setting.key.clone(),
                });
            }
            RowEvent::CopyKey => actions.push(SettingsAction::CopyKey(row.setting.key.clone())),
            RowEvent::CopyDetails(text) => actions.push(SettingsAction::CopyDetails(text)),
            RowEvent::EditJson => *modal = Some(Modal::Json(JsonModal::new(row))),
            RowEvent::EditSecret => *modal = Some(Modal::Secret(SecretModal::new(row))),
        }
    }
}

fn show_modals(ctx: &egui::Context, pane: &mut OmpPane, actions: &mut Vec<SettingsAction>) {
    let outcome = match &mut pane.modal {
        None => return,
        Some(Modal::Json(m)) => (m.key.clone(), show_json_modal(ctx, m)),
        Some(Modal::Secret(m)) => (m.key.clone(), show_secret_modal(ctx, m)),
    };
    match outcome {
        (_, ModalOutcome::Open) => {}
        (_, ModalOutcome::Cancel) => pane.modal = None,
        (key, ModalOutcome::Save(value)) => {
            pane.modal = None;
            if let Some(row) = pane.model.as_mut().and_then(|m| m.find_mut(&key)) {
                row.begin(Some(value.clone()));
                actions.push(SettingsAction::WriteSetting {
                    setting: Box::new(row.setting.clone()),
                    value,
                });
            }
        }
    }
}

fn not_found_card(ui: &mut Ui, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    ui.label(
        theme::strong("omp settings")
            .size(22.0)
            .color(pal.text_strong),
    );
    ui.add_space(16.0);
    card(ui, "omp not found", |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.label(icon_text(ICON_TRIANGLE_ALERT, 26.0, pal.warn));
            ui.vertical(|ui| {
                ui.label(
                    theme::strong("omp not found on PATH")
                        .size(15.0)
                        .color(pal.text_strong),
                );
                ui.add_space(2.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            format!("OmpDeck edits omp's settings through the `omp` command, but could not find it in {}. Install it with omp's official installer, or add it to PATH and restart OmpDeck.", crate::update::OMP_LOOKUP),
                        )
                        .color(pal.weak),
                    )
                    .wrap_mode(egui::TextWrapMode::Wrap),
                );
                ui.add_space(10.0);
                if crate::widgets::install_omp_button(ui) {
                    actions.push(SettingsAction::InstallOmp);
                }
            });
        });
    });
}

fn load_error_card(ui: &mut Ui, err: &str, actions: &mut Vec<SettingsAction>, loading: &mut bool) {
    let pal = Palette::of_ui(ui);
    ui.label(
        theme::strong("omp settings")
            .size(22.0)
            .color(pal.text_strong),
    );
    ui.add_space(16.0);
    card(ui, "Could not load settings", |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.label(icon_text(ICON_CIRCLE_ALERT, 26.0, pal.danger));
            ui.vertical(|ui| {
                ui.add(
                    egui::Label::new(RichText::new(err).monospace().small().color(pal.text))
                        .wrap_mode(egui::TextWrapMode::Wrap),
                );
                ui.add_space(10.0);
                let retry = egui::Button::new((
                    icon_text(ICON_REFRESH_CW, 17.0, theme::on_accent(pal)),
                    RichText::new("Retry").color(theme::on_accent(pal)),
                ))
                .fill(pal.accent)
                .min_size(vec2(96.0, 32.0))
                .corner_radius(theme::rad(8, 10));
                if ui.add(retry).clicked() {
                    *loading = true;
                    actions.push(SettingsAction::LoadCatalog);
                }
            });
        });
    });
}

// ------------------------------------------------------------------ OmpDeck's own preferences

/// One OmpDeck subpage: its title and one-line subtitle, then only its own cards.
fn ompdeck_page(
    ui: &mut Ui,
    page: OmpDeckPage,
    input: &SettingsInput<'_>,
    server_form: &mut ServerForm,
    actions: &mut Vec<SettingsAction>,
) {
    let pal = Palette::of_ui(ui);
    ui.label(
        theme::strong(page.label())
            .size(22.0)
            .color(pal.text_strong),
    );
    ui.label(RichText::new(page.subtitle()).color(pal.weak));
    ui.add_space(16.0);
    match page {
        OmpDeckPage::General => general_cards(ui, input, actions),
        OmpDeckPage::Appearance => appearance_cards(ui, input, actions),
        OmpDeckPage::Notifications => actions.extend(
            notify_view::card_ui(ui, &input.notify)
                .into_iter()
                .map(SettingsAction::Notify),
        ),
        OmpDeckPage::Updates => updates_cards(ui, input, actions),
        OmpDeckPage::Backup => backup_card(ui, input, server_form, actions),
    }
}

fn appearance_cards(ui: &mut Ui, input: &SettingsInput<'_>, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    card(ui, "Appearance", |ui| {
        // The whole app zooms; the terminal row below compensates, so its size stays absolute.
        let percent = (input.ui_scale * 100.0).round() as i32;
        let effective = theme::body_size(input.style) * input.ui_scale;
        let effective = if (effective - effective.round()).abs() < 0.05 {
            format!("{effective:.0} pt")
        } else {
            format!("{effective:.1} pt")
        };
        setting_row(
            ui,
            "App font size",
            &format!(
                "{effective} at {percent} % — menus, tabs, sidebar and dialogs; the terminal has its own size"
            ),
            |ui| {
                if ui
                    .add(
                        egui::Button::new(icon_text(crate::icons::ICON_ROTATE_CCW, 16.0, pal.text))
                            .min_size(vec2(30.0, 28.0)),
                    )
                    .on_hover_text("Reset to 100 %")
                    .clicked()
                {
                    actions.push(SettingsAction::UiScale(1.0));
                }
                ui.add_space(6.0);
                if let Some(p) = stepper(
                    ui,
                    "ui-scale",
                    percent as f32,
                    (MIN_UI_SCALE * 100.0, MAX_UI_SCALE * 100.0),
                    5.0,
                    "%",
                ) {
                    actions.push(SettingsAction::UiScale(p / 100.0));
                }
            },
        );
        ui.add_space(4.0);
        setting_row(
            ui,
            "Terminal font size",
            "Ctrl+Shift+= / - / 0 also work",
            |ui| {
                if ui
                    .add(
                        egui::Button::new(icon_text(crate::icons::ICON_ROTATE_CCW, 16.0, pal.text))
                            .min_size(vec2(30.0, 28.0)),
                    )
                    .on_hover_text("Reset to default")
                    .clicked()
                {
                    actions.push(SettingsAction::FontSize(input.font_default));
                }
                ui.add_space(6.0);
                if let Some(size) = stepper(
                    ui,
                    "term-font",
                    input.font_size,
                    input.font_range,
                    1.0,
                    "pt",
                ) {
                    actions.push(SettingsAction::FontSize(size));
                }
            },
        );
        ui.add_space(4.0);
        let dark_only = input.style.dark_only();
        setting_row(
            ui,
            "Theme",
            if dark_only {
                "Chrome and terminal palette (this style is dark-only)"
            } else {
                "Chrome and terminal palette"
            },
            |ui| {
                ui.add_enabled_ui(!dark_only, |ui| {
                    let mut idx = usize::from(input.theme == Theme::Light && !dark_only);
                    if segmented(ui, &mut idx, &[(ICON_MOON, "Dark"), (ICON_SUN, "Light")]) {
                        actions.push(SettingsAction::Theme(if idx == 0 {
                            Theme::Dark
                        } else {
                            Theme::Light
                        }));
                    }
                })
                .response
                .on_disabled_hover_text(format!("{} is dark-only", input.style.label()));
            },
        );
        ui.add_space(8.0);
        style_gallery(ui, input.style, actions);
        ui.add_space(4.0);
        ui_font_row(ui, input.ui_font, actions);
        ui.add_space(4.0);
        setting_row(
            ui,
            "Density",
            "Spacing of the sidebar, tabs, menus and lists",
            |ui| {
                let mut idx = usize::from(input.density == Density::Comfortable);
                if segmented(
                    ui,
                    &mut idx,
                    &[
                        (ICON_FOLD_VERTICAL, "Compact"),
                        (ICON_CHEVRONS_UP_DOWN, "Comfortable"),
                    ],
                ) {
                    actions.push(SettingsAction::Density(if idx == 0 {
                        Density::Compact
                    } else {
                        Density::Comfortable
                    }));
                }
            },
        );
        ui.add_space(4.0);
        setting_row(
            ui,
            "Separated panels",
            "Sidebar and main area as separate rounded, bordered panels. Graphite Islands and \
             Two-Tone Frame keep their own layout.",
            |ui| {
                let mut on = input.separated_panels;
                if toggle(ui, &mut on).changed() {
                    actions.push(SettingsAction::SeparatedPanels(on));
                }
            },
        );
    });
}

/// The UI font picker: every choice set in its own face, grouped Default / Sans / Mono.
fn ui_font_row(ui: &mut Ui, current: UiFont, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    setting_row(
        ui,
        "UI font",
        "Menus, tabs, sidebar and dialogs (not the terminal)",
        |ui| {
            ui.add_enabled_ui(current != UiFont::Auto, |ui| {
                if ui
                    .add(
                        egui::Button::new(icon_text(crate::icons::ICON_ROTATE_CCW, 16.0, pal.text))
                            .min_size(vec2(30.0, 28.0)),
                    )
                    .on_hover_text("Reset to the style default")
                    .clicked()
                {
                    actions.push(SettingsAction::UiFont(UiFont::Auto));
                }
            });
            egui::ComboBox::from_id_salt("ui-font")
                .selected_text(
                    RichText::new(current.label())
                        .family(crate::ui_fonts::preview_family(current))
                        .color(pal.text_strong),
                )
                .width(220.0)
                .height(520.0)
                .show_ui(ui, |ui| {
                    for (kind, caption) in [
                        (UiFontKind::Default, "Default"),
                        (UiFontKind::Sans, "Sans"),
                        (UiFontKind::Mono, "Mono"),
                    ] {
                        ui.add_space(4.0);
                        ui.label(theme::section_caption(caption, pal.weak));
                        for font in UiFont::ALL.into_iter().filter(|f| f.kind() == kind) {
                            let text = RichText::new(font.label())
                                .family(crate::ui_fonts::preview_family(font))
                                .size(14.0);
                            if ui.selectable_label(font == current, text).clicked()
                                && font != current
                            {
                                actions.push(SettingsAction::UiFont(font));
                            }
                        }
                    }
                });
        },
    );
}

fn general_cards(ui: &mut Ui, input: &SettingsInput<'_>, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    card(ui, "Sessions", |ui| {
        setting_row(
            ui,
            "Open sessions in",
            "Chat view runs sessions on the OmpDeck session host",
            |ui| {
                let mut view = input.session_view;
                egui::ComboBox::from_id_salt("session-view")
                    .selected_text(if view == deck_core::config::SessionView::Chat {
                        "Chat view"
                    } else {
                        "Terminal"
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut view,
                            deck_core::config::SessionView::Terminal,
                            "Terminal",
                        );
                        ui.selectable_value(
                            &mut view,
                            deck_core::config::SessionView::Chat,
                            "Chat view",
                        );
                    });
                if view != input.session_view {
                    actions.push(SettingsAction::SessionView(view));
                }
            },
        );
        if cfg!(windows) {
            setting_row(
                ui,
                "Session backend",
                "Sessions run in the OmpDeck session host and keep running when you close their tab",
                |ui| {
                    ui.label(theme::strong("OmpDeck host").color(pal.text_strong));
                },
            );
            return;
        }
        let b = input.backend;
        let mut missing = Vec::new();
        if !b.tmux {
            missing.push("tmux");
        }
        if !b.host {
            missing.push("omp-deck-host");
        }
        let note = if missing.is_empty() {
            "Where new sessions run. Running sessions keep their backend.".to_owned()
        } else {
            format!(
                "Where new sessions run. Running sessions keep their backend. Not found: {}.",
                missing.join(", ")
            )
        };
        setting_row(ui, "Session backend", &note, |ui| {
            let mut idx = match b.choice {
                BackendChoice::Auto => 0,
                BackendChoice::Tmux => 1,
                BackendChoice::Host => 2,
            };
            if segmented(
                ui,
                &mut idx,
                &[
                    (ICON_SPARKLES, "Auto"),
                    (ICON_TERMINAL, "tmux"),
                    (ICON_CPU, "OmpDeck host"),
                ],
            ) {
                actions.push(SettingsAction::SessionBackend(match idx {
                    0 => BackendChoice::Auto,
                    1 => BackendChoice::Tmux,
                    _ => BackendChoice::Host,
                }));
            }
        });
    });

    card(ui, "Window", |ui| {
        setting_row(
            ui,
            "Native title bar",
            "Use the window manager's frame instead of OmpDeck's own. Applies after restarting OmpDeck.",
            |ui| {
                let mut on = input.native_decorations;
                if toggle(ui, &mut on).changed() {
                    actions.push(SettingsAction::NativeDecorations(on));
                }
            },
        );
    });

    mobile_card(ui, input, actions);
}

/// What the Mobile access card shows: the gateway's own record of its lifetime mode.
pub struct MobileCard<'a> {
    /// `None` until the gateway has reported.
    pub status: Option<&'a GatewayStatus>,
    pub link_up: bool,
    /// A mode change is in flight.
    pub pending: bool,
    /// Why the last change failed.
    pub error: Option<&'a str>,
    /// Relay, link, devices and open pairing (`None` until the gateway reported).
    pub view: Option<&'a MobileView>,
    /// Now, unix milliseconds (relative "last seen" times).
    pub now_ms: u64,
}

fn requested_text(mode: GatewayMode) -> &'static str {
    match mode {
        GatewayMode::Off => "Off",
        GatewayMode::WhileOpen => "While open",
        GatewayMode::Background => "Always on",
    }
}

/// `(headline, detail)`: the headline fits a row, the detail wraps under it.
type StatusText = (String, Option<String>);

fn effective_text(e: &Effective) -> StatusText {
    match e {
        Effective::OnDemand => ("Runs while needed".to_owned(), None),
        Effective::Background => ("Running in the background".to_owned(), None),
        Effective::Maintenance(p) => (
            "Maintenance".to_owned(),
            Some(format!("{p:?}: draining, refusing new work")),
        ),
        Effective::Stopping => (
            "Stopping".to_owned(),
            Some("No window and no work left.".to_owned()),
        ),
    }
}

/// Strips a `Failed("…")` Debug wrapper a message may have been given on the way.
fn plain_message(e: &str) -> &str {
    e.strip_prefix("Failed(\"")
        .and_then(|r| r.strip_suffix("\")"))
        .unwrap_or(e)
}

fn supervision_text(s: &Supervision) -> StatusText {
    match s {
        Supervision::None => ("Not requested".to_owned(), None),
        Supervision::Active { registration } => ("Active".to_owned(), Some(registration.clone())),
        Supervision::Registered { registration } => (
            "Registered, waiting for the supervisor".to_owned(),
            Some(registration.clone()),
        ),
        Supervision::Failed(e) => (
            "Registration failed".to_owned(),
            Some(plain_message(e).to_owned()),
        ),
    }
}

/// One status line: the label keeps its natural width, the headline sits right-aligned in what
/// remains (truncated, full text on hover) and the detail wraps in the card under the row.
fn status_row(ui: &mut Ui, pal: &Palette, title: &str, headline: &str, detail: Option<&str>) {
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(RichText::new(title).color(pal.text_strong)).extend());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new(headline).color(pal.text)).truncate())
                .on_hover_text(headline);
        });
    });
    if let Some(d) = detail {
        ui.add(egui::Label::new(RichText::new(d).small().color(pal.weak)).wrap())
            .on_hover_text(d);
    }
    ui.add_space(4.0);
}

/// The always-on gateway that mobile clients will use. Off by default; turning it on keeps the
/// gateway running in the background.
fn mobile_card(ui: &mut Ui, input: &SettingsInput<'_>, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    let m = &input.mobile;
    card(ui, "Mobile access", |ui| {
        let mut mobile_actions = Vec::new();
        let mode = m.status.map_or(GatewayMode::Off, |s| s.mode);
        let editable = m.link_up && m.status.is_some() && !m.pending;
        let note = if m.pending {
            "Applying the change…"
        } else if !m.link_up {
            "The gateway is not reachable right now."
        } else {
            "Keep the OmpDeck gateway running so sessions stay reachable without this window."
        };
        setting_row(ui, "Allow mobile access", note, |ui| {
            ui.add_enabled_ui(editable, |ui| {
                let mut on = mode != GatewayMode::Off;
                if toggle(ui, &mut on).changed() {
                    actions.push(SettingsAction::GatewayMode(if on {
                        GatewayMode::Background
                    } else {
                        GatewayMode::Off
                    }));
                }
            });
        });
        if mode != GatewayMode::Off {
            setting_row(
                ui,
                "Gateway lifetime",
                "Always on survives logins and crashes; While open stops with the last window.",
                |ui| {
                    ui.add_enabled_ui(editable, |ui| {
                        let mut idx = usize::from(mode == GatewayMode::Background);
                        if segmented(
                            ui,
                            &mut idx,
                            &[(ICON_TERMINAL, "While open"), (ICON_CPU, "Always on")],
                        ) {
                            actions.push(SettingsAction::GatewayMode(if idx == 1 {
                                GatewayMode::Background
                            } else {
                                GatewayMode::WhileOpen
                            }));
                        }
                    });
                },
            );
        }
        if let Some(s) = m.status {
            let jobs = if s.jobs.is_empty() {
                ("No running jobs".to_owned(), None)
            } else {
                (format!("{} running job(s)", s.jobs.len()), None)
            };
            let lines = [
                ("Requested", (requested_text(mode).to_owned(), None)),
                ("Effective", effective_text(&s.effective)),
                ("Supervision", supervision_text(&s.supervision)),
                ("Jobs", jobs),
            ];
            for (title, (headline, detail)) in lines {
                status_row(ui, pal, title, &headline, detail.as_deref());
            }
        }
        if let Some(e) = m.error {
            ui.add(egui::Label::new(RichText::new(e).color(pal.warn)).wrap());
        }
        mobile_view::show(
            ui,
            &MobileInput {
                view: m.view,
                mode_off: mode == GatewayMode::Off,
                gateway_up: m.link_up,
                now_ms: m.now_ms,
            },
            &mut mobile_actions,
        );
        actions.extend(mobile_actions.into_iter().map(SettingsAction::Mobile));
    });
}

fn updates_cards(ui: &mut Ui, input: &SettingsInput<'_>, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    actions.extend(
        crate::app_update::settings_card(ui, &input.app_update)
            .into_iter()
            .map(SettingsAction::AppUpdate),
    );

    card(ui, "Updates", |ui| {
        setting_row(
            ui,
            "Check for omp updates automatically",
            "Runs `omp update --check` at startup and every 6 hours.",
            |ui| {
                let mut on = input.check_updates;
                if toggle(ui, &mut on).changed() {
                    actions.push(SettingsAction::SetCheckUpdates(on));
                }
            },
        );
        ui.add_space(4.0);
        let version = match (input.omp_found, input.omp_version) {
            (false, _) => "omp not found on PATH".to_owned(),
            (true, Some(v)) => format!("omp {v}"),
            (true, None) => "omp".to_owned(),
        };
        setting_row(ui, &version, &status_line(input.update), |ui| {
            let checking = *input.update == UpdateState::Checking;
            if checking {
                ui.add(crate::widgets::Spin::new(18.0, pal.accent_b));
            } else if ui
                .add_enabled(
                    input.omp_found,
                    egui::Button::new((
                        icon_text(ICON_REFRESH_CW, 16.0, pal.glyph),
                        RichText::new("Check now"),
                    ))
                    .min_size(vec2(0.0, 30.0)),
                )
                .clicked()
            {
                actions.push(SettingsAction::CheckUpdates);
            }
        });
    });
}

fn backup_card(
    ui: &mut Ui,
    input: &SettingsInput<'_>,
    server_form: &mut ServerForm,
    actions: &mut Vec<SettingsAction>,
) {
    card(ui, "Backup", |ui| {
        actions.extend(
            backup_view::card(ui, &input.backup, server_form)
                .into_iter()
                .map(SettingsAction::Backup),
        );
    });
}

/// Width and height of one style tile's preview.
const TILE: Vec2 = vec2(148.0, 88.0);

/// Picker of every UI style: a miniature of its window (sidebar, canvas, a card, the accent) plus
/// its name. The active tile carries an accent ring; hover lifts the outline.
fn style_gallery(ui: &mut Ui, current: UiStyle, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    ui.label(theme::strong("Style").color(pal.text_strong));
    ui.label(
        RichText::new("Pick the look of the whole app; every style has its own colours and shapes")
            .color(pal.weak)
            .size(12.0),
    );
    ui.add_space(8.0);
    let gap = 12.0;
    let label_h = 22.0;
    let cols = (((ui.available_width() + gap) / (TILE.x + gap)) as usize).max(1);
    for row in UiStyle::ALL.chunks(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for style in row {
                let (rect, resp) =
                    ui.allocate_exact_size(vec2(TILE.x, TILE.y + label_h), Sense::click());
                let selected = *style == current;
                let preview = Rect::from_min_size(rect.min, TILE);
                paint_style_preview(ui.painter(), preview, *style);
                let ring = if selected {
                    Stroke::new(2.0, pal.accent)
                } else if resp.hovered() {
                    Stroke::new(2.0, theme::hover_stroke(pal))
                } else {
                    Stroke::new(1.0, pal.stroke)
                };
                ui.painter()
                    .rect_stroke(preview, 8, ring, egui::StrokeKind::Outside);
                let label_pos = pos2(preview.left() + 2.0, preview.bottom() + 5.0);
                ui.painter().text(
                    label_pos,
                    Align2::LEFT_TOP,
                    style.label(),
                    FontId::new(12.5, theme::ui_family()),
                    if selected { pal.text_strong } else { pal.text },
                );
                if resp.on_hover_text(style.label()).clicked() && !selected {
                    actions.push(SettingsAction::Style(*style));
                }
            }
        });
        ui.add_space(6.0);
    }
}

/// The miniature of `style`: window, sidebar, canvas, one card and the accent.
fn paint_style_preview(painter: &egui::Painter, rect: Rect, style: UiStyle) {
    use crate::theme::{CardShadow, Region};
    let p = theme::palette(style, true);
    let look = theme::look_of(style);
    painter.rect_filled(rect, 8, p.window);
    let inner = rect.shrink(4.0);
    let side_w = inner.width() * 0.26;
    let (side, canvas, r) = match look.region {
        Region::Islands => {
            let side = Rect::from_min_size(inner.min, vec2(side_w - 2.0, inner.height()));
            let canvas = Rect::from_min_max(pos2(side.right() + 4.0, inner.top()), inner.max);
            (side, canvas, 6)
        }
        _ => {
            let side = Rect::from_min_size(inner.min, vec2(side_w, inner.height()));
            let canvas = Rect::from_min_max(pos2(side.right(), inner.top()), inner.max);
            (side, canvas, 3)
        }
    };
    let rounded = |rect: Rect, radius: u8| {
        painter.rect_filled(rect, radius, p.panel);
    };
    match look.region {
        Region::Islands => {
            rounded(side, r);
            painter.rect_stroke(
                side,
                r,
                Stroke::new(1.0, p.separator),
                egui::StrokeKind::Inside,
            );
            painter.rect_filled(canvas, r, p.bg);
            painter.rect_stroke(
                canvas,
                r,
                Stroke::new(1.0, p.separator),
                egui::StrokeKind::Inside,
            );
        }
        Region::Frame => {
            painter.rect_filled(side, 0, p.window);
            painter.rect_filled(
                canvas,
                egui::CornerRadius {
                    nw: 6,
                    ne: 0,
                    sw: 0,
                    se: 3,
                },
                p.bg,
            );
        }
        _ => {
            painter.rect_filled(
                side,
                egui::CornerRadius {
                    nw: 3,
                    ne: 0,
                    sw: 3,
                    se: 0,
                },
                p.panel,
            );
            painter.rect_filled(
                canvas,
                egui::CornerRadius {
                    nw: 0,
                    ne: 3,
                    sw: 0,
                    se: 3,
                },
                p.bg,
            );
            painter.vline(side.right(), side.y_range(), Stroke::new(1.0, p.separator));
        }
    }
    // Sidebar rows; the first is the selected one.
    for i in 0..3 {
        let row = Rect::from_min_size(
            pos2(side.left() + 5.0, side.top() + 8.0 + i as f32 * 11.0),
            vec2(side.width() - 10.0, 7.0),
        );
        if i == 0 {
            painter.rect_filled(row, 2, p.raised);
            painter.rect_filled(
                Rect::from_min_size(row.min, vec2(2.0, row.height())),
                1,
                p.accent,
            );
        } else {
            painter.rect_filled(row, 2, theme::mix(p.panel, p.text, 0.12));
        }
    }
    // A card with a status line and the accent chip.
    let card = Rect::from_min_size(
        pos2(canvas.left() + 8.0, canvas.top() + 9.0),
        vec2(canvas.width() - 16.0, 36.0),
    );
    let cr = look.card.radius.min(8);
    if look.card.shadow == CardShadow::Hard {
        painter.rect_filled(card.translate(vec2(3.0, 3.0)), cr, egui::Color32::BLACK);
    }
    painter.rect_filled(card, cr, p.surface);
    painter.rect_stroke(
        card,
        cr,
        Stroke::new(look.card.border_w.clamp(1.0, 2.0), p.stroke),
        egui::StrokeKind::Inside,
    );
    painter.rect_filled(
        Rect::from_min_size(card.min + vec2(6.0, 7.0), vec2(card.width() * 0.5, 4.0)),
        2,
        p.text_strong,
    );
    painter.rect_filled(
        Rect::from_min_size(card.min + vec2(6.0, 15.0), vec2(card.width() * 0.7, 3.0)),
        1,
        p.weak,
    );
    painter.rect_filled(
        Rect::from_min_size(card.min + vec2(6.0, 24.0), vec2(card.width() * 0.3, 5.0)),
        2,
        p.accent,
    );
    painter.circle_filled(card.right_top() + vec2(-8.0, 8.0), 2.5, p.ok);
    // Second, dimmer card.
    let card2 = Rect::from_min_size(
        card.left_bottom() + vec2(0.0, 6.0),
        vec2(card.width(), 14.0),
    );
    if card2.bottom() < canvas.bottom() - 2.0 {
        painter.rect_filled(card2, cr.min(4), p.surface);
        painter.rect_stroke(
            card2,
            cr.min(4),
            Stroke::new(1.0, p.stroke),
            egui::StrokeKind::Inside,
        );
    }
}

#[cfg(test)]
mod tests {
    use deck_core::omp::SettingsTab;

    use super::*;

    fn catalog() -> SettingsCatalog {
        let tab = |id: &str| SettingsTab {
            id: id.into(),
            label: id.into(),
            settings: Vec::new(),
        };
        SettingsCatalog {
            tabs: vec![tab("appearance"), tab("model"), tab("shell")],
        }
    }

    fn selected(state: &SettingsState) -> Entry {
        state.selected
    }

    #[test]
    fn an_omp_page_asked_for_before_the_catalog_loads_shows_when_it_arrives() {
        let mut state = SettingsState::default();
        state.open_omp_page(Some("shell"));
        assert_eq!(selected(&state), Entry::Omp(0));
        state.catalog_loaded(Ok(catalog()), "~/.omp/agent".into());
        assert_eq!(selected(&state), Entry::Omp(2));
        // Consumed: a reload does not move the page again.
        state.show_omp_page("model");
        state.catalog_loaded(Ok(catalog()), "~/.omp/agent".into());
        assert_eq!(selected(&state), Entry::Omp(1));
    }

    #[test]
    fn an_omp_page_the_catalog_lacks_or_a_failed_load_leaves_the_first_page() {
        let mut state = SettingsState::default();
        state.open_omp_page(Some("bogus"));
        state.catalog_loaded(Ok(catalog()), String::new());
        assert_eq!(selected(&state), Entry::Omp(0));

        let mut state = SettingsState::default();
        state.open_omp_page(Some("shell"));
        state.catalog_loaded(Err("no omp".into()), String::new());
        state.catalog_loaded(Ok(catalog()), String::new());
        assert_eq!(selected(&state), Entry::Omp(0));
    }

    #[test]
    fn a_loaded_catalog_shows_the_page_at_once_and_none_means_the_first() {
        let mut state = SettingsState::default();
        state.catalog_loaded(Ok(catalog()), String::new());
        state.open_omp_page(Some("model"));
        assert_eq!(selected(&state), Entry::Omp(1));
        state.open_omp_page(None);
        assert_eq!(selected(&state), Entry::Omp(0));
    }
    #[test]
    fn omps_installed_page_lands_on_plugins_even_before_the_catalog_loads() {
        let mut state = SettingsState::default();
        state.open_omp_page(Some("plugins"));
        assert_eq!(selected(&state), Entry::Plugins);
        // Nothing waits for the catalog: a later load leaves the page where it is.
        state.catalog_loaded(Ok(catalog()), String::new());
        assert_eq!(selected(&state), Entry::Plugins);
        state.open_omp_page(Some("advisor"));
        assert_eq!(selected(&state), Entry::Advisor);
        assert!(state.show_omp_page("installed"));
        assert_eq!(selected(&state), Entry::Plugins);
        let pages = state.omp_pages();
        assert!(pages.iter().any(|(id, _)| id == "plugins"));
        assert!(pages.iter().any(|(id, _)| id == "advisor"));
    }

    #[test]
    fn the_project_file_is_followed_per_project_and_a_stale_reply_is_dropped() {
        let mut state = SettingsState::default();
        let a = Path::new("/proj/a");
        let b = Path::new("/proj/b");
        assert_eq!(state.layer_request(Some(a)), Some(a.to_path_buf()));
        // Asked once; the same project does not read the file again.
        assert_eq!(state.layer_request(Some(a)), None);
        assert_eq!(state.layer_request(Some(b)), Some(b.to_path_buf()));
        state.project_layer(a, Some(serde_json::json!({"x": 1})));
        assert!(state.omp.layer.is_none(), "the reply for a was dropped");
        state.project_layer(b, Some(serde_json::json!({"x": 1})));
        assert!(state.omp.layer.is_some());
        // No project: nothing to read, and the marks go.
        assert_eq!(state.layer_request(None), None);
        assert!(state.omp.layer.is_none());
        // Reopening the window reads it again.
        state.opened();
        assert_eq!(state.layer_request(Some(b)), Some(b.to_path_buf()));
    }
}
