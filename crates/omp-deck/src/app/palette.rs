//! App-side glue for the command palette (Ctrl+Shift+K) and the quick switcher (Ctrl+Shift+E):
//! which commands exist and whether they are available right now, the switcher's tabs / sessions /
//! projects, and the dispatch of the picked row through the same functions the menus call.

use std::path::PathBuf;

use deck_core::config::{
    Density, MAX_PALETTE_RECENT, MAX_UI_SCALE, MIN_UI_SCALE, SessionView, Theme, UiFont, UiStyle,
};
use deck_core::omp::UpdateKind;
use deck_core::sessions::abbreviate_home;
use egui::{Key, Modifiers};

use super::bar::ActionDialog;
use super::{App, DEFAULT_FONT};
use crate::backup_view::BackupAction;
use crate::icons::*;
use crate::mobile_view::{can_reconnect, pair_block};
use crate::model_picker;
use crate::notify_view::NotifyAction;
use crate::palette::{Item, Mode, Outcome, PaletteState, Target};
use crate::settings_page::OmpDeckPage;
use crate::sidebar::{SidebarAction, resume_command};
use crate::sync_view::SyncAction;
use crate::tabs::{self, Tab, TabRole};
use crate::transfer::TransferKind;
use crate::update::Channel;
use deck_service::session_actions::{
    self, Action, ActionCtx, Availability, CompactMode, Kind, ShakeMode,
};

const NEED_SESSION: &str = "Open an omp session tab first";
const NEED_OMP: &str = "omp was not found";
const NEED_TAB: &str = "Open a tab first";
const NEED_ENTRY: &str = "This session is not in the sidebar yet";
const NEED_PROJECT: &str = "Open a tab or focus a folder first";
const NOT_WORKTREE: &str = "This project is not a linked git worktree";
const NO_BACKEND: &str = "No session backend is available";

/// Why a command cannot run now (`None` = it can).
type Why = Option<&'static str>;

impl App {
    /// Ctrl+Shift+K / Ctrl+Shift+E (called from `handle_shortcuts`, before the terminal sees keys).
    pub(super) fn palette_shortcuts(&mut self, ctx: &egui::Context) {
        let ctrl_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let (commands, switcher) = ctx.input_mut(|i| {
            (
                i.consume_key(ctrl_shift, Key::K),
                i.consume_key(ctrl_shift, Key::E),
            )
        });
        for (pressed, mode) in [(commands, Mode::Commands), (switcher, Mode::Switcher)] {
            if !pressed {
                continue;
            }
            match &self.palette {
                Some(p) if p.mode == mode => self.close_palette(),
                Some(_) => self.open_palette(mode),
                None if !self.modal_open() => self.open_palette(mode),
                None => {}
            }
        }
    }

    pub(super) fn open_palette(&mut self, mode: Mode) {
        // Keys that arrived in the same frame as the shortcut must not reach the terminal that still
        // holds focus for this frame; the overlay takes over from the next frame.
        self.ctx.input_mut(|i| {
            i.events
                .retain(|e| !matches!(e, egui::Event::Key { .. } | egui::Event::Text(_)));
        });
        let items = match mode {
            Mode::Commands => self.palette_commands(),
            Mode::Switcher => self.switcher_items(),
        };
        self.palette = Some(PaletteState::new(
            mode,
            items,
            self.cfg.palette_recent.clone(),
        ));
    }

    fn close_palette(&mut self) {
        self.palette = None;
        self.focus_pending = true;
    }

    /// Per frame: draw the overlay while it is open and run what it picked.
    pub(super) fn show_palette(&mut self, ctx: &egui::Context) {
        let Some(state) = &mut self.palette else {
            return;
        };
        match state.show(ctx) {
            Outcome::Open => {}
            Outcome::Close => self.close_palette(),
            Outcome::Run(target) => {
                self.close_palette();
                self.run_target(target);
            }
        }
    }

    fn run_target(&mut self, target: Target) {
        match target {
            Target::Command(id) => {
                self.remember_command(&id);
                // The overlay was built a moment ago; the state may have moved on.
                if let Some(why) = self.command_blocked(&id) {
                    self.toasts.info(&self.ctx, why);
                    return;
                }
                self.run_command(&id);
            }
            Target::Tab(uid) => {
                if let Some(i) = self.tabs.iter().position(|t| t.uid == uid) {
                    self.activate(i);
                }
            }
            Target::Session(path) => match self.find_entry(&path) {
                Some(entry) => self.open_session(&entry),
                None => self.refresh_sessions(),
            },
            Target::Project(dir) => self.set_scope(Some(dir)),
        }
    }

    fn remember_command(&mut self, id: &str) {
        let recent = &mut self.cfg.palette_recent;
        recent.retain(|r| r != id);
        recent.insert(0, id.to_owned());
        recent.truncate(MAX_PALETTE_RECENT);
        self.dirty = true;
    }

    fn command_blocked(&self, id: &str) -> Why {
        self.palette_commands()
            .into_iter()
            .find(|i| matches!(&i.target, Target::Command(c) if c == id))
            .and_then(|i| i.disabled)
    }

    // ------------------------------------------------------------ switcher

