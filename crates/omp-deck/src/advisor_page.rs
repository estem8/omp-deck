//! Settings → Advisor: omp's `/advisor configure` (a `WATCHDOG.yml`). The shared instructions,
//! `maxNotesPerUpdate` and the advisor list are edited as a draft of the document omp read and
//! saved through omp's own advisor module (live in the open session when there is one, so omp
//! applies it at once). An empty document deletes the file, which is confirmed first.
//!
//! Pure UI: state in, [`AdvisorAction`]s out; `app/advisor.rs` runs them over the gateway.

use std::path::{Path, PathBuf};

use deck_core::omp::{AdvisorDoc, AdvisorEntry, AdvisorScope};
use deck_service::statusfmt::{short_model_name, thinking_short};
use egui::{CursorIcon, Id, Margin, RichText, TextEdit, Ui, vec2};

use crate::agent_config::AgentsAction;
use crate::badge::{self, Hue};
use crate::card;
use crate::confirm::{Dialog, show_dialog};
use crate::icons::{
    ICON_BOT, ICON_CHEVRON_DOWN, ICON_CHEVRON_RIGHT, ICON_CHEVRONS_UP_DOWN, ICON_CIRCLE_ALERT,
    ICON_CIRCLE_CHECK, ICON_COPY, ICON_FILE_PEN_LINE, ICON_FOLDER, ICON_INFO, ICON_PLUS,
    ICON_REFRESH_CW, ICON_SAVE, ICON_TRASH_2, ICON_TRIANGLE_ALERT, ICON_UNDO_2, ICON_USER, ICON_X,
};
use crate::model_picker::{self, ModelsCache, PickerAction, PickerInput, PickerState};
use crate::omp_rows::skeleton;
use crate::omp_settings::split_level;
use crate::summary::summarize;
use crate::theme::{self, Palette};
use crate::widgets::{
    DraftButton, DraftKind, Spin, card_frame, empty_state, icon_button, icon_text, segmented,
};

/// omp's built-in tool names (`BUILTIN_TOOL_NAMES`): what an advisor's `tools` may list.
pub const TOOLS: [&str; 30] = [
    "read",
    "bash",
    "edit",
    "ast_grep",
    "ast_edit",
    "ask",
    "debug",
    "ida",
    "eval",
    "github",
    "glob",
    "grep",
    "find",
    "lsp",
    "checkpoint",
    "rewind",
    "context_notes",
    "new_context",
    "security_scan",
    "task",
    "wait",
    "todo",
    "web_search",
    "write",
    "memory_edit",
    "retain",
    "recall",
    "reflect",
    "learn",
    "manage_skill",
];

const REVIEW_MODES: [&str; 2] = ["turn", "agent-end"];
const SYNC_BACKLOG: [&str; 5] = ["off", "1", "3", "5", "strict"];
const SCOPES: [&str; 3] = ["main", "subagent", "all"];
/// The setting that must be on for advisors to run at all.
pub const ENABLE_KEY: &str = "advisor.enabled";

#[derive(Debug, PartialEq)]
pub enum AdvisorAction {
    /// Read the document of `scope` (the project is the active tab's).
    Load { scope: AdvisorScope },
    /// Save `doc` as the document of `scope` of `project` (the folder the draft was read for;
    /// `None` for the user document); an empty one deletes the file.
    Save {
        scope: AdvisorScope,
        project: Option<PathBuf>,
        doc: AdvisorDoc,
    },
    /// The model dropdown's own controls (refresh, favourites, provider filter).
    Models(AgentsAction),
}

pub struct AdvisorInput<'a> {
    /// The project of the active tab; `None` leaves only the user document.
    pub project: Option<&'a Path>,
    pub models: &'a ModelsCache,
    pub favorites: &'a [String],
    pub provider: Option<&'a str>,
    /// `advisor.enabled` as the settings catalog has it (`None` until it is loaded).
    pub enabled: Option<bool>,
    /// An omp session is open to apply a saved document live.
    pub live_tab: bool,
}

// ------------------------------------------------------------------ the draft

/// One advisor being edited. Every field omp's document has is here; text fields are buffers and
/// the rest keep exactly what omp read until the user changes it, so a value the editor has no
/// widget for (or does not know) survives a save.
pub struct EntryDraft {
    uid: u64,
    pub name: String,
    pub model: Option<String>,
    pub tools: Option<Vec<String>>,
    pub review_mode: Option<String>,
    pub review_interval: String,
    pub sync_backlog: Option<String>,
    pub instructions: String,
    pub enabled: Option<bool>,
    pub max_notes: String,
    pub scope: Option<String>,
    pub include_reasoning: Option<bool>,
    /// Fields of the entry that omp keeps and the editor has no widget for: written back as read.
    extra: serde_json::Map<String, serde_json::Value>,
    open: bool,
    picker: PickerState,
}

fn num_text(n: Option<u32>) -> String {
    n.map(|n| n.to_string()).unwrap_or_default()
}

/// Empty → unset; else a whole number.
fn parse_count(text: &str, what: &str) -> Result<Option<u32>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<u32>()
        .map(Some)
        .map_err(|_| format!("{what} must be a whole number"))
}

fn text_opt(text: &str) -> Option<String> {
    (!text.trim().is_empty()).then(|| text.to_owned())
}

impl EntryDraft {
    fn from_entry(entry: AdvisorEntry, uid: u64, open: bool) -> EntryDraft {
        EntryDraft {
            name: entry.name.clone(),
            model: entry.model.clone(),
            tools: entry.tools.clone(),
            review_mode: entry.review_mode.clone(),
            review_interval: num_text(entry.review_interval),
            sync_backlog: entry.sync_backlog.clone(),
            instructions: entry.instructions.clone().unwrap_or_default(),
            extra: entry.extra.clone(),
            enabled: entry.enabled,
            max_notes: num_text(entry.max_notes_per_update),
            scope: entry.scope.clone(),
            include_reasoning: entry.include_reasoning,
            uid,
            open,
            picker: PickerState::default(),
        }
    }

    /// The entry as edited: what was read, with the edited fields replaced.
    pub fn to_entry(&self) -> Result<AdvisorEntry, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Every advisor needs a name".to_owned());
        }
        let label = |what: &str| format!("{name}: {what}");
        Ok(AdvisorEntry {
            extra: self.extra.clone(),
            name: name.to_owned(),
            model: self.model.clone(),
            tools: self.tools.clone(),
            review_mode: self.review_mode.clone(),
            review_interval: parse_count(&self.review_interval, "Review interval")
                .map_err(|e| label(&e))?,
            sync_backlog: self.sync_backlog.clone(),
            instructions: text_opt(&self.instructions),
            enabled: self.enabled,
            max_notes_per_update: parse_count(&self.max_notes, "Max notes per update")
                .map_err(|e| label(&e))?,
            scope: self.scope.clone(),
            include_reasoning: self.include_reasoning,
        })
    }

    /// Switch one tool of an explicit tool list (the default list becomes explicit first).
    pub fn set_tool(&mut self, tool: &str, on: bool) {
        let tools = self.tools.get_or_insert_with(Vec::new);
        let present = tools.iter().any(|t| t == tool);
        match (on, present) {
            (true, false) => tools.push(tool.to_owned()),
            (false, true) => tools.retain(|t| t != tool),
            _ => {}
        }
    }
}

