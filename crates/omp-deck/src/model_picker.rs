//! Model dropdown with thinking-effort pills, and the cached `omp models` catalog it browses.
//!
//! The dropdown only produces [`PickerAction`]s; the app turns them into `/switch` / `/effort`
//! injections. Row building and the pending-switch bookkeeping are pure and unit-tested.

use std::collections::HashMap;
use std::time::Duration;

use crate::icons::Icon;
use crate::icons::{
    ICON_CHECK, ICON_CHEVRON_DOWN, ICON_CHEVRON_UP, ICON_CIRCLE_ALERT, ICON_REFRESH_CW,
    ICON_SEARCH, ICON_STAR, ICON_UNDO_2,
};
use deck_core::omp::{ModelInfo, Pricing, SettingsCatalog};
use egui::{
    Align2, Color32, CursorIcon, FontId, Margin, Response, RichText, Sense, Stroke, StrokeKind,
    TextEdit, Ui, pos2, vec2,
};
use serde_json::Value;

use crate::agent_ctl::Link;
use crate::chat_view::{Ink, hairline, pop};
use crate::theme;
use crate::widgets::{icon_text, paint_icon};
use deck_service::statusfmt::{fmt_price, fmt_tokens, short_model_name, thinking_short};

// ------------------------------------------------------------------ catalog

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadState {
    NotLoaded,
    Loading,
    Ready,
    Failed(String),
}

/// The `Omp::models` catalog (with omp's metrics), loaded once on a worker and shared by every tab.
pub struct ModelsCache {
    pub state: LoadState,
    list: Vec<ModelInfo>,
    index: HashMap<String, usize>,
    /// The quick roles of omp's `modelRoles` config (see [`roles_from_config`]).
    roles: Vec<RoleEntry>,
    /// The task-agent model omp's config names (`task.agentModelOverrides.task`).
    task_model: Option<String>,
    /// Bumps whenever `list` or the roles change (invalidates cached rows and views).
    pub generation: u64,
}

impl Default for ModelsCache {
    fn default() -> Self {
        ModelsCache {
            state: LoadState::NotLoaded,
            list: Vec::new(),
            index: HashMap::new(),
            roles: Vec::new(),
            task_model: None,
            generation: 0,
        }
    }
}

impl ModelsCache {
    pub fn begin(&mut self) {
        self.state = LoadState::Loading;
    }

    pub fn finish(&mut self, result: Result<Vec<ModelInfo>, String>) {
        match result {
            Ok(list) => {
                self.index = list
                    .iter()
                    .enumerate()
                    .map(|(i, m)| (m.selector.clone(), i))
                    .collect();
                self.list = list;
                self.state = LoadState::Ready;
                self.generation += 1;
            }
            Err(e) => self.state = LoadState::Failed(e),
        }
    }

    pub fn get(&self, selector: &str) -> Option<&ModelInfo> {
        self.index.get(selector).and_then(|&i| self.list.get(i))
    }

    pub fn all(&self) -> &[ModelInfo] {
        &self.list
    }

    pub fn is_loading(&self) -> bool {
        self.state == LoadState::Loading
    }

    /// The quick roles, `@default`, `@smol`, `@slow` first; empty until omp's settings are read.
    pub fn roles(&self) -> &[RoleEntry] {
        &self.roles
    }

    /// Takes the roles and the configured task-agent model of a freshly read settings catalog.
    pub fn set_roles(&mut self, catalog: &SettingsCatalog) {
        let (roles, task_model) = roles_from_catalog(catalog);
        if roles != self.roles || task_model != self.task_model {
            self.roles = roles;
            self.task_model = task_model;
            self.generation += 1;
        }
    }
}

// ------------------------------------------------------------------ pure logic

/// One line of the dropdown list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    Header(String),
    /// Header of the starred models on top of the list.
    FavoritesHeader,
    /// Index into the catalog; a starred model listed under Favorites.
    Favorite(usize),
    /// Index into saved favorites; its provider/model is not currently available.
    UnavailableFavorite(usize),
    /// Index into the catalog.
    Model(usize),
    /// Header of the quick roles.
    RolesHeader,
    /// Index into the roles of the catalog.
    Role(usize),
    /// Task mode: the row that drops the session's task-agent override.
    ClearOverride,
}

/// A model matches when every query word occurs in its `provider name id` (case-insensitive).
fn matches_words(m: &ModelInfo, words: &[String]) -> bool {
    if words.is_empty() {
        return true;
    }
    let hay = format!("{} {} {}", m.provider, m.name, m.id).to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

// ------------------------------------------------------------------ quick roles

/// Roles that name no chat model (omp's media, search and judge kinds, the small helper roles):
/// switching the session to one makes no sense, so the picker leaves them out.
const NON_CHAT_ROLES: [&str; 7] = [
    "image",
    "web",
    "speech",
    "dictation",
    "judge",
    "tiny",
    "memory",
];
/// omp's quick roles, always offered and in this order.
const BUILTIN_ROLES: [&str; 3] = ["default", "smol", "slow"];

/// One quick role (`@smol`) and the model omp's config assigns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleEntry {
    pub role: String,
    /// The configured value, `:level` suffix kept; `None` = the role is not configured.
    pub model: Option<String>,
}

impl RoleEntry {
    /// What `/switch` takes for this role: omp resolves the alias itself.
    pub fn selector(&self) -> String {
        format!("@{}", self.role)
    }

    /// The configured model and its thinking level, the suffix split off.
    fn target(&self) -> Option<(&str, Option<&str>)> {
        self.model.as_deref().map(crate::omp_settings::split_level)
    }

    fn provider(&self) -> Option<&str> {
        self.target()
            .and_then(|(m, _)| m.split_once('/'))
            .map(|(p, _)| p)
    }
}

