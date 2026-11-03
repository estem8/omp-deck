//! Data model of the omp settings editor: the loaded catalog with per-row UI state, cached search
//! haystacks, value parsing for the editors and a few pure formatting helpers.

use std::path::Path;

use deck_core::omp::{SECRET_PLACEHOLDER, Setting, SettingKind, SettingsCatalog};
use serde_json::Value;

use crate::statusbar::Defaults;

/// Row height guess before a row has been laid out once.
pub const ROW_HEIGHT_GUESS: f32 = 76.0;

pub struct Row {
    pub setting: Setting,
    /// Humanized label, computed once.
    pub label: String,
    /// Lowercased `key \n label \n description`, computed once.
    haystack: String,
    /// A write is in flight.
    pub pending: bool,
    /// omp's message from the last failed write.
    pub error: Option<String>,
    /// Local validation message for the text being typed.
    pub input_error: Option<String>,
    /// Text being edited (number / string fields); `None` = show the stored value.
    pub buf: Option<String>,
    /// Value sent with the write in flight (fallback when omp echoes `null`).
    sent: Option<Value>,
    /// Last laid-out height, used to skip off-screen rows.
    pub height: f32,
    /// What supplies the effective value instead of omp's saved one (`project`), shown as a badge.
    pub overridden_by: Option<String>,
}

impl Row {
    fn new(setting: Setting) -> Row {
        let label = setting.label();
        let haystack =
            format!("{}\n{}\n{}", setting.key, label, setting.description).to_lowercase();
        Row {
            setting,
            label,
            haystack,
            pending: false,
            error: None,
            input_error: None,
            buf: None,
            sent: None,
            height: ROW_HEIGHT_GUESS,
            overridden_by: None,
        }
    }

    /// Stored value, falling back to the default.
    pub fn effective(&self) -> Option<&Value> {
        self.setting
            .value
            .as_ref()
            .or(self.setting.default.as_ref())
    }

    pub fn matches(&self, terms: &[String]) -> bool {
        terms.iter().all(|t| self.haystack.contains(t.as_str()))
    }

    /// omp hides the editors of these settings while the project supplies them: the saved value
    /// is not what its sessions use, and the project file is where it is changed.
    pub fn hidden(&self) -> bool {
        self.overridden_by.as_deref() == Some(PROJECT_LAYER)
            && HIDDEN_WHEN_PROJECT.contains(&self.setting.key.as_str())
    }

    /// Marks a write as started. `sent` is only kept for non-secret settings (a secret must not
    /// outlive the write in UI state).
    pub fn begin(&mut self, sent: Option<Value>) {
        self.pending = true;
        self.error = None;
        self.input_error = None;
        self.sent = if self.setting.secret { None } else { sent };
    }

    /// Applies a write result: success updates the value, failure keeps it and records omp's message.
    pub fn finish(&mut self, outcome: Result<Option<Value>, String>) {
        self.pending = false;
        match outcome {
            Ok(Some(saved)) => {
                self.setting.value = Some(if self.setting.secret {
                    Value::String(SECRET_PLACEHOLDER.to_owned())
                } else if saved.is_null() {
                    self.sent.take().unwrap_or(Value::Null)
                } else {
                    saved
                });
                self.buf = None;
                self.error = None;
            }
            Ok(None) => {
                self.setting.value = None;
                self.buf = None;
                self.error = None;
            }
            Err(e) => {
                self.buf = None;
                self.error = Some(e);
            }
        }
        self.sent = None;
    }
}

pub struct Section {
    pub title: String,
    /// Indices into `TabModel::rows`.
    pub rows: Vec<usize>,
}

pub struct TabModel {
    pub id: String,
    pub label: String,
    pub rows: Vec<Row>,
    /// Rows grouped by `Setting::section()`, in order of first appearance.
    pub sections: Vec<Section>,
}

impl TabModel {
    /// Rows shown: all but the editors omp hides while the project supplies them.
    pub fn visible(&self) -> usize {
        self.rows.iter().filter(|r| !r.hidden()).count()
    }
}

