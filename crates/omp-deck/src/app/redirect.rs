//! The answer to a chat view's [`Redirect`]: omp opened (or was about to open) one of its own
//! screens, and OmpDeck's counterpart opens instead. The pane that asked is already the active
//! tab when this runs.

use std::path::{Path, PathBuf};

use deck_core::sessions::ProjectGroup;

use super::App;
use super::session_manager::ResumeOrigin;
use crate::chat_view::Redirect;
use crate::tabs::TabRole;

/// The project the session manager lists first for a resume from the tab with `session` (its
/// session file) running in `cwd`: the sidebar group that holds the file (a worktree session is
/// listed under its project, not under the folder it runs in), else the folder itself.
pub(super) fn resume_scope(groups: &[ProjectGroup], session: Option<&Path>, cwd: &Path) -> PathBuf {
    session
        .and_then(|s| {
            groups
                .iter()
                .find(|g| g.sessions.iter().any(|e| e.path == s))
        })
        .map_or_else(|| cwd.to_path_buf(), |g| g.cwd.clone())
}

impl App {
    /// Open OmpDeck's counterpart of the omp screen `redirect` names.
    pub(super) fn apply_redirect(&mut self, redirect: Redirect) {
        match redirect {
            Redirect::OmpSettings { page } => {
                self.open_settings();
                self.settings.open_omp_page(page.as_deref());
            }
            Redirect::ModelPicker => {
                self.picker.request_open();
                self.ctx.request_repaint();
            }
            Redirect::Usage { reset } => {
                self.usage_reset = reset;
                self.open_usage();
            }
            Redirect::Resume { foreign } => {
                let Some(tab) = self
                    .tabs
                    .get(self.active)
                    .filter(|t| t.role == TabRole::Session)
                else {
                    return;
                };
                let origin = ResumeOrigin {
                    tab: tab.uid,
                    foreign,
                };
                let session = tab.session_path_owned();
                let scope = match self.groups() {
                    Some(groups) => resume_scope(&groups, session.as_deref(), &tab.cwd),
                    None => tab.cwd.clone(),
                };
                self.open_resume(origin, scope);
            }
            Redirect::Tree => self.open_tree_active(),
        }
    }
}

#[cfg(test)]
mod tests {
    use deck_core::sessions::SessionEntry;

    use super::*;

    fn group(cwd: &str, files: &[&str]) -> ProjectGroup {
        ProjectGroup {
            cwd: cwd.into(),
            display: cwd.into(),
            sessions: files
                .iter()
                .map(|f| SessionEntry {
                    id: (*f).into(),
                    path: (*f).into(),
                    cwd: cwd.into(),
                    title: None,
                    first_prompt: None,
                    modified: std::time::SystemTime::UNIX_EPOCH,
                })
                .collect(),
            worktree: None,
        }
    }

    #[test]
    fn resume_lists_the_project_that_holds_the_session_even_for_a_worktree() {
        let groups = [
            group("/p/app", &["/s/a.jsonl", "/s/wt.jsonl"]),
            group("/p/other", &["/s/o.jsonl"]),
        ];
        let run = Path::new("/p/app-wt/feature");
        assert_eq!(
            resume_scope(&groups, Some(Path::new("/s/wt.jsonl")), run),
            Path::new("/p/app")
        );
        // A session without a file yet, or one no group lists: where it runs.
        assert_eq!(resume_scope(&groups, None, run), run);
        assert_eq!(
            resume_scope(&groups, Some(Path::new("/s/new.jsonl")), run),
            run
        );
    }
}