/// The quick roles from omp's `modelRoles` value: `@default`, `@smol`, `@slow` always (configured
/// or not), then every other chat role the config assigns a model, alphabetical. Each role once.
pub fn roles_from_config(value: Option<&Value>) -> Vec<RoleEntry> {
    let configured = value.and_then(Value::as_object);
    let model_of = |role: &str| {
        configured
            .and_then(|m| m.get(role))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let mut roles: Vec<RoleEntry> = BUILTIN_ROLES
        .iter()
        .map(|r| RoleEntry {
            role: (*r).to_owned(),
            model: model_of(r),
        })
        .collect();
    if let Some(map) = configured {
        // A name with whitespace or a colon cannot be typed as `/switch @name`.
        let mut custom: Vec<&str> = map
            .keys()
            .map(String::as_str)
            .filter(|k| {
                !BUILTIN_ROLES.contains(k)
                    && !NON_CHAT_ROLES.contains(k)
                    && !k.is_empty()
                    && !k.chars().any(|c| c.is_whitespace() || c == ':')
            })
            .collect();
        custom.sort_unstable();
        roles.extend(custom.into_iter().filter_map(|k| {
            model_of(k).map(|model| RoleEntry {
                role: k.to_owned(),
                model: Some(model),
            })
        }));
    }
    roles
}

/// The quick roles and the configured task-agent model (`task.agentModelOverrides.task`, first
/// pattern of a list) of omp's settings catalog.
pub fn roles_from_catalog(catalog: &SettingsCatalog) -> (Vec<RoleEntry>, Option<String>) {
    let value = |key: &str| {
        catalog
            .tabs
            .iter()
            .flat_map(|t| &t.settings)
            .find(|s| s.key == key)
            .and_then(|s| s.value.as_ref().or(s.default.as_ref()))
    };
    let task = value("task.agentModelOverrides")
        .and_then(|v| v.get("task"))
        .and_then(|t| match t {
            Value::String(s) => Some(s.as_str()),
            Value::Array(a) => a.first().and_then(Value::as_str),
            _ => None,
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    (roles_from_config(value("modelRoles")), task)
}

/// A search that starts with `@` browses the quick roles, as omp's picker does; the text after
/// the `@` narrows them.
pub fn role_query(query: &str) -> Option<&str> {
    query.trim_start().strip_prefix('@').map(str::trim)
}

/// The roles an `@` search shows: those whose name contains the text, name-prefix matches first.
fn role_rows(roles: &[RoleEntry], rest: &str) -> Vec<Row> {
    let rest = rest.to_lowercase();
    let mut hits: Vec<usize> = (0..roles.len())
        .filter(|&i| roles[i].role.to_lowercase().contains(&rest))
        .collect();
    hits.sort_by_key(|&i| !roles[i].role.to_lowercase().starts_with(&rest));
    if hits.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![Row::RolesHeader];
    rows.extend(hits.into_iter().map(Row::Role));
    rows
}

/// The roles an ordinary search lists: every word occurs in `@role model`; a provider filter
/// keeps the roles whose model that provider serves.
fn role_hits(roles: &[RoleEntry], words: &[String], provider: Option<&str>) -> Vec<usize> {
    (0..roles.len())
        .filter(|&i| {
            let r = &roles[i];
            let hay = format!("@{} {}", r.role, r.model.as_deref().unwrap_or("")).to_lowercase();
            provider.is_none_or(|p| r.provider() == Some(p))
                && words.iter().all(|w| hay.contains(w.as_str()))
        })
        .collect()
}

/// Favorites first (in the order they were starred), then provider groups alphabetical with the
/// newest model first ([`crate::model_order`]). `provider` limits the list to one provider.
/// Favorites obey the provider filter and the search; groups without a match are omitted.
pub fn build_rows(
    models: &[ModelInfo],
    query: &str,
    provider: Option<&str>,
    favorites: &[String],
) -> Vec<Row> {
    build_rows_with_roles(models, &[], query, provider, favorites)
}

/// [`build_rows`] with the quick roles: a search starting with `@` lists only the matching roles;
/// otherwise the roles that match form a "Roles" group between the favorites and the providers.
pub fn build_rows_with_roles(
    models: &[ModelInfo],
    roles: &[RoleEntry],
    query: &str,
    provider: Option<&str>,
    favorites: &[String],
) -> Vec<Row> {
    if !roles.is_empty()
        && let Some(rest) = role_query(query)
    {
        return role_rows(roles, rest);
    }
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let wanted =
        |m: &ModelInfo| provider.is_none_or(|p| m.provider == p) && matches_words(m, &words);
    let mut rows = Vec::with_capacity(models.len() + roles.len() + 9);

    for (favorite, selector) in favorites.iter().enumerate() {
        let row = match models.iter().position(|m| m.selector == *selector) {
            Some(i) if wanted(&models[i]) => Row::Favorite(i),
            Some(_) => continue,
            None => {
                let favorite_provider = selector.split_once('/').map(|(p, _)| p);
                let searchable = selector.to_lowercase();
                if provider.is_some_and(|p| Some(p) != favorite_provider)
                    || !words.iter().all(|word| searchable.contains(word))
                {
                    continue;
                }
                Row::UnavailableFavorite(favorite)
            }
        };
        if rows.is_empty() {
            rows.push(Row::FavoritesHeader);
        }
        rows.push(row);
    }

    let hits = role_hits(roles, &words, provider);
    if !hits.is_empty() {
        rows.push(Row::RolesHeader);
        rows.extend(hits.into_iter().map(Row::Role));
    }

    // Rank within the provider's whole list, so a search never reshuffles what stays visible.
    let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
    for (i, m) in models.iter().enumerate() {
        if provider.is_some_and(|p| m.provider != p) {
            continue;
        }
        match groups.iter_mut().find(|(p, _)| *p == m.provider) {
            Some((_, v)) => v.push(i),
            None => groups.push((m.provider.as_str(), vec![i])),
        }
    }
    groups.sort_by_key(|(p, _)| p.to_lowercase());
    for (p, members) in groups {
        let pairs: Vec<(&str, &str)> = members
            .iter()
            .map(|&i| (models[i].id.as_str(), models[i].name.as_str()))
            .collect();
        let shown: Vec<usize> = crate::model_order::order(&pairs)
            .into_iter()
            .map(|k| members[k])
            .filter(|&i| matches_words(&models[i], &words))
            .collect();
        if shown.is_empty() {
            continue;
        }
        rows.push(Row::Header(p.to_owned()));
        rows.extend(shown.into_iter().map(Row::Model));
    }
    rows
}

/// Catalog indices in the order the palette lists models: starred first (in starring order, flagged
/// `true`), then the dropdown's provider groups without repeating the starred ones.
pub fn palette_models(models: &[ModelInfo], favorites: &[String]) -> Vec<(usize, bool)> {
    build_rows(models, "", None, favorites)
        .into_iter()
        .filter_map(|r| match r {
            Row::Favorite(i) => Some((i, true)),
            Row::Model(i) => Some((i, false)),
            Row::Header(_)
            | Row::FavoritesHeader
            | Row::UnavailableFavorite(_)
            | Row::RolesHeader
            | Row::Role(_)
            | Row::ClearOverride => None,
        })
        .fold(Vec::new(), |mut out, (i, star)| {
            if !out.iter().any(|(j, _)| *j == i) {
                out.push((i, star));
            }
            out
        })
}

/// The levels the pills offer: `off`, `auto` and the model's own; empty without thinking support.
pub fn level_options(model: Option<&ModelInfo>) -> Vec<String> {
    let Some(m) = model.filter(|m| !m.thinking.is_empty()) else {
        return Vec::new();
    };
    let mut v = vec!["off".to_owned(), "auto".to_owned()];
    v.extend(
        m.thinking
            .iter()
            .filter(|l| *l != "off" && *l != "auto")
            .cloned(),
    );
    v
}

/// The level to append to a `/switch`: only one the target model supports.
pub fn level_for_switch(target: &ModelInfo, staged: Option<&str>) -> Option<String> {
    let level = staged?;
    level_options(Some(target))
        .iter()
        .any(|l| l == level)
        .then(|| level.to_owned())
}

/// Tokens per second as omp's model browser prints them: whole from 10, one decimal below.
fn fmt_tps(tps: f64) -> String {
    if tps >= 10.0 {
        format!("{}", tps.round())
    } else {
        format!("{tps:.1}")
    }
}

/// A row's speed: measured on this machine (`118 t/s`), else omp's catalog estimate
/// (`~118 t/s`); `None` when neither exists.
fn speed_label(m: &ModelInfo) -> Option<String> {
    match (&m.measured, m.catalog_tps) {
        (Some(s), _) => Some(format!("{} t/s", fmt_tps(s.tps))),
        (None, Some(tps)) => Some(format!("~{} t/s", fmt_tps(tps))),
        (None, None) => None,
    }
}

/// omp's word for a rate card the `$` rates do not describe (`fixed` / unreported: `None`).
fn pricing_word(p: Option<Pricing>) -> Option<&'static str> {
    match p? {
        Pricing::Fixed => None,
        Pricing::Free => Some("free"),
        Pricing::Included => Some("included"),
        Pricing::Variable => Some("price varies"),
        Pricing::Unknown => Some("pricing unknown"),
    }
}

/// A row's price: `$in / $out` per million tokens, or omp's word for a card without rates;
/// `None` when omp does not know it (the tooltip says so).
fn price_label(m: &ModelInfo) -> Option<String> {
    if m.pricing == Some(Pricing::Unknown) {
        return None;
    }
    if let Some(word) = pricing_word(m.pricing) {
        return Some(word.to_owned());
    }
    let (i, o) = (m.cost_input?, m.cost_output?);
    Some(format!("${} / ${}", fmt_price(i), fmt_price(o)))
}

/// A row's tooltip: name, `/switch` target, then the facts omp's model browser details (only
/// those omp reports).
fn model_details(m: &ModelInfo) -> String {
    let mut facts: Vec<(&str, String)> = Vec::new();
    if let Some(w) = m.context_window {
        facts.push(("Context", format!("{} tokens", fmt_tokens(w))));
    }
    if let Some(t) = m.max_tokens {
        facts.push(("Max output", format!("{} tokens", fmt_tokens(t))));
    }
    let price = match pricing_word(m.pricing) {
        Some(word) => Some(word.to_owned()),
        None => m.cost_input.zip(m.cost_output).map(|(i, o)| {
            let mut s = format!("${} in · ${} out", fmt_price(i), fmt_price(o));
            if let Some(c) = m.cost_cache_read.filter(|c| *c > 0.0) {
                s.push_str(&format!(" · ${} cache read", fmt_price(c)));
            }
            s + " per M tokens"
        }),
    };
    if let Some(price) = price {
        facts.push(("Price", price));
    }
    if let Some(s) = &m.measured {
        let mut speed = format!("{} t/s", fmt_tps(s.tps));
        if let Some(tier) = &s.tier {
            speed.push_str(&format!(" ({tier} tier)"));
        }
        if let Some(ms) = s.ttft_ms {
            let secs = ms / 1000.0;
            if secs >= 10.0 {
                speed.push_str(&format!(" · {secs:.0} s to first token"));
            } else {
                speed.push_str(&format!(" · {secs:.1} s to first token"));
            }
        }
        let n = s.samples.round().max(1.0);
        speed.push_str(&format!(
            " · measured here, {n:.0} sample{}",
            if n == 1.0 { "" } else { "s" }
        ));
        facts.push(("Speed", speed));
    } else if let Some(tps) = m.catalog_tps {
        facts.push(("Speed", format!("~{} t/s (catalog estimate)", fmt_tps(tps))));
    }
    if let Some(iq) = m.intelligence {
        facts.push(("Intelligence", format!("{} (catalog score)", iq.round())));
    }
    if !m.input.is_empty() {
        facts.push(("Input", m.input.join(" · ")));
    }
    if let Some(r) = m.reasoning {
        facts.push(("Reasoning", if r { "yes" } else { "no" }.to_owned()));
    }
    let mut out = format!("{}\n/switch {}", m.name, m.selector);
    if !facts.is_empty() {
        out.push('\n');
    }
    for (k, v) in facts {
        out.push_str(&format!("\n{k}: {v}"));
    }
    out
}

// ------------------------------------------------------------------ UI

#[derive(Default)]
pub struct PickerState {
    pub search: String,
    /// A level chosen in this popup session, applied to the next model picked.
    pub staged_level: Option<String>,
    rows: Vec<Row>,
    rows_key: Option<RowsKey>,
    /// `(provider, model count)`, alphabetical; valid for `providers_gen`.
    providers: Vec<(String, usize)>,
    providers_gen: Option<u64>,
    was_open: bool,
    /// The provider options are expanded under the dropdown.
    provider_open: bool,
    /// Give the search field the keyboard back (a star or provider click took it).
    refocus_search: bool,
    /// Bumped on every open: part of the list's scroll id so an old scroll offset never survives.
    opens: u64,
    /// Open the popup on the next drawn frame (the command palette's "Switch model…", omp's own
    /// model picker redirected): asked at, so a request nobody drew for does not open it later.
    open_requested: Option<std::time::Instant>,
    /// The popup's target is the session's task agents instead of the session (omp's task mode).
    task_mode: bool,
    /// The task-agent model each tab's last `TaskModelOverride` left in effect (omp has no read
    /// path for the session-only layer), keyed by the tab.
    task_overrides: HashMap<u64, Option<String>>,
    /// A task-model pick of this frame (`None` inside = clear the override), taken by `show_with`.
    task_pick: Option<Option<String>>,
}

/// Everything the cached rows were built from.
struct RowsKey {
    search: String,
    generation: u64,
    provider: Option<String>,
    favorites: Vec<String>,
    roles: bool,
    task: bool,
}

impl PickerState {
    /// Ask for the dropdown to open as if its chip had been clicked.
    pub fn request_open(&mut self) {
        self.open_requested = Some(std::time::Instant::now());
    }

    /// Remember the model task agents of `tab` run on now (omp's reply to `TaskModelOverride`:
    /// the override, else the configured model; `None` = they inherit the session's).
    pub fn record_task_override(&mut self, tab: u64, effective: Option<String>) {
        self.task_overrides.insert(tab, effective);
    }

    /// The model task agents of `tab` run on: what omp last answered for the tab, else the
    /// configured one; `None` = they inherit the session's.
    fn task_now(&self, catalog: &ModelsCache, tab: Option<u64>) -> Option<String> {
        tab.and_then(|t| self.task_overrides.get(&t).cloned())
            .unwrap_or_else(|| catalog.task_model.clone())
    }

    fn refresh_providers(&mut self, catalog: &ModelsCache) {
        if self.providers_gen != Some(catalog.generation) {
            self.providers = provider_counts(catalog.all());
            self.providers_gen = Some(catalog.generation);
        }
    }

    /// The provider the list is limited to; an unknown one counts as "All providers".
    fn effective_provider<'a>(&self, wanted: Option<&'a str>) -> Option<&'a str> {
        wanted.filter(|w| self.providers.iter().any(|(p, _)| p == w))
    }

    /// `with_roles`: the session picker, which lists the quick roles (never in task mode: a task
    /// override names a model, not a role).
    fn refresh_rows(&mut self, catalog: &ModelsCache, input: &PickerInput<'_>, with_roles: bool) {
        let provider = self.effective_provider(input.provider);
        let roles = with_roles && !self.task_mode;
        let fresh = self.rows_key.as_ref().is_some_and(|k| {
            k.generation == catalog.generation
                && k.search == self.search
                && k.provider.as_deref() == provider
                && k.favorites == input.favorites
                && k.roles == roles
                && k.task == self.task_mode
        });
        if !fresh {
            let listed: &[RoleEntry] = if roles { catalog.roles() } else { &[] };
            self.rows = build_rows_with_roles(
                catalog.all(),
                listed,
                &self.search,
                provider,
                input.favorites,
            );
            if self.task_mode && self.search.trim().is_empty() {
                self.rows.insert(0, Row::ClearOverride);
            }
            self.rows_key = Some(RowsKey {
                search: self.search.clone(),
                generation: catalog.generation,
                provider: provider.map(str::to_owned),
                favorites: input.favorites.to_vec(),
                roles,
                task: self.task_mode,
            });
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PickerAction {
    /// `/switch <selector>[:level]`; the selector is a model or a role alias (`@smol`).
    Switch {
        selector: String,
        level: Option<String>,
    },
    /// `/effort <level>`
    Effort(String),
    Refresh,
    /// Star / unstar a model (by selector).
    ToggleFavorite(String),
    /// Limit the list to one provider (`None` = all providers).
    SetProvider(Option<String>),
}

impl PickerAction {
    /// Whether the popup stays open after this action.
    fn keeps_popup_open(&self) -> bool {
        matches!(
            self,
            PickerAction::Refresh | PickerAction::ToggleFavorite(_) | PickerAction::SetProvider(_)
        )
    }
}

pub struct PickerInput<'a> {
    pub catalog: &'a ModelsCache,
    /// Selector of the session's model.
    pub current: Option<&'a str>,
    /// Configured (else effective) thinking level of the session.
    pub level: Option<&'a str>,
    /// Starred model selectors, insertion order.
    pub favorites: &'a [String],
    /// Persisted provider filter (`None` = all providers).
    pub provider: Option<&'a str>,
    /// What the Thinking row says while there is no current model.
    pub unset: &'a str,
}

/// What only the session picker (the status bar's) offers on top of choosing a model.
pub struct SessionExtras<'a> {
    /// The tab the popup acts on: its task-agent model is remembered under this key.
    pub tab: u64,
    /// Why the "Task agents" target cannot be used now, `None` when it can (it needs live control).
    pub task_unavailable: Option<&'a str>,
}

/// What the session picker's user chose in a frame.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Picked {
    pub actions: Vec<PickerAction>,
    /// Task mode: the model task agents are to run on (`Some(Some(provider/id))`), or the
    /// override to be dropped (`Some(None)`). It needs live control.
    pub task_model: Option<Option<String>>,
}

