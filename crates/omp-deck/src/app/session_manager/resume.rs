//! App side of the session manager's resume mode: the gateway requests behind its actions
//! (resume into the origin tab, with the busy confirmation; import a Claude Code / Codex session;
//! pin; rename) and the pin / foreign-list reads. The rules are [`crate::resume_model`], the
//! screen is [`crate::sessions_view`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_core::omp::ForeignSource;
use deck_service::controller::SessionTarget;
use deck_service::gateway::protocol::{Answer, Done as Reply, Mutation, Query};

use super::{ResumeOrigin, SessionHub, session_entry};
use crate::app::redirect::resume_scope;
use crate::app::{App, PendingConfirm};
use crate::confirm::{self, Confirm, Dialog};
use crate::icons::ICON_PENCIL;
use crate::resume_model::{
    Epoch, Failure, ForeignLoad, ImportNext, ImportSettled, ResumeNext, Source, Then,
    delivery_result, import_cwd, settle_import, settle_resume,
};
use crate::tabs::{Tab, TabRole};
use crate::theme::Palette;

/// How long an imported session may take to show up in the list before opening it is given up.
const LISTED_WAIT: Duration = Duration::from_secs(10);

/// The rename dialog of the hub (a row's Rename…, Ctrl+R).
pub(super) struct RenameDialog {
    path: PathBuf,
    /// The title as it was, shown as the fact above the editor.
    was: String,
    text: String,
    /// Focus the editor the first frame.
    first: bool,
    /// The request is out.
    busy: bool,
    /// What the last attempt failed with.
    error: Option<String>,
}

/// The tab Resume here acts on, or why there is none.
pub(super) fn resume_here<'a>(
    origin: Option<&ResumeOrigin>,
    tabs: &'a [Tab],
    labels: &'a std::collections::HashMap<PathBuf, String>,
) -> Result<&'a str, &'static str> {
    let Some(origin) = origin else {
        return Err("Open this from a session tab to resume into it");
    };
    let Some(tab) = tabs.iter().find(|t| t.uid == origin.tab) else {
        return Err("The tab this was opened from is closed");
    };
    if tab.role != TabRole::Session {
        return Err("Resume here needs an omp session tab");
    }
    if tab.terminal().is_exited() {
        return Err("omp has exited in that tab");
    }
    Ok(tab.title(labels))
}

/// What a request that ends with a typed command reports: `Ok` once omp's terminal took it.
fn typed_result(r: Result<Reply, String>) -> Result<(), String> {
    match r? {
        Reply::Submitted(record) => delivery_result(&record.delivery),
        _ => Ok(()),
    }
}

impl App {
    // ------------------------------------------------------------ entering

    /// The hub opens in resume mode for a `/resume` redirected from `origin`'s tab.
    pub(in crate::app) fn open_resume(&mut self, origin: ResumeOrigin, scope: PathBuf) {
        let source = Source::of(origin.foreign);
        self.open_sessions(Some(scope.clone()));
        self.sessions.resume_origin = Some(origin);
        self.start_resume_mode(source, Some(scope));
    }

    /// The toolbar's Resume…: the active session tab (if any) is where Resume here goes.
    pub(in crate::app) fn enter_resume_mode(&mut self) {
        let mut project = self.sessions.model.scope.clone();
        if self.sessions.resume_origin.is_none()
            && let Some(tab) = self
                .tabs
                .get(self.active)
                .filter(|t| t.role == TabRole::Session)
        {
            let session = tab.session_path_owned();
            let scope = match self.groups() {
                Some(groups) => resume_scope(&groups, session.as_deref(), &tab.cwd),
                None => tab.cwd.clone(),
            };
            self.sessions.resume_origin = Some(ResumeOrigin {
                tab: tab.uid,
                foreign: None,
            });
            project.get_or_insert(scope);
        }
        let source = self.sessions.view.resume.source;
        self.start_resume_mode(source, project);
    }

    fn start_resume_mode(&mut self, source: Source, project: Option<PathBuf>) {
        let hub = &mut self.sessions;
        hub.mode = crate::running_model::HubMode::Resume;
        hub.view.resume.open(source, project);
        self.load_pins();
        if let Some(src) = source.foreign() {
            self.load_foreign(src);
        }
    }