pub struct Model {
    pub tabs: Vec<TabModel>,
    /// Where omp keeps its config, `~`-abbreviated, for the note banner.
    pub config_dir: String,
}

/// Matching rows of one tab.
pub struct TabHits {
    pub tab: usize,
    pub rows: Vec<usize>,
}

impl Model {
    pub fn from_catalog(catalog: SettingsCatalog, config_dir: String) -> Model {
        let tabs = catalog
            .tabs
            .into_iter()
            .map(|t| {
                let rows: Vec<Row> = t.settings.into_iter().map(Row::new).collect();
                let mut sections: Vec<Section> = Vec::new();
                for (i, row) in rows.iter().enumerate() {
                    let raw = row.setting.section();
                    let title = section_title(raw);
                    match sections.iter_mut().find(|s| s.title == title) {
                        Some(s) => s.rows.push(i),
                        None => sections.push(Section {
                            title,
                            rows: vec![i],
                        }),
                    }
                }
                TabModel {
                    id: t.id,
                    label: t.label,
                    rows,
                    sections,
                }
            })
            .collect();
        Model { tabs, config_dir }
    }

    pub fn find_mut(&mut self, key: &str) -> Option<&mut Row> {
        self.tabs
            .iter_mut()
            .flat_map(|t| t.rows.iter_mut())
            .find(|r| r.setting.key == key)
    }

    /// The row of setting `key`.
    pub fn find(&self, key: &str) -> Option<&Row> {
        self.tabs
            .iter()
            .flat_map(|t| t.rows.iter())
            .find(|r| r.setting.key == key)
    }

    /// Marks the rows the project's own settings file (`layer`, its parsed JSON; `None` = no file)
    /// supplies, and clears the marks of the others.
    pub fn set_project_layer(&mut self, layer: Option<&Value>) {
        for row in self.tabs.iter_mut().flat_map(|t| t.rows.iter_mut()) {
            let supplied = layer.is_some_and(|l| layer_sets(l, &row.setting.key));
            row.overridden_by = supplied.then(|| PROJECT_LAYER.to_owned());
        }
    }

    /// Rows of every tab matching all whitespace-separated terms of `query`, grouped by tab.
    pub fn search(&self, query: &str) -> Vec<TabHits> {
        let terms = query_terms(query);
        if terms.is_empty() {
            return Vec::new();
        }
        self.tabs
            .iter()
            .enumerate()
            .filter_map(|(tab, t)| {
                let rows: Vec<usize> = t
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| !r.hidden() && r.matches(&terms))
                    .map(|(i, _)| i)
                    .collect();
                (!rows.is_empty()).then_some(TabHits { tab, rows })
            })
            .collect()
    }
}

/// Lowercased whitespace-separated terms.
pub fn query_terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// `overriddenBy` omp reports for a value the project's settings file supplies.
pub const PROJECT_LAYER: &str = "project";

/// Settings whose editors omp hides while the project supplies them (its `…NotProject`
/// conditions: `contextFiles.extra`, `tools.restrictedFolders`, `task.perCallModelAllow`).
pub const HIDDEN_WHEN_PROJECT: [&str; 3] = [
    "contextFiles.extra",
    "tools.restrictedFolders",
    "task.perCallModelAllow",
];

/// The project's settings file, relative to the project folder (omp reads it from the working
/// directory only, not from parents).
pub const PROJECT_SETTINGS: [&str; 2] = [".omp", "settings.json"];

/// Largest project settings file read: a bigger one is not a settings file.
const PROJECT_SETTINGS_MAX: u64 = 1 << 20;

/// The project's settings file as JSON; `None` when there is none, it is unreadable or not an
/// object. Blocking file read: run on a worker.
pub fn read_project_layer(project: &Path) -> Option<Value> {
    use std::io::Read;
    let file =
        std::fs::File::open(project.join(PROJECT_SETTINGS[0]).join(PROJECT_SETTINGS[1])).ok()?;
    let mut text = String::new();
    file.take(PROJECT_SETTINGS_MAX)
        .read_to_string(&mut text)
        .ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()
        .filter(Value::is_object)
}