    fn switcher_items(&self) -> Vec<Item> {
        let running = self.gw.running();
        let mut items = Vec::new();
        for t in &self.tabs {
            items.push(Item {
                target: Target::Tab(t.uid),
                label: t.label(&self.labels).to_owned(),
                detail: abbreviate_home(&t.cwd),
                hint: None,
                icon: if t.role == TabRole::Session {
                    ICON_TERMINAL
                } else {
                    ICON_DOWNLOAD
                },
                disabled: None,
                running: !t.terminal().is_exited(),
            });
        }
        let Some(groups) = self.groups() else {
            return items;
        };
        let open: Vec<&std::path::Path> = self.tabs.iter().filter_map(Tab::session_path).collect();
        let mut sessions: Vec<(&deck_core::sessions::SessionEntry, &str)> = groups
            .iter()
            .flat_map(|g| g.sessions.iter().map(move |e| (e, g.display.as_str())))
            .filter(|(e, _)| !open.contains(&e.path.as_path()))
            .collect();
        sessions.sort_by_key(|(e, _)| std::cmp::Reverse(e.modified));
        for (e, project) in sessions {
            items.push(Item {
                target: Target::Session(e.path.clone()),
                label: e.label().to_owned(),
                detail: project.to_owned(),
                hint: None,
                icon: ICON_MESSAGES_SQUARE,
                disabled: None,
                running: running
                    .as_ref()
                    .is_some_and(|r| r.is_running(&e.id, &e.path)),
            });
        }
        let mut projects: Vec<_> = groups.iter().filter(|g| !g.sessions.is_empty()).collect();
        projects.sort_by_key(|g| std::cmp::Reverse(g.sessions.iter().map(|e| e.modified).max()));
        for g in projects {
            items.push(Item {
                target: Target::Project(g.cwd.clone()),
                label: g.display.clone(),
                detail: format!("focus folder · {} sessions", g.sessions.len()),
                hint: None,
                icon: ICON_FOLDER,
                disabled: None,
                running: false,
            });
        }
        items
    }

    /// The linked worktree of the active tab's folder: from the sidebar groups, else from the git
    /// status snapshot (a fresh worktree session has no session file, hence no group yet).
    fn active_worktree(&self) -> Option<deck_core::worktree::Worktree> {
        let term = self.tabs.get(self.active);
        let cwd = term.map(|t| t.cwd.clone()).or_else(|| self.scope.clone())?;
        let from_group = self.groups().and_then(|gs| {
            gs.iter()
                .find(|g| g.cwd == cwd)
                .and_then(|g| g.worktree.clone())
        });
        from_group.or_else(|| {
            let git = self.bar_view.git.as_ref()?;
            term.filter(|t| t.role == TabRole::Session)?;
            Some(deck_core::worktree::Worktree {
                path: cwd,
                primary: git.worktree_of.clone()?,
                branch: Some(git.branch.clone()).filter(|b| !b.is_empty()),
            })
        })
    }

    /// The worktree a picked Finish / Cancel / Remove acts on. The status-snapshot fallback of
    /// [`Self::active_worktree`] only gates the rows (the tab may sit in a subdirectory), so here the
    /// real root comes from `worktree::resolve` (a few small file reads, once per click).
    fn worktree_to_act_on(&self) -> Option<deck_core::worktree::Worktree> {
        let cwd = self
            .tabs
            .get(self.active)
            .map(|t| t.cwd.clone())
            .or_else(|| self.scope.clone())?;
        self.groups()
            .and_then(|gs| {
                gs.iter()
                    .find(|g| g.cwd == cwd)
                    .and_then(|g| g.worktree.clone())
            })
            .or_else(|| deck_core::worktree::resolve(&cwd))
    }

    // ------------------------------------------------------------ commands

    fn palette_commands(&self) -> Vec<Item> {
        let term = self.tabs.get(self.active);
        let session_tab = term.filter(|t| t.role == TabRole::Session);
        let actx: Option<ActionCtx> = session_tab.map(|t| self.action_ctx(t));
        let omp = self.omp_found();
        let entry = self.active_session().and_then(|p| self.find_entry(&p));
        let cwd: Option<PathBuf> = term.map(|t| t.cwd.clone()).or_else(|| self.scope.clone());
        let groups = self.groups();
        let worktree = self.active_worktree();
        let place = self.backup_place();
        let project_backup_why = self.backup_menu_off();
        let config_backup_why = self.backup_unlinked_reason();
        let live = term.is_some_and(|t| !t.terminal().is_exited());
        let can_stop = self.backends_available();
        let cfg = &self.cfg;

        let mut items: Vec<Item> = Vec::new();
        let mut add = |id: &str, label: &str, hint: Option<&'static str>, icon: Icon, why: Why| {
            items.push(Item {
                target: Target::Command(id.to_owned()),
                label: label.to_owned(),
                detail: if id.starts_with("view.") || id == "session.open_chat" {
                    why.unwrap_or_default().to_owned()
                } else {
                    String::new()
                },
                hint,
                icon,
                disabled: why,
                running: false,
            });
        };
        let need = |ok: bool, why: &'static str| (!ok).then_some(why);
        let kind_why = |kind: Kind| -> Why {
            let c = actx.as_ref()?;
            if !session_actions::visible(kind, c) {
                return Some(match kind {
                    Kind::Retry => "Only after a failed turn",
                    Kind::Stop => "Only while omp is working",
                    _ => "Not available right now",
                });
            }
            match session_actions::availability(kind, c) {
                Availability::Enabled => None,
                Availability::Disabled(why) => Some(why),
            }
        };
        let session_why = |kind: Kind| -> Why {
            if actx.is_none() {
                Some(NEED_SESSION)
            } else {
                kind_why(kind)
            }
        };
        let entry_why = need(entry.is_some(), NEED_ENTRY);

