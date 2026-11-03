//! Widgets of the omp settings editor: one row per setting with its editor, the JSON / secret
//! modals, the loading skeleton and the info banner.

use crate::icons::{
    ICON_CIRCLE_CHECK, ICON_INFO, ICON_KEY_ROUND, ICON_LOCK, ICON_PENCIL, ICON_ROTATE_CCW,
    ICON_SLIDERS_HORIZONTAL, ICON_TRASH_2,
};
use deck_core::omp::{Setting, SettingKind};
use egui::{
    Align2, CursorIcon, Frame, Id, Key, Margin, Rect, RichText, Sense, Stroke, TextEdit, Ui, pos2,
    vec2,
};
use serde_json::Value;

use crate::badge::{self, Hue};
use crate::card;
use crate::confirm::{Dialog, show_dialog};
use crate::omp_settings::{Row, parse_json_for, parse_number, pretty_json, preview, value_text};
use crate::summary::summarize;
use crate::theme::{self, Palette};
use crate::widgets::{icon_text, paint_icon, toggle};

/// What a row asks the page to do.
pub enum RowEvent {
    Set(Value),
    Reset,
    CopyKey,
    EditJson,
    EditSecret,
    /// Put an omp error message (full text) on the clipboard.
    CopyDetails(String),
}

const RESET_SLOT: f32 = 30.0;

/// Borrowed editing state of a row (disjoint from what the left half reads).
struct EditCtx<'a> {
    setting: &'a Setting,
    buf: &'a mut Option<String>,
    input_error: &'a mut Option<String>,
    pending: bool,
}

/// Draws one setting row. Rows that are far off-screen are replaced by blank space of their last
/// measured height, so only visible rows pay for layout.
pub fn show_row(ui: &mut Ui, row: &mut Row, first: bool) -> Option<RowEvent> {
    let pal = Palette::of_ui(ui);
    let width = ui.available_width();
    let top = ui.cursor().min;
    let probe = Rect::from_min_size(top, vec2(width, row.height));
    if !ui.is_rect_visible(probe) {
        ui.allocate_space(vec2(width, row.height));
        return None;
    }

    if !first {
        ui.add_space(8.0);
        let y = ui.cursor().top();
        ui.painter().hline(
            ui.max_rect().x_range(),
            y,
            Stroke::new(1.0, pal.stroke.gamma_multiply(0.55)),
        );
        ui.add_space(9.0);
    }

    let pending = row.pending;
    let current = row.setting.value.as_ref().or(row.setting.default.as_ref());
    let modified = row.setting.is_modified();
    let secret = row.setting.secret;
    let mut edit = EditCtx {
        setting: &row.setting,
        buf: &mut row.buf,
        input_error: &mut row.input_error,
        pending,
    };
    let (left_ev, right_ev) = egui::Sides::new().shrink_left().spacing(18.0).show(
        ui,
        |ui| {
            left_half(
                ui,
                &row.label,
                &row.setting.key,
                &row.setting.description,
                modified,
                row.overridden_by.as_deref(),
            )
        },
        |ui| {
            let mut ev = None;
            // Right-to-left: reset slot (always reserved so editors line up), spinner, editor.
            let (slot, _) = ui.allocate_exact_size(vec2(RESET_SLOT, 28.0), Sense::hover());
            if modified && !pending {
                let tip = if secret { "Clear" } else { "Reset to default" };
                let icon = if secret {
                    ICON_TRASH_2
                } else {
                    ICON_ROTATE_CCW
                };
                let resp = ui.interact(slot, ui.next_auto_id().with("reset"), Sense::click());
                let hov = ui
                    .ctx()
                    .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
                ui.painter()
                    .rect_filled(slot, 8, pal.raised.gamma_multiply(0.85 * hov));
                paint_icon(
                    ui.painter(),
                    slot.center(),
                    Align2::CENTER_CENTER,
                    icon,
                    18.0,
                    theme::mix(pal.weak, pal.text_strong, hov),
                );
                if resp
                    .on_hover_text(tip)
                    .on_hover_cursor(CursorIcon::PointingHand)
                    .clicked()
                {
                    ev = Some(RowEvent::Reset);
                }
            }
            if pending {
                ui.add(crate::widgets::Spin::new(16.0, pal.accent_b));
            }
            if let Some(e) = editor(ui, &mut edit, current) {
                ev = Some(e);
            }
            ev
        },
    );

    let mut detail_ev = None;
    if let Some(full) = &row.error {
        let (line, more) = summarize(full);
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::Label::new(RichText::new(line).small().color(pal.danger))
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );
            if more
                && ui
                    .add(
                        egui::Button::new(RichText::new("Copy details").small().color(pal.weak))
                            .frame(false),
                    )
                    .on_hover_text("Copy the full message to the clipboard")
                    .clicked()
            {
                detail_ev = Some(RowEvent::CopyDetails(full.clone()));
            }
        });
    } else if let Some(msg) = &row.input_error {
        ui.add_space(4.0);
        ui.add(
            egui::Label::new(RichText::new(msg).small().color(pal.danger))
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    }
    row.height = (ui.cursor().top() - top.y).max(40.0);
    right_ev.or(left_ev).or(detail_ev)
}

