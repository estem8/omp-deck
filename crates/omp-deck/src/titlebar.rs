//! The top bars. With client-side decorations: a title row (icon + name, active title, window
//! buttons) above the tab row; with native frames only the tab row (logo, tabs, settings, menu).
//! Also the edge/corner resize zones and outline for the undecorated window.

use crate::icons::{
    ICON_APP_WINDOW, ICON_ARROW_LEFT_RIGHT, ICON_COPY, ICON_DOWNLOAD, ICON_GLOBE, ICON_HISTORY,
    ICON_KEY_ROUND, ICON_LOG_OUT, ICON_MENU, ICON_MINUS, ICON_MOON, ICON_PLUS, ICON_PUZZLE,
    ICON_REFRESH_CW, ICON_ROCKET, ICON_ROTATE_CCW, ICON_SETTINGS, ICON_SQUARE, ICON_SUN,
    ICON_TERMINAL, ICON_X,
};
use deck_core::config::Theme;
use deck_core::omp::UpdateKind;
use egui::RichText;
use egui::viewport::ResizeDirection;
use egui::{
    Align, Context, CursorIcon, Frame, Layout, PointerButton, Sense, Stroke, StrokeKind, UiBuilder,
    ViewportCommand, pos2, vec2,
};

use crate::badge::{self, Hue};
use crate::notify_panel;
use crate::tabs::{self, TabAction, TabBarInput};
use crate::theme::{self, Palette};
use crate::update::{Channel, UpdateState, status_line};
use crate::widgets::{icon_button, icon_text, menu_item, paint_icon, win_button};
use crate::{card, tips};

const EDGE: f32 = 6.0;
const CORNER: f32 = 10.0;
/// Gap between the window's right edge and the window buttons (outside every resize zone).
const CONTROL_INSET: f32 = 12.0;
/// Width of the minimize / maximize / close cluster measured from the right window edge (three
/// 40 px buttons, two 2 px gaps, [`CONTROL_INSET`]). The north resize edge stops short of it: the
/// buttons are 30 px tall, centred in the [`STRIP_H`] strip, so they reach into the top `EDGE` pixels.
const CONTROL_W: f32 = CONTROL_INSET + 3.0 * 40.0 + 2.0 * 2.0;

pub struct BarInput<'a> {
    pub tabs: TabBarInput<'a>,
    /// Draw window buttons and handle drag/resize (no native frame).
    pub csd: bool,
    pub maximized: bool,
    pub settings_active: bool,
    pub can_reopen: bool,
    pub font_size: f32,
    pub theme: Theme,
    /// Why the Light theme is unavailable (the style only exists in dark).
    pub dark_only: Option<&'static str>,
    pub native_decorations: bool,
    pub strip: StripInput<'a>,
    pub omp: Option<OmpBar<'a>>,
    /// A newer OmpDeck release to announce (pill + popover).
    pub app_update: Option<crate::app_update::PillView<'a>>,
    /// A pairing put away behind a pill (click brings the dialog back).
    pub pairing: Option<PairingPill>,
    /// The notification bell (badge + popover).
    pub notifications: crate::notify_panel::PanelInput<'a>,
}

/// omp CLI state shown in the bar; absent when omp was not found.
pub struct OmpBar<'a> {
    pub version: Option<&'a str>,
    pub state: &'a UpdateState,
    /// `(current, latest)` while an update is known.
    pub available: Option<(&'a str, &'a str)>,
    pub channel: Channel,
}

#[derive(Debug)]
pub enum BarAction {
    Tab(TabAction),
    OpenSettings,
    Reopen,
    Refresh,
    /// Open the session manager hub.
    OpenSessions,
    /// Show or hide the agents dashboard.
    OpenAgents,
    FontLarger,
    FontSmaller,
    FontReset,
    SetTheme(Theme),
    SetNativeDecorations(bool),
    /// Open an update tab (Omp or Plugins).
    RunUpdate(UpdateKind),
    /// Ask before switching channel (Canary / Stable).
    SwitchChannel(UpdateKind),
    CheckUpdates,
    AppUpdate(crate::app_update::UpdateAction),
    /// The backgrounded pairing's pill was clicked: show its dialog again.
    ShowPairing,
    /// The notification popover asked for something.
    Inbox(crate::notify_panel::PanelAction),
    Quit,
}

/// Height of the title row (app name, active title, window buttons; client-side decorations only).
/// Everything in it is centred, so the extra height is equal padding above and below.
pub const STRIP_H: f32 = 40.0;
/// Width reserved at each end of the title row for the icon/name and the window buttons.
const STRIP_SIDE_W: f32 = 160.0;

/// The title strip's content: what the active tab is called, and where it lives.
pub struct StripInput<'a> {
    /// Already stripped of omp's `π <glyph> ` prefix; empty shows "OmpDeck".
    pub title: &'a str,
    pub cwd: Option<&'a std::path::Path>,
    pub session: Option<&'a std::path::Path>,
}

/// omp's terminal title is `π <glyph> <label>` (glyph = state marker or spinner frame); returns
/// the label, or the input unchanged when it does not carry that prefix.
pub fn strip_omp_prefix(title: &str) -> &str {
    let t = title.trim();
    let Some(rest) = t.strip_prefix('π') else {
        return t;
    };
    // `π` must be followed by whitespace, else it is part of the label.
    if !rest.starts_with(char::is_whitespace) {
        return t;
    }
    match rest.trim_start().split_once(char::is_whitespace) {
        Some((_glyph, label)) => label.trim(),
        None => "",
    }
}

