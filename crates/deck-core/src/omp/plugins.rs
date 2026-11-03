//! `omp plugin …` wrappers (verified against omp 18.16.0 in a sandbox agent dir; shapes pinned by
//! the tests below and `tests/omp_plugins.rs`). Every type tolerates unknown fields.
//!
//! Quirks of the CLI that these wrappers hide:
//! - `plugin features … --json` prints a `✔ Updated features for <pkg>` line **before** the JSON
//!   document when it changed something ([`json_tail`] skips it).
//! - `plugin features <pkg> --set ""` is a no-op (an empty value is falsy in omp), so clearing
//!   every feature is `--disable <all available>`.
//! - `config set`/`config delete` print no JSON: the exit code and stderr are the result.
//! - `config list --json` returns secret values in clear: [`Omp::plugin_config`] masks them.
//! - `--scope project` only means something for marketplace plugins and is relative to omp's
//!   working directory; npm plugins ignore it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::de::{Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Omp, OmpError, SECRET_PLACEHOLDER, failed};

/// Where `plugin enable|disable` records a marketplace plugin's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginScope {
    User,
    Project,
}

impl PluginScope {
    pub fn as_str(self) -> &'static str {
        match self {
            PluginScope::User => "user",
            PluginScope::Project => "project",
        }
    }
}

/// An object's entries in document order (omp lists features and settings in the manifest's order).
fn ordered<'de, D, T>(d: D) -> Result<Vec<(String, T)>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Ordered<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for Ordered<T> {
        type Value = Vec<(String, T)>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(entry) = map.next_entry::<String, T>()? {
                out.push(entry);
            }
            Ok(out)
        }
    }
    d.deserialize_map(Ordered(std::marker::PhantomData))
}

/// The inverse of [`ordered`]: the entries as an object, in order.
fn ordered_ser<S, T>(entries: &[(String, T)], s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
    T: Serialize,
{
    use serde::ser::SerializeMap;
    let mut map = s.serialize_map(Some(entries.len()))?;
    for (k, v) in entries {
        map.serialize_entry(k, v)?;
    }
    map.end()
}

/// One optional feature of a plugin manifest.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginFeature {
    #[serde(default)]
    pub description: Option<String>,
    /// Enabled when the plugin has no explicit feature selection.
    #[serde(default)]
    pub default: bool,
}

/// The typed part of one manifest setting (`type` discriminates).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PluginSettingKind {
    String {
        #[serde(default)]
        default: Option<String>,
    },
    Number {
        #[serde(default)]
        default: Option<f64>,
        #[serde(default)]
        min: Option<f64>,
        #[serde(default)]
        max: Option<f64>,
        #[serde(default)]
        step: Option<f64>,
    },
    Boolean {
        #[serde(default)]
        default: Option<bool>,
    },
    Enum {
        #[serde(default)]
        values: Vec<String>,
        #[serde(default)]
        default: Option<String>,
    },
    /// A setting type this OmpDeck does not know: shown read-only by the UI.
    #[serde(other)]
    Unknown,
}

/// One entry of a manifest's `settings` schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginSetting {
    #[serde(flatten)]
    pub kind: PluginSettingKind,
    #[serde(default)]
    pub description: Option<String>,
    /// The value must never be displayed or logged.
    #[serde(default)]
    pub secret: bool,
    /// Environment variable omp falls back to.
    #[serde(default)]
    pub env: Option<String>,
}

/// A plugin's `package.json` `omp` manifest.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "ordered", serialize_with = "ordered_ser")]
    pub features: Vec<(String, PluginFeature)>,
    #[serde(default, deserialize_with = "ordered", serialize_with = "ordered_ser")]
    pub settings: Vec<(String, PluginSetting)>,
}

fn yes() -> bool {
    true
}

/// An npm (or linked) plugin of `omp plugin list --json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NpmPlugin {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// `None`: omp uses the manifest defaults.
    #[serde(default, rename = "enabledFeatures")]
    pub enabled_features: Option<Vec<String>>,
    #[serde(default)]
    pub manifest: PluginManifest,
}