/// Why task mode is unavailable for a tab whose live-control link is `link`.
pub fn task_unavailable(link: Link) -> Option<&'static str> {
    match link {
        Link::Live => None,
        Link::Connecting => Some("Live control is still connecting; try again in a moment"),
        Link::ReadOnly => Some("Needs live control; this tab runs without it"),
        Link::Lost => Some("Needs live control; the connection to this tab was lost"),
        Link::Ended => Some("omp has exited in this tab"),
    }
}

/// `(provider, models)` alphabetical (case-insensitive).
fn provider_counts(models: &[ModelInfo]) -> Vec<(String, usize)> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for m in models {
        match counts.iter_mut().find(|(p, _)| *p == m.provider) {
            Some((_, n)) => *n += 1,
            None => counts.push((m.provider.clone(), 1)),
        }
    }
    counts.sort_by_key(|(p, _)| p.to_lowercase());
    counts
}

/// Height of the search and provider fields.
const FIELD_H: f32 = 32.0;

/// The plate of a field: the chat view's chip fill and hairline, the accent outline while
/// `active` (focused or open), a blend towards it under the pointer.
fn field_plate(ink: &Ink, rect: egui::Rect, hot: f32, active: bool) -> egui::Shape {
    let line = if active {
        ink.b
    } else {
        theme::mix(ink.line(), ink.b, 0.55 * hot)
    };
    egui::Shape::Rect(egui::epaint::RectShape::new(
        rect,
        theme::rad(8, 10),
        ink.chip(),
        Stroke::new(if active { 1.5 } else { 1.0 }, line),
        StrokeKind::Inside,
    ))
}

/// The search box: a plate with a magnifier and the editable query inside.
fn search_field(ui: &mut Ui, ink: &Ink, state: &mut PickerState, focus: bool) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), FIELD_H), Sense::hover());
    let slot = ui.painter().add(egui::Shape::Noop);
    let inner = egui::Rect::from_min_max(
        pos2(rect.left() + 34.0, rect.top() + 1.0),
        pos2(rect.right() - 10.0, rect.bottom() - 1.0),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let edit = TextEdit::singleline(&mut state.search)
        .hint_text(RichText::new("Search models…").color(ink.faint()))
        .font(ink.prose(14.0))
        .text_color(ink.strong())
        .frame(egui::Frame::NONE)
        .margin(Margin::ZERO)
        .desired_width(inner.width());
    let r = child.add(edit);
    if focus || std::mem::take(&mut state.refocus_search) {
        r.request_focus();
    }
    let hot =
        ui.ctx()
            .animate_bool_with_time(resp.id.with("hot"), r.has_focus() || r.hovered(), 0.08);
    let p = ui.painter();
    p.set(slot, field_plate(ink, rect, hot, r.has_focus()));
    pop(p, rect, theme::rad(8, 10), ink, hot);
    paint_icon(
        p,
        pos2(rect.left() + 17.0, rect.center().y),
        Align2::CENTER_CENTER,
        ICON_SEARCH,
        16.0,
        theme::mix(ink.dim(), ink.accent_text(), hot),
    );
}

/// The "All providers (N)" / provider button. Its options replace the model list in place
/// ([`provider_options`]): a popup inside this popup would count as a click outside and close the
/// whole picker, and growing the popup would move it.
fn provider_dropdown(ui: &mut Ui, ink: &Ink, state: &mut PickerState, input: &PickerInput<'_>) {
    if state.providers.is_empty() {
        return;
    }
    let total = input.catalog.all().len();
    let selected = state.effective_provider(input.provider);
    let label = |provider: Option<&str>| match provider {
        None => format!("All providers ({total})"),
        Some(p) => {
            let n = state
                .providers
                .iter()
                .find(|(q, _)| q == p)
                .map_or(0, |(_, n)| *n);
            format!("{p} ({n})")
        }
    };

    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), FIELD_H), Sense::click());
    let hot = ui.ctx().animate_bool_with_time(
        resp.id.with("hov"),
        resp.hovered() || state.provider_open,
        0.08,
    );
    let p = ui.painter();
    p.add(field_plate(ink, rect, hot, state.provider_open));
    pop(p, rect, theme::rad(8, 10), ink, hot);
    p.text(
        pos2(rect.left() + 12.0, rect.center().y),
        Align2::LEFT_CENTER,
        label(selected),
        ink.prose(13.5),
        if selected.is_some() {
            ink.strong()
        } else {
            ink.text()
        },
    );
    paint_icon(
        p,
        pos2(rect.right() - 18.0, rect.center().y),
        Align2::CENTER_CENTER,
        if state.provider_open {
            ICON_CHEVRON_UP
        } else {
            ICON_CHEVRON_DOWN
        },
        18.0,
        theme::mix(ink.dim(), ink.strong(), hot),
    );
    if resp
        .on_hover_text("Show the models of one provider")
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
    {
        state.provider_open = !state.provider_open;
        state.refocus_search = true;
    }
}

/// The provider choices, drawn where the model list normally is (`height` tall, like the list).
fn provider_options(
    ui: &mut Ui,
    ink: &Ink,
    state: &mut PickerState,
    input: &PickerInput<'_>,
    actions: &mut Vec<PickerAction>,
    height: f32,
) {
    let total = input.catalog.all().len();
    let selected = state.effective_provider(input.provider);
    let label = |provider: Option<&str>| match provider {
        None => format!("All providers ({total})"),
        Some(p) => {
            let n = state
                .providers
                .iter()
                .find(|(q, _)| q == p)
                .map_or(0, |(_, n)| *n);
            format!("{p} ({n})")
        }
    };
    let mut choice: Option<Option<String>> = None;
    egui::Frame::new()
        .fill(ink.card())
        .stroke(Stroke::new(1.0, ink.line()))
        .corner_radius(theme::rad(8, 10))
        .inner_margin(Margin::same(4))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            egui::ScrollArea::vertical()
                .id_salt("provider-options")
                .min_scrolled_height(height - 8.0)
                .max_height(height - 8.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if option_row(ui, ink, &label(None), selected.is_none()) {
                        choice = Some(None);
                    }
                    for (name, _) in &state.providers {
                        let on = selected == Some(name.as_str());
                        if option_row(ui, ink, &label(Some(name)), on) {
                            choice = Some(Some(name.clone()));
                        }
                    }
                });
        });
    if let Some(c) = choice {
        state.provider_open = false;
        if c.as_deref() != selected {
            actions.push(PickerAction::SetProvider(c));
        }
    }
}

/// The plate of a list row, as the chat view's pickers draw it: the current/selected row is an
/// accent wash with its edge and a bar on the left; a hovered one lifts with a pop outline.
fn row_plate(p: &egui::Painter, rect: egui::Rect, ink: &Ink, selected: bool, hov: f32) {
    let radius = theme::rad(5, 7);
    if selected {
        p.rect(
            rect,
            radius,
            ink.wash(ink.b),
            Stroke::new(1.0, ink.edge(ink.b)),
            StrokeKind::Inside,
        );
        p.rect_filled(
            egui::Rect::from_min_max(
                pos2(rect.left(), rect.top() + 3.0),
                pos2(rect.left() + 3.0, rect.bottom() - 3.0),
            ),
            1.0,
            ink.b,
        );
    } else if hov > 0.0 {
        p.rect_filled(rect, radius, ink.p.hover_bg.gamma_multiply(hov));
        pop(p, rect, radius, ink, hov);
    }
}

/// One entry of the provider list; true when clicked.
fn option_row(ui: &mut Ui, ink: &Ink, text: &str, selected: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.08);
    let p = ui.painter();
    row_plate(p, rect, ink, selected, hov);
    if selected {
        paint_icon(
            p,
            pos2(rect.left() + 18.0, rect.center().y),
            Align2::CENTER_CENTER,
            ICON_CHECK,
            16.0,
            ink.p.ok,
        );
    }
    p.text(
        pos2(rect.left() + 34.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        ink.prose(13.5),
        if selected { ink.strong() } else { ink.text() },
    );
    resp.on_hover_cursor(CursorIcon::PointingHand).clicked()
}

/// How long a request to open the popup stays valid while the chip is not being drawn.
const REQUEST_TTL: std::time::Duration = std::time::Duration::from_secs(3);

const POPUP_W: f32 = 500.0;
/// Space kept free at the window edges when the popup is clamped.
const POPUP_MARGIN: f32 = 16.0;
const ROW_H: f32 = 36.0;
const LIST_H: f32 = 330.0;