/// The document being edited.
pub struct Draft {
    /// The document as read: its read-only parts (path, warnings) and the baseline for Revert.
    base: AdvisorDoc,
    pub instructions: String,
    pub max_notes: String,
    pub advisors: Vec<EntryDraft>,
    next_uid: u64,
    /// Something was edited since the document was read or saved.
    pub touched: bool,
}

impl Draft {
    pub fn from_doc(doc: AdvisorDoc) -> Draft {
        let mut next_uid = 0;
        let advisors = doc
            .advisors
            .iter()
            .map(|e| {
                next_uid += 1;
                EntryDraft::from_entry(e.clone(), next_uid, false)
            })
            .collect();
        Draft {
            instructions: doc.instructions.clone().unwrap_or_default(),
            max_notes: num_text(doc.max_notes_per_update),
            advisors,
            base: doc,
            next_uid,
            touched: false,
        }
    }

    /// The document as edited (read-only parts as read).
    pub fn to_doc(&self) -> Result<AdvisorDoc, String> {
        let mut advisors = Vec::with_capacity(self.advisors.len());
        for a in &self.advisors {
            advisors.push(a.to_entry()?);
        }
        let mut seen = std::collections::HashSet::new();
        if let Some(dup) = advisors.iter().find(|a| !seen.insert(a.name.as_str())) {
            return Err(format!("Two advisors are called “{}”", dup.name));
        }
        Ok(AdvisorDoc {
            extra: self.base.extra.clone(),
            instructions: text_opt(&self.instructions),
            max_notes_per_update: parse_count(&self.max_notes, "Max notes per update")?,
            advisors,
            warnings: self.base.warnings.clone(),
            path: self.base.path.clone(),
        })
    }

    /// A new, empty advisor at the end, opened for editing.
    pub fn add_advisor(&mut self) {
        self.next_uid += 1;
        self.advisors.push(EntryDraft::from_entry(
            AdvisorEntry::default(),
            self.next_uid,
            true,
        ));
        self.touched = true;
    }

    pub fn remove_advisor(&mut self, index: usize) {
        if index < self.advisors.len() {
            self.advisors.remove(index);
            self.touched = true;
        }
    }

    /// The document omp read had content (so an empty save deletes something).
    pub fn had_content(&self) -> bool {
        !self.base.is_empty()
    }
}

// ------------------------------------------------------------------ state

enum Load {
    Idle,
    Loading,
    Failed(String),
}

/// What omp said about the last save.
enum Note {
    Saved(String),
    Failed(String),
}

pub struct AdvisorState {
    scope: AdvisorScope,
    /// What the draft / the request in flight belongs to.
    key: Option<(AdvisorScope, Option<PathBuf>)>,
    load: Load,
    draft: Option<Draft>,
    saving: bool,
    note: Option<Note>,
    problem: Option<String>,
    confirm_delete: Option<AdvisorDoc>,
    /// The document of the save in flight.
    in_flight: Option<AdvisorDoc>,
}

impl Default for AdvisorState {
    fn default() -> Self {
        AdvisorState {
            scope: AdvisorScope::Project,
            key: None,
            load: Load::Idle,
            draft: None,
            saving: false,
            note: None,
            problem: None,
            confirm_delete: None,
            in_flight: None,
        }
    }
}

impl AdvisorState {
    /// The page is shown (again): read the document anew.
    pub fn opened(&mut self) {
        if !self.unsaved() {
            self.key = None;
        }
    }

    pub fn unsaved(&self) -> bool {
        self.draft.as_ref().is_some_and(|d| d.touched)
    }

    /// The document of `scope` / `project` was read.
    pub fn loaded(
        &mut self,
        scope: AdvisorScope,
        project: Option<&Path>,
        result: Result<AdvisorDoc, String>,
    ) {
        if self.key.as_ref().map(|(s, p)| (*s, p.as_deref())) != Some((scope, project)) {
            return;
        }
        match result {
            Ok(doc) => {
                self.draft = Some(Draft::from_doc(doc));
                self.load = Load::Idle;
            }
            Err(e) => {
                self.draft = None;
                self.load = Load::Failed(e);
            }
        }
    }

    /// The save finished: omp's reply message, or why it failed. The draft is marked saved (and
    /// read back) only when it is still what was sent; edits made while the save ran stay
    /// unsaved, so the reply can never wipe them.
    pub fn saved(&mut self, result: Result<String, String>) {
        self.saving = false;
        let sent = self.in_flight.take();
        match result {
            Ok(message) => {
                self.note = Some(Note::Saved(message));
                self.problem = None;
                let unchanged = match (&self.draft, &sent) {
                    (Some(d), Some(sent)) => d.to_doc().is_ok_and(|now| &now == sent),
                    (None, _) => true,
                    (Some(_), None) => false,
                };
                if unchanged {
                    // The file is read back: path, warnings and omp's normalised content.
                    self.key = None;
                    if let Some(d) = &mut self.draft {
                        d.touched = false;
                    }
                } else if let Some(d) = &mut self.draft {
                    d.touched = true;
                }
            }
            Err(e) => self.note = Some(Note::Failed(e)),
        }
    }

    fn effective_scope(&self, project: Option<&Path>) -> AdvisorScope {
        if project.is_some() {
            self.scope
        } else {
            AdvisorScope::User
        }
    }
}

// ------------------------------------------------------------------ page