    /// Resume mode lists another source.
    pub(in crate::app) fn set_resume_source(&mut self, source: Source) {
        self.sessions.view.resume.set_source(source);
        if let Some(src) = source.foreign()
            && matches!(
                self.sessions.view.resume.foreign(src),
                ForeignLoad::Idle | ForeignLoad::Failed(_)
            )
        {
            self.load_foreign(src);
        }
    }

    // ------------------------------------------------------------- reads

    /// Reads omp's pins again.
    pub(in crate::app) fn load_pins(&mut self) {
        self.gw.query(Query::SessionPins, |app, r| match r {
            Ok(Answer::SessionPins(pins)) => {
                app.sessions
                    .model
                    .set_pins(pins.into_iter().collect::<HashSet<_>>());
            }
            Ok(_) => {}
            Err(e) => log::warn!("session pins: {e}"),
        });
    }

    /// Reads the Claude Code / Codex sessions (can take a moment: omp parses every file).
    pub(in crate::app) fn load_foreign(&mut self, src: ForeignSource) {
        self.sessions
            .view
            .resume
            .set_foreign(src, ForeignLoad::Loading);
        self.gw
            .query(Query::ForeignSessions { source: src }, move |app, r| {
                let load = match r {
                    Ok(Answer::ForeignSessions(list)) => ForeignLoad::Ready(list),
                    Ok(_) => ForeignLoad::Failed("unexpected answer".to_owned()),
                    Err(e) => ForeignLoad::Failed(e),
                };
                app.sessions.view.resume.set_foreign(src, load);
            });
    }

    // --------------------------------------------------------------- pin

    pub(in crate::app) fn pin_session(&mut self, id: String, pinned: bool) {
        self.gw
            .mutate(Mutation::SessionPin { id, pinned }, |app, r| match r {
                Ok(_) => app.load_pins(),
                Err(e) => app
                    .toasts
                    .error(&app.ctx, format!("Could not change the pin: {e}")),
            });
    }

    // ------------------------------------------------------------ rename

    pub(in crate::app) fn ask_rename(&mut self, path: &Path) {
        let Some(row) = self.sessions.model.row_by_path(path) else {
            return;
        };
        let was = row.entry.title.clone().unwrap_or_default();
        self.sessions.rename = Some(RenameDialog {
            path: path.to_path_buf(),
            text: was.clone(),
            was,
            first: true,
            busy: false,
            error: None,
        });
    }