/// Native window title: `<tab> — OmpDeck`, plus ` — <folder>` in folder mode.
pub fn window_title(tab: Option<&str>, folder: Option<&str>) -> String {
    let mut s = match tab.filter(|t| !t.is_empty()) {
        Some(t) => format!("{t} — OmpDeck"),
        None => "OmpDeck".to_owned(),
    };
    if let Some(f) = folder {
        s.push_str(" — ");
        s.push_str(f);
    }
    s
}

/// The laid-out title, reused while nothing it depends on changes (the strip repaints often, the
/// title rarely).
#[derive(Clone)]
struct StripGalley {
    title: String,
    max_w: f32,
    ppp: f32,
    color: egui::Color32,
    font: egui::FontId,
    galley: std::sync::Arc<egui::Galley>,
}

const STRIP_FONT_SIZE: f32 = 13.5;

fn strip_galley(
    ui: &egui::Ui,
    text: &str,
    max_w: f32,
    color: egui::Color32,
) -> std::sync::Arc<egui::Galley> {
    let id = egui::Id::unique("titlestrip-galley");
    let ppp = ui.ctx().pixels_per_point();
    let font = crate::theme::font(STRIP_FONT_SIZE);
    if let Some(c) = ui.data(|d| d.get_temp::<StripGalley>(id))
        && c.title == text
        && c.max_w == max_w
        && c.ppp == ppp
        && c.color == color
        && c.font == font
    {
        return c.galley;
    }
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font.clone(), color);
    job.wrap.max_width = max_w;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    ui.data_mut(|d| {
        d.insert_temp(
            id,
            StripGalley {
                title: text.to_owned(),
                max_w,
                ppp,
                color,
                font,
                galley: galley.clone(),
            },
        );
    });
    galley
}

/// The CSD title strip: centered, ellipsized active-tab title; drags / maximizes the window.
fn title_strip(ui: &mut egui::Ui, input: &BarInput<'_>) {
    let ctx = ui.ctx().clone();
    let pal = Palette::of_ui(ui);
    egui::Panel::top("titlestrip")
        .exact_size(STRIP_H)
        .show_separator_line(false)
        .frame(Frame::NONE)
        .show(ui, |ui| {
            let rect = ui.max_rect();
            theme::paint_bar(ui.painter(), rect, pal);
            let drag = ui.interact(
                rect,
                ui.next_auto_id().with("strip-drag"),
                Sense::click_and_drag(),
            );
            if drag.drag_started_by(PointerButton::Primary) {
                ctx.send_viewport_cmd(ViewportCommand::StartDrag);
            }
            if drag.double_clicked() {
                ctx.send_viewport_cmd(ViewportCommand::Maximized(!input.maximized));
            }
            let strip = &input.strip;
            let text = if strip.title.is_empty() {
                "OmpDeck"
            } else {
                strip.title
            };

            // Left: icon + name.
            let left_rect = egui::Rect::from_min_max(
                pos2(rect.left() + 12.0, rect.top()),
                pos2(rect.left() + STRIP_SIDE_W, rect.bottom()),
            );
            ui.scope_builder(
                UiBuilder::new()
                    .max_rect(left_rect)
                    .layout(Layout::left_to_right(Align::Center)),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    logo(ui, pal, true);
                },
            );

            // Right: minimize / maximize-restore / close.
            let right_rect = egui::Rect::from_min_max(
                pos2(rect.right() - STRIP_SIDE_W, rect.top()),
                pos2(rect.right() - CONTROL_INSET, rect.bottom()),
            );
            ui.scope_builder(
                UiBuilder::new()
                    .max_rect(right_rect)
                    .layout(Layout::right_to_left(Align::Center)),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if win_button(ui, ICON_X, "Close", true).clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                    let (icon, tip) = if input.maximized {
                        (ICON_COPY, "Restore")
                    } else {
                        (ICON_SQUARE, "Maximize")
                    };
                    if win_button(ui, icon, tip, false).clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Maximized(!input.maximized));
                    }
                    if win_button(ui, ICON_MINUS, "Minimize", false).clicked() {
                        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
                    }
                },
            );

            // Center: the active tab's title, centered on the window and clear of both groups.
            let max_w = (rect.width() - 2.0 * (STRIP_SIDE_W + 8.0)).max(40.0);
            let galley = strip_galley(ui, text, max_w, pal.text_strong);
            let pos = rect.center() - galley.size() / 2.0;
            ui.painter().galley(pos, galley, pal.text_strong);
            if strip.cwd.is_some() || strip.session.is_some() {
                card::hover(
                    &drag,
                    360.0,
                    false,
                    &card::Header::new(ICON_TERMINAL, text).subtitle("Active tab"),
                    |_| {},
                    |ui| {
                        if let Some(c) = strip.cwd {
                            card::kv(ui, 76.0, "Folder", |ui| tips::path(ui, c));
                        }
                        if let Some(s) = strip.session {
                            card::kv(ui, 76.0, "Session", |ui| tips::path(ui, s));
                        }
                        card::footer_hints(
                            ui,
                            &[("drag", "move"), ("double-click", "maximize / restore")],
                        );
                    },
                );
            }
        });
}