/// `open_setting` is set when the user asked to see `advisor.enabled` ([`ENABLE_KEY`]).
pub fn show(
    ui: &mut Ui,
    state: &mut AdvisorState,
    input: &AdvisorInput<'_>,
    open_setting: &mut bool,
) -> Vec<AdvisorAction> {
    let pal = Palette::of_ui(ui);
    let mut actions = Vec::new();
    let want_scope = state.effective_scope(input.project);
    let want = (want_scope, input.project);
    let foreign = state.unsaved() && owner_mismatch(state.key.as_ref(), want_scope, input.project);
    if state.key.as_ref().map(|(s, p)| (*s, p.as_deref())) != Some(want) && !state.unsaved() {
        state.key = Some((want_scope, input.project.map(Path::to_path_buf)));
        state.load = Load::Loading;
        actions.push(AdvisorAction::Load { scope: want_scope });
    }
    // A foreign draft keeps its own file: the page, its Save and the dialog all follow it.
    let owner = match &state.key {
        Some(k) if foreign => k.clone(),
        _ => (want_scope, input.project.map(Path::to_path_buf)),
    };
    let scope = owner.0;
    let loading = matches!(state.load, Load::Loading);

    egui::Sides::new().height(40.0).show(
        ui,
        |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::strong("Advisor").size(22.0).color(pal.text_strong));
                ui.label(RichText::new("WATCHDOG.yml").monospace().color(pal.weak));
            });
        },
        |ui| {
            if loading || state.saving {
                ui.add_space(8.0);
                ui.add(Spin::new(18.0, pal.accent_b));
            } else if icon_button(ui, ICON_REFRESH_CW, "Reread the file", false).clicked() {
                if state.unsaved() {
                    state.problem = Some("Save or revert your changes before rereading".into());
                } else {
                    state.key = None;
                }
            }
        },
    );
    ui.add_space(8.0);
    enable_banner(ui, input, open_setting);
    ui.add_space(10.0);

    scope_card(ui, state, input, scope);

    match &state.load {
        Load::Failed(e) => {
            let (line, _) = summarize(e);
            card_frame(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_top(|ui| {
                    ui.label(icon_text(ICON_CIRCLE_ALERT, 22.0, pal.danger));
                    ui.vertical(|ui| {
                        ui.label(theme::strong("Could not read the file").color(pal.text_strong));
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
                    state.key = None;
                }
            });
            return actions;
        }
        Load::Loading if state.draft.is_none() => {
            skeleton(ui);
            return actions;
        }
        _ => {}
    }
    if foreign {
        foreign_banner(ui, state, &owner, (want_scope, input.project), &mut actions);
    }
    let Some(draft) = state.draft.as_mut() else {
        return actions;
    };
    editor(ui, draft, input, &mut actions);
    footer(ui, state, &owner, &mut actions, input.live_tab, foreign);
    confirm_delete_dialog(ui.ctx(), state, owner.0, owner.1.clone(), &mut actions);
    actions
}

fn enable_banner(ui: &mut Ui, input: &AdvisorInput<'_>, open_setting: &mut bool) {
    let pal = Palette::of_ui(ui);
    let (tone, icon, text) = match input.enabled {
        Some(true) => (
            pal.ok,
            ICON_CIRCLE_CHECK,
            "Advisors run while the omp setting advisor.enabled is on, and it is on.",
        ),
        Some(false) => (
            pal.warn,
            ICON_TRIANGLE_ALERT,
            "Advisors only run while the omp setting advisor.enabled is on, and it is off.",
        ),
        None => (
            pal.weak,
            ICON_INFO,
            "Advisors only run while the omp setting advisor.enabled is on.",
        ),
    };
    let frame = egui::Frame::NONE
        .fill(theme::accent_wash(pal.surface, tone, 0.08))
        .stroke(egui::Stroke::new(1.0, tone.gamma_multiply(0.5)))
        .corner_radius(theme::rad(8, 12))
        .inner_margin(Margin::symmetric(12, 8));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(icon_text(icon, 16.0, tone));
            ui.add(
                egui::Label::new(RichText::new(text).color(pal.text))
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );
            if ui
                .add(egui::Button::new(RichText::new("Open setting").small()))
                .on_hover_cursor(CursorIcon::PointingHand)
                .clicked()
            {
                *open_setting = true;
            }
        });
    });
}

fn scope_card(
    ui: &mut Ui,
    state: &mut AdvisorState,
    input: &AdvisorInput<'_>,
    scope: AdvisorScope,
) {
    let pal = Palette::of_ui(ui);
    let dirty = state.unsaved();
    card_frame(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        card::kv(ui, 92.0, "Scope", |ui| {
            let mut sel = usize::from(scope == AdvisorScope::User);
            let options: &[(crate::icons::Icon, &str)] = if input.project.is_some() {
                &[(ICON_FOLDER, "Project"), (ICON_USER, "User")]
            } else {
                &[(ICON_USER, "User")]
            };
            let resp = ui.add_enabled_ui(!dirty, |ui| segmented(ui, &mut sel, options));
            if resp.inner && input.project.is_some() {
                state.scope = if sel == 1 {
                    AdvisorScope::User
                } else {
                    AdvisorScope::Project
                };
                state.note = None;
            }
            if dirty {
                resp.response
                    .on_hover_text("Save or revert your changes before switching");
            }
            if input.project.is_none() {
                ui.label(
                    RichText::new("Open a session tab to edit its project's file")
                        .small()
                        .color(pal.weak),
                );
            }
        });
        let path = state
            .draft
            .as_ref()
            .and_then(|d| d.base.path.as_deref())
            .map(|p| p.display().to_string());
        card::kv(ui, 92.0, "File", |ui| match &path {
            Some(p) => {
                ui.add(
                    egui::Label::new(RichText::new(p).monospace().small().color(pal.text_strong))
                        .truncate(),
                );
            }
            None => {
                ui.label(RichText::new("resolved when read").small().color(pal.weak));
            }
        });
        if let Some(d) = &state.draft {
            if !d.had_content() {
                ui.label(
                    RichText::new("The file does not exist yet: saving creates it.")
                        .small()
                        .color(pal.weak),
                );
            }
            for w in &d.base.warnings {
                ui.horizontal_top(|ui| {
                    ui.label(icon_text(ICON_TRIANGLE_ALERT, 14.0, pal.warn));
                    ui.add(
                        egui::Label::new(RichText::new(w).small().color(pal.warn))
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                });
            }
        }
    });
    ui.add_space(12.0);
}

/// The editable document: shared fields, then the advisors.
fn editor(
    ui: &mut Ui,
    draft: &mut Draft,
    input: &AdvisorInput<'_>,
    actions: &mut Vec<AdvisorAction>,
) {
    let pal = Palette::of_ui(ui);
    card_frame(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(theme::section_caption("Shared by every advisor", pal.weak));
        ui.add_space(6.0);
        card::kv(ui, 92.0, "Instructions", |_| {});
        let resp = ui.add(
            TextEdit::multiline(&mut draft.instructions)
                .id(Id::unique("advisor-shared-instructions"))
                .desired_rows(4)
                .desired_width(f32::INFINITY)
                .hint_text("Guidance every advisor reads (optional)")
                .margin(Margin::symmetric(10, 8)),
        );
        draft.touched |= resp.changed();
        ui.add_space(8.0);
        card::kv(ui, 92.0, "Max notes", |ui| {
            let resp = count_field(
                ui,
                "advisor-shared-notes",
                &mut draft.max_notes,
                "per update",
            );
            draft.touched |= resp;
        });
    });
    ui.add_space(12.0);

    ui.horizontal(|ui| {
        ui.label(theme::section_caption("Advisors", pal.weak));
        ui.label(
            RichText::new(format!("{}", draft.advisors.len()))
                .small()
                .color(pal.weak),
        );
    });
    ui.add_space(6.0);
    if draft.advisors.is_empty() {
        empty_state(
            ui,
            ICON_FILE_PEN_LINE,
            pal.weak,
            "No advisors",
            "An advisor reviews the session's work in the background and leaves notes. Add one below.",
        );
    }
    let mut remove = None;
    let mut touched = false;
    for (i, a) in draft.advisors.iter_mut().enumerate() {
        if advisor_card(ui, a, input, actions, &mut touched) {
            remove = Some(i);
        }
        ui.add_space(8.0);
    }
    draft.touched |= touched;
    if let Some(i) = remove {
        draft.remove_advisor(i);
    }
    if DraftButton::new("Add advisor")
        .icon(ICON_PLUS)
        .show(ui)
        .clicked()
    {
        draft.add_advisor();
    }
    ui.add_space(14.0);
}

