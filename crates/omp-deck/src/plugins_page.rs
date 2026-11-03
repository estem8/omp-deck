//! Settings → Plugins: omp's "Installed" page (`/settings` → Plugins). Lists the npm / linked and
//! marketplace plugins `omp plugin list --json` reports, with each one's enabled switch (and a
//! User / Project scope choice for marketplace plugins), the features checklist and the settings
//! its manifest declares (string, number, boolean and enum editors; a secret shows set / not set
//! only and is written but never displayed).
//!
//! Pure UI: state in, [`PluginsAction`]s out; `app/plugins.rs` runs them over the gateway, which
//! also asks the active omp session to `/reload-plugins` once it is idle.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use deck_core::omp::{
    MarketplacePlugin, NpmPlugin, PluginConfig, PluginFeatures, PluginList, PluginScope,
    PluginSetting, PluginSettingKind,
};
use egui::{CursorIcon, Id, Key, Margin, RichText, Sense, Stroke, TextEdit, Ui, vec2};
use serde_json::Value;

use crate::badge::{self, Hue};
use crate::card;
use crate::confirm::{Dialog, show_dialog};
use crate::icons::{
    ICON_CHEVRON_DOWN, ICON_CHEVRON_RIGHT, ICON_CIRCLE_ALERT, ICON_KEY_ROUND, ICON_LOCK,
    ICON_PACKAGE, ICON_PUZZLE, ICON_REFRESH_CW, ICON_ROTATE_CCW, ICON_TRASH_2,
};
use crate::omp_rows::{note_banner, skeleton};
use crate::omp_settings::{parse_number, preview, value_text};
use crate::summary::summarize;
use crate::theme::{self, Palette};
use crate::widgets::{
    DraftButton, Spin, card_frame, empty_state, icon_button, icon_text, segmented, toggle,
};

/// A value on its way to omp that must never reach a log line: `Debug` hides it.
#[derive(PartialEq)]
pub struct Sent(pub Option<Value>);

impl std::fmt::Debug for Sent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Sent(..)"
        } else {
            "Sent(unset)"
        })
    }
}

#[derive(Debug, PartialEq)]
pub enum PluginsAction {
    /// (Re)read `omp plugin list`.
    Load,
    /// `omp plugin enable|disable <name> --scope <scope>`; `scope` only matters for marketplace
    /// plugins.
    Enable {
        name: String,
        enabled: bool,
        scope: PluginScope,
    },
    /// The enabled features of `pkg` become exactly `features`.
    Features { pkg: String, features: Vec<String> },
    /// Read the stored settings of `pkg` (with its manifest schema).
    LoadConfig { pkg: String },
    /// Set (`Some`) or delete (`None`) one setting of `pkg`.
    SetConfig {
        pkg: String,
        key: String,
        value: Sent,
    },
}

/// What the page needs to know about the app around it.
pub struct PluginsInput<'a> {
    /// The project of the active tab: where a Project-scoped change is recorded.
    pub project: Option<&'a Path>,
    /// An omp session is open to reload its plugins when idle.
    pub reload_tab: bool,
}

// ------------------------------------------------------------------ pure model

/// The editor a manifest setting gets.
#[derive(Debug, PartialEq)]
pub enum Editor<'a> {
    Text,
    Number {
        min: Option<f64>,
        max: Option<f64>,
    },
    Toggle,
    Choice(&'a [String]),
    /// Write-only: set or clear, never shown.
    Secret,
    /// A type this OmpDeck does not know.
    ReadOnly,
}

pub fn editor_of(setting: &PluginSetting) -> Editor<'_> {
    if setting.secret {
        return Editor::Secret;
    }
    match &setting.kind {
        PluginSettingKind::String { .. } => Editor::Text,
        PluginSettingKind::Number { min, max, .. } => Editor::Number {
            min: *min,
            max: *max,
        },
        PluginSettingKind::Boolean { .. } => Editor::Toggle,
        PluginSettingKind::Enum { values, .. } => Editor::Choice(values),
        PluginSettingKind::Unknown => Editor::ReadOnly,
    }
}

/// What an editor displays for the stored value of a setting.
#[derive(Debug, PartialEq)]
pub enum Shown {
    Text(String),
    Number(String),
    Bool(bool),
    Choice(Option<String>),
    /// Only whether a value is stored.
    Secret {
        set: bool,
    },
    Raw(String),
}

/// The manifest default of a setting as a JSON value.
fn default_value(setting: &PluginSetting) -> Option<Value> {
    match &setting.kind {
        PluginSettingKind::String { default } => default.clone().map(Value::String),
        PluginSettingKind::Number { default, .. } => default
            .and_then(|d| parse_number(&d.to_string()).ok())
            .or_else(|| default.and_then(|d| serde_json::Number::from_f64(d).map(Value::Number))),
        PluginSettingKind::Boolean { default } => default.map(Value::Bool),
        PluginSettingKind::Enum { default, .. } => default.clone().map(Value::String),
        PluginSettingKind::Unknown => None,
    }
}

/// The value an editor shows: the stored one, else the manifest default. A secret's content is
/// never read: only whether something is stored.
pub fn shown(setting: &PluginSetting, stored: Option<&Value>) -> Shown {
    let stored = stored.filter(|v| !v.is_null());
    match editor_of(setting) {
        Editor::Secret => Shown::Secret {
            set: stored.is_some(),
        },
        Editor::Text => Shown::Text(value_text(
            stored.cloned().or_else(|| default_value(setting)).as_ref(),
        )),
        Editor::Number { .. } => Shown::Number(value_text(
            stored.cloned().or_else(|| default_value(setting)).as_ref(),
        )),
        Editor::Toggle => Shown::Bool(
            stored
                .cloned()
                .or_else(|| default_value(setting))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        ),
        Editor::Choice(_) => Shown::Choice(
            stored
                .cloned()
                .or_else(|| default_value(setting))
                .and_then(|v| v.as_str().map(str::to_owned)),
        ),
        Editor::ReadOnly => Shown::Raw(preview(stored, 40)),
    }
}