fn left_half(
    ui: &mut Ui,
    label: &str,
    key: &str,
    description: &str,
    modified: bool,
    overridden_by: Option<&str>,
) -> Option<RowEvent> {
    let pal = Palette::of_ui(ui);
    let mut ev = None;
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            ui.label(RichText::new(label).color(pal.text_strong));
            if modified {
                let (r, resp) = ui.allocate_exact_size(vec2(9.0, 9.0), Sense::hover());
                theme::glow_dot(ui.painter(), r.center(), 3.2, pal.accent_a, 0.4);
                resp.on_hover_text("Modified from the default");
            }
            if let Some(by) = overridden_by {
                badge::show(ui, None, &format!("overridden by {by}"), Hue::Amber, 11.0)
                    .on_hover_text(format!(
                        "The {by}'s own settings file supplies this value in the active tab's \
                         folder; the value saved here applies everywhere else."
                    ));
            }
        });
        let key_resp = ui.add(
            egui::Label::new(RichText::new(key).monospace().small().color(pal.weak))
                .sense(Sense::click()),
        );
        if key_resp
            .on_hover_text("Click to copy the key")
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
        {
            ev = Some(RowEvent::CopyKey);
        }
        if !description.is_empty() {
            ui.add(
                egui::Label::new(RichText::new(description).small().color(pal.weak))
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );
        }
    });
    ev
}

fn editor(ui: &mut Ui, e: &mut EditCtx<'_>, current: Option<&Value>) -> Option<RowEvent> {
    let pal = Palette::of_ui(ui);
    let setting = e.setting;
    ui.add_enabled_ui(!e.pending, |ui| {
        if setting.secret {
            return secret_editor(ui, current.is_some());
        }
        match &setting.kind {
            SettingKind::Bool => {
                let mut on = current.and_then(Value::as_bool).unwrap_or(false);
                toggle(ui, &mut on)
                    .changed()
                    .then_some(RowEvent::Set(Value::Bool(on)))
            }
            SettingKind::Enum(choices) => {
                let selected = current.and_then(Value::as_str).unwrap_or("—").to_owned();
                let mut ev = None;
                egui::ComboBox::from_id_salt(("omp-enum", &setting.key))
                    .selected_text(selected.clone())
                    .width(190.0)
                    .show_ui(ui, |ui| {
                        for c in choices {
                            if ui.selectable_label(*c == selected, c).clicked() && *c != selected {
                                ev = Some(RowEvent::Set(Value::String(c.clone())));
                            }
                        }
                    });
                ev
            }
            SettingKind::Number => text_editor(ui, e, current, true, 120.0),
            SettingKind::String => text_editor(ui, e, current, false, 230.0),
            SettingKind::Array | SettingKind::Record => {
                let mut ev = None;
                if ui
                    .add(
                        egui::Button::new((
                            icon_text(ICON_PENCIL, 16.0, pal.glyph),
                            RichText::new("Edit"),
                        ))
                        .min_size(vec2(0.0, 28.0)),
                    )
                    .clicked()
                {
                    ev = Some(RowEvent::EditJson);
                }
                ui.label(
                    RichText::new(preview(current, 26))
                        .monospace()
                        .small()
                        .color(pal.weak),
                );
                ev
            }
        }
    })
    .inner
}