/// One installed copy of a marketplace plugin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceEntry {
    #[serde(default)]
    pub install_path: Option<PathBuf>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub installed_at: Option<String>,
    #[serde(default)]
    pub last_updated: Option<String>,
    #[serde(default)]
    pub git_commit_sha: Option<String>,
    /// Absent means enabled.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// A marketplace plugin (`name@marketplace`) of `omp plugin list --json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplacePlugin {
    pub id: String,
    #[serde(default, deserialize_with = "lenient_scope")]
    pub scope: Option<PluginScope>,
    /// Another scope's copy wins over this one.
    #[serde(default)]
    pub shadowed_by: Option<String>,
    #[serde(default)]
    pub entries: Vec<MarketplaceEntry>,
}

fn lenient_scope<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PluginScope>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.and_then(|v| serde_json::from_value(v).ok()))
}

impl MarketplacePlugin {
    /// omp's rule: an entry without `enabled` is enabled; the plugin follows its first entry.
    pub fn enabled(&self) -> bool {
        self.entries
            .first()
            .is_none_or(|e| e.enabled != Some(false))
    }
}

/// `omp plugin list --json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginList {
    #[serde(default)]
    pub npm: Vec<NpmPlugin>,
    #[serde(default)]
    pub marketplace: Vec<MarketplacePlugin>,
}

/// `omp plugin features <pkg> --json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginFeatures {
    pub plugin: String,
    /// `None`: the manifest defaults apply.
    #[serde(default)]
    pub enabled_features: Option<Vec<String>>,
    #[serde(default)]
    pub available_features: Vec<String>,
}

/// `omp plugin config list <pkg> --json`: the stored values and the manifest schema. Values of
/// `secret` settings are replaced by [`SECRET_PLACEHOLDER`] (the key is present when a value is
/// set, so the UI can show "set").
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginConfig {
    #[serde(default)]
    pub settings: BTreeMap<String, Value>,
    #[serde(default, deserialize_with = "ordered", serialize_with = "ordered_ser")]
    pub schema: Vec<(String, PluginSetting)>,
}

impl PluginConfig {
    fn mask_secrets(&mut self) {
        for (key, setting) in &self.schema {
            if setting.secret
                && let Some(v) = self.settings.get_mut(key)
                && !v.is_null()
            {
                *v = Value::String(SECRET_PLACEHOLDER.into());
            }
        }
    }
}

/// The JSON document of `stdout`: from the first line that starts with `{` (omp may print a
/// status line first).
pub(super) fn json_tail(stdout: &str) -> &str {
    let mut at = 0;
    for line in stdout.split_inclusive('\n') {
        if line.starts_with('{') {
            return &stdout[at..];
        }
        at += line.len();
    }
    stdout
}

fn parse<T: for<'de> Deserialize<'de>>(stdout: &str) -> Result<T, OmpError> {
    serde_json::from_str(json_tail(stdout).trim())
        .map_err(|e| OmpError::Parse(format!("{e}: {}", stdout.trim())))
}

/// A package/plugin name or key becomes one argument; a leading `-` would be read as a flag.
fn arg<'a>(what: &str, value: &'a str) -> Result<&'a str, OmpError> {
    if value.is_empty() || value.starts_with('-') || value.contains('\0') {
        return Err(failed(format!("Invalid {what}: {value}")));
    }
    Ok(value)
}

impl Omp {
    /// `omp plugin list --json`.
    pub fn plugins(&self) -> Result<PluginList, OmpError> {
        parse(&self.run(&["plugin", "list", "--json"], None)?.stdout)
    }

    /// `omp plugin enable|disable <name> --scope <scope> --json`. Runs in omp's default working
    /// directory; see [`Omp::plugin_set_enabled_in`] for a project scope.
    pub fn plugin_set_enabled(
        &self,
        name: &str,
        enabled: bool,
        scope: PluginScope,
    ) -> Result<(), OmpError> {
        self.set_enabled(name, enabled, scope, None)
    }

    /// [`Omp::plugin_set_enabled`] run in `cwd`: the project a `--scope project` marketplace
    /// plugin belongs to.
    pub fn plugin_set_enabled_in(
        &self,
        name: &str,
        enabled: bool,
        scope: PluginScope,
        cwd: &Path,
    ) -> Result<(), OmpError> {
        self.set_enabled(name, enabled, scope, Some(cwd))
    }