fn count_field(ui: &mut Ui, id: &str, text: &mut String, hint: &str) -> bool {
    let pal = Palette::of_ui(ui);
    let invalid = parse_count(text, "").is_err();
    let resp = ui
        .scope(|ui| {
            if invalid {
                ui.visuals_mut().widgets.inactive.bg_stroke = egui::Stroke::new(1.0, pal.danger);
            }
            ui.add(
                TextEdit::singleline(text)
                    .id(Id::unique(id))
                    .desired_width(120.0)
                    .hint_text(hint)
                    .margin(Margin::symmetric(8, 5)),
            )
        })
        .inner;
    if invalid {
        ui.label(RichText::new("Whole number").small().color(pal.danger));
    }
    resp.changed()
}

/// One advisor: a header row (name, summary, remove) and, opened, every field. True = remove.
fn advisor_card(
    ui: &mut Ui,
    a: &mut EntryDraft,
    input: &AdvisorInput<'_>,
    actions: &mut Vec<AdvisorAction>,
    touched: &mut bool,
) -> bool {
    let pal = Palette::of_ui(ui);
    let mut remove = false;
    card_frame(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::Sides::new().shrink_left().show(
            ui,
            |ui| {
                let icon = if a.open {
                    ICON_CHEVRON_DOWN
                } else {
                    ICON_CHEVRON_RIGHT
                };
                let title = if a.name.trim().is_empty() {
                    "New advisor".to_owned()
                } else {
                    a.name.clone()
                };
                let resp = ui
                    .add(
                        egui::Button::new((
                            icon_text(icon, 14.0, pal.glyph),
                            theme::strong(title).size(14.5).color(pal.text_strong),
                        ))
                        .frame(false),
                    )
                    .on_hover_cursor(CursorIcon::PointingHand);
                if resp.clicked() {
                    a.open = !a.open;
                }
                if a.enabled == Some(false) {
                    badge::show(ui, None, "disabled", Hue::Amber, 11.0);
                }
                if let Some(m) = &a.model {
                    badge::show(
                        ui,
                        Some(ICON_BOT),
                        &chip_text(Some(m), input.models),
                        Hue::Blue,
                        11.0,
                    );
                }
            },
            |ui| {
                if icon_button(
                    ui,
                    ICON_TRASH_2,
                    "Remove this advisor (applies when saved)",
                    false,
                )
                .clicked()
                {
                    remove = true;
                }
            },
        );
        if !a.open {
            return;
        }
        ui.add_space(8.0);
        let uid = a.uid;
        card::kv(ui, 120.0, "Name", |ui| {
            let resp = ui.add(
                TextEdit::singleline(&mut a.name)
                    .id(Id::unique(("advisor-name", uid)))
                    .desired_width(260.0)
                    .hint_text("security-review")
                    .margin(Margin::symmetric(8, 5)),
            );
            *touched |= resp.changed();
        });
        ui.add_space(4.0);
        card::kv(ui, 120.0, "Model", |ui| {
            model_row(ui, a, input, actions, touched);
        });
        ui.add_space(4.0);
        card::kv(ui, 120.0, "Review mode", |ui| {
            *touched |= choice_combo(ui, ("advisor-mode", uid), &mut a.review_mode, &REVIEW_MODES);
        });
        card::kv(ui, 120.0, "Review every", |ui| {
            *touched |= count_field(
                ui,
                &format!("advisor-interval-{uid}"),
                &mut a.review_interval,
                "turns",
            );
        });
        card::kv(ui, 120.0, "Sync backlog", |ui| {
            *touched |= choice_combo(
                ui,
                ("advisor-backlog", uid),
                &mut a.sync_backlog,
                &SYNC_BACKLOG,
            );
        });
        card::kv(ui, 120.0, "Scope", |ui| {
            *touched |= choice_combo(ui, ("advisor-scope", uid), &mut a.scope, &SCOPES);
        });
        card::kv(ui, 120.0, "Max notes", |ui| {
            *touched |= count_field(
                ui,
                &format!("advisor-notes-{uid}"),
                &mut a.max_notes,
                "per update",
            );
        });
        card::kv(ui, 120.0, "Enabled", |ui| {
            *touched |= tri_combo(ui, ("advisor-enabled", uid), &mut a.enabled, "On (default)");
        });
        card::kv(ui, 120.0, "Reasoning", |ui| {
            *touched |= tri_combo(
                ui,
                ("advisor-reasoning", uid),
                &mut a.include_reasoning,
                "Off (default)",
            );
        });
        ui.add_space(4.0);
        card::kv(ui, 120.0, "Tools", |_| {});
        *touched |= tools_editor(ui, a);
        ui.add_space(6.0);
        card::kv(ui, 120.0, "Instructions", |_| {});
        let resp = ui.add(
            TextEdit::multiline(&mut a.instructions)
                .id(Id::unique(("advisor-instructions", uid)))
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("What this advisor looks for")
                .margin(Margin::symmetric(10, 8)),
        );
        *touched |= resp.changed();
    });
    remove
}

/// What the model chip says.
fn chip_text(model: Option<&str>, models: &ModelsCache) -> String {
    let Some(model) = model else {
        return "Default model".to_owned();
    };
    if model.contains(',') || model.starts_with('@') || model.starts_with("pi/") {
        return model.to_owned();
    }
    let (selector, level) = split_level(model);
    let name = models
        .get(selector)
        .map_or(selector, |m| short_model_name(&m.name));
    match level {
        Some(l) => format!("{name} · {}", thinking_short(l)),
        None => name.to_owned(),
    }
}