/// Text field → what to write: empty restores the default (`None` deletes the setting).
pub fn commit_text(text: &str) -> Option<Value> {
    (!text.is_empty()).then(|| Value::String(text.to_owned()))
}

/// Number field → what to write (empty deletes), checked against the manifest's bounds.
pub fn commit_number(
    min: Option<f64>,
    max: Option<f64>,
    text: &str,
) -> Result<Option<Value>, String> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let value = parse_number(text)?;
    let n = value.as_f64().unwrap_or(f64::NAN);
    if let Some(min) = min
        && n < min
    {
        return Err(format!("At least {min}"));
    }
    if let Some(max) = max
        && n > max
    {
        return Err(format!("At most {max}"));
    }
    Ok(Some(value))
}

/// A secret typed into the dialog → what to write: numbers as numbers, everything else text.
pub fn secret_value(setting: &PluginSetting, text: &str) -> Result<Value, String> {
    match setting.kind {
        PluginSettingKind::Number { .. } => parse_number(text),
        _ => Ok(Value::String(text.to_owned())),
    }
}

/// The features a plugin has on: the explicit selection, else the manifest's defaults.
pub fn resolve_features(plugin: &NpmPlugin, selection: Option<&[String]>) -> Vec<String> {
    match selection {
        Some(on) => on.to_vec(),
        None => plugin
            .manifest
            .features
            .iter()
            .filter(|(_, f)| f.default)
            .map(|(name, _)| name.clone())
            .collect(),
    }
}

/// `current` with `feature` switched, in the manifest's order (omp's own order is kept for what
/// the manifest no longer declares: those are dropped).
pub fn toggled_features(
    plugin: &NpmPlugin,
    current: &[String],
    feature: &str,
    on: bool,
) -> Vec<String> {
    plugin
        .manifest
        .features
        .iter()
        .map(|(name, _)| name)
        .filter(|name| {
            if name.as_str() == feature {
                on
            } else {
                current.contains(name)
            }
        })
        .cloned()
        .collect()
}

/// Where a marketplace row's switch writes by default: the scope omp listed it under.
fn scope_default(plugin: &MarketplacePlugin) -> PluginScope {
    plugin.scope.unwrap_or(PluginScope::User)
}

// ------------------------------------------------------------------ state

/// One setting field's editing state.
#[derive(Default)]
struct Field {
    /// Text being edited; `None` = show the stored value.
    buf: Option<String>,
    error: Option<String>,
    pending: bool,
}

/// The expanded part of an npm plugin: its stored settings.
#[derive(Default)]
struct Detail {
    config: Option<Result<PluginConfig, String>>,
    loading: bool,
    /// A change landed: read the settings again.
    stale: bool,
    fields: HashMap<String, Field>,
}

enum Modal {
    /// Type a secret (shown as dots; the text lives only here until written or cancelled).
    Secret {
        pkg: String,
        key: String,
        text: String,
        error: Option<String>,
        focus: bool,
    },
    ClearSecret {
        pkg: String,
        key: String,
    },
}

#[derive(Default)]
pub struct PluginsState {
    list: Option<PluginList>,
    loading: bool,
    load_error: Option<String>,
    want_load: bool,
    open: HashSet<String>,
    scopes: HashMap<String, PluginScope>,
    /// Plugins with an enable / features change in flight.
    busy: HashSet<String>,
    errors: HashMap<String, String>,
    /// Feature selections omp reported after a change (until the list is read again).
    features: HashMap<String, Option<Vec<String>>>,
    details: HashMap<String, Detail>,
    modal: Option<Modal>,
}

impl PluginsState {
    /// The page is shown (again): read the list anew.
    pub fn opened(&mut self) {
        self.want_load = true;
    }

    /// `omp plugin list` answered.
    pub fn listed(&mut self, result: Result<PluginList, String>) {
        self.loading = false;
        match result {
            Ok(list) => {
                self.list = Some(list);
                self.load_error = None;
                self.features.clear();
            }
            Err(e) => self.load_error = Some(e),
        }
    }

    /// An enable / disable finished; the list is read again either way.
    pub fn enabled(&mut self, name: &str, result: Result<(), String>) {
        self.busy.remove(name);
        match result {
            Ok(()) => {
                self.errors.remove(name);
            }
            Err(e) => {
                self.errors.insert(name.to_owned(), e);
            }
        }
        self.want_load = true;
    }

    /// A features change finished: `Ok` carries the selection omp now has.
    pub fn features_done(&mut self, pkg: &str, result: Result<PluginFeatures, String>) {
        self.busy.remove(pkg);
        match result {
            Ok(state) => {
                self.errors.remove(pkg);
                self.features.insert(pkg.to_owned(), state.enabled_features);
            }
            Err(e) => {
                self.errors.insert(pkg.to_owned(), e);
            }
        }
    }

    pub fn config_loaded(&mut self, pkg: &str, result: Result<PluginConfig, String>) {
        let d = self.details.entry(pkg.to_owned()).or_default();
        d.loading = false;
        d.stale = false;
        d.config = Some(result);
    }

    /// One setting write finished.
    pub fn config_set_done(&mut self, pkg: &str, key: &str, result: Result<(), String>) {
        let d = self.details.entry(pkg.to_owned()).or_default();
        let f = d.fields.entry(key.to_owned()).or_default();
        f.pending = false;
        match result {
            Ok(()) => {
                f.buf = None;
                f.error = None;
                d.stale = true;
            }
            Err(e) => f.error = Some(e),
        }
    }
}