    fn set_enabled(
        &self,
        name: &str,
        enabled: bool,
        scope: PluginScope,
        cwd: Option<&Path>,
    ) -> Result<(), OmpError> {
        let name = arg("plugin name", name)?;
        let action = if enabled { "enable" } else { "disable" };
        let args = ["plugin", action, name, "--scope", scope.as_str(), "--json"];
        let os: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
        let mut cmd = self.command(&os);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let out = self.finish(cmd, None)?;
        let key = if enabled { "enabled" } else { "disabled" };
        match serde_json::from_str::<Value>(out.stdout.trim()) {
            Ok(v) if v.get(key).and_then(Value::as_str) == Some(name) => Ok(()),
            _ => Err(OmpError::Parse(out.stdout.trim().to_owned())),
        }
    }

    /// The enabled features of `pkg` become exactly `features` (`plugin features <pkg> --set`);
    /// an empty list disables every available one. Returns the state omp reports afterwards.
    pub fn plugin_features(
        &self,
        pkg: &str,
        features: &[String],
    ) -> Result<PluginFeatures, OmpError> {
        let pkg = arg("plugin name", pkg)?;
        if let Some(bad) = features
            .iter()
            .find(|f| f.is_empty() || f.starts_with('-') || f.contains(','))
        {
            return Err(failed(format!("Invalid feature name: {bad}")));
        }
        if features.is_empty() {
            let current: PluginFeatures = parse(
                &self
                    .run(&["plugin", "features", pkg, "--json"], None)?
                    .stdout,
            )?;
            if current.available_features.is_empty()
                || current.enabled_features.as_deref() == Some(&[])
            {
                return Ok(current);
            }
            let all = current.available_features.join(",");
            return parse(
                &self
                    .run(
                        &["plugin", "features", pkg, "--disable", &all, "--json"],
                        None,
                    )?
                    .stdout,
            );
        }
        let set = features.join(",");
        parse(
            &self
                .run(&["plugin", "features", pkg, "--set", &set, "--json"], None)?
                .stdout,
        )
    }

    /// `omp plugin config list <pkg> --json`, secrets masked.
    pub fn plugin_config(&self, pkg: &str) -> Result<PluginConfig, OmpError> {
        let pkg = arg("plugin name", pkg)?;
        let mut config: PluginConfig = parse(
            &self
                .run(&["plugin", "config", "list", pkg, "--json"], None)?
                .stdout,
        )?;
        config.mask_secrets();
        Ok(config)
    }

    /// `omp plugin config set <pkg> <key> <value>` (omp validates against the manifest schema:
    /// type, `min`/`max`, enum values). Booleans and numbers are passed as their JSON text.
    /// `Null` is refused: use [`Omp::plugin_config_delete`]. A failure message never contains the
    /// value.
    pub fn plugin_config_set(&self, pkg: &str, key: &str, value: &Value) -> Result<(), OmpError> {
        let (pkg, key) = (arg("plugin name", pkg)?, arg("setting key", key)?);
        let text = match value {
            Value::String(s) => s.clone(),
            Value::Bool(_) | Value::Number(_) => value.to_string(),
            _ => return Err(failed("A plugin setting is a string, number or boolean")),
        };
        // omp joins the trailing words with spaces and reads `-x` as a flag.
        if text.starts_with('-') {
            return Err(failed("A setting value cannot start with '-'"));
        }
        log::debug!("omp plugin config set {pkg} {key} (value not logged)");
        self.run(&["plugin", "config", "set", pkg, key, &text], Some(&text))?;
        Ok(())
    }

