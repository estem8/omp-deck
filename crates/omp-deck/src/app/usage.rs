//! App-side glue for the Usage view and the status-bar usage chip: the gateway owns the fetch
//! schedule and the `omp usage` runs (see `Topic::Usage`); the window adopts its view, reads the
//! activity statistics on demand and sends the manual refresh.

use std::time::{Instant, SystemTime};

use deck_core::ctl::CtlCommand;
use deck_core::usage::{ACTIVITY_DAYS, AccountPolicy};
use deck_service::gateway::protocol::{Answer, Done, Mutation, Query, Topic};
use egui::{Key, Modifiers};

use super::{App, PendingConfirm};
use crate::confirm::Confirm;
use crate::usage_state::UsageEvent;
use crate::usage_view::{self, UsageAction, ViewInput};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
}

impl App {
    /// Per frame: adopt the gateway's usage view, keep the "resets in" labels fresh.
    pub(super) fn drive_usage(&mut self) {
        let rev = self.gw.revision(&Topic::Usage);
        if rev != self.usage_rev {
            self.usage_rev = rev;
            if let Some(view) = self.gw.usage() {
                self.usage.adopt(&view, now_ms());
            }
        }
        self.usage.omp_found(self.omp_found());
        let now = Instant::now();
        if self.usage.recompute_due(now) {
            self.usage.recompute(now, now_ms());
        }
    }