fn secret_editor(ui: &mut Ui, is_set: bool) -> Option<RowEvent> {
    let pal = Palette::of_ui(ui);
    let mut ev = None;
    if ui
        .add(
            egui::Button::new((icon_text(ICON_LOCK, 16.0, pal.glyph), RichText::new("Set…")))
                .min_size(vec2(0.0, 28.0)),
        )
        .clicked()
    {
        ev = Some(RowEvent::EditSecret);
    }
    let (text, color) = if is_set {
        ("•••••• set", pal.ok)
    } else {
        ("not set", pal.weak)
    };
    ui.label(RichText::new(text).monospace().small().color(color));
    ev
}

fn text_editor(
    ui: &mut Ui,
    e: &mut EditCtx<'_>,
    current: Option<&Value>,
    number: bool,
    width: f32,
) -> Option<RowEvent> {
    let pal = Palette::of_ui(ui);
    let stored = value_text(current);
    let mut text = e.buf.clone().unwrap_or_else(|| stored.clone());
    let invalid = e.input_error.is_some();
    let resp = ui
        .scope(|ui| {
            if invalid {
                ui.visuals_mut().widgets.inactive.bg_stroke = Stroke::new(1.0, pal.danger);
                ui.visuals_mut().widgets.hovered.bg_stroke = Stroke::new(1.0, pal.danger);
            }
            ui.add(
                TextEdit::singleline(&mut text)
                    .id(Id::unique(("omp-field", &e.setting.key)))
                    .desired_width(width)
                    .margin(Margin::symmetric(8, 5))
                    .align(if number {
                        egui::Align2::RIGHT_CENTER
                    } else {
                        egui::Align2::LEFT_CENTER
                    }),
            )
        })
        .inner;
    if resp.changed() {
        *e.input_error = if number {
            parse_number(&text).err()
        } else {
            None
        };
        *e.buf = Some(text.clone());
    }
    // A write in flight disables the field, and egui reports that as another focus loss; without
    // this guard the same text would be submitted (and toasted) twice.
    if e.pending || !resp.lost_focus() {
        return None;
    }
    if ui.input(|i| i.key_pressed(Key::Escape)) || text == stored {
        *e.buf = None;
        *e.input_error = None;
        return None;
    }
    let parsed = if number {
        parse_number(&text)
    } else if text.is_empty() {
        // Clearing a text setting restores its default instead of storing "".
        return Some(RowEvent::Reset);
    } else {
        Ok(Value::String(text))
    };
    match parsed {
        Ok(v) => Some(RowEvent::Set(v)),
        Err(m) => {
            *e.input_error = Some(m);
            None
        }
    }
}

// ------------------------------------------------------------------ modals

/// "Edit array / object as JSON" dialog state.
pub struct JsonModal {
    pub key: String,
    label: String,
    kind: SettingKind,
    text: String,
    error: Option<String>,
    focus: bool,
}

impl JsonModal {
    pub fn new(row: &Row) -> JsonModal {
        let text = pretty_json(&row.setting.kind, row.effective());
        JsonModal {
            key: row.setting.key.clone(),
            label: row.label.clone(),
            kind: row.setting.kind.clone(),
            error: parse_json_for(&row.setting.kind, &text).err(),
            text,
            focus: true,
        }
    }
}

pub enum ModalOutcome {
    Open,
    Cancel,
    Save(Value),
}