/// Draws the bar; returns the action requested this frame.
pub fn show(ui: &mut egui::Ui, input: &BarInput<'_>) -> Option<BarAction> {
    let ctx = ui.ctx().clone();
    let pal = Palette::of_ui(ui);
    let mut action = None;
    if input.csd {
        title_strip(ui, input);
    }
    egui::Panel::top("titlebar")
        .exact_size(tabs::tab_hit_h())
        .show_separator_line(false)
        .frame(Frame::NONE)
        .show(ui, |ui| {
            let rect = ui.max_rect();
            let p = ui.painter();
            theme::paint_bar(p, rect, pal);
            if theme::is_gallery() || theme::look().separated {
                theme::skin::paint_edge(p, rect, theme::skin::Edge::Bottom, pal);
            } else {
                p.hline(
                    rect.x_range(),
                    rect.bottom() - 0.5,
                    Stroke::new(1.0, pal.stroke),
                );
            }

            // The window drags from empty bar space only (tabs and buttons win hit-testing).
            if input.csd {
                let drag = ui.interact(
                    rect,
                    ui.next_auto_id().with("bar-drag"),
                    Sense::click_and_drag(),
                );
                if drag.drag_started_by(PointerButton::Primary) {
                    ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                }
                if drag.double_clicked() {
                    ctx.send_viewport_cmd(ViewportCommand::Maximized(!input.maximized));
                }
            }

            let pill_label = input.app_update.as_ref().map(|p| p.label());
            let badge_w = input
                .omp
                .as_ref()
                .and_then(|o| o.available)
                .map_or(0.0, |(_, latest)| {
                    4.0 + badge_width(&badge_galley(ui, &format!("Update {latest}")))
                })
                + pill_label
                    .as_ref()
                    .map_or(0.0, |l| 4.0 + badge_width(&badge_galley(ui, l)));
            let right_w = 68.0
                + notify_panel::bell_width()
                + 2.0
                + badge_w
                + input
                    .pairing
                    .as_ref()
                    .map_or(0.0, |p| 4.0 + badge_width(&badge_galley(ui, &p.label)))
                + 12.0;
            let right_rect = egui::Rect::from_min_max(
                pos2(rect.right() - right_w, rect.top()),
                pos2(rect.right(), rect.bottom()),
            );
            ui.scope_builder(
                UiBuilder::new()
                    .max_rect(right_rect)
                    .layout(Layout::right_to_left(Align::Center)),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    ui.add_space(8.0);
                    let menu = icon_button(ui, ICON_MENU, "Menu", false);
                    if let Some(a) = app_menu(&menu, input) {
                        action = Some(a);
                    }
                    if icon_button(ui, ICON_SETTINGS, "Settings", input.settings_active).clicked() {
                        action = Some(BarAction::OpenSettings);
                    }
                    if let Some(a) = notify_panel::bell(ui, &input.notifications) {
                        action = Some(BarAction::Inbox(a));
                    }
                    if let Some(o) = &input.omp
                        && let Some((current, latest)) = o.available
                    {
                        ui.add_space(4.0);
                        let badge = update_badge(
                            ui,
                            &format!("Update {latest}"),
                            "omp update available",
                            (current, latest),
                        );
                        if let Some(a) = update_popover(&badge, o) {
                            action = Some(a);
                        }
                    }
                    if let Some(pill) = &input.pairing {
                        ui.add_space(4.0);
                        if pairing_pill(ui, pill).clicked() {
                            action = Some(BarAction::ShowPairing);
                        }
                    }
                    if let (Some(p), Some(label)) = (&input.app_update, &pill_label) {
                        ui.add_space(4.0);
                        let badge = update_badge(
                            ui,
                            label,
                            "OmpDeck update available",
                            (crate::app_update::CURRENT, &p.release.version),
                        );
                        if let Some(a) = theme::dropdown_menu(&badge, 300.0, |ui| {
                            crate::app_update::popover(ui, p)
                        })
                        .and_then(|r| r.inner)
                        {
                            action = Some(BarAction::AppUpdate(a));
                        }
                    }
                },
            );

            let left_rect = egui::Rect::from_min_max(
                pos2(rect.left() + 12.0, rect.top()),
                pos2(right_rect.left() - 8.0, rect.bottom()),
            );
            let bar_at =
                |x: f32| theme::bar_tint(pal, ((x - rect.left()) / rect.width()).clamp(0.0, 1.0));
            let fade = (bar_at(left_rect.left()), bar_at(left_rect.right()));
            ui.scope_builder(
                UiBuilder::new()
                    .max_rect(left_rect)
                    .layout(Layout::left_to_right(Align::Center)),
                |ui| {
                    // Whatever does not fit is clipped, never drawn over the right-hand buttons.
                    let clip = ui.clip_rect().intersect(left_rect);
                    ui.set_clip_rect(clip);
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if input.csd {
                        // Icon, name and window buttons live in the title row above.
                        ui.add_space(2.0);
                    } else {
                        logo(ui, pal, false);
                        ui.add_space(4.0);
                        let (r, _) = ui.allocate_exact_size(vec2(1.0, 20.0), Sense::hover());
                        ui.painter().rect_filled(r, 0, pal.stroke);
                        ui.add_space(2.0);
                    }
                    if let Some(a) = tabs::show_tab_strip(ui, &input.tabs, fade)
                        && action.is_none()
                    {
                        action = Some(BarAction::Tab(a));
                    }
                },
            );
        });
    action
}

