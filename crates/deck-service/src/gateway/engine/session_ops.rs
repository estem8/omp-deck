//! Executes the mutations behind OmpDeck's own settings, usage, resume and tree screens: plugin
//! changes, saved usage resets, session pins, renames, labels, foreign imports, the advisor
//! document and `/resume`. Whether a change reaches a running omp (typed into its controller,
//! through live control) or the closed session file (omp's lease-checked headless helper) is
//! decided here from what this gateway runs; see [`super::deferred`] for the routing rules.

use std::path::Path;
use std::time::Instant;

use deck_core::ctl::CtlCommand;

use super::deferred::{ResumePlan, Route, label_route, rename_route, resume_plan};
use super::mutate::{Post, send};
use super::{Engine, Sched};
use crate::controller::{Command, Rejected, RequestKey};
use crate::gateway::jobs;
use crate::gateway::protocol::{Done, GatewayError, Mutation, Outcome, SessionRole};
use crate::session_actions::Action;

impl Engine {
    /// The controller (index) of a running omp session of this gateway whose file is `file`.
    fn controller_of(&self, file: &Path) -> Option<usize> {
        self.sess.managed.iter().position(|m| {
            !m.exited() && m.role == SessionRole::Session && m.ctl.session_path() == Some(file)
        })
    }