        // Tabs.
        add(
            "tab.reopen",
            "Tab: Reopen closed tab",
            Some("Ctrl+Shift+T"),
            ICON_HISTORY,
            need(!self.closed.is_empty(), "No closed tab to reopen"),
        );
        add(
            "tab.close",
            "Tab: Close tab",
            None,
            ICON_X,
            need(!self.tabs.is_empty(), NEED_TAB),
        );
        add(
            "tab.kill",
            "Tab: Kill session process",
            None,
            ICON_POWER,
            // The menus offer Kill only where Stop (with its confirm) does not apply.
            need(live, "No running process in this tab").or(need(
                !(entry.is_some() && can_stop),
                "Use “Session: Stop session process” for this tab",
            )),
        );
        add(
            "tab.next",
            "Tab: Next tab",
            Some("Ctrl+PgDn"),
            ICON_CHEVRON_RIGHT,
            need(self.tabs.len() > 1, "Only one tab is open"),
        );
        add(
            "tab.prev",
            "Tab: Previous tab",
            Some("Ctrl+PgUp"),
            ICON_CHEVRON_LEFT,
            need(self.tabs.len() > 1, "Only one tab is open"),
        );
        let split_ok = need(session_tab.is_some(), "Open a session tab first");
        let in_split = need(self.split_shown(), "No split is open");
        add(
            "split.right",
            "Split: Split right",
            None,
            ICON_COLUMNS_2,
            split_ok,
        );
        add(
            "split.down",
            "Split: Split down",
            None,
            ICON_ROWS_2,
            split_ok,
        );
        add(
            "split.grid",
            "Split: Grid (2×2)",
            None,
            ICON_GRID_2X2,
            split_ok,
        );
        add(
            "split.unsplit",
            "Split: Unsplit (keep focused pane)",
            None,
            ICON_SQUARE,
            in_split,
        );
        add(
            "pane.next",
            "Split: Focus next pane",
            None,
            ICON_CHEVRON_RIGHT,
            in_split,
        );
        add(
            "pane.prev",
            "Split: Focus previous pane",
            None,
            ICON_CHEVRON_LEFT,
            in_split,
        );

        if let Some(tab) = session_tab {
            let (view, _) = crate::sidebar::view_action(self.cfg.session_view, Some(tab.view()));
            add(
                if view == SessionView::Chat {
                    "view.chat"
                } else {
                    "view.terminal"
                },
                if view == SessionView::Chat {
                    "View: Switch to chat view"
                } else {
                    "View: Switch to terminal view"
                },
                None,
                if view == SessionView::Chat {
                    ICON_MESSAGES_SQUARE
                } else {
                    ICON_TERMINAL
                },
                crate::sidebar::view_switch_reason(tab, view, self.host_available()),
            );
        }
        add(
            "session.open_chat",
            "Session: Open in chat view",
            None,
            ICON_MESSAGES_SQUARE,
            entry_why.or_else(|| {
                session_tab
                    .filter(|t| t.view() != SessionView::Chat)
                    .and_then(|t| {
                        crate::sidebar::view_switch_reason(
                            t,
                            SessionView::Chat,
                            self.host_available(),
                        )
                    })
            }),
        );