/// Draws the popup anchored above `chip` while it is open; returns the user's choices.
pub fn show(
    chip: &Response,
    state: &mut PickerState,
    input: &PickerInput<'_>,
) -> Vec<PickerAction> {
    show_with(chip, state, input, None).actions
}

/// [`show`] for a session's picker: the quick roles join the list and the head offers the "Task
/// agents" target.
pub fn show_session(
    chip: &Response,
    state: &mut PickerState,
    input: &PickerInput<'_>,
    extras: &SessionExtras<'_>,
) -> Picked {
    show_with(chip, state, input, Some(extras))
}

fn show_with(
    chip: &Response,
    state: &mut PickerState,
    input: &PickerInput<'_>,
    extras: Option<&SessionExtras<'_>>,
) -> Picked {
    let mut actions = Vec::new();
    let ctx = chip.ctx.clone();
    let popup_id = egui::Popup::default_response_id(chip);
    if state
        .open_requested
        .take()
        .is_some_and(|at| at.elapsed() < REQUEST_TTL)
    {
        egui::Popup::open_id(&ctx, popup_id);
    }
    let open = egui::Popup::is_id_open(&ctx, popup_id);
    if !open {
        // Reset while closed: the frame that opens the popup runs egui's invisible sizing pass, and
        // the popup keeps the height measured there. A stale narrowed search would shrink it for good.
        if state.was_open {
            state.opens += 1;
        }
        state.search.clear();
        state.staged_level = None;
        state.provider_open = false;
        state.task_mode = false;
        state.rows_key = None;
        state.rows.clear();
    } else if !state.was_open {
        // Opened without us seeing it closed (the chip was not drawn meanwhile).
        state.search.clear();
        state.staged_level = None;
        state.provider_open = false;
        state.task_mode = false;
        state.rows_key = None;
        state.rows.clear();
        state.opens += 1;
    }
    let just_opened = open && !state.was_open;
    // Authentication changes the available catalog without changing this process's cache.
    // Re-open is a user-driven refresh; keep the previous rows visible while it runs.
    if just_opened && !input.catalog.is_loading() {
        actions.push(PickerAction::Refresh);
    }
    state.was_open = open;
    // Task agents need live control; without it (or in a plain picker) the popup stays on the session.
    if extras.is_none_or(|e| e.task_unavailable.is_some()) {
        state.task_mode = false;
    }
    let popup_w = POPUP_W.min(ctx.content_rect().width() - 2.0 * POPUP_MARGIN);
    egui::Popup::from_toggle_button_response(chip)
        .align(egui::RectAlign::TOP_START)
        .gap(8.0)
        .width(popup_w)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .frame(egui::Frame::NONE)
        .show(|ui| {
            let ink = Ink::new(ui, 13.0);
            let fill = if theme::flat() {
                ctx.global_style().visuals.window_fill
            } else {
                theme::mix(ink.p.surface, ink.p.bg, 0.15)
            };
            // The chat view's sheet: a header band flush with the edges over a padded body.
            crate::card::chrome(ui, Margin::ZERO, fill, |ui| {
                ui.set_width(popup_w);
                ui.spacing_mut().item_spacing.y = 0.0;
                head_band(ui, &ink, input, state, extras, &mut actions);
                egui::Frame::NONE
                    .inner_margin(Margin::same(BODY_PAD))
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        body(ui, &ink, state, input, extras, just_opened, &mut actions);
                    });
            });
        });
    state.refocus_search |= actions.iter().any(|a| {
        matches!(
            a,
            PickerAction::ToggleFavorite(_) | PickerAction::SetProvider(_)
        )
    });
    let task_model = state.task_pick.take();
    if task_model.is_some() || actions.iter().any(|a| !a.keeps_popup_open()) {
        egui::Popup::close_id(&ctx, popup_id);
    }
    Picked {
        actions,
        task_model,
    }
}

/// Height of the header band and the padding of the body under it.
const HEAD_H: f32 = 38.0;
const BODY_PAD: i8 = 14;

/// A band's outline: rounded at the top corners (`r`), square at the bottom.
fn band_points(rect: egui::Rect, r: f32) -> Vec<egui::Pos2> {
    use std::f32::consts::{FRAC_PI_2, PI};
    const STEPS: usize = 6;
    let mut pts = Vec::with_capacity(2 * (STEPS + 1) + 2);
    for (cx, a0) in [(rect.left() + r, PI), (rect.right() - r, PI + FRAC_PI_2)] {
        for i in 0..=STEPS {
            let a = a0 + FRAC_PI_2 * i as f32 / STEPS as f32;
            pts.push(pos2(cx + r * a.cos(), rect.top() + r + r * a.sin()));
        }
    }
    pts.push(rect.right_bottom());
    pts.push(rect.left_bottom());
    pts
}

/// The two targets of the picker head: the session itself, or the agents it spawns (omp's task
/// mode).
const TARGETS: [&str; 2] = ["This session", "Task agents"];

/// The head's target switch, its right edge at `right`; returns its left edge. The "Task agents"
/// side is dimmed and explains itself when `unavailable`.
fn target_toggle(
    ui: &mut Ui,
    ink: &Ink,
    band: egui::Rect,
    right: f32,
    state: &mut PickerState,
    unavailable: Option<&str>,
) -> f32 {
    const H: f32 = 26.0;
    const INSET: f32 = 3.0;
    let font = ink.label(12.0);
    let widths = TARGETS.map(|t| {
        ui.painter()
            .layout_no_wrap(t.to_owned(), font.clone(), ink.text())
            .size()
            .x
            + 20.0
    });
    let total = widths.iter().sum::<f32>() + 2.0 * INSET;
    let plate = egui::Rect::from_min_size(
        pos2(right - total, band.center().y - H / 2.0),
        vec2(total, H),
    );
    ui.painter().rect(
        plate,
        theme::rad(8, 12),
        ink.chip(),
        Stroke::new(1.0, ink.line()),
        StrokeKind::Inside,
    );
    let mut x = plate.left() + INSET;
    for (i, (label, w)) in TARGETS.iter().zip(widths).enumerate() {
        let seg = egui::Rect::from_min_size(pos2(x, plate.top() + INSET), vec2(w, H - 2.0 * INSET));
        x += w;
        let task = i == 1;
        let on = task == state.task_mode;
        let disabled = task && unavailable.is_some();
        let resp = ui.interact(
            seg,
            ui.scope_id().with(("picker-target", i)),
            if disabled {
                Sense::hover()
            } else {
                Sense::click()
            },
        );
        let hot =
            ui.ctx()
                .animate_bool_with_time(resp.id.with("hot"), resp.hovered() && !disabled, 0.08);
        let p = ui.painter();
        let fg = if disabled {
            ink.faint()
        } else if on {
            ink.accent_text()
        } else {
            theme::mix(ink.dim(), ink.strong(), hot)
        };
        if on {
            p.rect(
                seg,
                theme::rad(6, 10),
                ink.wash(ink.b),
                Stroke::new(1.0, ink.edge(ink.b)),
                StrokeKind::Inside,
            );
        } else if hot > 0.0 {
            pop(p, seg, theme::rad(6, 10), ink, hot);
        }
        let g = p.layout_no_wrap((*label).to_owned(), font.clone(), fg);
        p.galley(seg.center() - g.size() / 2.0, g, fg);
        let tip = match (task, unavailable) {
            (true, Some(why)) => why,
            (true, None) => "Choose the model task agents use (this session only)",
            (false, _) => "Switch the model of this session",
        };
        let clicked = resp.clicked();
        resp.on_hover_text(tip).on_hover_cursor(if disabled {
            CursorIcon::NotAllowed
        } else {
            CursorIcon::PointingHand
        });
        if clicked && !on {
            state.task_mode = task;
            state.refocus_search = true;
        }
    }
    plate.left()
}

/// The sheet's header: title and model count on the accent-washed band, the target switch (session
/// picker) and the reload button at its right.
fn head_band(
    ui: &mut Ui,
    ink: &Ink,
    input: &PickerInput<'_>,
    state: &mut PickerState,
    extras: Option<&SessionExtras<'_>>,
    actions: &mut Vec<PickerAction>,
) {
    let (band, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEAD_H), Sense::hover());
    let r = f32::from(theme::popup_radius()).min(HEAD_H / 2.0);
    let (c0, c1) = ink.head();
    let p = ui.painter();
    crate::deco::fill_linear(p, &band_points(band, r), c0, c1, vec2(1.0, 0.0));
    hairline(
        p,
        ink,
        band.left() + r,
        band.right() - r,
        band.top() + 0.5,
        0.9,
    );
    p.line_segment(
        [
            pos2(band.left(), band.bottom() - 0.5),
            pos2(band.right(), band.bottom() - 0.5),
        ],
        Stroke::new(1.0, ink.line()),
    );
    let reload = egui::Rect::from_center_size(
        pos2(band.right() - f32::from(BODY_PAD) - 13.0, band.center().y),
        vec2(26.0, 26.0),
    );
    let free_right = match extras {
        Some(e) => target_toggle(
            ui,
            ink,
            band,
            reload.left() - 8.0,
            state,
            e.task_unavailable,
        ),
        None => reload.left(),
    };
    let p = ui.painter();
    let title = p.layout_no_wrap(
        if state.task_mode {
            "Task model"
        } else {
            "Model"
        }
        .to_owned(),
        ink.label(13.0),
        ink.strong(),
    );
    let x = band.left() + f32::from(BODY_PAD);
    let after = x + title.size().x + 12.0;
    p.galley(
        pos2(x, band.center().y - title.size().y / 2.0),
        title,
        ink.strong(),
    );
    let n = input.catalog.all().len();
    if n > 0 {
        let count = p.layout_no_wrap(format!("{n} models"), ink.label(12.0), ink.dim());
        // The count gives way to the target switch in a narrow window.
        if after + count.size().x + 12.0 <= free_right {
            p.galley(
                pos2(after, band.center().y - count.size().y / 2.0),
                count,
                ink.dim(),
            );
        }
    }

    // Reload.
    let rect = reload;
    let resp = ui.interact(rect, ui.scope_id().with("picker-reload"), Sense::click());
    let hot = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hot"), resp.hovered(), 0.08);
    let spinning = input.catalog.is_loading();
    let p = ui.painter();
    if hot > 0.0 {
        p.rect(
            rect,
            theme::rad(6, 8),
            ink.chip().gamma_multiply(hot),
            Stroke::new(1.0, ink.line().gamma_multiply(hot)),
            StrokeKind::Inside,
        );
        pop(p, rect, theme::rad(6, 8), ink, hot);
    }
    paint_icon(
        p,
        rect.center(),
        Align2::CENTER_CENTER,
        ICON_REFRESH_CW,
        16.0,
        if spinning {
            ink.accent_text()
        } else {
            theme::mix(ink.dim(), ink.strong(), hot)
        },
    );
    if spinning {
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
    if resp
        .on_hover_text("Reload the model list (omp models)")
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
    {
        actions.push(PickerAction::Refresh);
    }
}