fn model_row(
    ui: &mut Ui,
    a: &mut EntryDraft,
    input: &AdvisorInput<'_>,
    actions: &mut Vec<AdvisorAction>,
    touched: &mut bool,
) {
    let pal = Palette::of_ui(ui);
    let label = chip_text(a.model.as_deref(), input.models);
    let chip = ui
        .add(
            egui::Button::new((
                icon_text(ICON_BOT, 15.0, pal.glyph),
                RichText::new(label).color(if a.model.is_some() {
                    pal.text_strong
                } else {
                    pal.weak
                }),
                icon_text(ICON_CHEVRONS_UP_DOWN, 15.0, pal.weak),
            ))
            .min_size(vec2(260.0, 30.0))
            .corner_radius(theme::rad(6, 10)),
        )
        .on_hover_cursor(CursorIcon::PointingHand);
    let (current, level) = match a.model.as_deref() {
        Some(m) if !m.contains(',') && !m.starts_with('@') && !m.starts_with("pi/") => {
            let (s, l) = split_level(m);
            (Some(s.to_owned()), l.map(str::to_owned))
        }
        _ => (None, None),
    };
    let picked = model_picker::show(
        &chip,
        &mut a.picker,
        &PickerInput {
            catalog: input.models,
            current: current.as_deref(),
            level: level.as_deref(),
            favorites: input.favorites,
            provider: input.provider,
            unset: "Pick a model first",
        },
    );
    for p in picked {
        match p {
            PickerAction::Switch { selector, level } => {
                let level = level.or_else(|| {
                    input
                        .models
                        .get(&selector)
                        .and_then(|m| model_picker::level_for_switch(m, level_of(a)))
                });
                a.model = Some(match level {
                    Some(l) => format!("{selector}:{l}"),
                    None => selector,
                });
                *touched = true;
            }
            PickerAction::Effort(level) => {
                if let Some(s) = &current {
                    a.model = Some(format!("{s}:{level}"));
                    *touched = true;
                }
            }
            PickerAction::Refresh => {
                actions.push(AdvisorAction::Models(AgentsAction::ReloadModels))
            }
            PickerAction::ToggleFavorite(s) => {
                actions.push(AdvisorAction::Models(AgentsAction::ToggleFavorite(s)));
            }
            PickerAction::SetProvider(p) => {
                actions.push(AdvisorAction::Models(AgentsAction::ModelProvider(p)));
            }
        }
    }
    if a.model.is_some()
        && icon_button(ui, ICON_X, "Remove the model: omp picks its default", false).clicked()
    {
        a.model = None;
        *touched = true;
    }
}

/// The thinking level the advisor's model value carries.
fn level_of(a: &EntryDraft) -> Option<&str> {
    a.model.as_deref().and_then(|m| split_level(m).1)
}

/// A dropdown of `options` plus "Default" (unset). A value omp has that is not an option is
/// listed too, so it stays visible and selectable.
fn choice_combo(
    ui: &mut Ui,
    id: (&str, u64),
    value: &mut Option<String>,
    options: &[&str],
) -> bool {
    let mut changed = false;
    let text = value.clone().unwrap_or_else(|| "Default".to_owned());
    egui::ComboBox::from_id_salt(id)
        .selected_text(text)
        .width(190.0)
        .show_ui(ui, |ui| {
            if ui.selectable_label(value.is_none(), "Default").clicked() && value.is_some() {
                *value = None;
                changed = true;
            }
            if let Some(v) = value.clone()
                && !options.contains(&v.as_str())
            {
                let _ = ui.selectable_label(true, &v);
            }
            for o in options {
                let sel = value.as_deref() == Some(*o);
                if ui.selectable_label(sel, *o).clicked() && !sel {
                    *value = Some((*o).to_owned());
                    changed = true;
                }
            }
        });
    changed
}

/// Default / Yes / No for an optional flag; `default_text` names what unset means.
fn tri_combo(ui: &mut Ui, id: (&str, u64), value: &mut Option<bool>, default_text: &str) -> bool {
    let mut changed = false;
    let text = match value {
        None => default_text,
        Some(true) => "Yes",
        Some(false) => "No",
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(text)
        .width(190.0)
        .show_ui(ui, |ui| {
            for (v, label) in [
                (None, default_text),
                (Some(true), "Yes"),
                (Some(false), "No"),
            ] {
                if ui.selectable_label(*value == v, label).clicked() && *value != v {
                    *value = v;
                    changed = true;
                }
            }
        });
    changed
}

/// The tools checklist: omp's default list, or every built-in tool as a checkbox (names the
/// file has that omp does not list stay as extra checked entries).
fn tools_editor(ui: &mut Ui, a: &mut EntryDraft) -> bool {
    let pal = Palette::of_ui(ui);
    let mut changed = false;
    let mut default = a.tools.is_none();
    if ui
        .checkbox(&mut default, "omp's default tools (read, grep, glob)")
        .changed()
    {
        a.tools = if default {
            None
        } else {
            Some(vec!["read".into(), "grep".into(), "glob".into()])
        };
        changed = true;
    }
    if a.tools.is_none() {
        return changed;
    }
    let extras: Vec<String> = a
        .tools
        .iter()
        .flatten()
        .filter(|t| !TOOLS.contains(&t.as_str()))
        .cloned()
        .collect();
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(14.0, 4.0);
        for tool in TOOLS
            .iter()
            .copied()
            .chain(extras.iter().map(String::as_str))
        {
            let mut on = a.tools.iter().flatten().any(|t| t == tool);
            let extra = !TOOLS.contains(&tool);
            let mut text = RichText::new(tool).monospace().small();
            if extra {
                text = text.color(pal.warn);
            }
            let resp = ui.checkbox(&mut on, text);
            if extra {
                resp.clone()
                    .on_hover_text("Not one of omp's tools; omp may drop it");
            }
            if resp.changed() {
                a.set_tool(tool, on);
                changed = true;
            }
        }
    });
    changed
}

/// Whether a saved document reaches the live session of the active tab: a project document
/// only if that tab is in the draft's own project, the user document any live tab.
pub fn apply_to_active(
    scope: AdvisorScope,
    project: Option<&Path>,
    tab_project: Option<&Path>,
) -> bool {
    match scope {
        AdvisorScope::User => true,
        AdvisorScope::Project => project.is_some() && project == tab_project,
    }
}
/// Sends `doc` to be saved as the document of `scope` / `project`, remembering what was sent so
/// the reply only settles the edits it contained.
fn begin_save(
    state: &mut AdvisorState,
    scope: AdvisorScope,
    project: Option<PathBuf>,
    doc: AdvisorDoc,
    actions: &mut Vec<AdvisorAction>,
) {
    state.problem = None;
    state.saving = true;
    state.note = None;
    state.in_flight = Some(doc.clone());
    actions.push(AdvisorAction::Save {
        scope,
        project,
        doc,
    });
}

/// The Save button: validates the draft, then saves, confirms a delete, or settles a no-op.
fn request_save(
    state: &mut AdvisorState,
    scope: AdvisorScope,
    project: Option<PathBuf>,
    actions: &mut Vec<AdvisorAction>,
) {
    let Some(draft) = &state.draft else { return };
    match draft.to_doc() {
        Err(e) => state.problem = Some(e),
        Ok(doc) if doc.is_empty() && !draft.had_content() => {
            // Nothing to write and nothing to delete.
            state.problem = None;
            if let Some(d) = &mut state.draft {
                d.touched = false;
            }
        }
        Ok(doc) if doc.is_empty() => {
            state.problem = None;
            state.confirm_delete = Some(doc);
        }
        Ok(doc) => begin_save(state, scope, project, doc, actions),
    }
}