    /// The Usage view's keyboard shortcut (Ctrl+Shift+U).
    pub(super) fn usage_shortcut(&self, ctx: &egui::Context) -> bool {
        ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::U))
    }

    pub(super) fn toggle_usage(&mut self) {
        if self.usage_open {
            self.close_usage();
        } else {
            self.open_usage();
        }
    }

    pub(super) fn open_usage(&mut self) {
        self.usage_open = true;
        // omp's `/usage reset` lands on the Saved resets page; every opening reads them afresh.
        if std::mem::take(&mut self.usage_reset) {
            self.usage_ui.page = usage_view::Page::Resets;
        }
        self.usage_ui.resets.invalidate();
        if self.usage.activity_due(Instant::now()) {
            self.start_activity_read();
        }
    }

    /// Asks omp for the saved resets of every stored account (the providers are queried, which
    /// can take seconds; the page keeps the last list meanwhile).
    fn load_resets(&mut self) {
        if self.usage_ui.resets.loading {
            return;
        }
        self.usage_ui.resets.begin_load();
        self.gw
            .query(Query::UsageResets { provider: None }, |app, r| {
                let r = match r {
                    Ok(Answer::UsageResets(list)) => Ok(list),
                    Ok(_) => Err("unexpected answer".to_owned()),
                    Err(e) => Err(e),
                };
                app.usage_ui.resets.loaded(r);
            });
    }

    /// The spend was confirmed: omp spends one saved reset. The gateway re-reads usage itself
    /// afterwards; the saved-reset list is read again here.
    pub(super) fn execute_spend_reset(&mut self, provider: String, credential: String) {
        if self.usage_ui.resets.spending.is_some() {
            return;
        }
        self.usage_ui.resets.spending = Some((provider.clone(), credential.clone()));
        self.gw.mutate(
            Mutation::UsageResetSpend {
                provider,
                credential,
            },
            |app, r| {
                app.usage_ui.resets.spending = None;
                match r {
                    Ok(Done::Message(m)) if !m.is_empty() => app.toasts.success(&app.ctx, m),
                    Ok(_) => app.toasts.success(&app.ctx, "Saved reset spent"),
                    Err(e) => app
                        .toasts
                        .error(&app.ctx, format!("Could not spend the reset: {e}")),
                }
                app.load_resets();
            },
        );
    }

    pub(super) fn close_usage(&mut self) {
        self.usage_open = false;
        self.focus_pending = true;
    }

    /// The gateway re-reads the quotas now (`omp usage --json`, through omp's report cache like
    /// omp's own `/usage`).
    fn start_usage_fetch(&mut self) {
        self.gw.mutate(Mutation::UsageRefresh, |app, r| {
            if let Err(e) = r {
                app.toasts
                    .error(&app.ctx, format!("Usage refresh failed: {e}"));
            }
        });
    }

    /// `stats.db` (read by the gateway, read-only).
    fn start_activity_read(&mut self) {
        self.usage.begin_activity();
        self.gw.query(
            Query::UsageActivity {
                days: ACTIVITY_DAYS,
            },
            |app, r| {
                let r = r.and_then(|a| match a {
                    Answer::UsageActivity(a) => Ok(a),
                    other => Err(format!("unexpected answer: {other:?}")),
                });
                app.on_usage_event(UsageEvent::Activity(r));
            },
        );
    }

    pub(super) fn on_usage_event(&mut self, ev: UsageEvent) {
        self.usage.apply(ev, Instant::now());
    }

    /// Per frame: draw the Usage view while it is open and run what it asks for.
    pub(super) fn show_usage(&mut self, ctx: &egui::Context) {
        if !self.usage_open {
            return;
        }
        let now = Instant::now();
        let live = self.live_snapshot();
        let input = ViewInput {
            load: self.usage.load(),
            summary: self.usage.summary(),
            activity: self.usage.activity(),
            error: self.usage.error(),
            activity_error: self.usage.activity_error(),
            activity_loading: self.usage.activity_loading(),
            fetching: self.usage.fetching(),
            age_secs: self.usage.age_secs(now_ms()),
            session: live.as_ref().and_then(|s| s.account.as_ref()),
            // omp's own default when the catalog has not been read.
            global_reserve: self.omp_usage_reserve.unwrap_or(10),
        };
        let actions = usage_view::show(ctx, &mut self.usage_ui, &input);
        // "Updated Xs ago" ticks while the view is open.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
        for a in actions {
            match a {
                UsageAction::Close => self.close_usage(),
                UsageAction::Refresh => {
                    if self.usage.can_refresh() {
                        self.start_usage_fetch();
                        if self.usage.activity_due(now) {
                            self.start_activity_read();
                        }
                    }
                }
                UsageAction::InstallOmp => {
                    self.close_usage();
                    self.open_install_tab();
                }
                UsageAction::SaveAccounts {
                    provider,
                    name,
                    policies,
                } => self.save_account_policies(provider, name, policies),
                UsageAction::Pin {
                    provider,
                    credential,
                    label,
                } => self.pin_account(provider, credential, label),
                UsageAction::LoadResets => self.load_resets(),
                UsageAction::SpendReset {
                    provider,
                    credential,
                    question,
                } => {
                    self.confirm = Some((
                        Confirm {
                            title: "Spend a saved reset?".to_owned(),
                            body: format!(
                                "{question} The limit it covers clears right away; this cannot \
                                 be undone."
                            ),
                            confirm_label: "Spend reset".to_owned(),
                            danger: false,
                        },
                        PendingConfirm::SpendReset {
                            provider,
                            credential,
                        },
                    ));
                }
            }
        }
    }

    /// Writes `provider`'s `auth.accountPolicies` entries, then re-reads omp's usage so the page
    /// shows how omp now routes.
    fn save_account_policies(
        &mut self,
        provider: String,
        name: String,
        policies: Vec<AccountPolicy>,
    ) {
        let reset = policies.is_empty();
        self.usage_ui.saving = Some(provider.clone());
        self.gw.mutate(
            Mutation::AccountPolicies {
                provider: provider.clone(),
                policies,
            },
            move |app, result| {
                app.usage_ui.saving = None;
                match result {
                    Ok(_) => {
                        app.usage_ui.saved(provider, app.usage.summary());
                        app.toasts.success(
                            &app.ctx,
                            if reset {
                                format!("{name}: omp orders the accounts by itself again")
                            } else {
                                format!("{name}: account order saved")
                            },
                        );
                        app.start_usage_fetch();
                    }
                    Err(e) => app.toasts.error(
                        &app.ctx,
                        format!(
                            "Account order not saved: {}",
                            crate::summary::strip_context(&e)
                        ),
                    ),
                }
                app.ctx.request_repaint();
            },
        );
    }

    /// `/session pin` (or unpin, `credential: None`) for the focused tab's omp session.
    fn pin_account(&mut self, provider: String, credential: Option<String>, label: Option<String>) {
        let command = CtlCommand::PinAccount {
            root: self.ctl_root().map(std::path::Path::to_path_buf),
            provider,
            credential,
        };
        let sent = self.ctl_command(command, move |app, result| {
            app.usage_ui.pinning = false;
            let done = match &label {
                Some(l) => format!("This tab is pinned to {l}"),
                None => "omp chooses this tab's account again".to_owned(),
            };
            match result {
                Ok(r) if r.ok => app.toasts.success(&app.ctx, done),
                Ok(r) => app.toasts.error(
                    &app.ctx,
                    format!(
                        "omp refused: {}",
                        r.error.as_deref().unwrap_or("no reason given")
                    ),
                ),
                Err(e) => app.toasts.error(
                    &app.ctx,
                    format!("Pin failed: {}", crate::summary::strip_context(&e)),
                ),
            }
            app.ctx.request_repaint();
        });
        self.usage_ui.pinning = sent;
    }
}