// ------------------------------------------------------------------ page

pub fn show(ui: &mut Ui, state: &mut PluginsState, input: &PluginsInput<'_>) -> Vec<PluginsAction> {
    let pal = Palette::of_ui(ui);
    let mut actions = Vec::new();
    if state.want_load || (state.list.is_none() && !state.loading && state.load_error.is_none()) {
        state.want_load = false;
        state.loading = true;
        actions.push(PluginsAction::Load);
    }

    egui::Sides::new().height(40.0).show(
        ui,
        |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::strong("Plugins").size(22.0).color(pal.text_strong));
                if let Some(l) = &state.list {
                    ui.label(
                        RichText::new(format!("{} installed", l.npm.len() + l.marketplace.len()))
                            .color(pal.weak),
                    );
                }
            });
        },
        |ui| {
            if state.loading {
                ui.add_space(8.0);
                ui.add(Spin::new(18.0, pal.accent_b));
            } else if icon_button(ui, ICON_REFRESH_CW, "Reread the installed plugins", false)
                .clicked()
            {
                state.want_load = true;
            }
        },
    );
    ui.add_space(8.0);
    note_banner(
        ui,
        if input.reload_tab {
            "Changes go through `omp plugin …`; the open session reloads its plugins as soon as it is idle."
        } else {
            "Changes go through `omp plugin …`; sessions pick them up when they reload plugins or start."
        },
    );

    let PluginsState {
        list,
        loading,
        load_error,
        open,
        scopes,
        busy,
        errors,
        features,
        details,
        modal,
        want_load,
        ..
    } = state;
    let Some(list) = list.as_ref() else {
        match load_error.as_deref() {
            Some(e) => {
                error_card(ui, "Could not read the plugins", e, || *want_load = true);
            }
            None => skeleton(ui),
        }
        show_modal(ui.ctx(), modal, details, &mut actions);
        return actions;
    };
    if let Some(e) = load_error.as_deref()
        && !*loading
    {
        error_card(ui, "Could not reread the plugins", e, || *want_load = true);
    }
    if list.npm.is_empty() && list.marketplace.is_empty() {
        empty_state(
            ui,
            ICON_PUZZLE,
            pal.weak,
            "No plugins installed",
            "Install one in a terminal with `omp plugin install <package>`, then reread this page.",
        );
    }
    let mut cx = Cx {
        input,
        open,
        scopes,
        busy,
        errors,
        features,
        details,
        modal,
        actions: &mut actions,
    };
    if !list.npm.is_empty() {
        ui.label(theme::section_caption("npm and linked", pal.weak));
        ui.add_space(6.0);
        for p in &list.npm {
            npm_card(ui, p, &mut cx);
        }
    }
    if !list.marketplace.is_empty() {
        ui.label(theme::section_caption("Marketplace", pal.weak));
        ui.add_space(6.0);
        for (i, p) in list.marketplace.iter().enumerate() {
            marketplace_card(ui, p, i, &mut cx);
        }
    }
    show_modal(ui.ctx(), modal, details, &mut actions);
    actions
}

/// Everything a plugin card reads and changes.
struct Cx<'a> {
    input: &'a PluginsInput<'a>,
    open: &'a mut HashSet<String>,
    scopes: &'a mut HashMap<String, PluginScope>,
    busy: &'a mut HashSet<String>,
    errors: &'a mut HashMap<String, String>,
    features: &'a HashMap<String, Option<Vec<String>>>,
    details: &'a mut HashMap<String, Detail>,
    modal: &'a mut Option<Modal>,
    actions: &'a mut Vec<PluginsAction>,
}

fn error_card(ui: &mut Ui, title: &str, error: &str, mut retry: impl FnMut()) {
    let pal = Palette::of_ui(ui);
    let (line, _) = summarize(error);
    card_frame(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            ui.label(icon_text(ICON_CIRCLE_ALERT, 22.0, pal.danger));
            ui.vertical(|ui| {
                ui.label(theme::strong(title).color(pal.text_strong));
                ui.add(
                    egui::Label::new(RichText::new(line).small().color(pal.danger))
                        .wrap_mode(egui::TextWrapMode::Wrap),
                );
            });
        });
        ui.add_space(6.0);
        if DraftButton::new("Retry")
            .icon(ICON_REFRESH_CW)
            .show(ui)
            .clicked()
        {
            retry();
        }
    });
    ui.add_space(12.0);
}

/// The card chrome of one plugin: icon, name, badges, description and path; `right` (the
/// switch) and a spinner while a change is in flight sit at the right.
#[allow(clippy::too_many_arguments)]
fn plugin_head(
    ui: &mut Ui,
    name: &str,
    version: Option<&str>,
    badges: &[(&str, Hue)],
    description: Option<&str>,
    path: Option<&Path>,
    spinner: bool,
    right: impl FnOnce(&mut Ui),
) {
    let pal = Palette::of_ui(ui);
    egui::Sides::new().shrink_left().spacing(14.0).show(
        ui,
        |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    ui.label(icon_text(ICON_PACKAGE, 17.0, pal.glyph));
                    ui.label(theme::strong(name).size(15.0).color(pal.text_strong));
                    if let Some(v) = version.filter(|v| !v.is_empty()) {
                        badge::show(ui, None, &format!("v{v}"), Hue::Blue, 11.0);
                    }
                    for (text, hue) in badges {
                        badge::show(ui, None, text, *hue, 11.0);
                    }
                });
                if let Some(d) = description.filter(|d| !d.is_empty()) {
                    ui.add(
                        egui::Label::new(RichText::new(d).color(pal.text))
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                }
                if let Some(p) = path {
                    ui.add(
                        egui::Label::new(
                            RichText::new(p.display().to_string())
                                .monospace()
                                .small()
                                .color(pal.weak),
                        )
                        .truncate(),
                    );
                }
            });
        },
        |ui| {
            right(ui);
            if spinner {
                ui.add(Spin::new(16.0, pal.accent_b));
            }
        },
    );
}