pub fn show_json_modal(ctx: &egui::Context, m: &mut JsonModal) -> ModalOutcome {
    let pal = Palette::of_ctx(ctx);
    let title = format!("Edit {}", m.label);
    let what = if m.kind == SettingKind::Record {
        "JSON object"
    } else {
        "JSON array"
    };
    let ok = m.error.is_none();
    let answer = show_dialog(
        ctx,
        &Dialog {
            id: "json",
            title: &title,
            width: 560.0,
            confirm_label: "Save",
            confirm_enabled: ok,
            danger: false,
            enter_confirms: false,
            icon: Some(ICON_SLIDERS_HORIZONTAL),
            subtitle: Some("Writes via omp config set"),
            ..Default::default()
        },
        |ui| {
            card::kv(ui, 84.0, "Setting", |ui| {
                ui.label(RichText::new(&m.key).monospace().color(pal.text_strong));
            });
            card::kv(ui, 84.0, "Type", |ui| {
                badge::show(ui, None, what, Hue::Violet, 11.0);
            });
            ui.add_space(6.0);
            ui.label(RichText::new(format!("Edit the value as a {what}.")).color(pal.weak));
            ui.add_space(10.0);
            let mut changed = false;
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let resp = ui.add(
                        TextEdit::multiline(&mut m.text)
                            .id(Id::unique("omp-json-editor"))
                            .code_editor()
                            .desired_rows(12)
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(10, 8)),
                    );
                    if m.focus {
                        resp.request_focus();
                        m.focus = false;
                    }
                    changed = resp.changed();
                });
            if changed {
                m.error = parse_json_for(&m.kind, &m.text).err();
            }
            ui.add_space(6.0);
            match &m.error {
                Some(e) => {
                    ui.add(
                        egui::Label::new(RichText::new(e).small().color(pal.danger))
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                }
                None => {
                    badge::show(ui, Some(ICON_CIRCLE_CHECK), "Valid JSON", Hue::Green, 11.0);
                }
            }
        },
    );
    match answer {
        Some(true) => match parse_json_for(&m.kind, &m.text) {
            Ok(v) => ModalOutcome::Save(v),
            Err(e) => {
                m.error = Some(e);
                ModalOutcome::Open
            }
        },
        Some(false) => ModalOutcome::Cancel,
        None => ModalOutcome::Open,
    }
}

/// "Set secret" dialog state; the typed text lives only here until saved or cancelled.
pub struct SecretModal {
    pub key: String,
    label: String,
    kind: SettingKind,
    text: String,
    /// Parse failure of a record-typed secret (never echoes the text).
    error: Option<String>,
    focus: bool,
}

impl SecretModal {
    pub fn new(row: &Row) -> SecretModal {
        SecretModal {
            key: row.setting.key.clone(),
            label: row.label.clone(),
            kind: row.setting.kind.clone(),
            text: String::new(),
            error: None,
            focus: true,
        }
    }

    fn is_record(&self) -> bool {
        matches!(self.kind, SettingKind::Record)
    }

    /// The value to save: the text as a string, or — for record-typed secrets — a JSON object.
    fn value(&mut self) -> Result<Value, String> {
        if self.is_record() {
            parse_json_for(&self.kind, &self.text)
        } else {
            Ok(Value::String(std::mem::take(&mut self.text)))
        }
    }
}

pub fn show_secret_modal(ctx: &egui::Context, m: &mut SecretModal) -> ModalOutcome {
    let pal = Palette::of_ctx(ctx);
    let title = format!("Set {}", m.label);
    let ok = !m.text.is_empty();
    let answer = show_dialog(
        ctx,
        &Dialog {
            id: "secret",
            title: &title,
            width: 440.0,
            confirm_label: "Save",
            confirm_enabled: ok,
            danger: false,
            enter_confirms: true,
            ctrl_enter_confirms: false,
            icon: Some(ICON_KEY_ROUND),
            subtitle: Some("Writes via omp config set"),
            tone: Some(pal.warn),
            esc_hint: None,
        },
        |ui| {
            card::kv(ui, 84.0, "Setting", |ui| {
                ui.label(RichText::new(&m.key).monospace().color(pal.text_strong));
            });
            card::kv(ui, 84.0, "Storage", |ui| {
                badge::show(ui, Some(ICON_LOCK), "write-only", Hue::Amber, 11.0);
            });
            ui.add_space(6.0);
            ui.add(
                egui::Label::new(
                    RichText::new("The value is passed to omp and never shown again.")
                        .color(pal.weak),
                )
                .wrap_mode(egui::TextWrapMode::Wrap),
            );
            ui.add_space(10.0);
            let hint = if m.is_record() {
                "Paste the JSON object"
            } else {
                "Paste the secret"
            };
            let resp = ui.add(
                TextEdit::singleline(&mut m.text)
                    .id(Id::unique("omp-secret-field"))
                    .password(true)
                    .hint_text(hint)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 8)),
            );
            if resp.changed() {
                m.error = None;
            }
            if let Some(e) = &m.error {
                ui.add_space(6.0);
                ui.add(
                    egui::Label::new(RichText::new(e).small().color(pal.danger))
                        .wrap_mode(egui::TextWrapMode::Wrap),
                );
            }
            if m.focus {
                resp.request_focus();
                m.focus = false;
            }
        },
    );
    match answer {
        Some(true) => match m.value() {
            Ok(v) => ModalOutcome::Save(v),
            Err(e) => {
                m.error = Some(e);
                ModalOutcome::Open
            }
        },
        Some(false) => ModalOutcome::Cancel,
        None => ModalOutcome::Open,
    }
}

