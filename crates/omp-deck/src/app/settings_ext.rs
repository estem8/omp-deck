//! Settings → Plugins and Advisor over the gateway, the project settings file behind the
//! "overridden by" badges, and the follow-up of `defaultThinkingLevel` on the open session.

use std::path::PathBuf;

use deck_core::omp::{AdvisorScope, PluginScope};
use deck_service::controller::SessionTarget;
use deck_service::gateway::protocol::{Answer, Done, Mutation, Query};

use super::App;
use crate::advisor_page::{AdvisorAction, apply_to_active};
use crate::omp_settings::{
    SessionLevel, THINKING_KEY, read_project_layer, split_level, thinking_to_apply,
};
use crate::plugins_page::PluginsAction;
use crate::statusbar;
use crate::tabs::TabRole;

fn unexpected<T: std::fmt::Debug>(other: T) -> String {
    format!("unexpected reply {other:?}")
}

impl App {
    /// The folder of the active session tab: where project-scoped changes are recorded.
    fn settings_project(&self) -> Option<PathBuf> {
        match self.tabs.get(self.active) {
            Some(t) if t.role == TabRole::Session => Some(t.cwd.clone()),
            _ => None,
        }
    }

    /// The session plugin changes and saved advisors are applied to: the active omp tab.
    fn live_target(&self) -> Option<SessionTarget> {
        let tab = self.tabs.get(self.active)?;
        (tab.role == TabRole::Session && !tab.terminal().is_exited())
            .then(|| self.active_target())
            .flatten()
    }

    /// Per frame while Settings is open: follow the active tab's project settings file.
    pub(super) fn sync_project_layer(&mut self) {
        let project = self.settings_project();
        let Some(dir) = self.settings.layer_request(project.as_deref()) else {
            return;
        };
        let owner = dir.clone();
        self.gw.native(
            move || read_project_layer(&dir),
            move |app, layer| {
                app.settings.project_layer(&owner, layer);
            },
        );
    }

    pub(super) fn apply_plugins_action(&mut self, action: PluginsAction) {
        match action {
            PluginsAction::Load => self.gw.query(Query::PluginList, |app, result| {
                let list = result.and_then(|a| match a {
                    Answer::PluginList(l) => Ok(l),
                    other => Err(unexpected(other)),
                });
                app.settings.plugins.listed(list);
            }),
            PluginsAction::Enable {
                name,
                enabled,
                scope,
            } => {
                let cwd = match scope {
                    PluginScope::Project => self.settings_project(),
                    PluginScope::User => None,
                };
                let reload = self.live_target();
                self.gw.mutate(
                    Mutation::PluginEnable {
                        name: name.clone(),
                        enabled,
                        scope,
                        cwd,
                        reload,
                    },
                    move |app, result| {
                        app.settings.plugins.enabled(&name, result.map(|_| ()));
                    },
                );
            }
            PluginsAction::Features { pkg, features } => {
                let reload = self.live_target();
                self.gw.mutate(
                    Mutation::PluginFeatures {
                        pkg: pkg.clone(),
                        features,
                        reload,
                    },
                    move |app, result| {
                        let state = result.and_then(|d| match d {
                            Done::PluginFeatures(f) => Ok(f),
                            other => Err(unexpected(other)),
                        });
                        app.settings.plugins.features_done(&pkg, state);
                    },
                );
            }
            PluginsAction::LoadConfig { pkg } => {
                self.gw.query(
                    Query::PluginConfig { pkg: pkg.clone() },
                    move |app, result| {
                        let config = result.and_then(|a| match a {
                            Answer::PluginConfig(c) => Ok(c),
                            other => Err(unexpected(other)),
                        });
                        app.settings.plugins.config_loaded(&pkg, config);
                    },
                );
            }
            PluginsAction::SetConfig { pkg, key, value } => {
                let reload = self.live_target();
                self.gw.mutate(
                    Mutation::PluginConfigSet {
                        pkg: pkg.clone(),
                        key: key.clone(),
                        value: value.0,
                        reload,
                    },
                    move |app, result| {
                        app.settings
                            .plugins
                            .config_set_done(&pkg, &key, result.map(|_| ()));
                    },
                );
            }
        }
    }

    /// The directory advisor queries run in: the active tab's project; the user document needs
    /// none, but omp still runs in some directory.
    fn advisor_cwd(&self) -> PathBuf {
        self.settings_project()
            .or_else(|| self.home.clone())
            .unwrap_or_else(deck_core::platform::root_dir)
    }

    pub(super) fn apply_advisor_action(&mut self, action: AdvisorAction) {
        match action {
            AdvisorAction::Load { scope } => {
                let project = self.settings_project();
                let cwd = self.advisor_cwd();
                self.gw
                    .query(Query::AdvisorConfig { cwd, scope }, move |app, result| {
                        let doc = result.and_then(|a| match a {
                            Answer::AdvisorConfig(d) => Ok(d),
                            other => Err(unexpected(other)),
                        });
                        app.settings.advisor.loaded(scope, project.as_deref(), doc);
                    });
            }
            AdvisorAction::Save {
                scope,
                project,
                doc,
            } => {
                if scope == AdvisorScope::Project && project.is_none() {
                    self.settings
                        .advisor
                        .saved(Err("This document has no project folder to save to".into()));
                    return;
                }
                // The draft's own folder, never the active tab's.
                let cwd = project
                    .clone()
                    .or_else(|| self.home.clone())
                    .unwrap_or_else(deck_core::platform::root_dir);
                let tab_project = self.settings_project();
                let apply = if apply_to_active(scope, project.as_deref(), tab_project.as_deref()) {
                    self.live_target()
                } else {
                    None
                };
                self.gw.mutate(
                    Mutation::AdvisorSave {
                        cwd,
                        scope,
                        doc,
                        apply,
                    },
                    |app, result| {
                        let message = result.and_then(|d| match d {
                            Done::Message(m) => Ok(m),
                            other => Err(unexpected(other)),
                        });
                        app.settings.advisor.saved(message);
                    },
                );
            }
            AdvisorAction::Models(a) => self.apply_agents_action(a),
        }
    }

    /// `defaultThinkingLevel` was saved or reset in Settings: the status bar's default follows,
    /// and so does the session of the active tab, as omp's own `/settings` does — through
    /// `/effort`, which the gateway sends once the session is idle.
    pub(super) fn follow_default_thinking(&mut self) {
        let Some(level) = self
            .settings
            .omp_effective(THINKING_KEY)
            .and_then(|v| v.as_str())
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
        else {
            return;
        };
        // The level on a model selector (`modelRoles.default`) wins over the setting.
        let role_has_level = self
            .settings
            .omp_effective("modelRoles")
            .and_then(|v| v.get("default"))
            .and_then(|v| v.as_str())
            .is_some_and(|m| split_level(m).1.is_some());
        if !role_has_level {
            self.omp_defaults.level = Some(level.clone());
        }

        let live = self.live_target().is_some();
        let snap = self.status_snapshot();
        let live_snap = self.live_snapshot();
        let model = live_snap.as_deref().and_then(|l| l.model.as_ref());
        let stats = snap.as_deref().map(|s| &s.stats);
        let state = stats.map(|s| statusbar::model_state(s, model));
        let session = state.map(|s| SessionLevel {
            live,
            level: s.thinking,
            configured: s.configured,
        });
        if let Some(level) = thinking_to_apply(&level, session) {
            self.queue_effort(level);
        }
    }
}