fn body(
    ui: &mut Ui,
    ink: &Ink,
    state: &mut PickerState,
    input: &PickerInput<'_>,
    extras: Option<&SessionExtras<'_>>,
    just_opened: bool,
    actions: &mut Vec<PickerAction>,
) {
    ui.spacing_mut().item_spacing = vec2(6.0, 8.0);

    search_field(ui, ink, state, just_opened);
    state.refresh_providers(input.catalog);
    provider_dropdown(ui, ink, state, input);
    let catalog = input.catalog;
    // The model task agents run on now: what omp last answered for this tab, else its config.
    let task_now = state.task_now(catalog, extras.map(|e| e.tab));
    let task_mode = state.task_mode;
    if task_mode {
        task_summary(ui, ink, catalog, task_now.as_deref());
    } else {
        // Thinking pills for the session's current model.
        let current_model = input.current.and_then(|s| catalog.get(s));
        let options = level_options(current_model);
        ui.label(
            RichText::new("THINKING")
                .font(ink.label(11.0))
                .color(ink.dim()),
        );
        if options.is_empty() {
            let text = match (input.current, current_model) {
                (None, _) => input.unset,
                (Some(_), None) => "Current model is not in the catalog",
                (Some(_), Some(_)) => "This model has no adjustable thinking",
            };
            ui.label(RichText::new(text).font(ink.prose(12.5)).color(ink.dim()));
        } else {
            let selected = state
                .staged_level
                .clone()
                .or_else(|| input.level.map(str::to_owned));
            if let Some(l) = pills(ui, ink, &options, selected.as_deref()) {
                state.staged_level = Some(l.clone());
                if selected.as_deref() != Some(l.as_str()) {
                    actions.push(PickerAction::Effort(l));
                }
            }
        }
    }
    ui.add_space(2.0);

    // Model list. Its height is fixed for the whole popup session (egui keeps the popup size
    // measured when it opened), sized for the longest list this catalog can produce: every model,
    // favorites repeated, headers. The provider options replace it in place, so opening them never
    // resizes (and thus never moves) the popup.
    let longest = catalog.all().len()
        + input.favorites.len()
        + catalog.roles().len()
        + state.providers.len()
        + 4;
    let fixed_list_h = LIST_H.min(ROW_H * longest as f32);
    if state.provider_open && !state.providers.is_empty() {
        provider_options(ui, ink, state, input, actions, fixed_list_h);
        return;
    }
    match &catalog.state {
        LoadState::Loading | LoadState::NotLoaded if catalog.all().is_empty() => {
            ui.horizontal(|ui| {
                ui.add(crate::widgets::Spin::new(14.0, ink.accent_text()));
                ui.label(
                    RichText::new("Loading models…")
                        .font(ink.prose(13.5))
                        .color(ink.dim()),
                );
            });
            if input.favorites.is_empty() {
                return;
            }
        }
        LoadState::Failed(e) => {
            ui.horizontal_wrapped(|ui| {
                ui.label(icon_text(ICON_CIRCLE_ALERT, 16.0, ink.p.danger));
                ui.label(
                    RichText::new(format!("Could not list models: {e}"))
                        .font(ink.prose(13.5))
                        .color(ink.p.danger),
                );
            });
            if input.favorites.is_empty() {
                return;
            }
        }
        _ => {}
    }
    state.refresh_rows(catalog, input, extras.is_some());
    if state.rows.is_empty() {
        ui.horizontal(|ui| {
            ui.label(icon_text(ICON_SEARCH, 16.0, ink.dim()));
            let roles_only = role_query(&state.search).is_some()
                && !catalog.roles().is_empty()
                && extras.is_some()
                && !task_mode;
            let text = if catalog.all().is_empty() && !roles_only {
                "omp lists no models (no provider configured?)"
            } else if roles_only {
                "No role matches"
            } else {
                "No model matches"
            };
            ui.label(RichText::new(text).font(ink.prose(13.5)).color(ink.dim()));
        });
        return;
    }
    ui.spacing_mut().item_spacing.y = 0.0;
    let list_h = fixed_list_h;
    let mut task_pick: Option<Option<String>> = None;
    egui::ScrollArea::vertical()
        // A new query, provider or reopened popup starts at the top.
        .id_salt((
            "model-list",
            state.opens,
            state.search.as_str(),
            input.provider,
        ))
        .min_scrolled_height(list_h)
        .max_height(list_h)
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, state.rows.len(), |ui, range| {
            for i in range {
                let (idx, section) = match &state.rows[i] {
                    Row::Header(p) => {
                        header_row(ui, ink, p, None);
                        continue;
                    }
                    Row::FavoritesHeader => {
                        header_row(ui, ink, "Favorites", Some(ICON_STAR));
                        continue;
                    }
                    Row::RolesHeader => {
                        header_row(ui, ink, "Roles", None);
                        continue;
                    }
                    Row::Role(role) => {
                        if let Some(entry) = catalog.roles().get(*role) {
                            let model = entry.target().map(|(m, _)| m);
                            let info = model.and_then(|m| catalog.get(m));
                            let current = model.is_some() && input.current == model;
                            if role_row(ui, ink, entry, info, current) {
                                actions.push(role_action(entry));
                            }
                        }
                        continue;
                    }
                    Row::ClearOverride => {
                        if clear_row(ui, ink) {
                            task_pick = Some(None);
                        }
                        continue;
                    }
                    Row::UnavailableFavorite(favorite) => {
                        if let Some(selector) = input.favorites.get(*favorite)
                            && unavailable_row(ui, ink, selector)
                        {
                            actions.push(PickerAction::ToggleFavorite(selector.clone()));
                        }
                        continue;
                    }
                    Row::Favorite(idx) => (*idx, 0),
                    Row::Model(idx) => (*idx, 1),
                };
                let Some(m) = catalog.all().get(idx) else {
                    continue;
                };
                let is_current = if task_mode {
                    task_now.as_deref() == Some(m.selector.as_str())
                } else {
                    input.current == Some(m.selector.as_str())
                };
                let starred = input.favorites.contains(&m.selector);
                let show_provider = section == 0 && input.provider.is_none();
                let out = model_row(ui, ink, m, is_current, starred, section, show_provider);
                if out.star_clicked {
                    actions.push(PickerAction::ToggleFavorite(m.selector.clone()));
                } else if out.clicked && !is_current {
                    if task_mode {
                        task_pick = Some(Some(m.selector.clone()));
                    } else {
                        actions.push(PickerAction::Switch {
                            selector: m.selector.clone(),
                            level: level_for_switch(m, state.staged_level.as_deref()),
                        });
                    }
                }
            }
        });
    state.task_pick = task_pick;
}

/// What picking a quick role sends: `/switch @role`, omp resolving the alias (and the role's own
/// thinking level) itself.
fn role_action(entry: &RoleEntry) -> PickerAction {
    PickerAction::Switch {
        selector: entry.selector(),
        level: None,
    }
}

/// Task mode's summary under the search: the model task agents run on now.
fn task_summary(ui: &mut Ui, ink: &Ink, catalog: &ModelsCache, now: Option<&str>) {
    ui.label(
        RichText::new("TASK AGENTS")
            .font(ink.label(11.0))
            .color(ink.dim()),
    );
    match now {
        Some(selector) => {
            let name = catalog.get(selector).map_or_else(
                || selector.to_owned(),
                |m| format!("{} ({selector})", short_model_name(&m.name)),
            );
            ui.label(
                RichText::new(format!("Run on {name}"))
                    .font(ink.prose(13.0))
                    .color(ink.strong()),
            );
        }
        None => {
            ui.label(
                RichText::new("No override: task agents use the session's model")
                    .font(ink.prose(12.5))
                    .color(ink.dim()),
            );
        }
    }
    ui.label(
        RichText::new("Applies to this session only. Use /agents to persist.")
            .font(ink.prose(12.0))
            .color(ink.dim()),
    );
}

/// One quick-role row: `@role`, the thinking level of its model and the model itself. True when
/// clicked.
fn role_row(
    ui: &mut Ui,
    ink: &Ink,
    entry: &RoleEntry,
    model: Option<&ModelInfo>,
    current: bool,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.08);
    let p = ui.painter();
    row_plate(p, rect.shrink2(vec2(0.0, 1.0)), ink, current, hov);
    let left = rect.left() + 12.0;
    if current {
        paint_icon(
            p,
            pos2(left + 8.0, rect.center().y),
            Align2::CENTER_CENTER,
            ICON_CHECK,
            17.0,
            ink.p.ok,
        );
    }
    let name_x = left + 24.0;
    let (top_y, bottom_y) = (rect.center().y - 7.0, rect.center().y + 8.0);
    let right = rect.right() - 12.0;
    let mut name_right = right;
    if let Some((_, Some(level))) = entry.target() {
        let g = p.layout_no_wrap(
            thinking_short(level).to_owned(),
            ink.label(11.0),
            ink.text(),
        );
        let pill = egui::Rect::from_min_size(
            pos2(right - g.size().x - 10.0, top_y - 8.0),
            vec2(g.size().x + 10.0, 16.0),
        );
        p.rect(
            pill,
            theme::rad(4, 8),
            ink.chip(),
            Stroke::new(1.0, ink.line()),
            StrokeKind::Inside,
        );
        p.galley(pill.center() - g.size() / 2.0, g, ink.text());
        name_right = pill.left() - 10.0;
    }
    let g = fitted(
        p,
        &entry.selector(),
        ink.prose(13.5),
        if current {
            ink.strong()
        } else {
            ink.accent_text()
        },
        name_right - name_x,
    );
    p.galley(
        pos2(name_x, top_y - g.size().y / 2.0),
        g,
        Color32::PLACEHOLDER,
    );
    let detail = match (model, entry.target()) {
        (Some(m), _) => format!("{} · {}", short_model_name(&m.name), m.selector),
        (None, Some((selector, _))) => format!("{selector} · not in the model list"),
        (None, None) => "Not set: omp picks its own model".to_owned(),
    };
    let g = fitted(p, &detail, ink.prose(12.0), ink.dim(), right - name_x);
    p.galley(
        pos2(name_x, bottom_y - g.size().y / 2.0),
        g,
        Color32::PLACEHOLDER,
    );
    let tip = format!(
        "/switch {}\n{}",
        entry.selector(),
        match &entry.model {
            Some(m) => format!("Role model: {m}"),
            None => "The role has no model in omp's config".to_owned(),
        }
    );
    resp.on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
}