        // Sessions of the active tab.
        add(
            "session.new",
            "Session: New session in this project",
            None,
            ICON_PLUS,
            need(cwd.is_some(), NEED_PROJECT),
        );
        add(
            "session.new_worktree",
            "Session: New session in a new worktree…",
            None,
            ICON_GIT_BRANCH_PLUS,
            cwd.as_ref()
                .map_or(Some(NEED_PROJECT), |_| need(omp, NEED_OMP)),
        );
        add(
            "session.fork",
            "Session: Fork to new tab",
            None,
            ICON_GIT_FORK,
            session_why(Kind::Fork),
        );
        add(
            "session.compact",
            "Session: Compact",
            None,
            ICON_FOLD_VERTICAL,
            session_why(Kind::Compact),
        );
        add(
            "session.compact_soft",
            "Session: Compact (soft)",
            None,
            ICON_FOLD_VERTICAL,
            session_why(Kind::Compact),
        );
        add(
            "session.compact_snap",
            "Session: Compact (snapcompact)",
            None,
            ICON_FOLD_VERTICAL,
            session_why(Kind::Compact),
        );
        add(
            "session.compact_focus",
            "Session: Compact with focus…",
            None,
            ICON_FOLD_VERTICAL,
            session_why(Kind::Compact),
        );
        add(
            "session.shake",
            "Session: Shake (tool results + large blocks)",
            None,
            ICON_VIBRATE,
            session_why(Kind::Shake),
        );
        add(
            "session.shake_images",
            "Session: Shake images",
            None,
            ICON_VIBRATE,
            session_why(Kind::Shake),
        );
        add(
            "session.shake_thinking",
            "Session: Shake thinking",
            None,
            ICON_VIBRATE,
            session_why(Kind::Shake),
        );
        add(
            "session.handoff",
            "Session: Handoff",
            None,
            ICON_HANDSHAKE,
            session_why(Kind::Handoff),
        );
        add(
            "session.handoff_focus",
            "Session: Handoff with focus…",
            None,
            ICON_HANDSHAKE,
            session_why(Kind::Handoff),
        );
        add(
            "session.rename",
            "Session: Rename…",
            None,
            ICON_PENCIL,
            session_why(Kind::Rename),
        );
        add(
            "session.autoname",
            "Session: Auto-name",
            None,
            ICON_SPARKLES,
            session_why(Kind::AutoName),
        );
        add(
            "session.plan",
            "Session: Toggle plan mode",
            None,
            ICON_MAP,
            session_why(Kind::PlanMode),
        );
        add(
            "session.retry",
            "Session: Retry last turn",
            None,
            ICON_ROTATE_CCW,
            session_why(Kind::Retry),
        );
        add(
            "session.stop_turn",
            "Session: Stop current turn",
            None,
            ICON_CIRCLE_STOP,
            session_why(Kind::Stop),
        );
        add(
            "session.copy_last",
            "Session: Copy last response",
            None,
            ICON_COPY,
            session_why(Kind::CopyLast),
        );
        add(
            "session.export",
            "Session: Export to HTML",
            None,
            ICON_EXTERNAL_LINK,
            session_why(Kind::Export),
        );
        add(
            "session.tree",
            "Session: Session tree…",
            Some("Ctrl+Shift+B"),
            ICON_LIST_TREE,
            entry_why.or(need(
                entry.is_some() || session_tab.is_some_and(|t| t.session_path().is_some()),
                NEED_SESSION,
            )),
        );
        add(
            "session.copy_path",
            "Session: Copy session path",
            None,
            ICON_FILE_TEXT,
            entry_why,
        );
        add(
            "session.copy_id",
            "Session: Copy session id",
            None,
            ICON_KEY_ROUND,
            entry_why,
        );
        add(
            "session.copy_resume",
            "Session: Copy resume command",
            None,
            ICON_TERMINAL,
            entry_why,
        );
        add(
            "session.show_file",
            "Session: Show session file",
            None,
            ICON_EXTERNAL_LINK,
            entry_why,
        );
        let linked = entry.as_ref().is_some_and(|e| {
            groups
                .as_ref()
                .is_some_and(|gs| gs.iter().any(|g| g.cwd == e.cwd && g.worktree.is_some()))
        });
        add(
            "session.move_worktree",
            "Session: Move to new worktree…",
            None,
            ICON_GIT_BRANCH_PLUS,
            entry_why
                .or(need(omp, NEED_OMP))
                .or(need(live, "Open the session in a tab first"))
                .or(need(!linked, "This session already runs in a worktree")),
        );
        let transfer_busy = entry
            .as_ref()
            .is_some_and(|e| self.moving.contains(&e.path));
        let transfer_why = entry_why.or(need(omp, NEED_OMP)).or(need(
            !transfer_busy,
            "A copy or move of this session is running",
        ));
        add(
            "session.copy_project",
            "Session: Copy to project…",
            None,
            ICON_FILES,
            transfer_why,
        );
        add(
            "session.move_project",
            "Session: Move to project…",
            None,
            ICON_FOLDER_OUTPUT,
            transfer_why,
        );
        add(
            "session.delete",
            "Session: Delete…",
            None,
            ICON_TRASH_2,
            transfer_why,
        );
        add(
            "session.restart",
            "Session: Restart",
            None,
            ICON_ROTATE_CCW,
            entry_why
                .or(need(live, "The session is not running"))
                .or(need(can_stop, NO_BACKEND)),
        );
        add(
            "session.stop",
            "Session: Stop session process",
            None,
            ICON_CIRCLE_STOP,
            entry_why
                .or(need(live, "The session is not running"))
                .or(need(can_stop, NO_BACKEND)),
        );

        // Models and thinking of the active omp tab (same gating as the status-bar picker).
        let model_why = need(
            session_tab.is_some_and(|t| !t.terminal().is_exited()),
            NEED_SESSION,
        );
        add(
            "model.switch",
            "Model: Switch model…",
            None,
            ICON_BRAIN,
            model_why,
        );
        let current = self.current_model();
        let info = current.as_deref().and_then(|s| self.models.get(s));
        for (i, star) in model_picker::palette_models(self.models.all(), &cfg.favorite_models) {
            let m = &self.models.all()[i];
            let label = format!(
                "Model: {}{} / {}",
                if star { "★ " } else { "" },
                m.provider,
                m.name
            );
            let why = model_why.or(need(
                current.as_deref() != Some(m.selector.as_str()),
                "Already the current model",
            ));
            add(
                &format!("model.pick.{}", m.selector),
                &label,
                None,
                ICON_BRAIN,
                why,
            );
        }
        let levels = model_picker::level_options(info);
        let level_why = model_why.or(need(
            current.is_some(),
            "The current model is not known yet",
        ));
        for level in &levels {
            add(
                &format!("thinking.{level}"),
                &format!("Thinking: {level}"),
                None,
                ICON_BRAIN,
                level_why,
            );
        }

        // Projects.
        let project_why = need(cwd.is_some(), NEED_PROJECT);
        add(
            "project.focus",
            "Project: Focus this folder",
            None,
            ICON_FOCUS,
            project_why.or(need(
                cwd.is_none() || self.scope != cwd,
                "This folder is already focused",
            )),
        );
        add(
            "project.unfocus",
            "Project: Show all projects",
            None,
            ICON_GLOBE,
            need(self.scope.is_some(), "No folder is focused"),
        );
        add(
            "project.open_folder",
            "Project: Open folder",
            None,
            ICON_FOLDER_OPEN,
            project_why,
        );
        add(
            "project.copy_path",
            "Project: Copy path",
            None,
            ICON_COPY,
            project_why,
        );
        add(
            "project.backup",
            &format!("Project: Back up to {place}…"),
            None,
            ICON_CLOUD_UPLOAD,
            project_why.or(project_backup_why),
        );
        add(
            "project.restore",
            &format!("Project: Restore from {place}…"),
            None,
            ICON_ARCHIVE_RESTORE,
            project_why.or(project_backup_why),
        );
        let finish_state = worktree
            .as_ref()
            .and_then(|w| self.wt.finishing.get(&w.path).copied());
        let finishing = finish_state.is_some();
        let wt_why = need(worktree.is_some(), NOT_WORKTREE);
        add(
            "worktree.finish",
            "Worktree: Finish",
            None,
            ICON_GIT_MERGE,
            wt_why
                .or(need(omp, NEED_OMP))
                .or(need(!finishing, "Already finishing")),
        );
        add(
            "worktree.cancel",
            "Worktree: Cancel finishing",
            None,
            ICON_X,
            wt_why.or(need(finishing, "Not finishing")).or(need(
                finish_state == Some(true),
                "Another device or window started it",
            )),
        );
        add(
            "worktree.remove",
            "Worktree: Remove…",
            None,
            ICON_TRASH_2,
            wt_why,
        );