/// Words for whose file a document is: "the user file" or "project <folder>".
fn owner_label(scope: AdvisorScope, project: Option<&Path>) -> String {
    match (scope, project) {
        (AdvisorScope::Project, Some(p)) => format!(
            "project {}",
            p.file_name().map_or_else(
                || p.display().to_string(),
                |n| n.to_string_lossy().into_owned()
            )
        ),
        _ => "the user file".to_owned(),
    }
}

/// The unsaved draft belongs to another file than the active tab shows: it must be saved to
/// its own file or discarded, never written to the file now in front of the user.
fn owner_mismatch(
    owner: Option<&(AdvisorScope, Option<PathBuf>)>,
    scope: AdvisorScope,
    project: Option<&Path>,
) -> bool {
    owner.is_some_and(|(s, p)| (*s, p.as_deref()) != (scope, project))
}

/// Banner above a foreign draft: where it belongs, and the two ways out.
fn foreign_banner(
    ui: &mut Ui,
    state: &mut AdvisorState,
    owner: &(AdvisorScope, Option<PathBuf>),
    now: (AdvisorScope, Option<&Path>),
    actions: &mut Vec<AdvisorAction>,
) {
    let pal = Palette::of_ui(ui);
    let theirs = owner_label(owner.0, owner.1.as_deref());
    let ours = owner_label(now.0, now.1);
    egui::Frame::NONE
        .stroke(egui::Stroke::new(1.0, pal.warn))
        .corner_radius(theme::rad(6, 10))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(icon_text(ICON_TRIANGLE_ALERT, 16.0, pal.warn));
                ui.add(
                    egui::Label::new(
                        RichText::new(format!(
                            "These unsaved changes are for {theirs}. The active tab shows {ours}."
                        ))
                        .color(pal.text_strong),
                    )
                    .wrap_mode(egui::TextWrapMode::Wrap),
                );
            });
            ui.add_space(6.0);
            show_issues(ui, state);
            ui.horizontal(|ui| {
                let idle = !state.saving;
                let save = ui
                    .add_enabled_ui(idle, |ui| {
                        DraftButton::new(&format!("Save to {theirs}"))
                            .icon(ICON_SAVE)
                            .kind(DraftKind::Primary)
                            .show(ui)
                    })
                    .inner;
                let discard = ui
                    .add_enabled_ui(idle, |ui| {
                        DraftButton::new(&format!("Discard and load {ours}"))
                            .icon(ICON_UNDO_2)
                            .show(ui)
                    })
                    .inner;
                if save.clicked() {
                    request_save(state, owner.0, owner.1.clone(), actions);
                }
                if discard.clicked() {
                    state.draft = None;
                    state.key = None;
                    state.problem = None;
                    state.note = None;
                    state.confirm_delete = None;
                }
            });
        });
    ui.add_space(10.0);
}
/// Where the validation error and omp's reply of a save are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IssuesHome {
    /// Above the Save button at the page's foot.
    Footer,
    /// Inside the draft-owner banner, which is where its Save button is.
    Banner,
}

/// A foreign draft is saved from the banner, so its errors belong there: the footer may be far
/// out of view.
fn issues_home(foreign: bool) -> IssuesHome {
    if foreign {
        IssuesHome::Banner
    } else {
        IssuesHome::Footer
    }
}

/// The validation problem and omp's last save reply, as `(is error, text)` lines.
fn issue_lines(state: &AdvisorState) -> Vec<(bool, String)> {
    let mut lines = Vec::new();
    if let Some(p) = &state.problem {
        lines.push((true, p.clone()));
    }
    match &state.note {
        Some(Note::Saved(m)) => lines.push((false, m.clone())),
        Some(Note::Failed(e)) => lines.push((true, summarize(e).0)),
        None => {}
    }
    lines
}

fn show_issues(ui: &mut Ui, state: &AdvisorState) {
    let pal = Palette::of_ui(ui);
    for (error, text) in issue_lines(state) {
        let (icon, tone) = if error {
            (ICON_CIRCLE_ALERT, pal.danger)
        } else {
            (ICON_CIRCLE_CHECK, pal.ok)
        };
        ui.horizontal_top(|ui| {
            ui.label(icon_text(icon, 15.0, tone));
            ui.add(
                egui::Label::new(RichText::new(text).color(tone))
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );
        });
        ui.add_space(6.0);
    }
}

fn footer(
    ui: &mut Ui,
    state: &mut AdvisorState,
    owner: &(AdvisorScope, Option<PathBuf>),
    actions: &mut Vec<AdvisorAction>,
    live_tab: bool,
    foreign: bool,
) {
    let (scope, project) = (owner.0, owner.1.clone());
    let pal = Palette::of_ui(ui);
    let dirty = state.unsaved();
    if issues_home(foreign) == IssuesHome::Footer {
        show_issues(ui, state);
    }
    ui.horizontal(|ui| {
        let save = ui
            .add_enabled_ui(dirty && !state.saving, |ui| {
                DraftButton::new("Save")
                    .icon(ICON_SAVE)
                    .kind(DraftKind::Primary)
                    .show(ui)
            })
            .inner;
        let revert = ui
            .add_enabled_ui(dirty && !state.saving, |ui| {
                DraftButton::new("Revert").icon(ICON_UNDO_2).show(ui)
            })
            .inner;
        if dirty {
            ui.label(RichText::new("Unsaved changes").color(pal.warn));
        } else if !state.saving {
            ui.label(
                RichText::new(if live_tab {
                    "Saving also applies to the open omp session"
                } else {
                    "Applies to omp sessions as they start"
                })
                .small()
                .color(pal.weak),
            );
        }
        if revert.clicked()
            && let Some(d) = state.draft.take()
        {
            state.draft = Some(Draft::from_doc(d.base));
            state.problem = None;
            state.note = None;
        }
        if save.clicked() {
            request_save(state, scope, project, actions);
        }
    });
    ui.add_space(6.0);
    if let Some(d) = &state.draft
        && let Some(path) = d.base.path.as_ref()
        && ui
            .add(egui::Button::new((
                icon_text(ICON_COPY, 13.0, pal.glyph),
                RichText::new("Copy file path").small(),
            )))
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
    {
        actions.push(AdvisorAction::Models(AgentsAction::CopyPath(
            path.display().to_string(),
        )));
    }
}