/// App icon plus the "OmpDeck" wordmark; `compact` is the slim title-row variant.
fn logo(ui: &mut egui::Ui, pal: &Palette, compact: bool) {
    let (side, radius, glyph, text) = if compact {
        (22.0, 6.0, 14.0, 14.0)
    } else {
        (28.0, 8.0, 17.0, 15.0)
    };
    let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
    let p = ui.painter();
    if theme::flat() {
        p.rect(
            rect,
            radius as u8 - 1,
            pal.surface,
            Stroke::new(1.0, pal.stroke),
            StrokeKind::Inside,
        );
        paint_icon(
            p,
            rect.center(),
            egui::Align2::CENTER_CENTER,
            ICON_TERMINAL,
            glyph - 1.0,
            pal.accent,
        );
        ui.label(
            theme::strong("OmpDeck")
                .size(text - 1.0)
                .color(pal.text_strong),
        );
        return;
    }
    let glow = egui::Shadow {
        offset: [0, 2],
        blur: if compact { 10 } else { 14 },
        spread: 0,
        color: pal.accent_a.gamma_multiply(0.45),
    };
    p.add(glow.as_shape(rect, egui::CornerRadius::same(radius as u8)));
    theme::gradient_rounded(p, rect, radius, pal.accent_a, pal.accent_b);
    paint_icon(
        p,
        rect.center(),
        egui::Align2::CENTER_CENTER,
        ICON_TERMINAL,
        glyph,
        egui::Color32::WHITE,
    );
    ui.label(theme::strong("OmpDeck").size(text).color(pal.text_strong));
}

fn app_menu(button: &egui::Response, input: &BarInput<'_>) -> Option<BarAction> {
    let mut action = None;
    theme::dropdown_menu(button, 250.0, |ui| {
        ui.set_min_width(250.0);
        let pal = Palette::of_ui(ui);
        fn pick(ui: &mut egui::Ui, slot: &mut Option<BarAction>, a: BarAction) {
            *slot = Some(a);
            ui.close();
        }
        if menu_item(
            ui,
            ICON_HISTORY,
            "Reopen closed tab",
            Some("Ctrl+Shift+T"),
            input.can_reopen,
            false,
        )
        .clicked()
        {
            pick(ui, &mut action, BarAction::Reopen);
        }
        if menu_item(ui, ICON_REFRESH_CW, "Refresh sessions", None, true, false).clicked() {
            pick(ui, &mut action, BarAction::Refresh);
        }
        if menu_item(
            ui,
            crate::icons::ICON_HARD_DRIVE,
            "Session manager…",
            Some("Ctrl+Shift+S"),
            true,
            false,
        )
        .clicked()
        {
            pick(ui, &mut action, BarAction::OpenSessions);
        }
        if menu_item(
            ui,
            crate::icons::ICON_NETWORK,
            "Agents dashboard",
            Some("Ctrl+Shift+A"),
            true,
            false,
        )
        .clicked()
        {
            pick(ui, &mut action, BarAction::OpenAgents);
        }
        if let Some(o) = &input.omp {
            omp_submenu(ui, o, &mut action);
        }
        if menu_item(
            ui,
            ICON_ROCKET,
            "Check for OmpDeck updates",
            None,
            true,
            false,
        )
        .clicked()
        {
            pick(
                ui,
                &mut action,
                BarAction::AppUpdate(crate::app_update::UpdateAction::Check),
            );
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.add_space(10.0);
            ui.label(icon_text(crate::icons::ICON_A_LARGE_SMALL, 17.0, pal.glyph));
            ui.label("Font size");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(4.0);
                let small = |icon| {
                    egui::Button::new(icon_text(icon, 16.0, pal.text)).min_size(vec2(26.0, 26.0))
                };
                if ui
                    .add(small(ICON_ROTATE_CCW))
                    .on_hover_text("Reset (Ctrl+Shift+0)")
                    .clicked()
                {
                    action = Some(BarAction::FontReset);
                }
                if ui
                    .add(small(ICON_PLUS))
                    .on_hover_text("Larger (Ctrl+Shift+=)")
                    .clicked()
                {
                    action = Some(BarAction::FontLarger);
                }
                ui.label(theme::strong(format!("{:.0}", input.font_size)).color(pal.text_strong));
                if ui
                    .add(small(ICON_MINUS))
                    .on_hover_text("Smaller (Ctrl+Shift+-)")
                    .clicked()
                {
                    action = Some(BarAction::FontSmaller);
                }
            });
        });
        let (icon, label, target) = match input.theme {
            Theme::Dark => (ICON_SUN, "Light theme", Theme::Light),
            Theme::Light => (ICON_MOON, "Dark theme", Theme::Dark),
        };
        let blocked = input.dark_only.filter(|_| target == Theme::Light);
        let item = menu_item(ui, icon, label, None, blocked.is_none(), false);
        let item = if let Some(why) = blocked {
            item.on_disabled_hover_text(why)
        } else {
            item
        };
        if item.clicked() {
            pick(ui, &mut action, BarAction::SetTheme(target));
        }
        let state = if input.native_decorations {
            "On"
        } else {
            "Off"
        };
        if menu_item(
            ui,
            ICON_APP_WINDOW,
            "Native title bar",
            Some(state),
            true,
            false,
        )
        .clicked()
        {
            pick(
                ui,
                &mut action,
                BarAction::SetNativeDecorations(!input.native_decorations),
            );
        }
        ui.separator();
        if menu_item(ui, ICON_LOG_OUT, "Quit", None, true, false).clicked() {
            pick(ui, &mut action, BarAction::Quit);
        }
    });
    action
}