/// Whether the settings file `layer` sets `key`: nested by the key's dotted segments (how omp
/// writes it), or under the dotted key itself.
pub fn layer_sets(layer: &Value, key: &str) -> bool {
    if layer.get(key).is_some_and(|v| !v.is_null()) {
        return true;
    }
    let mut at = layer;
    for seg in key.split('.') {
        match at.get(seg) {
            Some(next) => at = next,
            None => return false,
        }
    }
    !at.is_null()
}

/// `statusLine.preset` → "Status line › Preset"; "" → "General".
pub fn section_title(section: &str) -> String {
    if section.is_empty() {
        return "General".to_owned();
    }
    section
        .split('.')
        .map(|seg| {
            let mut out = String::new();
            let mut prev_lower = false;
            for c in seg.chars() {
                if c.is_uppercase() && prev_lower {
                    out.push(' ');
                }
                prev_lower = c.is_lowercase() || c.is_ascii_digit();
                if out.is_empty() {
                    out.extend(c.to_uppercase());
                } else {
                    out.extend(c.to_lowercase());
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join(" › ")
}

/// Config directory shown in the banner: `PI_CODING_AGENT_DIR` or `~/.omp/agent`, `~`-abbreviated.
pub fn config_dir_display(env: Option<&str>, home: Option<&Path>) -> String {
    let sep = std::path::MAIN_SEPARATOR;
    let dir = match (env.filter(|e| !e.is_empty()), home) {
        (Some(e), _) => Path::new(e).to_path_buf(),
        (None, Some(h)) => h.join(".omp").join("agent"),
        (None, None) => return format!("~{sep}.omp{sep}agent"),
    };
    match home.and_then(|h| dir.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~{sep}{}", rest.display()),
        None => dir.display().to_string(),
    }
}

/// Number field text → JSON number (integers stay integers); rejects NaN/inf and junk.
pub fn parse_number(text: &str) -> Result<Value, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("Enter a number".to_owned());
    }
    if let Ok(i) = t.parse::<i64>() {
        return Ok(Value::from(i));
    }
    match t.parse::<f64>() {
        Ok(f) if f.is_finite() => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .ok_or_else(|| "Not a valid number".to_owned()),
        Ok(_) => Err("Number must be finite".to_owned()),
        Err(_) => Err("Not a valid number".to_owned()),
    }
}

/// JSON editor text → value, which must be an array (`Array`) or an object (`Record`).
pub fn parse_json_for(kind: &SettingKind, text: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("Invalid JSON: {e}"))?;
    match (kind, &value) {
        (SettingKind::Array, Value::Array(_)) | (SettingKind::Record, Value::Object(_)) => {
            Ok(value)
        }
        (SettingKind::Array, _) => Err("Expected a JSON array ([ … ])".to_owned()),
        (SettingKind::Record, _) => Err("Expected a JSON object ({ … })".to_owned()),
        _ => Err("This setting does not take JSON".to_owned()),
    }
}

/// Text shown in a number/string field for the stored value.
pub fn value_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

/// One-line compact JSON, cut to `max` characters.
pub fn preview(value: Option<&Value>, max: usize) -> String {
    let text = match value {
        None | Some(Value::Null) => return "empty".to_owned(),
        Some(Value::Array(a)) if a.is_empty() => return "[ ]".to_owned(),
        Some(Value::Object(o)) if o.is_empty() => return "{ }".to_owned(),
        Some(v) => v.to_string(),
    };
    if text.chars().count() <= max {
        text
    } else {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Pretty JSON for the modal editor (empty array/object when unset).
pub fn pretty_json(kind: &SettingKind, value: Option<&Value>) -> String {
    let fallback = match kind {
        SettingKind::Record => Value::Object(serde_json::Map::new()),
        _ => Value::Array(Vec::new()),
    };
    serde_json::to_string_pretty(value.filter(|v| !v.is_null()).unwrap_or(&fallback))
        .unwrap_or_else(|_| "[]".to_owned())
}

const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// `provider/id:level` → (`provider/id`, `Some(level)`) when the suffix is a thinking level omp
/// knows (`auto` included); anything else is returned whole.
pub fn split_level(selector: &str) -> (&str, Option<&str>) {
    match selector.rsplit_once(':') {
        Some((m, l)) if l == "auto" || THINKING_LEVELS.contains(&l) => (m, Some(l)),
        _ => (selector, None),
    }
}

/// The default model (`modelRoles.default`, an optional `:level` suffix split off) and thinking
/// level (`defaultThinkingLevel`) a fresh omp session starts with.
pub fn defaults_from_catalog(catalog: &SettingsCatalog) -> Defaults {
    let value = |key: &str| {
        catalog
            .tabs
            .iter()
            .flat_map(|t| &t.settings)
            .find(|s| s.key == key)
            .and_then(|s| s.value.clone().or_else(|| s.default.clone()))
    };
    let role = value("modelRoles")
        .and_then(|v| v.get("default").and_then(Value::as_str).map(str::to_owned))
        .filter(|m| !m.trim().is_empty());
    let (model, suffix) = match role.as_deref().map(split_level) {
        Some((m, l)) => (Some(m.to_owned()), l.map(str::to_owned)),
        None => (None, None),
    };
    let level = suffix.or_else(|| {
        value("defaultThinkingLevel")
            .and_then(|v| v.as_str().map(str::to_owned))
            .filter(|l| !l.is_empty())
    });
    Defaults { model, level }
}

/// omp's `retry.usageReservePct` (configured, else its default): the reserve of accounts whose
/// `auth.accountPolicies` entry sets none.
pub fn usage_reserve_from_catalog(catalog: &SettingsCatalog) -> Option<u32> {
    catalog
        .tabs
        .iter()
        .flat_map(|t| &t.settings)
        .find(|s| s.key == "retry.usageReservePct")
        .and_then(|s| s.value.as_ref().or(s.default.as_ref()))
        .and_then(Value::as_f64)
        .filter(|v| (0.0..=100.0).contains(v))
        .map(|v| v.round() as u32)
}

/// The settings key whose change also moves the open session's thinking level.
pub const THINKING_KEY: &str = "defaultThinkingLevel";

/// The thinking level of the session in the active tab, as far as the app knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionLevel<'a> {
    /// omp is running in the tab (not exited).
    pub live: bool,
    /// The level omp is using (`None`: not known yet, or the model has none).
    pub level: Option<&'a str>,
    /// The level as configured (`auto` while omp picks it).
    pub configured: Option<&'a str>,
}

/// What omp's `/settings` does when `defaultThinkingLevel` changes: the live session switches to
/// the new level too. Returns the level to send as `/effort` — only to a live session whose
/// level is known and differs; `None` for no session tab, an exited one, an unknown level, or a
/// session already there.
pub fn thinking_to_apply(new_level: &str, session: Option<SessionLevel<'_>>) -> Option<String> {
    let s = session.filter(|s| s.live)?;
    let current = s.configured.or(s.level)?;
    (current != new_level && !new_level.is_empty()).then(|| new_level.to_owned())
}

#[cfg(test)]
mod tests {
    use deck_core::omp::SettingsTab;
    use serde_json::json;

    use super::*;

    fn setting(
        key: &str,
        kind: SettingKind,
        value: Option<Value>,
        default: Option<Value>,
    ) -> Setting {
        Setting {
            key: key.into(),
            kind,
            value,
            default,
            description: format!("About {key}"),
            secret: false,
        }
    }

    fn model() -> Model {
        let catalog = SettingsCatalog {
            tabs: vec![
                SettingsTab {
                    id: "appearance".into(),
                    label: "Appearance".into(),
                    settings: vec![
                        setting(
                            "theme.dark",
                            SettingKind::String,
                            None,
                            Some(json!("titanium")),
                        ),
                        setting("statusLine.preset", SettingKind::String, None, None),
                        setting(
                            "showHints",
                            SettingKind::Bool,
                            Some(json!(true)),
                            Some(json!(false)),
                        ),
                        setting("theme.light", SettingKind::String, None, None),
                    ],
                },
                SettingsTab {
                    id: "shell".into(),
                    label: "Shell".into(),
                    settings: vec![setting("shell.path", SettingKind::String, None, None)],
                },
            ],
        };
        Model::from_catalog(catalog, "~/.omp/agent".into())
    }

    fn catalog_with(settings: Vec<Setting>) -> SettingsCatalog {
        SettingsCatalog {
            tabs: vec![SettingsTab {
                id: "model".into(),
                label: "Model".into(),
                settings,
            }],
        }
    }

    #[test]
    fn defaults_come_from_model_role_and_thinking_setting() {
        let c = catalog_with(vec![
            setting(
                "modelRoles",
                SettingKind::Record,
                Some(json!({"default": "fake/fake-1", "smol": "x/y"})),
                None,
            ),
            setting(
                "defaultThinkingLevel",
                SettingKind::String,
                Some(json!("high")),
                Some(json!("high")),
            ),
        ]);
        assert_eq!(
            defaults_from_catalog(&c),
            Defaults {
                model: Some("fake/fake-1".into()),
                level: Some("high".into())
            }
        );
        // A `:level` suffix on the role wins and is not part of the selector.
        let c = catalog_with(vec![setting(
            "modelRoles",
            SettingKind::Record,
            Some(json!({"default": "a/b:low"})),
            None,
        )]);
        assert_eq!(
            defaults_from_catalog(&c),
            Defaults {
                model: Some("a/b".into()),
                level: Some("low".into())
            }
        );
        // Nothing configured.
        assert_eq!(
            defaults_from_catalog(&catalog_with(Vec::new())),
            Defaults::default()
        );
    }

    #[test]
    fn search_is_case_insensitive_multi_word_and_grouped_by_tab() {
        let m = model();
        let hits = m.search("THEME dark");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tab, 0);
        assert_eq!(hits[0].rows, [0]);

        // Matches key, label and description; terms may hit different fields.
        assert_eq!(m.search("preset line").len(), 1);
        assert_eq!(m.search("about shell.path")[0].tab, 1);
        let all = m.search("about");
        assert_eq!(all.len(), 2);
        assert!(m.search("nothing-like-this").is_empty());
        assert!(m.search("   ").is_empty());
    }

    #[test]
    fn rows_group_by_section_in_first_appearance_order() {
        let m = model();
        let titles: Vec<&str> = m.tabs[0]
            .sections
            .iter()
            .map(|s| s.title.as_str())
            .collect();
        assert_eq!(titles, ["Theme", "Status line", "General"]);
        assert_eq!(m.tabs[0].sections[0].rows, [0, 3]);
    }

    #[test]
    fn section_titles() {
        assert_eq!(section_title(""), "General");
        assert_eq!(section_title("statusLine"), "Status line");
        assert_eq!(section_title("a.bC"), "A › B c");
    }

    #[test]
    fn number_parsing() {
        assert_eq!(parse_number(" 42 "), Ok(json!(42)));
        assert_eq!(parse_number("-3"), Ok(json!(-3)));
        assert_eq!(parse_number("0.5"), Ok(json!(0.5)));
        assert_eq!(parse_number("1e3"), Ok(json!(1000.0)));
        assert!(parse_number("").is_err());
        assert!(parse_number("abc").is_err());
        assert!(parse_number("NaN").is_err());
        assert!(parse_number("inf").is_err());
    }

    #[test]
    fn json_editor_validation() {
        assert_eq!(
            parse_json_for(&SettingKind::Array, "[1, \"a\"]"),
            Ok(json!([1, "a"]))
        );
        assert!(parse_json_for(&SettingKind::Array, "{}").is_err());
        assert!(parse_json_for(&SettingKind::Record, "[]").is_err());
        assert_eq!(
            parse_json_for(&SettingKind::Record, "{\"a\": 1}"),
            Ok(json!({"a": 1}))
        );
        assert!(parse_json_for(&SettingKind::Record, "{oops").is_err());
        assert!(parse_json_for(&SettingKind::Record, "").is_err());
    }

    #[test]
    fn write_results_update_the_row() {
        let mut m = model();
        let row = m.find_mut("theme.dark").expect("row");
        row.begin(Some(json!("graphite")));
        assert!(row.pending);
        row.finish(Ok(Some(Value::Null)));
        assert_eq!(
            row.setting.value,
            Some(json!("graphite")),
            "null echo keeps the sent value"
        );
        assert!(row.setting.is_modified());

        row.begin(Some(json!("x")));
        row.finish(Err("bad value".into()));
        assert_eq!(
            row.setting.value,
            Some(json!("graphite")),
            "failure keeps the old value"
        );
        assert_eq!(row.error.as_deref(), Some("bad value"));
        assert!(!row.pending);

        row.begin(None);
        row.finish(Ok(None));
        assert_eq!(row.setting.value, None);
        assert!(!row.setting.is_modified());
        assert!(row.error.is_none());
    }

    #[test]
    fn secrets_are_never_stored() {
        let mut s = setting("auth.token", SettingKind::String, None, None);
        s.secret = true;
        let mut row = Row::new(s);
        row.begin(Some(json!("hunter2")));
        assert!(
            row.sent.is_none(),
            "a secret is not retained for the fallback"
        );
        row.finish(Ok(Some(Value::Null)));
        assert_eq!(row.setting.value, Some(json!(SECRET_PLACEHOLDER)));
        assert!(row.sent.is_none(), "a secret never outlives the write call");
    }

    #[test]
    fn config_dir_banner_path() {
        let sep = std::path::MAIN_SEPARATOR;
        let default = format!("~{sep}.omp{sep}agent");
        let (home, inside, outside, nested) = if cfg!(windows) {
            (
                r"C:\Users\u",
                r"C:\Users\u\x\agent",
                r"D:\tmp\agent",
                r"~\x\agent",
            )
        } else {
            ("/home/u", "/home/u/x/agent", "/tmp/agent", "~/x/agent")
        };
        let home = Path::new(home);
        assert_eq!(config_dir_display(None, Some(home)), default);
        assert_eq!(config_dir_display(Some(inside), Some(home)), nested);
        assert_eq!(config_dir_display(Some(outside), Some(home)), outside);
        assert_eq!(config_dir_display(Some(""), Some(home)), default);
        assert_eq!(config_dir_display(None, None), default);
    }

    #[test]
    fn previews() {
        assert_eq!(preview(None, 20), "empty");
        assert_eq!(preview(Some(&json!([])), 20), "[ ]");
        assert_eq!(preview(Some(&json!({"a": 1})), 20), "{\"a\":1}");
        assert_eq!(preview(Some(&json!(["abcdefghij"])), 6), "[\"abc…");
        assert_eq!(value_text(Some(&json!(7))), "7");
        assert_eq!(value_text(Some(&json!("s"))), "s");
        assert_eq!(pretty_json(&SettingKind::Record, None), "{}");
        assert_eq!(pretty_json(&SettingKind::Array, None), "[]");
    }

    fn model_with_hidden_candidates() -> Model {
        let catalog = SettingsCatalog {
            tabs: vec![SettingsTab {
                id: "context".into(),
                label: "Context".into(),
                settings: vec![
                    setting("theme.dark", SettingKind::String, None, None),
                    setting("contextFiles.extra", SettingKind::Array, None, None),
                    setting("tools.restrictedFolders", SettingKind::Array, None, None),
                    setting("task.perCallModelAllow", SettingKind::Array, None, None),
                    setting("showHints", SettingKind::Bool, None, None),
                ],
            }],
        };
        Model::from_catalog(catalog, String::new())
    }

    #[test]
    fn the_project_file_marks_the_rows_it_supplies() {
        let mut m = model_with_hidden_candidates();
        let layer = json!({"theme": {"dark": "nord"}, "showHints": null});
        m.set_project_layer(Some(&layer));
        let by = |m: &Model, key: &str| m.find(key).and_then(|r| r.overridden_by.clone());
        assert_eq!(by(&m, "theme.dark").as_deref(), Some("project"));
        // A null is not a value; a key the file lacks is untouched.
        assert_eq!(by(&m, "showHints"), None);
        assert_eq!(by(&m, "contextFiles.extra"), None);
        // The marks follow the file: gone with it.
        m.set_project_layer(None);
        assert_eq!(by(&m, "theme.dark"), None);
    }

    #[test]
    fn a_key_is_found_nested_or_under_its_dotted_name() {
        let nested = json!({"a": {"b": {"c": 1}}});
        assert!(layer_sets(&nested, "a.b.c"));
        assert!(!layer_sets(&nested, "a.b.d"));
        assert!(!layer_sets(&nested, "a.x.c"));
        let flat = json!({"a.b": 2});
        assert!(layer_sets(&flat, "a.b"));
        assert!(!layer_sets(&json!({}), "a"));
    }

    #[test]
    fn the_three_editors_omp_hides_disappear_only_while_the_project_supplies_them() {
        let mut m = model_with_hidden_candidates();
        let layer = json!({
            "theme": {"dark": "nord"},
            "contextFiles": {"extra": ["AGENTS.local.md"]},
            "tools": {"restrictedFolders": ["/secret"]},
            "task": {"perCallModelAllow": []},
        });
        m.set_project_layer(Some(&layer));
        let hidden: Vec<&str> = m.tabs[0]
            .rows
            .iter()
            .filter(|r| r.hidden())
            .map(|r| r.setting.key.as_str())
            .collect();
        assert_eq!(
            hidden,
            [
                "contextFiles.extra",
                "tools.restrictedFolders",
                "task.perCallModelAllow"
            ]
        );
        // Search skips them too; an overridden row that omp keeps visible stays.
        assert!(m.search("restricted").is_empty());
        assert_eq!(m.search("theme.dark").len(), 1);
        m.set_project_layer(None);
        assert!(m.tabs[0].rows.iter().all(|r| !r.hidden()));
        assert_eq!(m.search("restricted").len(), 1);
    }

    #[test]
    fn the_project_file_is_read_bounded_and_only_when_it_is_an_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_project_layer(dir.path()), None);
        std::fs::create_dir(dir.path().join(".omp")).expect("mkdir");
        let file = dir.path().join(".omp").join("settings.json");
        std::fs::write(&file, r#"{"theme":{"dark":"nord"}}"#).expect("write");
        assert_eq!(
            read_project_layer(dir.path()),
            Some(json!({"theme": {"dark": "nord"}}))
        );
        std::fs::write(&file, "[1,2]").expect("write");
        assert_eq!(read_project_layer(dir.path()), None);
        std::fs::write(&file, "not json").expect("write");
        assert_eq!(read_project_layer(dir.path()), None);
    }

    #[test]
    fn the_session_follows_a_changed_default_thinking_level_only_when_it_differs() {
        let live = |level, configured| {
            Some(SessionLevel {
                live: true,
                level,
                configured,
            })
        };
        // Differs → /effort <new>.
        assert_eq!(
            thinking_to_apply("high", live(Some("low"), Some("low"))).as_deref(),
            Some("high")
        );
        // `auto` is a configured level of its own: omp picking `low` is still not `high`.
        assert_eq!(
            thinking_to_apply("high", live(Some("low"), Some("auto"))).as_deref(),
            Some("high")
        );
        // Same level → nothing to send; the configured one wins over the one in use.
        assert_eq!(thinking_to_apply("high", live(Some("high"), None)), None);
        assert_eq!(
            thinking_to_apply("auto", live(Some("low"), Some("auto"))),
            None
        );
        // No tab, an exited one, or a level not known yet → nothing.
        assert_eq!(thinking_to_apply("high", None), None);
        let exited = SessionLevel {
            live: false,
            level: Some("low"),
            configured: Some("low"),
        };
        assert_eq!(thinking_to_apply("high", Some(exited)), None);
        assert_eq!(thinking_to_apply("high", live(None, None)), None);
        // A cleared value is not a level.
        assert_eq!(thinking_to_apply("", live(Some("low"), None)), None);
    }
}