    /// Draws the rename dialog while one is open and sends it when confirmed.
    pub(super) fn show_rename(&mut self, ctx: &egui::Context) {
        let Some(d) = self.sessions.rename.as_mut() else {
            return;
        };
        let pal = Palette::of_ctx(ctx);
        let result = confirm::show_dialog(
            ctx,
            &Dialog {
                id: "hub-rename",
                title: "Rename session",
                width: 440.0,
                confirm_label: "Rename",
                confirm_enabled: !d.text.trim().is_empty() && !d.busy,
                danger: false,
                enter_confirms: true,
                icon: Some(ICON_PENCIL),
                subtitle: Some("Shown in tabs and session lists"),
                ..Default::default()
            },
            |ui| {
                if !d.was.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Now").color(pal.weak));
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&d.was).color(pal.text_strong).strong(),
                            )
                            .truncate(),
                        );
                    });
                    ui.add_space(6.0);
                }
                let r = ui.add_enabled(
                    !d.busy,
                    egui::TextEdit::singleline(&mut d.text)
                        .hint_text("New title")
                        .desired_width(f32::INFINITY)
                        .margin(egui::vec2(10.0, 8.0)),
                );
                if std::mem::take(&mut d.first) {
                    r.request_focus();
                }
                if let Some(e) = &d.error {
                    ui.add_space(6.0);
                    ui.add(
                        egui::Label::new(egui::RichText::new(e).color(pal.danger))
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                }
            },
        );
        match result {
            Some(true) => self.send_rename(),
            Some(false) => {
                self.sessions.rename = None;
                self.focus_pending = true;
            }
            None => {}
        }
    }

    fn send_rename(&mut self) {
        let Some(d) = self.sessions.rename.as_mut() else {
            return;
        };
        if d.busy {
            return;
        }
        d.busy = true;
        d.error = None;
        let (file, title) = (d.path.clone(), d.text.trim().to_owned());
        let path = file.clone();
        self.gw
            .mutate(Mutation::SessionRename { file, title }, move |app, r| {
                let r = typed_result(r);
                let still = app.sessions.rename.as_ref().is_some_and(|d| d.path == path);
                match r {
                    Ok(()) => {
                        if still {
                            app.sessions.rename = None;
                            app.focus_pending = true;
                        }
                        app.refresh_sessions();
                        app.sessions.inventory_rev = 0;
                    }
                    Err(e) if still => {
                        if let Some(d) = app.sessions.rename.as_mut() {
                            d.busy = false;
                            d.error = Some(e);
                        }
                    }
                    Err(e) => app.toasts.error(&app.ctx, format!("Could not rename: {e}")),
                }
            });
    }

    // ------------------------------------------------------------ resume

    /// Where a resume asked for now goes: the origin tab and its session, as they are at this
    /// moment (later changes of the hub do not redirect a request that is already out).
    fn origin_dest(&self) -> Option<ResumeDest> {
        let uid = self.sessions.resume_origin.as_ref()?.tab;
        let tab = self.tabs.iter().find(|t| t.uid == uid)?;
        Some(ResumeDest {
            tab: uid,
            target: tab.target().clone(),
        })
    }

    /// Resume `file` into the origin tab.
    pub(in crate::app) fn start_resume(&mut self, file: PathBuf) {
        if !self.sessions.view.resume.flow.begin_resume(file.clone()) {
            return;
        }
        self.sessions.view.resume.error = None;
        self.sessions.resume_dest = self.origin_dest();
        self.send_resume(file, false);
    }

    /// Sends the resume to the destination fixed when the request was started.
    fn send_resume(&mut self, file: PathBuf, force: bool) {
        let asked = self.sessions.resume_epoch;
        let Some(dest) = self.sessions.resume_dest.clone() else {
            self.resume_finished(
                asked,
                None,
                Err(Failure::Other(
                    "The tab this was opened from is closed".to_owned(),
                )),
            );
            return;
        };
        let tab = dest.tab;
        self.gw.mutate_typed(
            Mutation::ResumeHere {
                target: dest.target,
                file,
                force,
            },
            move |app, r| {
                let r = r.map_err(Failure::from).and_then(|done| match done {
                    Reply::Submitted(record) => {
                        delivery_result(&record.delivery).map_err(Failure::Other)
                    }
                    _ => Ok(()),
                });
                app.resume_finished(asked, Some(tab), r);
            },
        );
    }

    /// A resume request asked in `asked` ended. A request of an earlier opening of the manager
    /// changes nothing here; only a failure is told.
    fn resume_finished(&mut self, asked: Epoch, tab: Option<u64>, result: Result<(), Failure>) {
        let failed = result.as_ref().err().cloned();
        let now = self.sessions.resume_epoch;
        let hub = &mut self.sessions;
        match settle_resume(now, asked, &mut hub.view.resume.flow, result) {
            None => {
                if let Some(f) = failed {
                    self.toasts
                        .error(&self.ctx, format!("Could not resume: {}", f.text()));
                }
            }
            Some(ResumeNext::Done) => {
                self.close_sessions();
                if let Some(i) = tab.and_then(|u| self.tabs.iter().position(|t| t.uid == u)) {
                    self.activate(i);
                }
            }
            Some(ResumeNext::AskForce(_)) => {
                self.confirm = Some((
                    Confirm {
                        title: "Stop the running turn?".to_owned(),
                        body: "omp is working in this tab. Resuming another session stops the \
                               current turn first."
                            .to_owned(),
                        confirm_label: "Stop and resume".to_owned(),
                        danger: false,
                    },
                    PendingConfirm::ResumeForce,
                ));
            }
            Some(ResumeNext::Failed(e)) => self.sessions.view.resume.error = Some(e),
        }
    }

    /// The busy question was answered.
    pub(in crate::app) fn answer_resume_force(&mut self, confirmed: bool) {
        if let Some(file) = self.sessions.view.resume.flow.force(confirmed) {
            self.send_resume(file, true);
        }
    }

    // ------------------------------------------------------------ import

    /// Import the foreign session `id` (it ran in `cwd`), then resume it here or open it.
    pub(in crate::app) fn start_import(
        &mut self,
        source: ForeignSource,
        id: String,
        cwd: &str,
        then: Then,
    ) {
        if !self
            .sessions
            .view
            .resume
            .flow
            .begin_import(id.clone(), then)
        {
            return;
        }
        self.sessions.view.resume.error = None;
        // The destination is fixed now: a resume that follows goes to this tab, whatever the
        // manager is opened for while the import runs.
        self.sessions.resume_dest = match then {
            Then::ResumeHere => self.origin_dest(),
            Then::NewTab => None,
        };
        let origin = self
            .sessions
            .resume_origin
            .as_ref()
            .and_then(|o| self.tabs.iter().find(|t| t.uid == o.tab))
            .map(|t| t.cwd.as_path());
        let cwd = import_cwd(cwd, origin, self.sessions.view.resume.scope(), |p| {
            p.exists()
        });
        let asked = self.sessions.resume_epoch;
        self.gw.mutate(
            Mutation::ForeignImport { source, id, cwd },
            move |app, r| {
                let r = match r {
                    Ok(Reply::Path(file)) => Ok(file),
                    Ok(_) => Err("omp did not report the imported file".to_owned()),
                    Err(e) => Err(e),
                };
                app.import_finished(asked, r);
            },
        );
    }

    /// An import asked in `asked` ended. One of an earlier opening of the manager only reports
    /// itself: it resumes nothing and opens nothing.
    fn import_finished(&mut self, asked: Epoch, result: Result<PathBuf, String>) {
        let now = self.sessions.resume_epoch;
        match settle_import(now, asked, &mut self.sessions.view.resume.flow, result) {
            ImportSettled::Stale(Ok(file)) => {
                self.refresh_for_import();
                let name = file
                    .file_stem()
                    .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
                self.toasts.success(&self.ctx, format!("Imported {name}"));
            }
            ImportSettled::Stale(Err(e)) => self
                .toasts
                .error(&self.ctx, format!("Could not import: {e}")),
            ImportSettled::Live(ImportNext::ResumeHere(file)) => {
                self.refresh_for_import();
                self.send_resume(file, false);
            }
            ImportSettled::Live(ImportNext::NewTab(file)) => {
                self.refresh_for_import();
                self.sessions.open_when_listed = Some((file, Instant::now()));
                self.ctx.request_repaint();
            }
            ImportSettled::Live(ImportNext::Failed(e)) => {
                self.sessions.view.resume.error = Some(e);
            }
        }
    }

    fn refresh_for_import(&mut self) {
        self.refresh_sessions();
        self.sessions.inventory_rev = 0;
    }

    /// Per frame while the hub is open: opens the imported session in a tab once the inventory
    /// lists its file (the tab needs the session's id), or gives up after [`LISTED_WAIT`].
    pub(super) fn poll_listed(&mut self) {
        let Some((file, since)) = self.sessions.open_when_listed.clone() else {
            return;
        };
        if let Some(row) = self.sessions.model.row_by_path(&file) {
            let entry = session_entry(&row.entry);
            self.sessions.open_when_listed = None;
            self.sessions.view.resume.flow.listed();
            self.close_sessions();
            self.open_session(&entry);
        } else if since.elapsed() > LISTED_WAIT {
            self.sessions.open_when_listed = None;
            self.sessions.view.resume.flow.listed();
            self.sessions.view.resume.error = Some(
                "The imported session did not show up in the list — refresh and retry.".into(),
            );
        } else {
            self.ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

/// Where a resume goes, fixed when it was asked for: the origin tab and the session it ran.
#[derive(Clone)]
pub(super) struct ResumeDest {
    tab: u64,
    target: SessionTarget,
}

impl SessionHub {
    /// The hub closes: nothing keeps waiting for a list that is no longer shown.
    pub(super) fn drop_waits(&mut self) {
        self.open_when_listed = None;
        self.view.resume.flow.reset();
        self.resume_dest = None;
        self.rename = None;
        // Whatever is still in flight answers into a manager that is no longer this one.
        self.resume_epoch.advance();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resume_without_an_origin_says_why() {
        let labels = std::collections::HashMap::new();
        assert!(resume_here(None, &[], &labels).is_err());
        let gone = ResumeOrigin {
            tab: 7,
            foreign: None,
        };
        assert_eq!(
            resume_here(Some(&gone), &[], &labels),
            Err("The tab this was opened from is closed")
        );
    }

    #[test]
    fn a_typed_command_only_succeeds_when_omp_took_it() {
        assert_eq!(typed_result(Ok(Reply::Unit)), Ok(()));
        assert_eq!(typed_result(Err("boom".to_owned())), Err("boom".to_owned()));
        assert!(crate::resume_model::Flow::default().is_idle());
    }
}