/// Pill-shaped accent-gradient badge announcing an available update; its hover card names the
/// installed and the available version.
fn update_badge(
    ui: &mut egui::Ui,
    label: &str,
    title: &str,
    (current, latest): (&str, &str),
) -> egui::Response {
    let resp = pill(ui, label, ICON_DOWNLOAD, None);
    card::hover(
        &resp,
        260.0,
        false,
        &card::Header::new(ICON_DOWNLOAD, title),
        |_| {},
        |ui| {
            card::kv(ui, 84.0, "Installed", |ui| {
                badge::show(ui, None, current, Hue::Amber, tips::VALUE);
            });
            card::kv(ui, 84.0, "Available", |ui| {
                badge::show(ui, Some(ICON_DOWNLOAD), latest, Hue::Green, tips::VALUE);
            });
            card::footer_hints(ui, &[("click", "update options")]);
        },
    );
    resp
}

/// A pairing the user put away (Esc / click outside the dialog) while it stays open in the
/// gateway: the dialog comes back when the pill is clicked.
pub struct PairingPill {
    /// `Pairing · 1:42`, or `Approve device` once the phone asked to pair.
    pub label: String,
    /// `(name, platform)` of the device asking to pair.
    pub device: Option<(String, String)>,
    /// Time left, `m:ss`.
    pub expires: String,
}

/// The pill of a backgrounded pairing: amber while a device waits for approval.
fn pairing_pill(ui: &mut egui::Ui, p: &PairingPill) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let approval = p.device.is_some();
    let resp = pill(
        ui,
        &p.label,
        if approval { ICON_KEY_ROUND } else { ICON_GLOBE },
        approval.then_some(pal.warn),
    );
    card::hover(
        &resp,
        260.0,
        false,
        &card::Header::new(
            if approval { ICON_KEY_ROUND } else { ICON_GLOBE },
            "Pairing in progress",
        )
        .tone(if approval {
            pal.warn
        } else {
            egui::Color32::TRANSPARENT
        }),
        |_| {},
        |ui| {
            card::kv(ui, 84.0, "State", |ui| {
                let (text, hue) = if approval {
                    ("Needs your approval", Hue::Amber)
                } else {
                    ("Waiting for the phone", Hue::Blue)
                };
                badge::show(ui, None, text, hue, tips::VALUE);
            });
            if let Some((name, platform)) = &p.device {
                card::kv(ui, 84.0, "Device", |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(name).strong().color(pal.text_strong))
                            .truncate(),
                    );
                });
                card::kv(ui, 84.0, "Platform", |ui| {
                    badge::show(ui, None, platform, Hue::Teal, tips::VALUE);
                });
            }
            card::kv(ui, 84.0, "Expires in", |ui| {
                badge::show(ui, None, &p.expires, Hue::Amber, tips::VALUE);
            });
            card::footer_hints(ui, &[("click", "show pairing")]);
        },
    );
    resp
}

/// Pill-shaped badge with a glyph and a label: accent gradient (flat themes: accent outline),
/// or `tone` instead of the accent.
fn pill(
    ui: &mut egui::Ui,
    label: &str,
    icon: crate::icons::Icon,
    tone: Option<egui::Color32>,
) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let galley = badge_galley(ui, label);
    let (rect, resp) = ui.allocate_exact_size(
        vec2(badge_width(&galley), theme::dens(24.0, 28.0)),
        Sense::click(),
    );
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.12);
    let p = ui.painter();
    let (fg, label_fg) = if theme::flat() {
        // Flat chip: the one deliberate accent in the bar.
        let accent = tone.unwrap_or(pal.accent);
        p.rect(
            rect,
            8,
            theme::mix(pal.bar, accent, 0.10 + 0.10 * hov),
            Stroke::new(1.0, accent.gamma_multiply(0.55 + 0.35 * hov)),
            StrokeKind::Inside,
        );
        (accent, accent)
    } else {
        let (a, b) = tone.map_or((pal.accent_a, pal.accent_b), |t| {
            (t, theme::mix(t, egui::Color32::BLACK, 0.25))
        });
        let glow = egui::Shadow {
            offset: [0, 2],
            blur: 12,
            spread: 0,
            color: a.gamma_multiply(0.30 + 0.25 * hov),
        };
        p.add(glow.as_shape(rect, egui::CornerRadius::same(14)));
        theme::gradient_rounded(p, rect, 14.0, a, b);
        p.rect_stroke(
            rect,
            14,
            Stroke::new(1.0, egui::Color32::WHITE.gamma_multiply(0.18 + 0.25 * hov)),
            StrokeKind::Inside,
        );
        (egui::Color32::WHITE, egui::Color32::WHITE)
    };
    paint_icon(
        p,
        pos2(rect.left() + 10.0 + 8.0, rect.center().y),
        egui::Align2::CENTER_CENTER,
        icon,
        16.0,
        fg,
    );
    p.galley(
        pos2(
            rect.left() + 10.0 + 16.0 + 6.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        label_fg,
    );
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn badge_galley(ui: &egui::Ui, label: &str) -> std::sync::Arc<egui::Galley> {
    ui.painter().layout_no_wrap(
        label.to_owned(),
        crate::theme::font(13.0),
        egui::Color32::PLACEHOLDER,
    )
}

fn badge_width(galley: &egui::Galley) -> f32 {
    10.0 + 16.0 + 6.0 + galley.size().x + 14.0
}

/// The popover under the update badge.
fn update_popover(badge: &egui::Response, o: &OmpBar<'_>) -> Option<BarAction> {
    let mut action = None;
    theme::dropdown_menu(badge, 280.0, |ui| {
        ui.set_min_width(280.0);
        let pal = Palette::of_ui(ui);
        if let Some((current, latest)) = o.available {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        let title = |t: String| theme::strong(t).size(14.5).color(pal.text_strong);
                        ui.label(title("omp".to_owned()));
                        ui.label(title(latest.to_owned()));
                        ui.label(title("is available".to_owned()));
                    });
                    ui.label(
                        RichText::new(format!("Installed: {current}"))
                            .small()
                            .color(pal.weak),
                    );
                });
            });
            ui.add_space(2.0);
            ui.separator();
        }
        update_actions(ui, o, &mut action);
    });
    action
}