/// "Delete WATCHDOG.yml?" — an empty document removes the file.
fn confirm_delete_dialog(
    ctx: &egui::Context,
    state: &mut AdvisorState,
    scope: AdvisorScope,
    project: Option<PathBuf>,
    actions: &mut Vec<AdvisorAction>,
) {
    if state.confirm_delete.is_none() {
        return;
    }
    let pal = Palette::of_ctx(ctx);
    let path = state
        .draft
        .as_ref()
        .and_then(|d| d.base.path.as_ref())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "WATCHDOG.yml".to_owned());
    let answer = show_dialog(
        ctx,
        &Dialog {
            id: "advisor-delete",
            title: "Delete WATCHDOG.yml",
            width: 460.0,
            confirm_label: "Delete file",
            confirm_enabled: true,
            danger: true,
            enter_confirms: false,
            ctrl_enter_confirms: false,
            icon: Some(ICON_TRASH_2),
            subtitle: Some("An empty document removes the file"),
            tone: None,
            esc_hint: None,
        },
        |ui| {
            card::kv(ui, 84.0, "Scope", |ui| {
                badge::show(ui, None, scope.as_str(), Hue::Violet, 11.0);
            });
            card::kv(ui, 84.0, "File", |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(&path)
                            .monospace()
                            .small()
                            .color(pal.text_strong),
                    )
                    .truncate(),
                );
            });
            ui.add_space(6.0);
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "You removed every advisor and cleared the shared fields. Saving an empty document deletes the file, and its advisors with it.",
                    )
                    .color(pal.weak),
                )
                .wrap_mode(egui::TextWrapMode::Wrap),
            );
        },
    );
    match answer {
        Some(true) => {
            if let Some(doc) = state.confirm_delete.take() {
                begin_save(state, scope, project, doc, actions);
            }
        }
        Some(false) => state.confirm_delete = None,
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> AdvisorEntry {
        AdvisorEntry {
            name: name.to_owned(),
            ..AdvisorEntry::default()
        }
    }

    fn json_map(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        match v {
            serde_json::Value::Object(m) => m,
            _ => serde_json::Map::new(),
        }
    }

    fn full() -> AdvisorDoc {
        AdvisorDoc {
            extra: json_map(serde_json::json!({"futureTopLevel": {"a": [1, 2]}})),
            instructions: Some("Be terse.".into()),
            max_notes_per_update: Some(3),
            advisors: vec![
                AdvisorEntry {
                    extra: json_map(serde_json::json!({"futureField": "keep", "n": 2})),
                    name: "sec".into(),
                    model: Some("anthropic/claude-x:high".into()),
                    tools: Some(vec!["read".into(), "my_plugin_tool".into()]),
                    review_mode: Some("agent-end".into()),
                    review_interval: Some(4),
                    sync_backlog: Some("strict".into()),
                    instructions: Some("Look for secrets.".into()),
                    enabled: Some(false),
                    max_notes_per_update: Some(2),
                    scope: Some("subagent".into()),
                    include_reasoning: Some(true),
                },
                entry("plain"),
            ],
            warnings: vec!["advisor 3 dropped: no name".into()],
            path: Some(PathBuf::from("/p/WATCHDOG.yml")),
        }
    }

    #[test]
    fn an_untouched_document_round_trips_exactly() {
        let doc = full();
        assert_eq!(Draft::from_doc(doc.clone()).to_doc(), Ok(doc));
    }

    #[test]
    fn editing_one_field_changes_only_that_field() {
        let doc = full();
        let mut d = Draft::from_doc(doc.clone());
        d.advisors[0].name = "security".into();
        d.advisors[0].review_interval = "7".into();
        d.instructions = "Be thorough.".into();
        let out = d.to_doc().expect("valid");
        assert_eq!(out.advisors[0].name, "security");
        assert_eq!(out.advisors[0].review_interval, Some(7));
        assert_eq!(out.instructions.as_deref(), Some("Be thorough."));
        // Everything else, including a tool omp does not list and read-only parts, is kept.
        let mut expect = doc;
        expect.advisors[0].name = "security".into();
        expect.advisors[0].review_interval = Some(7);
        expect.instructions = Some("Be thorough.".into());
        assert_eq!(out, expect);
    }

    #[test]
    fn values_the_editor_does_not_know_survive_a_save() {
        let mut doc = full();
        doc.advisors[0].review_mode = Some("future-mode".into());
        doc.advisors[0].sync_backlog = Some("7".into());
        let d = Draft::from_doc(doc.clone());
        assert_eq!(d.to_doc(), Ok(doc));
    }

    #[test]
    fn clearing_a_field_unsets_it_and_bad_numbers_are_refused() {
        let mut d = Draft::from_doc(full());
        d.advisors[0].instructions.clear();
        d.advisors[0].max_notes.clear();
        d.max_notes = "  ".into();
        let out = d.to_doc().expect("valid");
        assert_eq!(out.advisors[0].instructions, None);
        assert_eq!(out.advisors[0].max_notes_per_update, None);
        assert_eq!(out.max_notes_per_update, None);

        d.advisors[0].review_interval = "often".into();
        let e = d.to_doc().expect_err("not a number");
        assert!(e.contains("sec") && e.contains("whole number"), "{e}");
        d.advisors[0].review_interval = "2".into();
        d.max_notes = "-1".into();
        assert!(d.to_doc().is_err());
    }

    #[test]
    fn advisors_can_be_added_removed_and_must_be_named_apart() {
        let mut d = Draft::from_doc(AdvisorDoc::default());
        assert!(!d.touched);
        d.add_advisor();
        assert!(d.touched);
        assert!(d.to_doc().expect_err("unnamed").contains("name"));
        d.advisors[0].name = "a".into();
        d.add_advisor();
        d.advisors[1].name = "a".into();
        assert!(d.to_doc().expect_err("duplicate").contains("“a”"));
        d.advisors[1].name = "b".into();
        assert_eq!(d.to_doc().expect("valid").advisors.len(), 2);
        d.remove_advisor(0);
        let doc = d.to_doc().expect("valid");
        assert_eq!(doc.advisors.len(), 1);
        assert_eq!(doc.advisors[0].name, "b");
    }

    #[test]
    fn a_draft_with_everything_removed_is_an_empty_document() {
        let mut doc = full();
        let unknown_top_level = std::mem::take(&mut doc.extra);
        let mut d = Draft::from_doc(doc);
        assert!(d.had_content());
        d.remove_advisor(1);
        d.remove_advisor(0);
        d.instructions = "   ".into();
        d.max_notes.clear();
        let doc = d.to_doc().expect("valid");
        assert!(doc.is_empty(), "saving it deletes the file");
        assert!(!Draft::from_doc(AdvisorDoc::default()).had_content());
        // Keys omp keeps that the editor does not know keep the file alive.
        let d = Draft::from_doc(AdvisorDoc {
            extra: unknown_top_level,
            ..AdvisorDoc::default()
        });
        assert!(d.had_content());
        assert!(!d.to_doc().expect("valid").is_empty());
    }

    #[test]
    fn the_tools_list_switches_one_tool_and_keeps_the_rest() {
        let mut e = EntryDraft::from_entry(entry("a"), 1, false);
        assert_eq!(e.tools, None, "unset = omp's default list");
        e.set_tool("bash", true);
        assert_eq!(e.tools, Some(vec!["bash".to_owned()]));
        e.set_tool("bash", true);
        e.set_tool("edit", true);
        assert_eq!(e.tools, Some(vec!["bash".to_owned(), "edit".to_owned()]));
        e.set_tool("bash", false);
        assert_eq!(e.tools, Some(vec!["edit".to_owned()]));
        e.set_tool("edit", false);
        assert_eq!(
            e.tools,
            Some(Vec::new()),
            "an explicit empty list means no tools"
        );
    }

    #[test]
    fn a_reply_for_another_file_is_ignored() {
        let mut st = AdvisorState {
            key: Some((AdvisorScope::User, None)),
            load: Load::Loading,
            ..AdvisorState::default()
        };
        st.loaded(AdvisorScope::Project, Some(Path::new("/p")), Ok(full()));
        assert!(st.draft.is_none());
        st.loaded(AdvisorScope::User, None, Ok(full()));
        assert!(st.draft.is_some());
        // A save's reply reads the file back and clears the unsaved mark.
        if let Some(d) = &mut st.draft {
            d.touched = true;
        }
        assert!(st.unsaved());
        send(&mut st, AdvisorScope::User, None);
        st.saved(Ok("Saved user WATCHDOG.yml".into()));
        assert!(!st.unsaved());
        assert!(st.key.is_none());
        assert!(matches!(st.note, Some(Note::Saved(_))));
        st.saved(Err("nope".into()));
        assert!(matches!(st.note, Some(Note::Failed(_))));
    }

    /// Sends the draft's own document, as the Save button does.
    fn send(st: &mut AdvisorState, scope: AdvisorScope, project: Option<PathBuf>) -> AdvisorDoc {
        let doc = st.draft.as_ref().expect("draft").to_doc().expect("valid");
        let mut actions = Vec::new();
        begin_save(st, scope, project.clone(), doc.clone(), &mut actions);
        assert_eq!(
            actions,
            vec![AdvisorAction::Save {
                scope,
                project,
                doc: doc.clone()
            }]
        );
        doc
    }

    #[test]
    fn a_draft_is_saved_where_it_was_read_not_where_the_tab_now_is() {
        let a = PathBuf::from("/work/a");
        let mut st = AdvisorState {
            key: Some((AdvisorScope::Project, Some(a.clone()))),
            draft: Some(Draft::from_doc(full())),
            ..AdvisorState::default()
        };
        if let Some(d) = &mut st.draft {
            d.touched = true;
        }
        // Same file in front: not foreign.
        let owner = st.key.as_ref();
        assert!(!owner_mismatch(
            owner,
            AdvisorScope::Project,
            Some(a.as_path())
        ));
        // Another project, the project tab closed (user file only), or the other scope: foreign.
        assert!(owner_mismatch(
            owner,
            AdvisorScope::Project,
            Some(Path::new("/work/b"))
        ));
        assert!(owner_mismatch(owner, AdvisorScope::User, None));
        assert!(owner_mismatch(owner, AdvisorScope::User, Some(a.as_path())));
        // No draft owner yet: nothing to protect.
        assert!(!owner_mismatch(None, AdvisorScope::User, None));
        // The Save the banner offers carries the draft's owner.
        let mut actions = Vec::new();
        let (scope, project) = st.key.clone().expect("owner");
        request_save(&mut st, scope, project, &mut actions);
        assert!(matches!(
            actions.as_slice(),
            [AdvisorAction::Save { scope: AdvisorScope::Project, project: Some(p), .. }] if *p == a
        ));
        // A live session gets the document only when it is the draft's own project (or user).
        let b = Path::new("/work/b");
        assert!(apply_to_active(
            AdvisorScope::Project,
            Some(a.as_path()),
            Some(a.as_path())
        ));
        assert!(!apply_to_active(
            AdvisorScope::Project,
            Some(a.as_path()),
            Some(b)
        ));
        assert!(!apply_to_active(
            AdvisorScope::Project,
            Some(a.as_path()),
            None
        ));
        assert!(apply_to_active(AdvisorScope::User, None, Some(b)));
    }

    #[test]
    fn a_save_reply_leaves_edits_made_while_it_ran_unsaved() {
        let mut st = AdvisorState {
            key: Some((AdvisorScope::User, None)),
            draft: Some(Draft::from_doc(full())),
            ..AdvisorState::default()
        };
        if let Some(d) = &mut st.draft {
            d.touched = true;
        }
        send(&mut st, AdvisorScope::User, None);
        assert!(st.saving);
        // The user keeps typing while omp writes the file.
        if let Some(d) = &mut st.draft {
            d.instructions = "Typed during the save".into();
        }
        st.saved(Ok("Saved".into()));
        assert!(st.unsaved(), "the newer edit is not marked saved");
        assert!(st.key.is_some(), "and the draft is not reloaded over");
        assert_eq!(
            st.draft.as_ref().map(|d| d.instructions.as_str()),
            Some("Typed during the save")
        );
        assert!(matches!(st.note, Some(Note::Saved(_))));
        // Saving that newer state then settles normally.
        send(&mut st, AdvisorScope::User, None);
        st.saved(Ok("Saved".into()));
        assert!(!st.unsaved());
        assert!(st.key.is_none());
    }

    #[test]
    fn a_foreign_drafts_validation_error_is_shown_in_the_banner_not_the_footer() {
        // An advisor without a name, owned by another project than the one now in front.
        let mut doc = full();
        doc.advisors[0].name = String::new();
        let a = PathBuf::from("/work/a");
        let mut st = AdvisorState {
            key: Some((AdvisorScope::Project, Some(a.clone()))),
            draft: Some(Draft::from_doc(doc)),
            ..AdvisorState::default()
        };
        if let Some(d) = &mut st.draft {
            d.touched = true;
        }
        let foreign = owner_mismatch(
            st.key.as_ref(),
            AdvisorScope::Project,
            Some(Path::new("/b")),
        );
        assert!(foreign);
        // The banner's "Save to" refuses, and says why.
        let mut actions = Vec::new();
        request_save(&mut st, AdvisorScope::Project, Some(a), &mut actions);
        assert!(actions.is_empty(), "nothing is sent for an invalid draft");
        let lines = issue_lines(&st);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].0, "it is an error");
        assert!(lines[0].1.contains("needs a name"), "{lines:?}");
        // Foreign drafts show the lines in the banner; the footer stays free of them.
        assert_eq!(issues_home(foreign), IssuesHome::Banner);
        assert_eq!(issues_home(false), IssuesHome::Footer);
    }
}