fn inline_error(ui: &mut Ui, errors: &HashMap<String, String>, name: &str) {
    if let Some(e) = errors.get(name) {
        let pal = Palette::of_ui(ui);
        let (line, _) = summarize(e);
        ui.add_space(4.0);
        ui.add(
            egui::Label::new(RichText::new(line).small().color(pal.danger))
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    }
}

fn npm_card(ui: &mut Ui, p: &NpmPlugin, cx: &mut Cx<'_>) {
    let pal = Palette::of_ui(ui);
    let name = p.name.as_str();
    let busy = cx.busy.contains(name);
    let expandable = !p.manifest.features.is_empty() || !p.manifest.settings.is_empty();
    let is_open = expandable && cx.open.contains(name);
    let badges: &[(&str, Hue)] = if p.enabled {
        &[]
    } else {
        &[("disabled", Hue::Amber)]
    };
    card_frame(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        plugin_head(
            ui,
            name,
            Some(&p.version),
            badges,
            p.manifest.description.as_deref(),
            p.path.as_deref(),
            busy,
            |ui| {
                let mut on = p.enabled;
                if ui
                    .add_enabled_ui(!busy, |ui| toggle(ui, &mut on))
                    .inner
                    .changed()
                {
                    cx.busy.insert(name.to_owned());
                    cx.actions.push(PluginsAction::Enable {
                        name: name.to_owned(),
                        enabled: on,
                        scope: PluginScope::User,
                    });
                }
            },
        );
        inline_error(ui, cx.errors, name);
        if !expandable {
            return;
        }
        ui.add_space(6.0);
        let label = format!(
            "{} feature{} · {} setting{}",
            p.manifest.features.len(),
            plural(p.manifest.features.len()),
            p.manifest.settings.len(),
            plural(p.manifest.settings.len()),
        );
        let icon = if is_open {
            ICON_CHEVRON_DOWN
        } else {
            ICON_CHEVRON_RIGHT
        };
        if ui
            .add(
                egui::Button::new((icon_text(icon, 14.0, pal.glyph), RichText::new(label)))
                    .corner_radius(theme::rad(6, 10)),
            )
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
        {
            if is_open {
                cx.open.remove(name);
            } else {
                cx.open.insert(name.to_owned());
            }
        }
        if is_open {
            ui.add_space(8.0);
            if !p.manifest.features.is_empty() {
                features_section(ui, p, busy, cx);
            }
            if !p.manifest.settings.is_empty() {
                settings_section(ui, p, cx);
            }
        }
    });
    ui.add_space(10.0);
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn features_section(ui: &mut Ui, p: &NpmPlugin, busy: bool, cx: &mut Cx<'_>) {
    let pal = Palette::of_ui(ui);
    card::section(ui, "Features");
    ui.add_space(4.0);
    let selection = match cx.features.get(&p.name) {
        Some(over) => over.as_deref(),
        None => p.enabled_features.as_deref(),
    };
    let current = resolve_features(p, selection);
    ui.add_enabled_ui(!busy, |ui| {
        for (feature, meta) in &p.manifest.features {
            let mut on = current.contains(feature);
            let resp = ui.checkbox(&mut on, RichText::new(feature).color(pal.text_strong));
            if let Some(d) = meta.description.as_deref().filter(|d| !d.is_empty()) {
                ui.indent(("plugin-feature", &p.name, feature), |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(d).small().color(pal.weak))
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                });
            }
            if resp.changed() {
                cx.busy.insert(p.name.clone());
                cx.actions.push(PluginsAction::Features {
                    pkg: p.name.clone(),
                    features: toggled_features(p, &current, feature, on),
                });
            }
        }
    });
    ui.add_space(8.0);
}