/// Update / channel / check items shared by the badge popover and the app menu's omp submenu.
fn update_actions(ui: &mut egui::Ui, o: &OmpBar<'_>, action: &mut Option<BarAction>) {
    fn pick(ui: &mut egui::Ui, slot: &mut Option<BarAction>, a: BarAction) {
        *slot = Some(a);
        ui.close();
    }
    if menu_item(ui, ICON_DOWNLOAD, "Update omp", None, true, false).clicked() {
        pick(ui, action, BarAction::RunUpdate(UpdateKind::Omp));
    }
    if menu_item(ui, ICON_PUZZLE, "Update plugins", None, true, false).clicked() {
        pick(ui, action, BarAction::RunUpdate(UpdateKind::Plugins));
    }
    let kind = o.channel.switch_kind();
    let label = match o.channel {
        Channel::Stable => "Switch to canary channel…",
        Channel::Canary => "Switch to stable channel…",
    };
    if menu_item(ui, ICON_ARROW_LEFT_RIGHT, label, None, true, false).clicked() {
        pick(ui, action, BarAction::SwitchChannel(kind));
    }
    ui.separator();
    let checking = *o.state == UpdateState::Checking;
    let check = if checking {
        "Checking for updates…"
    } else {
        "Check for updates"
    };
    if menu_item(ui, ICON_REFRESH_CW, check, None, !checking, false).clicked() {
        pick(ui, action, BarAction::CheckUpdates);
    }
}

/// "omp" submenu of the app menu: version, status and the update actions.
fn omp_submenu(ui: &mut egui::Ui, o: &OmpBar<'_>, action: &mut Option<BarAction>) {
    let pal = Palette::of_ui(ui);
    ui.menu_button(
        (
            icon_text(ICON_ROCKET, 17.0, pal.glyph),
            RichText::new("omp").color(pal.text),
        ),
        |ui| {
            ui.set_min_width(250.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    let version = o
                        .version
                        .map_or_else(|| "omp".to_owned(), |v| format!("omp {v}"));
                    ui.label(theme::strong(version).color(pal.text_strong));
                    ui.label(RichText::new(status_line(o.state)).small().color(pal.weak));
                });
            });
            ui.add_space(2.0);
            ui.separator();
            update_actions(ui, o, action);
        },
    );
}

/// Window outline plus edge/corner resize zones (undecorated window, not maximized).
///
/// Each zone is its own foreground widget that only starts a resize when it owns the press, so
/// the window controls (inset by [`CONTROL_INSET`]) and any popup above it are never shadowed.
pub fn window_chrome(ctx: &Context, maximized: bool) {
    let screen = ctx.content_rect();
    if maximized {
        return;
    }
    let pal = Palette::of_ctx(ctx);
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::unique("window-outline"),
    ));
    painter.rect_stroke(screen, 0, Stroke::new(1.0, pal.stroke), StrokeKind::Inside);
    for (i, (rect, dir, cursor)) in resize_zones(screen).into_iter().enumerate() {
        egui::Area::new(egui::Id::unique(("window-resize-zone", i)))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.min)
            .constrain(false)
            .show(ctx, |ui| {
                let resp = ui.allocate_rect(rect, Sense::click_and_drag());
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(cursor);
                    if ui.input(|i| i.pointer.button_pressed(PointerButton::Primary)) {
                        ui.ctx()
                            .send_viewport_cmd(ViewportCommand::BeginResize(dir));
                    }
                }
            });
    }
}