/// Task mode's first row: drops the session-only task model override. True when clicked.
fn clear_row(ui: &mut Ui, ink: &Ink) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.08);
    let p = ui.painter();
    row_plate(p, rect.shrink2(vec2(0.0, 1.0)), ink, false, hov);
    paint_icon(
        p,
        pos2(rect.left() + 20.0, rect.center().y),
        Align2::CENTER_CENTER,
        ICON_UNDO_2,
        17.0,
        ink.accent_text(),
    );
    let x = rect.left() + 36.0;
    p.text(
        pos2(x, rect.center().y - 7.0),
        Align2::LEFT_CENTER,
        "Clear override",
        ink.prose(13.5),
        ink.text(),
    );
    let g = fitted(
        p,
        "Task agents go back to the configured model, else the session's",
        ink.prose(12.0),
        ink.dim(),
        rect.right() - 12.0 - x,
    );
    p.galley(
        pos2(x, rect.center().y + 8.0 - g.size().y / 2.0),
        g,
        Color32::PLACEHOLDER,
    );
    resp.on_hover_text("Drop the session-only task model override")
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
}

/// A group label in the chat view's picker voice: the group's name in small accent capitals with
/// a hairline running out to the edge.
fn header_row(ui: &mut Ui, ink: &Ink, label: &str, icon: Option<Icon>) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
    let p = ui.painter();
    let mut x = rect.left() + 6.0;
    let y = rect.bottom() - 13.0;
    if let Some(icon) = icon {
        paint_icon(
            p,
            pos2(x, y),
            Align2::LEFT_CENTER,
            icon,
            14.0,
            if theme::flat() {
                ink.p.warn
            } else {
                ink.accent_text()
            },
        );
        x += 19.0;
    }
    let g = p.text(
        pos2(x, y),
        Align2::LEFT_CENTER,
        label.to_uppercase(),
        ink.label(11.0),
        ink.accent_text(),
    );
    p.line_segment(
        [pos2(g.right() + 10.0, y), pos2(rect.right() - 6.0, y)],
        Stroke::new(1.0, ink.line()),
    );
}

/// What the user did to one model row.
#[derive(Default)]
struct RowOutcome {
    clicked: bool,
    star_clicked: bool,
}

/// Width reserved at the right edge of a row for the star toggle.
const STAR_W: f32 = 30.0;

/// Corner points of a five-pointed star, for the empty (outline) state.
fn star_points(center: egui::Pos2, outer: f32) -> Vec<egui::Pos2> {
    let inner = outer * 0.45;
    (0..10)
        .map(|i| {
            let r = if i % 2 == 0 { outer } else { inner };
            let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
            center + vec2(a.cos(), a.sin()) * r
        })
        .collect()
}

/// `text` on one line, ellipsized to `max_w` (at least a few pixels).
fn fitted(
    p: &egui::Painter,
    text: &str,
    font: FontId,
    color: Color32,
    max_w: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), font, color, max_w.max(12.0));
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    p.layout_job(job)
}

/// One model row. `section` keeps the ids of a model listed under Favorites and its provider apart.
fn model_row(
    ui: &mut Ui,
    ink: &Ink,
    m: &ModelInfo,
    current: bool,
    starred: bool,
    section: u8,
    show_provider: bool,
) -> RowOutcome {
    let pal = ink.p;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let star_rect = egui::Rect::from_min_max(
        pos2(rect.right() - STAR_W, rect.top()),
        pos2(rect.right() - 2.0, rect.bottom()),
    );
    let star = ui.interact(
        star_rect,
        ui.scope_id().with(("star", section, &m.selector)),
        Sense::click(),
    );
    let hov = ui.ctx().animate_bool_with_time(
        resp.id.with("hov"),
        resp.hovered() || star.hovered(),
        0.08,
    );
    let p = ui.painter();
    row_plate(p, rect.shrink2(vec2(0.0, 1.0)), ink, current, hov);
    let left = rect.left() + 12.0;
    if current {
        paint_icon(
            p,
            pos2(left + 8.0, rect.center().y),
            Align2::CENTER_CENTER,
            ICON_CHECK,
            17.0,
            pal.ok,
        );
    }
    let name_x = left + 24.0;
    let mid = rect.center().y;
    let (top_y, bottom_y) = (mid - 7.0, mid + 8.0);
    let meta = ink.label(11.0);
    let right = star_rect.left() - 4.0;
    // Right side first, one block per text line; each block's left edge bounds that line.
    // Top: speed, then the intelligence pill (beside the name).
    let mut top_x = right;
    if let Some(speed) = speed_label(m) {
        let g = p.text(
            pos2(top_x, top_y),
            Align2::RIGHT_CENTER,
            speed,
            meta.clone(),
            ink.dim(),
        );
        top_x = g.left() - 8.0;
    }
    if let Some(iq) = m.intelligence {
        let g = p.layout_no_wrap(format!("IQ {}", iq.round()), meta.clone(), ink.text());
        let pill = egui::Rect::from_min_size(
            pos2(top_x - g.size().x - 10.0, top_y - 8.0),
            vec2(g.size().x + 10.0, 16.0),
        );
        p.rect(
            pill,
            theme::rad(4, 8),
            ink.chip(),
            Stroke::new(1.0, ink.line()),
            StrokeKind::Inside,
        );
        p.galley(pill.center() - g.size() / 2.0, g, ink.text());
        top_x = pill.left() - 10.0;
    }
    // Bottom: $/M in/out, then the context window (beside the id).
    let mut bottom_x = right;
    if let Some(price) = price_label(m) {
        let g = p.text(
            pos2(bottom_x, bottom_y),
            Align2::RIGHT_CENTER,
            price,
            meta.clone(),
            ink.dim(),
        );
        bottom_x = g.left() - 10.0;
    }
    if let Some(w) = m.context_window {
        let g = p.text(
            pos2(bottom_x, bottom_y),
            Align2::RIGHT_CENTER,
            format!("{} ctx", fmt_tokens(w)),
            meta,
            ink.text(),
        );
        bottom_x = g.left() - 10.0;
    }
    let g = fitted(
        p,
        short_model_name(&m.name),
        ink.prose(13.5),
        if current { ink.strong() } else { ink.text() },
        top_x - name_x,
    );
    p.galley(
        pos2(name_x, top_y - g.size().y / 2.0),
        g,
        Color32::PLACEHOLDER,
    );
    let mut id_x = name_x;
    if show_provider {
        // Favorites mix providers: say which one each belongs to.
        let g = fitted(
            p,
            &m.provider,
            theme::label_font(10.0),
            ink.accent_text(),
            (bottom_x - name_x - 10.0).min(120.0),
        );
        let tag =
            egui::Rect::from_min_size(pos2(name_x, bottom_y - 8.0), vec2(g.size().x + 10.0, 15.0));
        p.rect(
            tag,
            theme::rad(4, 7),
            ink.wash(ink.b),
            Stroke::new(1.0, ink.edge(ink.b)),
            StrokeKind::Inside,
        );
        p.galley(
            pos2(tag.left() + 5.0, tag.center().y - g.size().y / 2.0),
            g,
            Color32::PLACEHOLDER,
        );
        id_x = tag.right() + 6.0;
    }
    let g = fitted(p, &m.id, ink.mono_px(10.5), ink.dim(), bottom_x - id_x);
    p.galley(
        pos2(id_x, bottom_y - g.size().y / 2.0),
        g,
        Color32::PLACEHOLDER,
    );
    let sh = ui
        .ctx()
        .animate_bool_with_time(star.id.with("hov"), star.hovered(), 0.08);
    let p = ui.painter();
    let center = star_rect.center();
    if starred {
        paint_icon(
            p,
            center,
            Align2::CENTER_CENTER,
            ICON_STAR,
            19.0 + 2.0 * sh,
            if theme::flat() {
                pal.warn
            } else {
                ink.accent_text()
            },
        );
    } else {
        let color = theme::mix(
            pal.weak.gamma_multiply(0.7),
            if theme::flat() {
                pal.warn
            } else {
                ink.accent_text()
            },
            sh,
        );
        p.add(egui::Shape::closed_line(
            star_points(center, 8.0 + 1.5 * sh),
            Stroke::new(1.4, color),
        ));
    }
    let star = star
        .on_hover_text(if starred {
            "Remove from favorites"
        } else {
            "Add to favorites"
        })
        .on_hover_cursor(CursorIcon::PointingHand);
    let star_clicked = star.clicked();
    let clicked = resp
        .on_hover_text(model_details(m))
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
        && !star_clicked;
    RowOutcome {
        clicked,
        star_clicked,
    }
}

/// Saved favorites outlive credentials and catalog changes. Only the star is interactive:
/// an unavailable model must never send a switch command.
fn unavailable_row(ui: &mut Ui, ink: &Ink, selector: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
    let star_rect = egui::Rect::from_min_max(
        pos2(rect.right() - STAR_W, rect.top()),
        pos2(rect.right() - 2.0, rect.bottom()),
    );
    let star = ui.interact(
        star_rect,
        ui.scope_id().with(("unavailable-star", selector)),
        Sense::click(),
    );
    let hot = ui.ctx().animate_bool_with_time(
        star.id.with("hot"),
        resp.hovered() || star.hovered(),
        0.08,
    );
    let p = ui.painter();
    row_plate(p, rect.shrink2(vec2(0.0, 1.0)), ink, false, hot);
    let left = rect.left() + 12.0;
    let text = fitted(
        p,
        selector,
        ink.prose(13.0),
        ink.dim(),
        star_rect.left() - left - 4.0,
    );
    p.galley(
        pos2(left, rect.center().y - 15.0),
        text,
        Color32::PLACEHOLDER,
    );
    p.text(
        pos2(left, rect.center().y + 8.0),
        Align2::LEFT_CENTER,
        "Not available now",
        ink.label(10.5),
        ink.dim(),
    );
    paint_icon(
        p,
        star_rect.center(),
        Align2::CENTER_CENTER,
        ICON_STAR,
        19.0 + 2.0 * hot,
        theme::mix(ink.dim(), ink.accent_text(), hot),
    );
    resp.on_hover_text(
        "This saved favorite is unavailable. Sign in to its provider or reload the model list.",
    );
    star.on_hover_text("Remove from favorites")
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
}