fn settings_section(ui: &mut Ui, p: &NpmPlugin, cx: &mut Cx<'_>) {
    let pal = Palette::of_ui(ui);
    card::section(ui, "Settings");
    ui.add_space(4.0);
    let detail = cx.details.entry(p.name.clone()).or_default();
    if (detail.config.is_none() && !detail.loading) || (detail.stale && !detail.loading) {
        detail.loading = true;
        cx.actions.push(PluginsAction::LoadConfig {
            pkg: p.name.clone(),
        });
    }
    let Detail {
        config,
        loading,
        fields,
        ..
    } = detail;
    match config {
        None => {
            ui.add(Spin::new(16.0, pal.accent_b));
        }
        Some(Err(e)) => {
            let (line, _) = summarize(e);
            ui.add(
                egui::Label::new(RichText::new(line).small().color(pal.danger))
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );
        }
        Some(Ok(config)) => {
            if *loading {
                ui.add(Spin::new(14.0, pal.accent_b));
            }
            // The stored schema wins over the list's manifest copy (same data, read just now).
            let mut first = true;
            for (key, setting) in &config.schema {
                if !first {
                    ui.add_space(6.0);
                    let y = ui.cursor().top();
                    ui.painter().hline(
                        ui.max_rect().x_range(),
                        y,
                        Stroke::new(1.0, pal.stroke.gamma_multiply(0.55)),
                    );
                    ui.add_space(7.0);
                }
                first = false;
                let field = fields.entry(key.clone()).or_default();
                setting_row(
                    ui,
                    &p.name,
                    key,
                    setting,
                    config.settings.get(key),
                    field,
                    cx.modal,
                    cx.actions,
                );
            }
            if config.schema.is_empty() {
                ui.label(RichText::new("The manifest declares no settings.").color(pal.weak));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn setting_row(
    ui: &mut Ui,
    pkg: &str,
    key: &str,
    setting: &PluginSetting,
    stored: Option<&Value>,
    field: &mut Field,
    modal: &mut Option<Modal>,
    actions: &mut Vec<PluginsAction>,
) {
    let pal = Palette::of_ui(ui);
    let editor = editor_of(setting);
    let shown = shown(setting, stored);
    let has_value = stored.is_some_and(|v| !v.is_null());
    egui::Sides::new().shrink_left().spacing(16.0).show(
        ui,
        |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 7.0;
                    ui.label(RichText::new(key).monospace().color(pal.text_strong));
                    if has_value {
                        let (r, resp) = ui.allocate_exact_size(vec2(9.0, 9.0), Sense::hover());
                        theme::glow_dot(ui.painter(), r.center(), 3.2, pal.accent_a, 0.4);
                        resp.on_hover_text("Set (the manifest default no longer applies)");
                    }
                    if let Some(env) = &setting.env {
                        badge::show(ui, None, &format!("${env}"), Hue::Teal, 10.5)
                            .on_hover_text("omp falls back to this environment variable");
                    }
                    if setting.secret {
                        badge::show(ui, Some(ICON_LOCK), "secret", Hue::Amber, 10.5);
                    }
                });
                if let Some(d) = setting.description.as_deref().filter(|d| !d.is_empty()) {
                    ui.add(
                        egui::Label::new(RichText::new(d).small().color(pal.weak))
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                }
            });
        },
        |ui| {
            ui.add_enabled_ui(!field.pending, |ui| {
                // Right-to-left: the reset slot first so editors line up.
                let reset = if setting.secret {
                    (ICON_TRASH_2, "Clear the stored secret")
                } else {
                    (ICON_ROTATE_CCW, "Back to the manifest default")
                };
                if has_value {
                    if icon_button(ui, reset.0, reset.1, false).clicked() {
                        if setting.secret {
                            *modal = Some(Modal::ClearSecret {
                                pkg: pkg.to_owned(),
                                key: key.to_owned(),
                            });
                        } else {
                            send(field, actions, pkg, key, None);
                        }
                    }
                } else {
                    ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
                }
                match (&editor, &shown) {
                    (Editor::Toggle, Shown::Bool(on)) => {
                        let mut v = *on;
                        if toggle(ui, &mut v).changed() {
                            send(field, actions, pkg, key, Some(Value::Bool(v)));
                        }
                    }
                    (Editor::Choice(values), Shown::Choice(current)) => {
                        let text = current.as_deref().unwrap_or("—");
                        egui::ComboBox::from_id_salt(("plugin-enum", pkg, key))
                            .selected_text(text)
                            .width(190.0)
                            .show_ui(ui, |ui| {
                                for v in *values {
                                    let sel = current.as_deref() == Some(v.as_str());
                                    if ui.selectable_label(sel, v).clicked() && !sel {
                                        send(
                                            field,
                                            actions,
                                            pkg,
                                            key,
                                            Some(Value::String(v.clone())),
                                        );
                                    }
                                }
                            });
                    }
                    (Editor::Text, Shown::Text(text)) => {
                        text_field(ui, pkg, key, text, None, field, actions);
                    }
                    (Editor::Number { min, max }, Shown::Number(text)) => {
                        text_field(ui, pkg, key, text, Some((*min, *max)), field, actions);
                    }
                    (Editor::Secret, Shown::Secret { set }) => {
                        let (text, color) = if *set {
                            ("•••••• set", pal.ok)
                        } else {
                            ("not set", pal.weak)
                        };
                        ui.label(RichText::new(text).monospace().small().color(color));
                        if ui
                            .add(
                                egui::Button::new((
                                    icon_text(ICON_KEY_ROUND, 15.0, pal.glyph),
                                    RichText::new("Set…"),
                                ))
                                .min_size(vec2(0.0, 28.0)),
                            )
                            .clicked()
                        {
                            *modal = Some(Modal::Secret {
                                pkg: pkg.to_owned(),
                                key: key.to_owned(),
                                text: String::new(),
                                error: None,
                                focus: true,
                            });
                        }
                    }
                    (_, Shown::Raw(text)) => {
                        ui.label(RichText::new(text).monospace().small().color(pal.weak))
                            .on_hover_text("This setting type is not editable here");
                    }
                    _ => {}
                }
                if field.pending {
                    ui.add(Spin::new(16.0, pal.accent_b));
                }
            });
        },
    );
    if let Some(e) = &field.error {
        let (line, _) = summarize(e);
        ui.add_space(3.0);
        ui.add(
            egui::Label::new(RichText::new(line).small().color(pal.danger))
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    }
}

fn send(
    field: &mut Field,
    actions: &mut Vec<PluginsAction>,
    pkg: &str,
    key: &str,
    value: Option<Value>,
) {
    field.pending = true;
    field.error = None;
    actions.push(PluginsAction::SetConfig {
        pkg: pkg.to_owned(),
        key: key.to_owned(),
        value: Sent(value),
    });
}

/// A string or number field: written when it loses focus with a changed, valid text.
fn text_field(
    ui: &mut Ui,
    pkg: &str,
    key: &str,
    stored: &str,
    number: Option<(Option<f64>, Option<f64>)>,
    field: &mut Field,
    actions: &mut Vec<PluginsAction>,
) {
    let pal = Palette::of_ui(ui);
    let mut text = field.buf.clone().unwrap_or_else(|| stored.to_owned());
    let invalid = field.error.is_some();
    let resp = ui
        .scope(|ui| {
            if invalid {
                ui.visuals_mut().widgets.inactive.bg_stroke = Stroke::new(1.0, pal.danger);
                ui.visuals_mut().widgets.hovered.bg_stroke = Stroke::new(1.0, pal.danger);
            }
            ui.add(
                TextEdit::singleline(&mut text)
                    .id(Id::unique(("plugin-field", pkg, key)))
                    .desired_width(if number.is_some() { 120.0 } else { 230.0 })
                    .margin(Margin::symmetric(8, 5))
                    .align(if number.is_some() {
                        egui::Align2::RIGHT_CENTER
                    } else {
                        egui::Align2::LEFT_CENTER
                    }),
            )
        })
        .inner;
    if resp.changed() {
        field.error = number.and_then(|(min, max)| commit_number(min, max, &text).err());
        field.buf = Some(text.clone());
    }
    if field.pending || !resp.lost_focus() {
        return;
    }
    if ui.input(|i| i.key_pressed(Key::Escape)) || text == stored {
        field.buf = None;
        field.error = None;
        return;
    }
    let parsed = match number {
        Some((min, max)) => commit_number(min, max, &text),
        None => Ok(commit_text(&text)),
    };
    match parsed {
        Ok(value) => send(field, actions, pkg, key, value),
        Err(e) => field.error = Some(e),
    }
}

fn marketplace_card(ui: &mut Ui, p: &MarketplacePlugin, index: usize, cx: &mut Cx<'_>) {
    let pal = Palette::of_ui(ui);
    let name = p.id.as_str();
    let busy = cx.busy.contains(name);
    let version = p.entries.first().and_then(|e| e.version.clone());
    let path = p.entries.first().and_then(|e| e.install_path.clone());
    let mut badges: Vec<(String, Hue)> = Vec::new();
    if let Some(s) = p.scope {
        badges.push((s.as_str().to_owned(), Hue::Violet));
    }
    if let Some(s) = &p.shadowed_by {
        badges.push((format!("shadowed by {s}"), Hue::Amber));
    }
    if !p.enabled() {
        badges.push(("disabled".to_owned(), Hue::Amber));
    }
    let badge_refs: Vec<(&str, Hue)> = badges.iter().map(|(t, h)| (t.as_str(), *h)).collect();
    // A project copy and a user copy of one plugin are two rows: the choice is per row.
    let scope_key = format!("{name}#{index}");
    let scope = *cx
        .scopes
        .entry(scope_key.clone())
        .or_insert_with(|| scope_default(p));
    card_frame(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        plugin_head(
            ui,
            name,
            version.as_deref(),
            &badge_refs,
            None,
            path.as_deref(),
            busy,
            |ui| {
                let mut on = p.enabled();
                if ui
                    .add_enabled_ui(!busy, |ui| toggle(ui, &mut on))
                    .inner
                    .changed()
                {
                    cx.busy.insert(name.to_owned());
                    cx.actions.push(PluginsAction::Enable {
                        name: name.to_owned(),
                        enabled: on,
                        scope,
                    });
                }
            },
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            card::kv(ui, 92.0, "Apply to", |ui| {
                let mut sel = usize::from(scope == PluginScope::Project);
                let project_ok = cx.input.project.is_some();
                if project_ok {
                    if segmented(
                        ui,
                        &mut sel,
                        &[
                            (crate::icons::ICON_USER, "User"),
                            (crate::icons::ICON_FOLDER, "Project"),
                        ],
                    ) {
                        let next = if sel == 1 {
                            PluginScope::Project
                        } else {
                            PluginScope::User
                        };
                        cx.scopes.insert(scope_key.clone(), next);
                    }
                    if let Some(dir) = cx.input.project {
                        let folder = dir.file_name().map_or_else(
                            || dir.display().to_string(),
                            |n| n.to_string_lossy().into_owned(),
                        );
                        ui.label(
                            RichText::new(format!("Project: {folder}"))
                                .small()
                                .color(pal.weak),
                        )
                        .on_hover_text(dir.display().to_string());
                    }
                } else {
                    let mut user = 0usize;
                    segmented(ui, &mut user, &[(crate::icons::ICON_USER, "User")]);
                    ui.label(
                        RichText::new("Open a session tab to choose a project")
                            .small()
                            .color(pal.weak),
                    );
                    cx.scopes.insert(scope_key.clone(), PluginScope::User);
                }
            });
        });
        inline_error(ui, cx.errors, name);
    });
    ui.add_space(10.0);
}