    /// `omp plugin config delete <pkg> <key>`: the setting falls back to its default (succeeds
    /// when it was not set).
    pub fn plugin_config_delete(&self, pkg: &str, key: &str) -> Result<(), OmpError> {
        let (pkg, key) = (arg("plugin name", pkg)?, arg("setting key", key)?);
        self.run(&["plugin", "config", "delete", pkg, key], None)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_tail_skips_the_status_line_omp_prints_first() {
        assert_eq!(
            json_tail("✔ Updated features for x\n{\n  \"a\": 1\n}\n"),
            "{\n  \"a\": 1\n}\n"
        );
        assert_eq!(json_tail("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(json_tail("no json here"), "no json here");
    }

    #[test]
    fn plugin_list_keeps_manifest_order_defaults_enabled_and_ignores_unknown_fields() {
        // A raw document: `json!` would sort the keys and hide what this test is about.
        let list: PluginList = parse(
            r#"{
                "npm": [{
                    "name": "p", "version": "1.0.0", "path": "/x/p", "enabledFeatures": null,
                    "somethingNew": 1,
                    "manifest": {
                        "description": "d", "version": "1.0.0",
                        "features": { "zeta": { "default": true }, "alpha": { "description": "A" } },
                        "settings": {
                            "key": { "type": "string", "secret": true, "env": "K" },
                            "n": { "type": "number", "min": 1, "max": 9, "default": 3 },
                            "m": { "type": "enum", "values": ["a", "b"] },
                            "w": { "type": "weird" }
                        }
                    }
                }],
                "marketplace": [{
                    "id": "x@mkt", "scope": "project", "shadowedBy": "user",
                    "entries": [{ "installPath": "/m", "version": "2", "enabled": false }]
                }, { "id": "y@mkt", "scope": "galaxy", "entries": [{ "version": "1" }] }]
            }"#,
        )
        .unwrap();
        let p = &list.npm[0];
        assert!(p.enabled, "a plugin without `enabled` is enabled");
        assert_eq!(p.enabled_features, None);
        let names: Vec<_> = p
            .manifest
            .features
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(names, ["zeta", "alpha"], "document order, not alphabetical");
        assert!(p.manifest.features[0].1.default);
        let kinds: Vec<_> = p.manifest.settings.iter().map(|(_, s)| &s.kind).collect();
        assert!(matches!(
            kinds[0],
            PluginSettingKind::String { default: None }
        ));
        assert!(matches!(
            kinds[1],
            PluginSettingKind::Number { default: Some(d), min: Some(lo), max: Some(hi), step: None }
                if *d == 3.0 && *lo == 1.0 && *hi == 9.0
        ));
        assert!(
            matches!(kinds[2], PluginSettingKind::Enum { values, .. } if values == &["a", "b"])
        );
        assert!(matches!(kinds[3], PluginSettingKind::Unknown));
        assert!(p.manifest.settings[0].1.secret);
        let (x, y) = (&list.marketplace[0], &list.marketplace[1]);
        assert_eq!(x.scope, Some(PluginScope::Project));
        assert_eq!(x.shadowed_by.as_deref(), Some("user"));
        assert!(!x.enabled());
        assert_eq!(y.scope, None, "an unknown scope does not fail the list");
        assert!(y.enabled(), "an entry without `enabled` is enabled");
    }

    #[test]
    fn config_masks_only_secrets_that_have_a_value() {
        let mut c: PluginConfig = parse(
            &json!({
                "settings": { "apiKey": "sekret", "retries": 5 },
                "schema": {
                    "apiKey": { "type": "string", "secret": true },
                    "token": { "type": "string", "secret": true },
                    "retries": { "type": "number" }
                }
            })
            .to_string(),
        )
        .unwrap();
        c.mask_secrets();
        assert_eq!(c.settings["apiKey"], json!(SECRET_PLACEHOLDER));
        assert_eq!(c.settings["retries"], json!(5));
        assert!(
            !c.settings.contains_key("token"),
            "an unset secret stays absent"
        );
    }

    #[test]
    fn plugin_documents_round_trip_through_json_keeping_order() {
        let list: PluginList = parse(
            r#"{"npm":[{"name":"p","version":"1","manifest":{"features":{"zeta":{"default":true},"alpha":{}},
                "settings":{"b":{"type":"string"},"a":{"type":"number","min":1}}}}],"marketplace":[]}"#,
        )
        .unwrap();
        let back: PluginList =
            serde_json::from_str(&serde_json::to_string(&list).unwrap()).unwrap();
        assert_eq!(back, list);
        assert_eq!(back.npm[0].manifest.features[0].0, "zeta");
        let cfg: PluginConfig = serde_json::from_str(
            r#"{"settings":{"b":1},"schema":{"z":{"type":"boolean"},"a":{"type":"string"}}}"#,
        )
        .unwrap();
        let back: PluginConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(back.schema[0].0, "z");
    }

    #[test]
    fn names_that_would_become_flags_are_refused() {
        assert!(arg("x", "-rf").is_err());
        assert!(arg("x", "").is_err());
        assert_eq!(arg("x", "@scope/pkg").unwrap(), "@scope/pkg");
    }
}