    pub(super) fn execute_session_ops(
        &mut self,
        conn: u64,
        id: u64,
        key: RequestKey,
        op: Mutation,
    ) {
        match op {
            Mutation::PluginEnable {
                name,
                enabled,
                scope,
                cwd,
                reload,
            } => {
                self.accepted(conn, id, &key);
                let post = reload.map_or(Post::None, Post::ReloadPlugins);
                self.job(
                    key,
                    "plugin-enable",
                    false,
                    false,
                    post,
                    move |ctx, _, _| {
                        jobs::plugin_enable(ctx, &name, enabled, scope, cwd.as_deref())
                    },
                );
            }
            Mutation::PluginFeatures {
                pkg,
                features,
                reload,
            } => {
                self.accepted(conn, id, &key);
                let post = reload.map_or(Post::None, Post::ReloadPlugins);
                self.job(
                    key,
                    "plugin-features",
                    false,
                    false,
                    post,
                    move |ctx, _, _| jobs::plugin_features(ctx, &pkg, &features),
                );
            }
            Mutation::PluginConfigSet {
                pkg,
                key: setting,
                value,
                reload,
            } => {
                self.accepted(conn, id, &key);
                let post = reload.map_or(Post::None, Post::ReloadPlugins);
                self.job(
                    key,
                    "plugin-config-set",
                    false,
                    false,
                    post,
                    move |ctx, _, _| jobs::plugin_config_set(ctx, &pkg, &setting, value.as_ref()),
                );
            }
            Mutation::UsageResetSpend {
                provider,
                credential,
            } => {
                self.accepted(conn, id, &key);
                if !self.sched.usage_fetching() {
                    self.sched.begin_usage_fetch(Instant::now());
                    self.dirty.usage = true;
                }
                let tx = self.tx.clone();
                self.job(
                    key,
                    "usage-reset-spend",
                    false,
                    false,
                    Post::None,
                    move |ctx, _, _| {
                        let spent = jobs::usage_reset_spend(ctx, &provider, &credential);
                        // Whatever omp answered, usage is read again: omp drops its own report
                        // cache when a spend succeeds (never `usage invalidate`: it would make
                        // a provider that cannot be reached right now "unavailable"), and a
                        // refusal ("already redeemed elsewhere") usually means it changed.
                        let fetched = super::mutate::fetch_usage(ctx);
                        send(&tx, Sched::Usage(fetched).into());
                        spent.map(Done::Message)
                    },
                );
            }
            Mutation::SessionPin {
                id: session,
                pinned,
            } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "session-pin",
                    false,
                    false,
                    Post::Refresh,
                    move |ctx, _, _| jobs::session_pin(ctx, &session, pinned),
                );
            }
            Mutation::SessionRename { file, title } => {
                match rename_route(self.controller_of(&file)) {
                    Route::Inject(i) => {
                        let target = self.sess.managed[i].ctl.target().clone();
                        self.submit_now(
                            conn,
                            id,
                            key,
                            i,
                            target,
                            Command::Action(Action::Rename(title)),
                        );
                    }
                    _ => {
                        self.accepted(conn, id, &key);
                        self.job(
                            key,
                            "session-rename",
                            false,
                            false,
                            Post::Refresh,
                            move |ctx, _, _| jobs::session_rename(ctx, &file, &title),
                        );
                    }
                }
            }
            Mutation::SessionLabel {
                file,
                entry_id,
                label,
            } => {
                let controller = self
                    .controller_of(&file)
                    .map(|i| (i, self.sess.managed[i].ctl_key.is_some()));
                let holder = self.ctl_holder(&file);
                match label_route(controller, holder) {
                    Route::Ctl(i) => {
                        self.track(&key, "session-label", Post::None);
                        self.accepted(conn, id, &key);
                        self.queue_ctl(
                            i,
                            key,
                            CtlCommand::SetLabel {
                                root: file,
                                entry_id,
                                label,
                            },
                        );
                    }
                    Route::CtlNamed(name) => {
                        self.accepted(conn, id, &key);
                        self.job(
                            key,
                            "session-label",
                            false,
                            false,
                            Post::None,
                            move |ctx, _, _| {
                                jobs::session_label_live(
                                    ctx,
                                    &name,
                                    &file,
                                    &entry_id,
                                    label.as_deref(),
                                )
                            },
                        );
                    }
                    Route::Inject(_) | Route::Helper => {
                        self.accepted(conn, id, &key);
                        self.job(
                            key,
                            "session-label",
                            false,
                            false,
                            Post::None,
                            move |ctx, _, _| {
                                jobs::session_label(ctx, &file, &entry_id, label.as_deref())
                            },
                        );
                    }
                }
            }
            Mutation::ForeignImport {
                source,
                id: foreign,
                cwd,
            } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "foreign-import",
                    false,
                    false,
                    Post::Refresh,
                    move |ctx, _, _| jobs::foreign_import(ctx, source, &foreign, &cwd),
                );
            }
            Mutation::AdvisorSave {
                cwd,
                scope,
                mut doc,
                apply,
            } => {
                // Read-only fields of what a read returned never travel back.
                doc.warnings.clear();
                doc.path = None;
                // The session to apply it in: any generation of a tab that still runs with live
                // control (the advisors belong to its omp process, not to one conversation).
                let live = apply
                    .filter(|t| t.epoch == self.epoch)
                    .and_then(|t| self.sess.find(&t.key))
                    .filter(|&i| {
                        let m = &self.sess.managed[i];
                        !m.exited() && m.ctl_key.is_some()
                    });
                let value = serde_json::to_value(&doc);
                match (live, value) {
                    (Some(i), Ok(value)) => {
                        let root = self.sess.managed[i]
                            .ctl
                            .session_path()
                            .map(Path::to_path_buf);
                        self.track(&key, "advisor-save", Post::None);
                        self.accepted(conn, id, &key);
                        self.queue_ctl(
                            i,
                            key,
                            CtlCommand::AdvisorConfig {
                                root,
                                scope,
                                doc: Some(value),
                            },
                        );
                    }
                    _ => {
                        self.accepted(conn, id, &key);
                        self.job(
                            key,
                            "advisor-save",
                            false,
                            false,
                            Post::None,
                            move |ctx, _, _| jobs::advisor_save(ctx, &cwd, scope, &doc),
                        );
                    }
                }
            }
            Mutation::ResumeHere {
                target,
                file,
                force,
            } => {
                let Some(i) = self.sess.find(&target.key) else {
                    return self.vanished(conn, id, key);
                };
                let m = &self.sess.managed[i];
                match resume_plan(m.ctl.activity(), m.ctl.is_busy(), force) {
                    ResumePlan::Send => {
                        self.submit_now(conn, id, key, i, target, Command::Resume { file });
                    }
                    // Turned away before it was journaled; kept total for a state that changed
                    // between the check and now.
                    ResumePlan::Refuse => {
                        let r = Rejected::Busy;
                        self.journal.finish(&key, Outcome::Rejected(r.clone()));
                        self.reply(conn, id, Err(GatewayError::Rejected(r)));
                    }
                    plan => {
                        self.track(&key, "resume-here", Post::None);
                        self.accepted(conn, id, &key);
                        self.queue_forced_resume(key, &target, file, plan);
                    }
                }
            }
            other => {
                let e = GatewayError::Failed(format!("{} does not run here", other.label()));
                self.journal.finish(&key, Outcome::Failed(e.to_string()));
                self.reply(conn, id, Err(e));
            }
        }
    }
}