fn show_modal(
    ctx: &egui::Context,
    modal: &mut Option<Modal>,
    details: &mut HashMap<String, Detail>,
    actions: &mut Vec<PluginsAction>,
) {
    let pal = Palette::of_ctx(ctx);
    match modal {
        None => {}
        Some(Modal::Secret {
            pkg,
            key,
            text,
            error,
            focus,
        }) => {
            let ok = !text.is_empty();
            let answer = show_dialog(
                ctx,
                &Dialog {
                    id: "plugin-secret",
                    title: "Set secret",
                    width: 440.0,
                    confirm_label: "Save",
                    confirm_enabled: ok,
                    danger: false,
                    enter_confirms: true,
                    ctrl_enter_confirms: false,
                    icon: Some(ICON_KEY_ROUND),
                    subtitle: Some("Written with omp plugin config set"),
                    tone: Some(pal.warn),
                    esc_hint: None,
                },
                |ui| {
                    card::kv(ui, 84.0, "Plugin", |ui| {
                        ui.label(
                            RichText::new(pkg.as_str())
                                .monospace()
                                .color(pal.text_strong),
                        );
                    });
                    card::kv(ui, 84.0, "Setting", |ui| {
                        ui.label(
                            RichText::new(key.as_str())
                                .monospace()
                                .color(pal.text_strong),
                        );
                    });
                    card::kv(ui, 84.0, "Storage", |ui| {
                        badge::show(ui, Some(ICON_LOCK), "write-only", Hue::Amber, 11.0);
                    });
                    ui.add_space(6.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new("The value is passed to omp and never shown again.")
                                .color(pal.weak),
                        )
                        .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                    ui.add_space(10.0);
                    let resp = ui.add(
                        TextEdit::singleline(text)
                            .id(Id::unique("plugin-secret-field"))
                            .password(true)
                            .hint_text("Paste the secret")
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(10, 8)),
                    );
                    if resp.changed() {
                        *error = None;
                    }
                    if let Some(e) = error.as_deref() {
                        ui.add_space(6.0);
                        ui.add(
                            egui::Label::new(RichText::new(e).small().color(pal.danger))
                                .wrap_mode(egui::TextWrapMode::Wrap),
                        );
                    }
                    if *focus {
                        resp.request_focus();
                        *focus = false;
                    }
                },
            );
            match answer {
                Some(true) => {
                    let (pkg, key) = (pkg.clone(), key.clone());
                    // The typed text is taken out of the dialog the moment it is used.
                    let typed = std::mem::take(text);
                    let kind = details
                        .get(&pkg)
                        .and_then(|d| d.config.as_ref())
                        .and_then(|c| c.as_ref().ok())
                        .and_then(|c| c.schema.iter().find(|(k, _)| *k == key))
                        .map(|(_, s)| s);
                    let value = match kind {
                        Some(s) => secret_value(s, &typed),
                        None => Ok(Value::String(typed)),
                    };
                    match value {
                        Ok(v) => {
                            if let Some(f) = details
                                .get_mut(&pkg)
                                .map(|d| d.fields.entry(key.clone()).or_default())
                            {
                                f.pending = true;
                                f.error = None;
                            }
                            actions.push(PluginsAction::SetConfig {
                                pkg,
                                key,
                                value: Sent(Some(v)),
                            });
                            *modal = None;
                        }
                        Err(e) => *error = Some(e),
                    }
                }
                Some(false) => *modal = None,
                None => {}
            }
        }
        Some(Modal::ClearSecret { pkg, key }) => {
            let answer = show_dialog(
                ctx,
                &Dialog {
                    id: "plugin-secret-clear",
                    title: "Clear secret",
                    width: 420.0,
                    confirm_label: "Clear",
                    confirm_enabled: true,
                    danger: true,
                    enter_confirms: false,
                    ctrl_enter_confirms: false,
                    icon: Some(ICON_TRASH_2),
                    subtitle: Some("This cannot be undone"),
                    tone: None,
                    esc_hint: None,
                },
                |ui| {
                    card::kv(ui, 84.0, "Plugin", |ui| {
                        ui.label(
                            RichText::new(pkg.as_str())
                                .monospace()
                                .color(pal.text_strong),
                        );
                    });
                    card::kv(ui, 84.0, "Setting", |ui| {
                        ui.label(
                            RichText::new(key.as_str())
                                .monospace()
                                .color(pal.text_strong),
                        );
                    });
                    ui.add_space(6.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "The stored value is removed from omp. You will have to enter it again to use it.",
                            )
                            .color(pal.weak),
                        )
                        .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                },
            );
            match answer {
                Some(true) => {
                    let (pkg, key) = (pkg.clone(), key.clone());
                    if let Some(f) = details
                        .get_mut(&pkg)
                        .map(|d| d.fields.entry(key.clone()).or_default())
                    {
                        f.pending = true;
                        f.error = None;
                    }
                    actions.push(PluginsAction::SetConfig {
                        pkg,
                        key,
                        value: Sent(None),
                    });
                    *modal = None;
                }
                Some(false) => *modal = None,
                None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use deck_core::omp::{PluginFeature, PluginManifest};

    use super::*;

    fn setting(kind: PluginSettingKind, secret: bool) -> PluginSetting {
        PluginSetting {
            kind,
            description: None,
            secret,
            env: None,
        }
    }

    #[test]
    fn each_schema_kind_gets_its_own_editor() {
        let values = vec!["fast".to_owned(), "slow".to_owned()];
        let cases = [
            (
                setting(PluginSettingKind::String { default: None }, false),
                Editor::Text,
            ),
            (
                setting(
                    PluginSettingKind::Number {
                        default: None,
                        min: Some(1.0),
                        max: Some(9.0),
                        step: None,
                    },
                    false,
                ),
                Editor::Number {
                    min: Some(1.0),
                    max: Some(9.0),
                },
            ),
            (
                setting(PluginSettingKind::Boolean { default: None }, false),
                Editor::Toggle,
            ),
            (
                setting(
                    PluginSettingKind::Enum {
                        values: values.clone(),
                        default: None,
                    },
                    false,
                ),
                Editor::Choice(&values),
            ),
            (setting(PluginSettingKind::Unknown, false), Editor::ReadOnly),
        ];
        for (s, want) in &cases {
            assert_eq!(&editor_of(s), want);
        }
        // A secret is write-only whatever its type.
        let secret = setting(PluginSettingKind::String { default: None }, true);
        assert_eq!(editor_of(&secret), Editor::Secret);
        let secret_number = setting(
            PluginSettingKind::Number {
                default: None,
                min: None,
                max: None,
                step: None,
            },
            true,
        );
        assert_eq!(editor_of(&secret_number), Editor::Secret);
    }

    #[test]
    fn editors_show_the_stored_value_else_the_manifest_default() {
        let text = setting(
            PluginSettingKind::String {
                default: Some("dflt".into()),
            },
            false,
        );
        assert_eq!(shown(&text, None), Shown::Text("dflt".into()));
        assert_eq!(
            shown(&text, Some(&Value::from("mine"))),
            Shown::Text("mine".into())
        );
        let number = setting(
            PluginSettingKind::Number {
                default: Some(3.0),
                min: None,
                max: None,
                step: None,
            },
            false,
        );
        assert_eq!(shown(&number, None), Shown::Number("3".into()));
        assert_eq!(
            shown(&number, Some(&Value::from(7))),
            Shown::Number("7".into())
        );
        let flag = setting(
            PluginSettingKind::Boolean {
                default: Some(true),
            },
            false,
        );
        assert_eq!(shown(&flag, None), Shown::Bool(true));
        assert_eq!(shown(&flag, Some(&Value::Bool(false))), Shown::Bool(false));
        let choice = setting(
            PluginSettingKind::Enum {
                values: vec!["a".into(), "b".into()],
                default: Some("a".into()),
            },
            false,
        );
        assert_eq!(
            shown(&choice, Some(&Value::from("b"))),
            Shown::Choice(Some("b".into()))
        );
        assert_eq!(shown(&choice, None), Shown::Choice(Some("a".into())));
    }

    #[test]
    fn a_secret_never_shows_its_value_only_whether_it_is_set() {
        let secret = setting(
            PluginSettingKind::String {
                default: Some("hunter2".into()),
            },
            true,
        );
        // Neither the placeholder omp-deck stores nor a leaked clear value nor the manifest
        // default reaches what the editor shows.
        for stored in [
            Some(Value::from("********")),
            Some(Value::from("hunter2-real-token")),
        ] {
            let s = shown(&secret, stored.as_ref());
            assert_eq!(s, Shown::Secret { set: true });
            assert!(!format!("{s:?}").contains("hunter2"));
        }
        assert_eq!(shown(&secret, None), Shown::Secret { set: false });
        assert_eq!(
            shown(&secret, Some(&Value::Null)),
            Shown::Secret { set: false }
        );
        // The action carrying the new value cannot print it either.
        let action = PluginsAction::SetConfig {
            pkg: "p".into(),
            key: "token".into(),
            value: Sent(Some(Value::from("hunter2-real-token"))),
        };
        assert!(!format!("{action:?}").contains("hunter2"));
    }

    #[test]
    fn text_and_number_fields_commit_or_restore_the_default() {
        assert_eq!(commit_text(""), None);
        assert_eq!(commit_text("x"), Some(Value::from("x")));
        assert_eq!(commit_number(None, None, " "), Ok(None));
        assert_eq!(commit_number(None, None, "12"), Ok(Some(Value::from(12))));
        assert_eq!(commit_number(None, None, "1.5"), Ok(Some(Value::from(1.5))));
        assert!(commit_number(None, None, "abc").is_err());
        assert!(commit_number(Some(1.0), None, "0").is_err());
        assert!(commit_number(None, Some(10.0), "11").is_err());
        assert_eq!(
            commit_number(Some(1.0), Some(10.0), "10"),
            Ok(Some(Value::from(10)))
        );
        let n = setting(
            PluginSettingKind::Number {
                default: None,
                min: None,
                max: None,
                step: None,
            },
            true,
        );
        assert_eq!(secret_value(&n, "42"), Ok(Value::from(42)));
        assert!(secret_value(&n, "no").is_err());
        let s = setting(PluginSettingKind::String { default: None }, true);
        assert_eq!(secret_value(&s, "abc"), Ok(Value::from("abc")));
    }

    fn plugin_with_features() -> NpmPlugin {
        NpmPlugin {
            name: "p".into(),
            version: "1".into(),
            path: None,
            enabled: true,
            enabled_features: None,
            manifest: PluginManifest {
                description: None,
                features: vec![
                    (
                        "a".into(),
                        PluginFeature {
                            description: None,
                            default: true,
                        },
                    ),
                    (
                        "b".into(),
                        PluginFeature {
                            description: None,
                            default: false,
                        },
                    ),
                    (
                        "c".into(),
                        PluginFeature {
                            description: None,
                            default: true,
                        },
                    ),
                ],
                settings: Vec::new(),
            },
        }
    }

    #[test]
    fn the_features_checklist_starts_from_the_defaults_and_keeps_manifest_order() {
        let p = plugin_with_features();
        let now = resolve_features(&p, None);
        assert_eq!(now, ["a", "c"]);
        assert_eq!(toggled_features(&p, &now, "b", true), ["a", "b", "c"]);
        assert_eq!(toggled_features(&p, &now, "a", false), ["c"]);
        let explicit = vec!["b".to_owned()];
        assert_eq!(resolve_features(&p, Some(&explicit)), ["b"]);
        // An empty selection stays empty (it is not "the defaults").
        assert!(resolve_features(&p, Some(&[])).is_empty());
        assert!(toggled_features(&p, &["a".to_owned()], "a", false).is_empty());
    }

    #[test]
    fn results_settle_the_state_of_a_change() {
        let mut st = PluginsState::default();
        st.busy.insert("p".into());
        st.enabled("p", Err("boom".into()));
        assert!(!st.busy.contains("p"));
        assert_eq!(st.errors.get("p").map(String::as_str), Some("boom"));
        assert!(st.want_load, "the list is read again after a change");
        st.enabled("p", Ok(()));
        assert!(st.errors.is_empty());

        st.config_set_done("p", "k", Err("bad".into()));
        let d = &st.details["p"];
        assert_eq!(d.fields["k"].error.as_deref(), Some("bad"));
        assert!(!d.stale);
        st.config_set_done("p", "k", Ok(()));
        assert!(st.details["p"].stale, "the stored settings are read again");
        assert!(st.details["p"].fields["k"].error.is_none());
        st.config_loaded("p", Ok(PluginConfig::default()));
        assert!(!st.details["p"].stale);
    }
}