/// Segmented pills in the chat view's chip look; returns the clicked value.
fn pills(ui: &mut Ui, ink: &Ink, options: &[String], selected: Option<&str>) -> Option<String> {
    let mut clicked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
        for opt in options {
            let on = selected == Some(opt.as_str());
            let label = thinking_short(opt);
            let font = ink.label(12.0);
            let g = ui
                .painter()
                .layout_no_wrap(label.to_owned(), font.clone(), ink.text());
            let size = vec2(g.size().x + 22.0, 26.0);
            let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
            let hot = ui
                .ctx()
                .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.08);
            let p = ui.painter();
            let radius = theme::rad(8, 12);
            let (fill, fg, stroke) = if on {
                (ink.wash(ink.b), ink.accent_text(), ink.edge(ink.b))
            } else {
                (
                    ink.chip().gamma_multiply(0.85 + 0.15 * hot),
                    theme::mix(ink.dim(), ink.strong(), hot),
                    theme::mix(ink.line(), ink.b, 0.55 * hot),
                )
            };
            p.rect(
                rect,
                radius,
                fill,
                Stroke::new(1.0, stroke),
                StrokeKind::Inside,
            );
            pop(p, rect, radius, ink, hot);
            let g = p.layout_no_wrap(label.to_owned(), font, fg);
            p.galley(rect.center() - g.size() / 2.0, g, fg);
            if resp
                .on_hover_text(format!("/effort {opt}"))
                .on_hover_cursor(CursorIcon::PointingHand)
                .clicked()
            {
                clicked = Some(opt.clone());
            }
        }
    });
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, id: &str, name: &str, thinking: &[&str]) -> ModelInfo {
        ModelInfo {
            selector: format!("{provider}/{id}"),
            provider: provider.to_owned(),
            id: id.to_owned(),
            name: name.to_owned(),
            context_window: Some(100_000),
            thinking: thinking.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn signed_out_provider_favorites_stay_visible() {
        let favorites = favs(&["anthropic/claude-opus-5-5"]);
        let rows = build_rows(&[], "", None, &favorites);
        assert_eq!(rows.first(), Some(&Row::FavoritesHeader));
        assert_eq!(
            rows.len(),
            2,
            "the saved favorite remains visible without its provider"
        );
        assert_eq!(favorites, ["anthropic/claude-opus-5-5"]);
        assert_eq!(
            build_rows(&[], "OPUS anthropic", Some("anthropic"), &favorites),
            rows
        );
        assert!(build_rows(&[], "sonnet", None, &favorites).is_empty());
        assert!(build_rows(&[], "", Some("other"), &favorites).is_empty());
        let mut config = deck_core::config::AppConfig {
            favorite_models: favorites,
            ..Default::default()
        };
        config.toggle_favorite("other/other-model");
        let returned = vec![model("anthropic", "claude-opus-5-5", "Opus", &[])];
        assert_eq!(
            &build_rows(&returned, "", None, &config.favorite_models)[..3],
            &[
                Row::FavoritesHeader,
                Row::Favorite(0),
                Row::UnavailableFavorite(1)
            ]
        );
        config.toggle_favorite("other/other-model");
        assert_eq!(config.favorite_models, ["anthropic/claude-opus-5-5"]);
    }

    #[test]
    fn opening_picker_refreshes_auth_dependent_catalog_once() {
        let ctx = egui::Context::default();
        crate::ui_fonts::install(&ctx, crate::ui_fonts::UiFonts::none());
        let mut cache = ModelsCache::default();
        cache.finish(Ok(catalog()));
        let input = PickerInput {
            catalog: &cache,
            current: None,
            level: None,
            favorites: &[],
            provider: None,
            unset: "No current model",
        };
        let mut state = PickerState::default();
        state.request_open();
        for expected in [true, false] {
            let mut actions = Vec::new();
            ctx.run_ui(egui::RawInput::default(), |ui| {
                let chip = ui.button("Models");
                actions.extend(show(&chip, &mut state, &input));
            })
            .textures_delta
            .clear();
            assert_eq!(actions.contains(&PickerAction::Refresh), expected);
        }
    }

    #[test]
    fn unavailable_favorites_render_while_loading_or_failed() {
        for load in [
            LoadState::Ready,
            LoadState::Loading,
            LoadState::Failed("offline".into()),
        ] {
            let ctx = egui::Context::default();
            crate::ui_fonts::install(&ctx, crate::ui_fonts::UiFonts::none());
            let cache = ModelsCache {
                state: load,
                ..Default::default()
            };
            let favorites = favs(&["anthropic/claude-opus-5-5"]);
            let input = PickerInput {
                catalog: &cache,
                current: None,
                level: None,
                favorites: &favorites,
                provider: None,
                unset: "No current model",
            };
            let mut state = PickerState::default();
            ctx.run_ui(egui::RawInput::default(), |ui| {
                let ink = Ink::new(ui, 13.0);
                body(ui, &ink, &mut state, &input, None, false, &mut Vec::new());
            })
            .textures_delta
            .clear();
            assert_eq!(
                state.rows,
                [Row::FavoritesHeader, Row::UnavailableFavorite(0)],
                "{:?}",
                cache.state
            );
        }
    }

    #[test]
    fn palette_lists_starred_models_first_and_once() {
        let models = vec![
            model("beta", "b1", "B", &[]),
            model("alpha", "a1", "A", &[]),
            model("alpha", "a2", "A2", &[]),
        ];
        let favs = vec![
            "alpha/a2".to_owned(),
            "beta/b1".to_owned(),
            "gone/x".to_owned(),
        ];
        let list = palette_models(&models, &favs);
        assert_eq!(list[..2], [(2, true), (0, true)]);
        let mut rest: Vec<usize> = list[2..].iter().map(|(i, _)| *i).collect();
        assert!(list[2..].iter().all(|(_, star)| !star));
        rest.sort_unstable();
        assert_eq!(rest, vec![1], "a starred model is not listed again");
    }

    fn catalog() -> Vec<ModelInfo> {
        vec![
            model("zeta", "z1", "Zed One", &[]),
            model(
                "anthropic",
                "claude-opus-5-5",
                "Claude Opus 5.5",
                &["low", "high"],
            ),
            model("anthropic", "claude-haiku", "Claude Haiku", &["low"]),
        ]
    }

    fn favs(sels: &[&str]) -> Vec<String> {
        sels.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn rows_group_by_provider_newest_first() {
        let rows = build_rows(&catalog(), "", None, &[]);
        assert_eq!(
            rows,
            vec![
                Row::Header("anthropic".to_owned()),
                Row::Model(1),
                Row::Model(2),
                Row::Header("zeta".to_owned()),
                Row::Model(0),
            ]
        );
    }

    #[test]
    fn search_requires_every_word_and_drops_empty_groups() {
        let c = catalog();
        assert_eq!(
            build_rows(&c, "OPUS 5.5", None, &[]),
            vec![Row::Header("anthropic".to_owned()), Row::Model(1)]
        );
        assert_eq!(
            build_rows(&c, "zeta one", None, &[]),
            vec![Row::Header("zeta".to_owned()), Row::Model(0)]
        );
        assert!(build_rows(&c, "opus zed", None, &[]).is_empty());
    }

    #[test]
    fn provider_filter_limits_groups() {
        let c = catalog();
        assert_eq!(
            build_rows(&c, "", Some("zeta"), &[]),
            vec![Row::Header("zeta".to_owned()), Row::Model(0)]
        );
        assert!(build_rows(&c, "", Some("nobody"), &[]).is_empty());
        assert_eq!(
            provider_counts(&c),
            vec![("anthropic".to_owned(), 2), ("zeta".to_owned(), 1)]
        );
    }

    #[test]
    fn row_metadata_omits_unknowns_and_prefers_measured_speed() {
        let mut m = model("p", "m", "M", &[]);
        // omp's all-zero card it cannot call free: no "$0 / $0", no speed or IQ.
        m.cost_input = Some(0.0);
        m.cost_output = Some(0.0);
        m.pricing = Some(Pricing::Unknown);
        assert_eq!((price_label(&m), speed_label(&m)), (None, None));
        assert!(!model_details(&m).contains("Intelligence"));
        m.pricing = Some(Pricing::Fixed);
        assert_eq!(price_label(&m).as_deref(), Some("$0 / $0"));
        m.catalog_tps = Some(7.26);
        assert_eq!(speed_label(&m).as_deref(), Some("~7.3 t/s"));
        m.measured = Some(deck_core::omp::MeasuredSpeed {
            tps: 118.4,
            ttft_ms: Some(900.0),
            samples: 1.0,
            tier: None,
        });
        assert_eq!(speed_label(&m).as_deref(), Some("118 t/s"));
    }

    #[test]
    fn favorites_lead_in_star_order_and_obey_filter_and_search() {
        let c = catalog();
        let f = favs(&["zeta/z1", "anthropic/claude-haiku", "gone/missing"]);
        let rows = build_rows(&c, "", None, &f);
        assert_eq!(
            rows[..4],
            [
                Row::FavoritesHeader,
                Row::Favorite(0),
                Row::Favorite(2),
                Row::UnavailableFavorite(2),
            ]
        );
        // Still listed in their provider groups.
        assert_eq!(rows.len(), 4 + 5);
        // Provider filter hides other providers' favorites.
        assert_eq!(
            build_rows(&c, "", Some("anthropic"), &f)[..2],
            [Row::FavoritesHeader, Row::Favorite(2)]
        );
        // Search narrows the favorites; no match, no section.
        let rows = build_rows(&c, "zed", None, &f);
        assert_eq!(rows[..2], [Row::FavoritesHeader, Row::Favorite(0)]);
        assert!(!build_rows(&c, "opus", None, &f).contains(&Row::FavoritesHeader));
    }

    #[test]
    fn unknown_provider_filter_falls_back_to_all() {
        let mut s = PickerState::default();
        let cache = {
            let mut c = ModelsCache::default();
            c.finish(Ok(catalog()));
            c
        };
        s.refresh_providers(&cache);
        assert_eq!(s.effective_provider(Some("zeta")), Some("zeta"));
        assert_eq!(s.effective_provider(Some("retired")), None);
        assert_eq!(s.effective_provider(None), None);
    }

    #[test]
    fn level_options_add_off_and_auto() {
        let c = catalog();
        assert_eq!(level_options(Some(&c[1])), ["off", "auto", "low", "high"]);
        assert!(level_options(Some(&c[0])).is_empty());
        assert!(level_options(None).is_empty());
    }

    #[test]
    fn staged_level_only_applies_when_supported() {
        let c = catalog();
        assert_eq!(
            level_for_switch(&c[1], Some("high")).as_deref(),
            Some("high")
        );
        assert_eq!(level_for_switch(&c[1], Some("off")).as_deref(), Some("off"));
        assert_eq!(level_for_switch(&c[1], Some("xhigh")), None);
        assert_eq!(level_for_switch(&c[0], Some("high")), None);
        assert_eq!(level_for_switch(&c[1], None), None);
    }
}

#[cfg(test)]
mod roles_and_task_tests {
    use deck_core::omp::{Setting, SettingKind, SettingsTab};
    use serde_json::json;

    use super::*;

    fn role(name: &str, model: Option<&str>) -> RoleEntry {
        RoleEntry {
            role: name.to_owned(),
            model: model.map(str::to_owned),
        }
    }

    fn model(provider: &str, id: &str) -> ModelInfo {
        ModelInfo {
            selector: format!("{provider}/{id}"),
            provider: provider.to_owned(),
            id: id.to_owned(),
            name: id.to_owned(),
            ..Default::default()
        }
    }

    fn catalog_with(settings: Vec<(&str, SettingKind, Value)>) -> SettingsCatalog {
        SettingsCatalog {
            tabs: vec![SettingsTab {
                id: "model".into(),
                label: "Model".into(),
                settings: settings
                    .into_iter()
                    .map(|(key, kind, value)| Setting {
                        key: key.into(),
                        kind,
                        value: Some(value),
                        default: None,
                        description: String::new(),
                        secret: false,
                    })
                    .collect(),
            }],
        }
    }

    /// The role names in list order.
    fn names(roles: &[RoleEntry]) -> Vec<&str> {
        roles.iter().map(|r| r.role.as_str()).collect()
    }

    #[test]
    fn role_list_is_the_builtins_then_custom_roles_without_duplicates() {
        let roles = roles_from_config(Some(&json!({
            "smol": "a/b:low",
            "review": "c/d",
            "default": "x/y",
            "commit": "e/f",
            // Not chat roles, not typable, or not configured: never offered.
            "image": "g/h",
            "tiny": "i/j",
            "bad name": "k/l",
            "a:b": "m/n",
            "blank": "  ",
            "number": 3,
        })));
        assert_eq!(
            names(&roles),
            ["default", "smol", "slow", "commit", "review"]
        );
        assert_eq!(roles[0].model.as_deref(), Some("x/y"));
        assert_eq!(roles[1].model.as_deref(), Some("a/b:low"));
        assert_eq!(
            roles[2].model, None,
            "an unconfigured built-in is still listed"
        );
        let mut unique = names(&roles);
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), roles.len());

        for nothing in [None, Some(&json!(null)), Some(&json!([1, 2]))] {
            let roles = roles_from_config(nothing);
            assert_eq!(names(&roles), ["default", "smol", "slow"]);
            assert!(roles.iter().all(|r| r.model.is_none()));
        }
    }

    #[test]
    fn roles_and_task_model_come_from_the_settings_catalog() {
        let mut cache = ModelsCache::default();
        let list = catalog_with(vec![
            (
                "modelRoles",
                SettingKind::Record,
                json!({"default": "a/b", "plan": "c/d:high"}),
            ),
            (
                "task.agentModelOverrides",
                SettingKind::Record,
                json!({"task": ["e/f", "g/h"], "explore": "i/j"}),
            ),
        ]);
        cache.set_roles(&list);
        assert_eq!(names(cache.roles()), ["default", "smol", "slow", "plan"]);
        assert_eq!(cache.task_model.as_deref(), Some("e/f"));
        let generation = cache.generation;
        cache.set_roles(&list);
        assert_eq!(
            cache.generation, generation,
            "unchanged roles keep the cached rows"
        );

        cache.set_roles(&catalog_with(vec![(
            "task.agentModelOverrides",
            SettingKind::Record,
            json!({"task": "k/l"}),
        )]));
        assert_eq!(names(cache.roles()), ["default", "smol", "slow"]);
        assert_eq!(cache.task_model.as_deref(), Some("k/l"));
        assert!(cache.generation > generation);
        cache.set_roles(&catalog_with(vec![]));
        assert_eq!(cache.task_model, None);
    }

    #[test]
    fn an_at_search_lists_only_the_matching_roles_prefix_matches_first() {
        let roles = vec![
            role("default", Some("a/b")),
            role("smol", Some("a/c")),
            role("slow", None),
            role("lint", Some("a/d")),
        ];
        let models = vec![model("a", "b"), model("a", "c")];
        let list = |q: &str| build_rows_with_roles(&models, &roles, q, None, &[]);
        assert_eq!(
            list("@"),
            [
                Row::RolesHeader,
                Row::Role(0),
                Row::Role(1),
                Row::Role(2),
                Row::Role(3)
            ],
            "roles lead in their list order and no model follows"
        );
        // `l` starts "lint" and only occurs inside the others.
        assert_eq!(
            list("@l"),
            [
                Row::RolesHeader,
                Row::Role(3),
                Row::Role(0),
                Row::Role(1),
                Row::Role(2)
            ]
        );
        assert_eq!(
            list("  @ S "),
            [Row::RolesHeader, Row::Role(1), Row::Role(2)]
        );
        assert_eq!(list("@SMOL"), [Row::RolesHeader, Row::Role(1)]);
        assert!(list("@nothing").is_empty());
        // Without roles an `@` query is an ordinary (empty) model search.
        assert!(build_rows_with_roles(&models, &[], "@", None, &[]).is_empty());
    }

    #[test]
    fn roles_form_a_group_between_favorites_and_providers() {
        let roles = vec![
            role("default", Some("a/b")),
            role("smol", Some("z/c:low")),
            role("slow", None),
        ];
        let models = vec![model("a", "b"), model("z", "c")];
        let favorites = vec!["z/c".to_owned()];
        let rows = build_rows_with_roles(&models, &roles, "", None, &favorites);
        assert_eq!(
            rows[..7],
            [
                Row::FavoritesHeader,
                Row::Favorite(1),
                Row::RolesHeader,
                Row::Role(0),
                Row::Role(1),
                Row::Role(2),
                Row::Header("a".to_owned()),
            ]
        );
        // Words match `@role model`, so a model name finds the roles that use it.
        assert_eq!(
            build_rows_with_roles(&models, &roles, "z/c", None, &[])[..2],
            [Row::RolesHeader, Row::Role(1)]
        );
        // A provider filter keeps the roles whose model that provider serves (an unset role has none).
        assert_eq!(
            build_rows_with_roles(&models, &roles, "", Some("a"), &[])[..2],
            [Row::RolesHeader, Row::Role(0)]
        );
        // The plain picker passes no roles.
        assert!(!build_rows(&models, "", None, &[]).contains(&Row::RolesHeader));
    }

    #[test]
    fn picking_sends_the_selector_strings_omp_takes() {
        // A role is switched to by alias, never by its model: omp resolves it and applies its level.
        assert_eq!(
            role_action(&role("smol", Some("a/b:low"))),
            PickerAction::Switch {
                selector: "@smol".to_owned(),
                level: None
            }
        );
        assert_eq!(role("review", None).selector(), "@review");
        assert_eq!(
            role("smol", Some("a/b:low")).target(),
            Some(("a/b", Some("low")))
        );
        assert_eq!(role("slow", Some("a/b")).target(), Some(("a/b", None)));
        assert_eq!(role("slow", None).target(), None);
        assert_eq!(role_query("@smol"), Some("smol"));
        assert_eq!(role_query("smol"), None);
    }

    #[test]
    fn task_agents_need_live_control() {
        assert_eq!(task_unavailable(Link::Live), None);
        for link in [Link::Connecting, Link::ReadOnly, Link::Lost, Link::Ended] {
            assert!(task_unavailable(link).is_some(), "{link:?}");
        }
    }

    #[test]
    fn the_task_model_in_effect_is_omps_last_answer_else_its_config() {
        let mut cache = ModelsCache::default();
        cache.set_roles(&catalog_with(vec![(
            "task.agentModelOverrides",
            SettingKind::Record,
            json!({"task": "cfg/m"}),
        )]));
        let mut state = PickerState::default();
        assert_eq!(state.task_now(&cache, Some(1)).as_deref(), Some("cfg/m"));
        state.record_task_override(1, Some("set/m".to_owned()));
        assert_eq!(state.task_now(&cache, Some(1)).as_deref(), Some("set/m"));
        // Another tab is unaffected; an answer of "nothing" is remembered too.
        assert_eq!(state.task_now(&cache, Some(2)).as_deref(), Some("cfg/m"));
        state.record_task_override(2, None);
        assert_eq!(state.task_now(&cache, Some(2)), None);
        assert_eq!(state.task_now(&cache, None).as_deref(), Some("cfg/m"));
    }

    /// Runs the session picker `frames` frames with `state`; returns the actions it produced.
    fn run(
        ctx: &egui::Context,
        state: &mut PickerState,
        cache: &ModelsCache,
        extras: &SessionExtras<'_>,
        frames: usize,
    ) -> Vec<PickerAction> {
        let input = PickerInput {
            catalog: cache,
            current: Some("a/b"),
            level: None,
            favorites: &[],
            provider: None,
            unset: "No current model",
        };
        let mut actions = Vec::new();
        for _ in 0..frames {
            ctx.run_ui(egui::RawInput::default(), |ui| {
                let chip = ui.button("Models");
                actions.extend(show_session(&chip, state, &input, extras).actions);
            })
            .textures_delta
            .clear();
        }
        actions
    }

    fn loaded() -> ModelsCache {
        let mut cache = ModelsCache::default();
        cache.finish(Ok(vec![model("a", "b"), model("a", "c")]));
        cache.set_roles(&catalog_with(vec![(
            "modelRoles",
            SettingKind::Record,
            json!({"smol": "a/c"}),
        )]));
        cache
    }

    #[test]
    fn session_picker_lists_roles_and_task_mode_swaps_them_for_the_clear_row() {
        let ctx = egui::Context::default();
        crate::ui_fonts::install(&ctx, crate::ui_fonts::UiFonts::none());
        let cache = loaded();
        let live = SessionExtras {
            tab: 7,
            task_unavailable: None,
        };
        let mut state = PickerState::default();
        state.request_open();
        run(&ctx, &mut state, &cache, &live, 3);
        assert!(state.rows.contains(&Row::RolesHeader));
        assert!(!state.rows.contains(&Row::ClearOverride));

        state.task_mode = true;
        run(&ctx, &mut state, &cache, &live, 2);
        assert_eq!(state.rows.first(), Some(&Row::ClearOverride));
        assert!(!state.rows.contains(&Row::RolesHeader), "{:?}", state.rows);
        assert!(state.rows.contains(&Row::Model(0)));
    }

    #[test]
    fn task_mode_is_refused_without_live_control() {
        let ctx = egui::Context::default();
        crate::ui_fonts::install(&ctx, crate::ui_fonts::UiFonts::none());
        let cache = loaded();
        let blind = SessionExtras {
            tab: 7,
            task_unavailable: task_unavailable(Link::ReadOnly),
        };
        let mut state = PickerState::default();
        state.request_open();
        run(&ctx, &mut state, &cache, &blind, 3);
        state.task_mode = true;
        run(&ctx, &mut state, &cache, &blind, 2);
        assert!(!state.task_mode);
        assert!(!state.rows.contains(&Row::ClearOverride));
    }
}