        // Views.
        add(
            "view.usage",
            "View: Usage",
            Some("Ctrl+Shift+U"),
            ICON_CHART_PIE,
            None,
        );
        add(
            "view.sessions",
            "View: Session manager",
            Some("Ctrl+Shift+S"),
            ICON_HARD_DRIVE,
            None,
        );
        add(
            "view.dashboard",
            "View: Agents dashboard",
            Some("Ctrl+Shift+A"),
            ICON_NETWORK,
            None,
        );
        add(
            "view.changes",
            "View: Review changes",
            Some("Ctrl+Shift+G"),
            ICON_GIT_BRANCH,
            need(self.changes_available(), NEED_TAB),
        );
        add(
            "view.switcher",
            "Go to tab, session or project…",
            Some("Ctrl+Shift+E"),
            ICON_SEARCH,
            None,
        );

        // Settings pages.
        add(
            "app.settings",
            "Settings: Open settings",
            None,
            ICON_SETTINGS,
            None,
        );
        for page in OmpDeckPage::ALL {
            add(
                &format!("settings.ompdeck.{}", page.slug()),
                &format!("Settings: OmpDeck — {}", page.label()),
                None,
                page.icon(),
                None,
            );
        }
        add("settings.sync", "Settings: Sync", None, ICON_CLOUD, None);
        add("settings.agents", "Settings: Agents", None, ICON_BOT, None);
        for (id, label) in self.settings.omp_pages() {
            add(
                &format!("settings.omp.{id}"),
                &format!("Settings: omp — {label}"),
                None,
                ICON_SETTINGS_2,
                None,
            );
        }

        // Appearance.
        add(
            "font.larger",
            "Font: Larger",
            Some("Ctrl+Shift+="),
            ICON_A_LARGE_SMALL,
            None,
        );
        add(
            "font.smaller",
            "Font: Smaller",
            Some("Ctrl+Shift+-"),
            ICON_A_LARGE_SMALL,
            None,
        );
        add(
            "font.reset",
            "Font: Reset size",
            Some("Ctrl+Shift+0"),
            ICON_A_LARGE_SMALL,
            None,
        );
        add(
            "appscale.larger",
            "App font size: Larger",
            None,
            ICON_A_LARGE_SMALL,
            need(cfg.ui_scale < MAX_UI_SCALE, "Already at the maximum"),
        );
        add(
            "appscale.smaller",
            "App font size: Smaller",
            None,
            ICON_A_LARGE_SMALL,
            need(cfg.ui_scale > MIN_UI_SCALE, "Already at the minimum"),
        );
        add(
            "appscale.reset",
            "App font size: Reset",
            None,
            ICON_A_LARGE_SMALL,
            need((cfg.ui_scale - 1.0).abs() > 0.001, "Already at 100 %"),
        );
        add(
            "theme.dark",
            "Theme: Dark",
            None,
            ICON_MOON,
            need(!cfg.dark(), "Already active"),
        );
        add(
            "theme.light",
            "Theme: Light",
            None,
            ICON_SUN,
            crate::theme::dark_only_reason(cfg.style)
                .or_else(|| need(cfg.theme != Theme::Light, "Already active")),
        );
        for style in UiStyle::ALL {
            add(
                &format!("style.{}", style.key()),
                &format!("Style: {}", style.label()),
                None,
                if style == UiStyle::Neutral {
                    ICON_SQUARE
                } else {
                    ICON_SPARKLES
                },
                need(cfg.style != style, "Already active"),
            );
        }
        add(
            "density.compact",
            "Density: Compact",
            None,
            ICON_FOLD_VERTICAL,
            need(cfg.density != Density::Compact, "Already active"),
        );
        add(
            "density.comfortable",
            "Density: Comfortable",
            None,
            ICON_CHEVRONS_UP_DOWN,
            need(cfg.density != Density::Comfortable, "Already active"),
        );
        add(
            "layout.separated_panels",
            if cfg.separated_panels {
                "Layout: Join sidebar and main area"
            } else {
                "Layout: Separate sidebar and main area into panels"
            },
            None,
            ICON_COLUMNS_2,
            None,
        );
        for font in UiFont::ALL {
            add(
                &format!("ui_font.{}", font.key()),
                &format!("UI font: {}", font.label()),
                None,
                ICON_A_LARGE_SMALL,
                need(cfg.ui_font != font, "Already active"),
            );
        }
        let native = if cfg.native_decorations {
            "Window: Use OmpDeck's own title bar"
        } else {
            "Window: Use the native title bar"
        };
        add(
            "window.native_titlebar",
            native,
            None,
            ICON_APP_WINDOW,
            None,
        );