/// The 8 resize zones of a window occupying `screen`: four corners (`CORNER`²) and the four edges
/// between them (`EDGE` thick; the north one ends left of the window controls). They never
/// overlap each other.
fn resize_zones(screen: egui::Rect) -> [(egui::Rect, ResizeDirection, CursorIcon); 8] {
    use ResizeDirection as D;
    let (l, r, t, b) = (screen.left(), screen.right(), screen.top(), screen.bottom());
    let rect =
        |x0: f32, y0: f32, x1: f32, y1: f32| egui::Rect::from_min_max(pos2(x0, y0), pos2(x1, y1));
    [
        (
            rect(l, t, l + CORNER, t + CORNER),
            D::NorthWest,
            CursorIcon::ResizeNorthWest,
        ),
        (
            rect(r - CORNER, t, r, t + CORNER),
            D::NorthEast,
            CursorIcon::ResizeNorthEast,
        ),
        (
            rect(l, b - CORNER, l + CORNER, b),
            D::SouthWest,
            CursorIcon::ResizeSouthWest,
        ),
        (
            rect(r - CORNER, b - CORNER, r, b),
            D::SouthEast,
            CursorIcon::ResizeSouthEast,
        ),
        (
            rect(l + CORNER, t, r - CONTROL_W, t + EDGE),
            D::North,
            CursorIcon::ResizeNorth,
        ),
        (
            rect(l + CORNER, b - EDGE, r - CORNER, b),
            D::South,
            CursorIcon::ResizeSouth,
        ),
        (
            rect(l, t + CORNER, l + EDGE, b - CORNER),
            D::West,
            CursorIcon::ResizeWest,
        ),
        (
            rect(r - EDGE, t + CORNER, r, b - CORNER),
            D::East,
            CursorIcon::ResizeEast,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> egui::Rect {
        egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 700.0))
    }

    /// Positive-area intersection (rectangles that merely touch do not overlap).
    fn overlap(a: egui::Rect, b: egui::Rect) -> bool {
        let i = a.intersect(b);
        i.width() > 0.0 && i.height() > 0.0
    }

    #[test]
    fn zones_do_not_overlap_each_other() {
        let zones = resize_zones(screen());
        for (i, a) in zones.iter().enumerate() {
            for b in &zones[i + 1..] {
                assert!(!overlap(a.0, b.0), "{:?} overlaps {:?}", a.1, b.1);
            }
        }
    }

    #[test]
    fn zones_leave_the_window_controls_alone() {
        // The control cluster: three 40x30 buttons centered in the strip, inset from the edge.
        let controls = egui::Rect::from_min_max(
            pos2(
                screen().right() - CONTROL_W,
                screen().top() + (STRIP_H - 30.0) / 2.0,
            ),
            pos2(
                screen().right() - CONTROL_INSET,
                screen().top() + (STRIP_H + 30.0) / 2.0,
            ),
        );
        for (rect, dir, _) in resize_zones(screen()) {
            assert!(!overlap(rect, controls), "{dir:?} covers the controls");
        }
    }

    #[test]
    fn zone_directions_match_their_position() {
        let s = screen();
        let dir_at = |p: egui::Pos2| {
            resize_zones(s)
                .into_iter()
                .find(|z| z.0.contains(p))
                .map(|z| z.1)
        };
        assert_eq!(dir_at(pos2(1.0, 1.0)), Some(ResizeDirection::NorthWest));
        assert_eq!(dir_at(pos2(999.0, 699.0)), Some(ResizeDirection::SouthEast));
        assert_eq!(dir_at(pos2(999.0, 300.0)), Some(ResizeDirection::East));
        assert_eq!(dir_at(pos2(500.0, 699.0)), Some(ResizeDirection::South));
        assert_eq!(dir_at(pos2(500.0, 300.0)), None);
    }

    #[test]
    fn omp_prefix_is_stripped_from_titles() {
        assert_eq!(strip_omp_prefix("π > my project"), "my project");
        assert_eq!(strip_omp_prefix("π ⠦ fix the bug"), "fix the bug");
        assert_eq!(strip_omp_prefix("π ! "), "");
        assert_eq!(strip_omp_prefix("π"), "π");
        assert_eq!(strip_omp_prefix("Settings"), "Settings");
        assert_eq!(strip_omp_prefix("πrate ship"), "πrate ship");
    }

    #[test]
    fn window_title_names_tab_app_and_folder() {
        assert_eq!(window_title(Some("proj"), None), "proj — OmpDeck");
        assert_eq!(
            window_title(Some("proj"), Some("work")),
            "proj — OmpDeck — work"
        );
        assert_eq!(window_title(None, None), "OmpDeck");
        assert_eq!(window_title(Some(""), Some("work")), "OmpDeck — work");
    }

    /// A tab on a connection that stays silent until the terminal is dropped (no process, no
    /// PTY).
    fn test_tab(ctx: &Context, uid: u64, title: &str) -> tabs::Tab {
        use deck_service::controller::{ServiceEpoch, SessionKey, SessionTarget};
        use deck_term::{Remote, RemoteEvent, RemoteRx, RemoteTx, TermOptions, Terminal};
        use std::io;
        use std::sync::mpsc;

        struct Silent(mpsc::Sender<()>, mpsc::Receiver<()>);
        struct SilentRx(mpsc::Receiver<()>);
        struct SilentTx(#[allow(dead_code)] mpsc::Sender<()>);
        impl Remote for Silent {
            fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn RemoteTx>) {
                (Box::new(SilentRx(self.1)), Box::new(SilentTx(self.0)))
            }
        }
        impl RemoteRx for SilentRx {
            fn recv(&mut self) -> io::Result<RemoteEvent> {
                // The sender lives in the tx half: this unblocks when the terminal is dropped.
                let _ = self.0.recv();
                Ok(RemoteEvent::Closed)
            }
        }
        impl RemoteTx for SilentTx {
            fn input(&self, _: &[u8]) -> io::Result<()> {
                Ok(())
            }
            fn resize(&self, _: u16, _: u16, _: u16, _: u16) -> io::Result<()> {
                Ok(())
            }
            fn palette(&self, _: [[u8; 3]; 19]) -> io::Result<()> {
                Ok(())
            }
            fn kill(&self) -> io::Result<()> {
                Ok(())
            }
            fn detach(&self) -> io::Result<()> {
                Ok(())
            }
        }

        let (tx, rx) = mpsc::channel();
        let term = Terminal::connect(
            Box::new(Silent(tx, rx)),
            TermOptions::default(),
            ctx.clone(),
        );
        let target = SessionTarget {
            epoch: ServiceEpoch([0; 16]),
            key: SessionKey::tmux("test", title),
            generation: 1,
        };
        tabs::Tab::new(
            term,
            target,
            uid,
            tabs::TabLaunch {
                mux_name: None,
                cwd: std::path::PathBuf::from("/p"),
                session_path: None,
                title: title.to_owned(),
                role: tabs::TabRole::Session,
                view: Default::default(),
            },
        )
    }

    /// Every text and glyph of the tab row (logo, tab icons and titles, ✕, the update badge, the
    /// bell, settings and menu buttons) is vertically centred on the row, in every density.
    fn assert_row_centered(style: deck_core::config::UiStyle, density: deck_core::config::Density) {
        use std::collections::{HashMap, HashSet};
        let ctx = Context::default();
        crate::ui_fonts::install(&ctx, crate::ui_fonts::UiFonts::none());
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        crate::ui_fonts::finalize(&ctx);
        let font = deck_core::config::UiFont::Auto;
        let compact = density == deck_core::config::Density::Compact;
        let tabs = [test_tab(&ctx, 1, "alpha"), test_tab(&ctx, 2, "beta")];
        let labels = HashMap::new();
        let busy = HashSet::new();
        let finishing = std::collections::HashMap::new();
        let parkable = HashSet::new();
        let state = UpdateState::default();
        let input = BarInput {
            tabs: TabBarInput {
                tabs: &tabs,
                labels: &labels,
                active: 0,
                scroll_to_active: false,
                menu: crate::sidebar::MenuEnv {
                    groups: &[],
                    omp_available: true,
                    can_stop: true,
                    default_view: deck_core::config::SessionView::Terminal,
                    host_available: true,
                    tabs: &[],
                    busy: &busy,
                    backup: crate::sidebar::BackupMenu {
                        place: "Google Drive",
                        off: None,
                    },
                    live_tabs: &[],
                    finishing: &finishing,
                    parked: crate::worktree_ui::ParkView {
                        records: &[],
                        states: &[],
                        parkable: &parkable,
                    },
                },
                modal_open: false,
                shown: &[],
                dashboard: false,
            },
            csd: false,
            maximized: false,
            settings_active: true,
            can_reopen: false,
            font_size: 13.0,
            theme: Theme::Dark,
            dark_only: None,
            native_decorations: true,
            strip: StripInput {
                title: "",
                cwd: None,
                session: None,
            },
            omp: Some(OmpBar {
                version: Some("1.0.0"),
                state: &state,
                available: Some(("1.0.0", "1.1.0")),
                channel: Channel::Stable,
            }),
            app_update: None,
            pairing: None,
            notifications: notify_panel::PanelInput {
                entries: &[],
                badge: None,
                warn: false,
                now: std::time::SystemTime::UNIX_EPOCH,
            },
        };
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                pos2(0.0, 0.0),
                vec2(1200.0, 600.0),
            )),
            ..Default::default()
        };
        // The style is process-global and other tests switch it too: a frame counts only when
        // the style and density were ours from before the first pass to after the last, with no
        // other `apply` in between (one that switched density and back would go unnoticed).
        let ours = || theme::style() == style && theme::compact() == compact;
        for _ in 0..50 {
            theme::apply(&ctx, style, density, font, true);
            let applied = theme::applied();
            let row_mid = tabs::tab_hit_h() / 2.0;
            if !ours() {
                continue;
            }
            let mut shapes = Vec::new();
            for _ in 0..3 {
                let mut out = ctx.run_ui(raw.clone(), |ui| {
                    show(ui, &input);
                });
                out.textures_delta.clear();
                shapes = out.shapes;
            }
            if !ours() || theme::applied() != applied {
                continue;
            }
            let mut texts = 0;
            for cs in &shapes {
                if let egui::Shape::Text(t) = &cs.shape {
                    let y = t.galley.rect.translate(t.pos.to_vec2()).center().y;
                    let off = y - row_mid;
                    assert!(
                        off.abs() <= 0.5,
                        "{style:?}/{density:?}: {:?} off by {off}",
                        t.galley.text()
                    );
                    texts += 1;
                }
            }
            // Logo glyph + name, 2 × (icon, title), the active ✕, badge icon + label, bell,
            // gear, menu.
            assert!(
                texts >= 11,
                "{style:?}/{density:?}: row painted {texts} texts"
            );
            return;
        }
        panic!("{style:?}/{density:?}: the global style kept changing under the test");
    }

    #[test]
    fn tab_row_content_is_vertically_centered() {
        use deck_core::config::{Density, UiStyle};
        for style in UiStyle::ALL {
            for density in [Density::Compact, Density::Comfortable] {
                assert_row_centered(style, density);
            }
        }
    }
}