// ------------------------------------------------------------------ chrome

/// Info banner under a section title.
pub fn note_banner(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    Frame::NONE
        .fill(theme::accent_wash(pal.surface, pal.accent_b, 0.07))
        .stroke(if theme::flat() {
            Stroke::new(1.0, pal.stroke)
        } else {
            Stroke::new(1.0, pal.accent_b.gamma_multiply(0.28))
        })
        .corner_radius(theme::rad(8, 12))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                ui.label(icon_text(
                    ICON_INFO,
                    18.0,
                    if theme::flat() {
                        pal.glyph
                    } else {
                        pal.accent_b
                    },
                ));
                ui.add(
                    egui::Label::new(RichText::new(text).small().color(pal.text))
                        .wrap_mode(egui::TextWrapMode::Wrap),
                );
            });
        });
    ui.add_space(14.0);
}

/// Shimmering placeholder cards while the catalog loads.
pub fn skeleton(ui: &mut Ui) {
    let pal = Palette::of_ui(ui);
    let t = ui.ctx().input(|i| i.time) as f32;
    let shimmer = 0.30 + 0.20 * (t * 3.0).sin();
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(33));
    let bar = |ui: &mut Ui, w: f32, h: f32| {
        let (r, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
        ui.painter()
            .rect_filled(r, h / 2.0, pal.raised.gamma_multiply(shimmer + 0.35));
        r
    };
    for _ in 0..3 {
        Frame::NONE
            .fill(if theme::flat() {
                pal.panel
            } else {
                pal.surface.gamma_multiply(0.75)
            })
            .stroke(Stroke::new(1.0, pal.stroke))
            .corner_radius(theme::rad(8, 14))
            .inner_margin(if theme::flat() {
                Margin::symmetric(16, 12)
            } else {
                Margin::symmetric(20, 16)
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                bar(ui, 90.0, 9.0);
                ui.add_space(12.0);
                for i in 0..4 {
                    let right = ui.available_width();
                    let top = ui.cursor().top();
                    bar(ui, 150.0 + 30.0 * (i % 2) as f32, 12.0);
                    ui.add_space(5.0);
                    bar(ui, 280.0, 9.0);
                    let r = Rect::from_min_size(
                        pos2(ui.min_rect().left() + right - 80.0, top),
                        vec2(80.0, 24.0),
                    );
                    ui.painter()
                        .rect_filled(r, 12, pal.raised.gamma_multiply(shimmer + 0.35));
                    ui.add_space(16.0);
                }
            });
        ui.add_space(14.0);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn modal(kind: SettingKind, text: &str) -> SecretModal {
        SecretModal {
            key: "k".into(),
            label: "K".into(),
            kind,
            text: text.into(),
            error: None,
            focus: false,
        }
    }

    #[test]
    fn string_secret_is_taken_verbatim_and_cleared() {
        let mut m = modal(SettingKind::String, "s3cret");
        assert_eq!(m.value(), Ok(json!("s3cret")));
        assert!(m.text.is_empty(), "the typed secret does not linger");
    }

    #[test]
    fn record_secret_is_parsed_as_a_json_object() {
        let mut m = modal(SettingKind::Record, r#"{"https://a":"tok"}"#);
        assert_eq!(m.value(), Ok(json!({"https://a": "tok"})));
        assert!(modal(SettingKind::Record, "[1]").value().is_err());
        assert!(modal(SettingKind::Record, "not json").value().is_err());
    }
}