        // Updates.
        add(
            "omp.check",
            "Updates: Check for omp updates",
            None,
            ICON_REFRESH_CW,
            need(omp, NEED_OMP),
        );
        add(
            "omp.update",
            "Updates: Update omp",
            None,
            ICON_CIRCLE_ARROW_UP,
            need(omp, NEED_OMP),
        );
        add(
            "omp.update_plugins",
            "Updates: Update omp plugins",
            None,
            ICON_PUZZLE,
            need(omp, NEED_OMP),
        );
        match self.update.channel() {
            Channel::Stable => add(
                "omp.canary",
                "Updates: Switch omp to the canary channel",
                None,
                ICON_ROCKET,
                need(omp, NEED_OMP),
            ),
            Channel::Canary => add(
                "omp.stable",
                "Updates: Switch omp back to the stable channel",
                None,
                ICON_ROCKET,
                need(omp, NEED_OMP),
            ),
        }
        if !omp {
            add(
                "omp.install",
                "Updates: Install omp",
                None,
                ICON_DOWNLOAD,
                None,
            );
        } else {
            add(
                "omp.models",
                "Models: Reload model list",
                None,
                ICON_BRAIN,
                None,
            );
        }
        add(
            "app.check_update",
            "Updates: Check for OmpDeck updates",
            None,
            ICON_REFRESH_CW,
            None,
        );
        add(
            "app.start_update",
            "Updates: Update OmpDeck and restart",
            None,
            ICON_CIRCLE_ARROW_UP,
            self.app_update_blocked(),
        );

        // Notifications.
        let n = &cfg.notify;
        let on_off = |v: bool| if v { "turn off" } else { "turn on" };
        add(
            "notify.panel",
            "Notifications: Open panel",
            None,
            ICON_BELL,
            None,
        );
        add(
            "notify.turn_finished",
            &format!(
                "Notifications: {} “turn finished”",
                cap(on_off(n.turn_finished))
            ),
            None,
            ICON_BELL,
            None,
        );
        add(
            "notify.attention",
            &format!(
                "Notifications: {} “needs attention”",
                cap(on_off(n.attention))
            ),
            None,
            ICON_BELL,
            None,
        );
        add(
            "notify.exited",
            &format!("Notifications: {} “session ended”", cap(on_off(n.exited))),
            None,
            ICON_BELL,
            None,
        );
        add(
            "notify.toggle",
            &format!(
                "Desktop pop-ups: {}",
                if n.desktop { "Turn off" } else { "Turn on" }
            ),
            None,
            ICON_BELL,
            None,
        );
        add(
            "notify.unfocused",
            &format!(
                "Desktop pop-ups: {} “only when not focused”",
                cap(on_off(n.only_when_unfocused))
            ),
            None,
            ICON_BELL,
            need(n.desktop, "Desktop pop-ups are off"),
        );
        if cfg!(target_os = "linux") {
            add(
                "notify.sound",
                &format!("Desktop pop-ups: {} sound", cap(on_off(n.sound))),
                None,
                ICON_BELL,
                need(n.desktop, "Desktop pop-ups are off"),
            );
        }
        add(
            "notify.while_viewing",
            &format!(
                "Notifications: Also when viewing: {}",
                if n.while_viewing {
                    "Turn off"
                } else {
                    "Turn on"
                }
            ),
            None,
            ICON_BELL,
            None,
        );
        add(
            "notify.test",
            "Desktop pop-ups: Send test notification",
            None,
            ICON_BELL,
            None,
        );

        // Backups (Google Drive or the user's server).
        add(
            "backup.now",
            &format!("Backup: Back up omp config to {place} now"),
            None,
            ICON_CLOUD_UPLOAD,
            config_backup_why,
        );
        add(
            "backup.restore",
            &format!("Backup: Restore omp config from {place}…"),
            None,
            ICON_ARCHIVE_RESTORE,
            config_backup_why,
        );
        add(
            "backup.manage",
            "Backup: Manage settings backups…",
            None,
            ICON_LIST_CHECKS,
            config_backup_why,
        );
        add(
            "sync.restore",
            "Backup: Restore a project backup…",
            None,
            ICON_ARCHIVE_RESTORE,
            project_backup_why,
        );
        add(
            "sync.manage",
            "Backup: Manage project backups…",
            None,
            ICON_CLOUD,
            project_backup_why,
        );

        // Mobile access (the same gate as the settings button).
        let pair_why: Why = pair_block(
            self.gw.mobile().as_deref(),
            self.gw
                .gateway()
                .is_none_or(|g| g.mode == deck_core::config::GatewayMode::Off),
            self.gw.is_up(),
        )
        .map(|b| b.short());
        add(
            "mobile.pair",
            "Mobile access: Pair a device",
            None,
            ICON_GLOBE,
            pair_why,
        );
        if self.pairing_hidden() {
            add(
                "mobile.show-pairing",
                "Mobile access: Show pairing",
                None,
                ICON_GLOBE,
                None,
            );
        }
        add(
            "mobile.settings",
            "Mobile access: Open settings",
            None,
            ICON_GLOBE,
            None,
        );
        let reconnect_why: Why = (!(self.gw.is_up()
            && can_reconnect(
                self.gw.mobile().as_deref(),
                self.gw
                    .gateway()
                    .is_none_or(|g| g.mode == deck_core::config::GatewayMode::Off),
            )))
        .then_some("Only while the relay is refused or reconnecting");
        add(
            "mobile.reconnect",
            "Mobile access: Reconnect relay",
            None,
            ICON_REFRESH_CW,
            reconnect_why,
        );

        // Sessions list and app.
        add(
            "app.refresh",
            "Sessions: Refresh list",
            None,
            ICON_REFRESH_CW,
            None,
        );
        add("app.quit", "OmpDeck: Quit", None, ICON_LOG_OUT, None);
        items
    }

    fn run_command(&mut self, id: &str) {
        let active = self.active;
        let cwd = self
            .tabs
            .get(active)
            .map(|t| t.cwd.clone())
            .or_else(|| self.scope.clone());
        let entry = self.active_session().and_then(|p| self.find_entry(&p));
        if let Some(page) = id.strip_prefix("settings.omp.") {
            self.open_settings();
            if !self.settings.show_omp_page(page) {
                self.toasts
                    .info(&self.ctx, "That settings page is not loaded yet");
            }
            return;
        }
        if let Some(selector) = id.strip_prefix("model.pick.") {
            self.queue_change(selector, None);
            return;
        }
        if let Some(level) = id.strip_prefix("thinking.") {
            self.queue_effort(level.to_owned());
            return;
        }
        match id {
            "model.switch" => self.picker.request_open(),
            "tab.reopen" => self.reopen_closed(),
            "tab.close" => self.request_close(active),
            "tab.kill" => self.kill_tab(active),
            "tab.next" => self.activate(tabs::wrap_index(active, self.tabs.len(), 1)),
            "tab.prev" => self.activate(tabs::wrap_index(active, self.tabs.len(), -1)),
            "split.right" | "split.down" | "split.grid" | "split.unsplit" | "pane.next"
            | "pane.prev" => {
                self.run_split_command(id);
            }
            "view.chat" | "view.terminal" => {
                if let Some(uid) = self.tabs.get(active).map(|t| t.uid) {
                    self.request_switch_view(
                        uid,
                        if id == "view.chat" {
                            SessionView::Chat
                        } else {
                            SessionView::Terminal
                        },
                    );
                }
            }
            "session.open_chat" => {
                if let Some(e) = entry {
                    match self
                        .tabs
                        .iter()
                        .find(|t| t.session_path() == Some(e.path.as_path()))
                        .map(|t| (t.uid, t.view()))
                    {
                        Some((uid, SessionView::Terminal)) => {
                            self.request_switch_view(uid, SessionView::Chat)
                        }
                        _ => self.open_session_view(&e, SessionView::Chat),
                    }
                }
            }
            "session.new" => {
                if let Some(cwd) = cwd {
                    self.new_session(cwd);
                }
            }
            "session.new_worktree" => {
                if let Some(cwd) = cwd {
                    self.open_new_worktree(cwd);
                }
            }
            "session.fork" => self.run_action(Action::Fork),
            "session.compact" => self.run_action(Action::Compact(CompactMode::Default)),
            "session.compact_soft" => self.run_action(Action::Compact(CompactMode::Soft)),
            "session.compact_snap" => self.run_action(Action::Compact(CompactMode::Snapcompact)),
            "session.compact_focus" => {
                self.open_action_dialog(ActionDialog::CompactFocus {
                    focus: String::new(),
                    first: true,
                });
            }
            "session.shake" => self.run_action(Action::Shake(ShakeMode::Elide)),
            "session.shake_images" => self.run_action(Action::Shake(ShakeMode::Images)),
            "session.shake_thinking" => self.run_action(Action::Shake(ShakeMode::Thinking)),
            "session.handoff" => self.run_action(Action::Handoff(None)),
            "session.handoff_focus" => {
                self.open_action_dialog(ActionDialog::Handoff {
                    focus: String::new(),
                    first: true,
                });
            }
            "session.rename" => {
                self.open_action_dialog(ActionDialog::Rename {
                    title: String::new(),
                    first: true,
                });
            }
            "session.autoname" => self.run_action(Action::AutoName),
            "session.plan" => self.run_action(Action::PlanMode),
            "session.retry" => self.run_action(Action::Retry),
            "session.stop_turn" => self.run_action(Action::Stop),
            "session.copy_last" => self.run_action(Action::CopyLast),
            "session.export" => self.run_action(Action::ExportHtml),
            "session.tree" => match entry {
                Some(e) => self.apply_sidebar_action(SidebarAction::Tree(e)),
                None => self.open_tree_active(),
            },
            "session.copy_path" => {
                if let Some(e) = entry {
                    let text = e.path.to_string_lossy().into_owned();
                    self.copy_text(&text, "session path");
                }
            }
            "session.copy_id" => {
                if let Some(e) = entry {
                    self.copy_text(&e.id, "session id");
                }
            }
            "session.copy_resume" => {
                if let Some(e) = entry {
                    self.copy_text(&resume_command(&e.path), "resume command");
                }
            }
            "session.show_file" => {
                if let Some(dir) = entry.as_ref().and_then(|e| e.path.parent()) {
                    self.open_folder(dir.to_path_buf());
                }
            }
            "session.move_worktree" => {
                if let Some(e) = entry {
                    self.open_move_to_worktree(&e);
                }
            }
            "session.copy_project" | "session.move_project" => {
                if let Some(entry) = entry {
                    let kind = if id == "session.copy_project" {
                        TransferKind::Copy
                    } else {
                        TransferKind::Move
                    };
                    self.apply_sidebar_action(SidebarAction::Transfer {
                        kind,
                        entry,
                        dest: None,
                    });
                }
            }
            "session.delete" => {
                if let Some(e) = entry {
                    self.request_delete(e);
                }
            }
            "session.restart" => {
                if let Some(e) = entry {
                    self.request_restart_session(&e);
                }
            }
            "session.stop" => {
                if let Some(e) = entry {
                    self.request_stop(&e);
                }
            }
            "project.focus" => self.set_scope(cwd),
            "project.unfocus" => self.set_scope(None),
            "project.open_folder" => {
                if let Some(cwd) = cwd {
                    self.open_folder(cwd);
                }
            }
            "project.copy_path" => {
                if let Some(cwd) = cwd {
                    self.copy_text(&cwd.to_string_lossy(), "project path");
                }
            }
            "project.backup" => {
                if let Some(cwd) = cwd {
                    self.open_backup_dialog(cwd);
                }
            }
            "project.restore" => self.open_restore(cwd),
            "worktree.finish" | "worktree.cancel" | "worktree.remove" => {
                match self.worktree_to_act_on() {
                    Some(wt) => match id {
                        "worktree.finish" => self.request_finish(wt),
                        "worktree.cancel" => self.cancel_finish(&wt.path),
                        _ => self.request_remove(wt),
                    },
                    None => self.toasts.info(&self.ctx, NOT_WORKTREE),
                }
            }
            "view.usage" => self.toggle_usage(),
            "view.sessions" => self.toggle_sessions(),
            "view.dashboard" => self.toggle_dashboard(false),
            "view.changes" => self.toggle_changes_panel(),
            "view.switcher" => self.open_palette(Mode::Switcher),
            "app.settings" => self.open_settings(),
            "mobile.pair" => self.start_pairing(),
            "mobile.show-pairing" => self.mobile.restore(),
            "mobile.reconnect" => self.reconnect_relay(),
            "mobile.settings" => self.open_settings_page(OmpDeckPage::General),
            "settings.sync" => {
                self.open_settings();
                self.settings.show_sync();
            }
            "settings.agents" => {
                self.open_settings();
                self.settings.show_agents();
            }
            id if id.starts_with("settings.ompdeck.") => {
                if let Some(page) = OmpDeckPage::ALL
                    .into_iter()
                    .find(|p| id == format!("settings.ompdeck.{}", p.slug()))
                {
                    self.open_settings_page(page);
                }
            }
            "font.larger" => self.set_font_size(self.cfg.font_size + 1.0),
            "font.smaller" => self.set_font_size(self.cfg.font_size - 1.0),
            "font.reset" => self.set_font_size(DEFAULT_FONT),
            "appscale.larger" => self.set_ui_scale(self.cfg.ui_scale + 0.1),
            "appscale.smaller" => self.set_ui_scale(self.cfg.ui_scale - 0.1),
            "appscale.reset" => self.set_ui_scale(1.0),
            "theme.dark" => self.set_theme(Theme::Dark),
            "theme.light" => self.set_theme(Theme::Light),
            other if other.starts_with("style.") => {
                if let Some(style) = UiStyle::ALL
                    .into_iter()
                    .find(|s| other.strip_prefix("style.") == Some(s.key()))
                {
                    self.set_style(style);
                }
            }
            other if other.starts_with("ui_font.") => {
                if let Some(font) = UiFont::ALL
                    .into_iter()
                    .find(|f| other.strip_prefix("ui_font.") == Some(f.key()))
                {
                    self.set_ui_font(font);
                }
            }
            "density.compact" => self.set_density(Density::Compact),
            "density.comfortable" => self.set_density(Density::Comfortable),
            "layout.separated_panels" => self.set_separated_panels(!self.cfg.separated_panels),
            "window.native_titlebar" => self.set_native_decorations(!self.cfg.native_decorations),
            "omp.check" => self.check_updates_manually(),
            "omp.update" => self.open_update_tab(UpdateKind::Omp),
            "omp.update_plugins" => self.open_update_tab(UpdateKind::Plugins),
            "omp.canary" => self.confirm_channel_switch(UpdateKind::Canary),
            "omp.stable" => self.confirm_channel_switch(UpdateKind::Stable),
            "omp.install" => self.open_install_tab(),
            "omp.models" => self.reload_models(),
            "app.check_update" => self.check_app_update_manually(),
            "app.start_update" => self.start_app_update(),
            "notify.panel" => self.open_inbox_panel(),
            "notify.toggle" => {
                self.apply_notify_action(NotifyAction::Desktop(!self.cfg.notify.desktop))
            }
            "notify.turn_finished" => {
                self.apply_notify_action(NotifyAction::TurnFinished(!self.cfg.notify.turn_finished))
            }
            "notify.attention" => {
                self.apply_notify_action(NotifyAction::Attention(!self.cfg.notify.attention))
            }
            "notify.exited" => {
                self.apply_notify_action(NotifyAction::Exited(!self.cfg.notify.exited))
            }
            "notify.unfocused" => self.apply_notify_action(NotifyAction::OnlyWhenUnfocused(
                !self.cfg.notify.only_when_unfocused,
            )),
            "notify.sound" => self.apply_notify_action(NotifyAction::Sound(!self.cfg.notify.sound)),
            "notify.while_viewing" => {
                self.apply_notify_action(NotifyAction::WhileViewing(!self.cfg.notify.while_viewing))
            }
            "notify.test" => self.send_test_notification(),
            "backup.now" => self.apply_backup_action(BackupAction::BackupNow),
            "backup.restore" => self.apply_backup_action(BackupAction::OpenRestore),
            "backup.manage" => self.apply_backup_action(BackupAction::OpenManager),
            "sync.restore" => self.apply_sync_action(SyncAction::OpenRestore),
            "sync.manage" => self.apply_sync_action(SyncAction::OpenManager),
            "app.refresh" => self.refresh_sessions(),
            "app.quit" => self.ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            other => log::warn!("unknown palette command {other}"),
        }
        self.focus_pending = true;
    }
}

/// First letter upper-case ("turn off" → "Turn off").
fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::cap;

    #[test]
    fn cap_uppercases_the_first_letter_only() {
        assert_eq!(cap("turn off"), "Turn off");
        assert_eq!(cap(""), "");
        assert_eq!(cap("élan"), "Élan");
    }
}
