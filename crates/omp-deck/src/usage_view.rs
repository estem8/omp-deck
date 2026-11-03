//! The Usage view (omp's `/usage`, natively): a modal with an Overview (stats strip, quota cards,
//! activity heatmap), a Details tab (per-account limit tables, top models) and an Accounts tab
//! (the order omp uses a provider's accounts in, and the focused tab's pin), plus the compact
//! quota cards the status-bar chip shows on hover.
//!
//! Everything painted here is pre-formatted by [`crate::usage_state`]; this module only lays out.

use std::collections::HashMap;
use std::sync::Arc;

use deck_core::ctl::LiveAccount;
use deck_core::usage::{AccountPolicy, Heatmap, LimitStatus, PolicyState};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{
    Align, Align2, Color32, CursorIcon, Frame, Galley, Id, Key, Layout, Margin, Modifiers, Order,
    Painter, Pos2, Rect, RichText, Sense, Sides, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::icons::{
    ICON_ACTIVITY, ICON_ARROW_DOWN, ICON_ARROW_UP, ICON_CHEVRON_LEFT, ICON_CHEVRON_RIGHT,
    ICON_CIRCLE_ALERT, ICON_GAUGE, ICON_KEY_ROUND, ICON_LIST_TREE, ICON_LOCK, ICON_REFRESH_CW,
    ICON_SPARKLES, ICON_TERMINAL, ICON_X, Icon,
};
use crate::theme::{self, Palette};
use crate::usage_fmt::{long_date, money, requests};
use crate::usage_state::{
    AccountGroup, AccountRow, ActivityView, CARD_ROWS, CardView, DetailBlock, DraftRow, KeyGroup,
    Load, ModelRow, NoDataWarning, QuotaRow, Summary, Tile, account_policies, merged_draft,
};
use crate::widgets::{DraftButton, DraftKind, Spin, empty_state, icon_text, paint_icon, stepper};

const GRID_GAP: f32 = 2.5;
const DAY_LETTERS: [&str; 7] = ["M", "T", "W", "T", "F", "S", "S"];

/// Which page of the view is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Overview,
    Details,
    Accounts,
    /// omp's `/usage reset`: the banked rate-limit resets.
    Resets,
}

#[derive(Default)]
pub struct ViewState {
    pub page: Page,
    /// Unsaved Accounts-page edits per provider, top first.
    pub drafts: HashMap<String, Vec<DraftRow>>,
    /// The provider whose rules are being written.
    pub saving: Option<String>,
    /// A saved provider's draft stays up until a summary other than this one (by address) arrives,
    /// so the old order does not flash back while omp's usage is re-read.
    pub saved: Option<(String, usize)>,
    /// A pin/unpin is waiting for the omp session.
    pub pinning: bool,
    /// The Saved resets page: what omp lists, and a spend in flight.
    pub resets: crate::usage_resets::ResetsState,
}

impl ViewState {
    /// `provider`'s rules were written while `summary` was showing.
    pub fn saved(&mut self, provider: String, summary: Option<&Arc<Summary>>) {
        match summary {
            Some(s) => self.saved = Some((provider, Arc::as_ptr(s) as usize)),
            None => {
                self.drafts.remove(&provider);
            }
        }
    }
}

/// What the view needs from the app.
pub struct ViewInput<'a> {
    pub load: Load,
    pub summary: Option<&'a Arc<Summary>>,
    pub activity: Option<&'a Arc<ActivityView>>,
    pub error: Option<&'a str>,
    pub activity_error: Option<&'a str>,
    pub activity_loading: bool,
    pub fetching: bool,
    pub age_secs: Option<u64>,
    /// The OAuth account of the focused tab's omp session (live control), with what it can be
    /// pinned to.
    pub session: Option<&'a LiveAccount>,
    /// omp's `retry.usageReservePct`: the reserve of accounts without their own.
    pub global_reserve: u32,
}

pub enum UsageAction {
    Close,
    Refresh,
    /// Run omp's official installer (omp was not found).
    InstallOmp,
    /// Replace `provider`'s `auth.accountPolicies` entries (empty: omp's automatic order).
    SaveAccounts {
        provider: String,
        name: String,
        policies: Vec<AccountPolicy>,
    },
    /// Pin the focused tab's session to a stored account of `provider` (`None`: unpin).
    Pin {
        provider: String,
        credential: Option<String>,
        label: Option<String>,
    },
    /// Read the saved resets of every stored account.
    LoadResets,
    /// Spend one saved reset of the stored account `credential` of `provider` (the app asks
    /// `question` first).
    SpendReset {
        provider: String,
        credential: String,
        question: String,
    },
}

/// Colour of a limit status; `Ok` is the accent (teal in Neutral).
pub fn status_color(pal: &Palette, s: LimitStatus) -> Color32 {
    match s {
        LimitStatus::Ok => pal.accent,
        LimitStatus::Warning => pal.warn,
        LimitStatus::Exhausted => pal.danger,
        LimitStatus::Unknown => pal.weak,
    }
}

fn track(pal: &Palette) -> Color32 {
    theme::mix(pal.surface, pal.weak, 0.28)
}

// ------------------------------------------------------------------ window

pub fn show(ctx: &egui::Context, st: &mut ViewState, input: &ViewInput<'_>) -> Vec<UsageAction> {
    let mut out = Vec::new();
    let pal = Palette::of_ctx(ctx);
    let screen = ctx.content_rect();

    egui::Area::new(Id::unique("usage-scrim"))
        .order(Order::Background)
        .fixed_pos(screen.min)
        .interactable(true)
        .show(ctx, |ui| {
            let (rect, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
            let dark = ctx.global_style().visuals.dark_mode;
            ui.painter().rect_filled(rect, 0, theme::backdrop(dark));
            if resp.clicked() {
                out.push(UsageAction::Close);
            }
        });

    if !egui::Popup::is_any_open(ctx)
        && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
    {
        out.push(UsageAction::Close);
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::R)) && !input.fetching {
        out.push(UsageAction::Refresh);
    }

    let size = vec2(
        (screen.width() * 0.62)
            .clamp(660.0, 1000.0)
            .min(screen.width() - 24.0),
        (screen.height() * 0.86)
            .clamp(440.0, 900.0)
            .min(screen.height() - 24.0),
    );
    let margin = Margin::symmetric(20, 16);
    let frame = Frame::NONE.inner_margin(margin);
    egui::Window::new("usage-window")
        .id(Id::unique("usage-window"))
        .title_bar(false)
        .frame(frame)
        .resizable(true)
        .movable(true)
        .collapsible(false)
        .constrain_to(screen)
        .min_size(vec2(600.0, 380.0))
        .default_size(size)
        .default_pos(screen.center() - size / 2.0)
        .show(ctx, |ui| {
            crate::card::chrome(ui, margin, pal.bar, |ui| {
                body(ui, st, input, &mut out);
            });
        });
    out
}

fn body(ui: &mut Ui, st: &mut ViewState, input: &ViewInput<'_>, out: &mut Vec<UsageAction>) {
    header(ui, input, out);
    ui.add_space(10.0);
    tabs(ui, st);
    ui.add_space(12.0);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            // Keep the scrollbar off the content.
            ui.set_width(ui.available_width() - 6.0);
            match st.page {
                Page::Overview => overview(ui, input, out),
                Page::Details => details(ui, input, out),
                Page::Accounts => accounts(ui, st, input, out),
                Page::Resets => resets(ui, st, input, out),
            }
        });
}

fn header(ui: &mut Ui, input: &ViewInput<'_>, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    Sides::new().height(32.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            crate::card::icon_tile(ui, 30.0, ICON_GAUGE, Color32::TRANSPARENT, true);
            ui.label(theme::strong("Usage").size(19.0).color(pal.text_strong));
            let when = crate::usage_fmt::header_when(
                input.fetching,
                input.age_secs,
                input.summary.is_some_and(|s| s.warning.is_some()),
            );
            if !when.is_empty() {
                ui.label(RichText::new(when).size(13.0).color(pal.weak));
            }
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if crate::card::close_button_sized(ui, 30.0, 18.0).clicked() {
                out.push(UsageAction::Close);
            }
            if input.fetching {
                let (rect, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
                Spin::new(17.0, pal.accent).paint_at(ui, rect.center());
            } else if ui
                .add_sized(
                    [30.0, 30.0],
                    egui::Button::new(icon_text(ICON_REFRESH_CW, 18.0, pal.weak)).frame(false),
                )
                .on_hover_text("Refresh now (R)")
                .on_hover_cursor(CursorIcon::PointingHand)
                .clicked()
            {
                out.push(UsageAction::Refresh);
            }
        },
    );
    let (left, right, y) = (
        ui.min_rect().left(),
        ui.min_rect().left() + ui.available_width(),
        ui.min_rect().bottom() + 3.0,
    );
    crate::card::header_rule(ui, left, right, y, Color32::TRANSPARENT);
}

fn tabs(ui: &mut Ui, st: &mut ViewState) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for (page, label) in [
            (Page::Overview, "Overview"),
            (Page::Details, "Details"),
            (Page::Accounts, "Accounts"),
            (Page::Resets, "Saved resets"),
        ] {
            if tab_chip(ui, label, st.page == page).clicked() {
                st.page = page;
            }
        }
    });
}

pub(crate) fn tab_chip(ui: &mut Ui, label: &str, on: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let g = ui
        .painter()
        .layout_no_wrap(label.to_owned(), theme::font(13.0), pal.text);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 28.0, 28.0), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
    let p = ui.painter();
    let r = theme::rad(8, 14);
    if on {
        p.rect_filled(
            rect,
            r,
            if theme::flat() {
                pal.raised
            } else {
                pal.accent.gamma_multiply(0.22)
            },
        );
        p.rect_stroke(
            rect,
            r,
            Stroke::new(1.0, pal.accent.gamma_multiply(0.7)),
            StrokeKind::Inside,
        );
    } else {
        p.rect_filled(rect, r, pal.raised.gamma_multiply(0.7 * hov));
        p.rect_stroke(
            rect,
            r,
            Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
            StrokeKind::Inside,
        );
    }
    let color = if on {
        pal.text_strong
    } else {
        theme::mix(pal.weak, pal.text_strong, hov)
    };
    p.galley(rect.center() - g.size() / 2.0, g, color);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

// ------------------------------------------------------------------ overview

fn overview(ui: &mut Ui, input: &ViewInput<'_>, out: &mut Vec<UsageAction>) {
    stats_strip(ui, input);
    ui.add_space(6.0);
    section(ui, "Quotas");
    quotas(ui, input, out);
    ui.add_space(6.0);
    section(ui, "Activity");
    activity(ui, input);
}

fn section(ui: &mut Ui, title: &str) {
    let pal = Palette::of_ui(ui);
    ui.label(theme::section_caption(title, pal.weak));
}

fn pick<'a>(t: Option<&'a Tile>, none: &'a str) -> (&'a str, &'a str) {
    t.map_or(("—", none), |t| (t.value.as_str(), t.sub.as_str()))
}

fn card_frame(pal: &Palette) -> Frame {
    Frame::NONE
        .fill(if theme::flat() {
            pal.panel
        } else {
            pal.surface.gamma_multiply(0.75)
        })
        .stroke(Stroke::new(1.0, pal.stroke))
        .corner_radius(theme::rad(8, 14))
        .inner_margin(Margin::symmetric(14, 11))
}

fn stats_strip(ui: &mut Ui, input: &ViewInput<'_>) {
    let pal = Palette::of_ui(ui);
    ui.columns(4, |cols| {
        // Tightest quota.
        let tight = input.summary.and_then(|s| s.chip.as_ref());
        tile(
            &mut cols[0],
            "Tightest quota",
            tight.map_or("—", |c| c.text.as_str()),
            tight.map_or(pal.weak, |c| status_color(pal, c.status)),
            tight.map_or("No quota data", |c| c.tight.as_str()),
            None,
        );
        let act = input.activity;
        let none = if input.activity_loading {
            "Reading activity…"
        } else {
            "No activity recorded"
        };
        let (v, s) = pick(act.filter(|a| !a.empty).map(|a| &a.past_year), none);
        tile(&mut cols[1], "Past year", v, pal.text_strong, s, None);
        let (v, s) = pick(act.filter(|a| !a.empty).map(|a| &a.last30), none);
        tile(
            &mut cols[2],
            "Last 30 days",
            v,
            pal.text_strong,
            s,
            act.filter(|a| !a.empty)
                .map(|a| a.heatmap.last30.daily.as_slice()),
        );
        let (v, s) = pick(
            act.filter(|a| !a.empty).and_then(|a| a.busiest.as_ref()),
            none,
        );
        tile(&mut cols[3], "Busiest day", v, pal.text_strong, s, None);
    });
}

fn tile(
    ui: &mut Ui,
    caption: &str,
    value: &str,
    value_color: Color32,
    sub: &str,
    spark: Option<&[f64]>,
) {
    let pal = Palette::of_ui(ui);
    card_frame(pal)
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.label(theme::section_caption(caption, pal.weak));
            ui.horizontal(|ui| {
                ui.add(
                    egui::Label::new(theme::strong(value).size(21.0).color(value_color))
                        .selectable(false),
                );
                if let Some(d) = spark {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        sparkline(ui, d, vec2(ui.available_width().min(64.0), 22.0));
                    });
                }
            });
            ui.add(
                egui::Label::new(RichText::new(sub).small().color(pal.weak))
                    .truncate()
                    .selectable(false),
            );
        });
}

fn sparkline(ui: &mut Ui, data: &[f64], size: egui::Vec2) {
    let pal = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let max = data.iter().copied().fold(0.0_f64, f64::max);
    if data.len() < 2 || max <= 0.0 {
        let y = rect.bottom() - 1.0;
        ui.painter().hline(
            rect.x_range(),
            y,
            Stroke::new(1.0, pal.stroke.gamma_multiply(1.4)),
        );
        return;
    }
    let n = (data.len() - 1) as f32;
    let pts: Vec<Pos2> = data
        .iter()
        .enumerate()
        .map(|(i, v)| {
            pos2(
                rect.left() + rect.width() * i as f32 / n,
                rect.bottom() - 2.0 - (rect.height() - 4.0) * (*v / max) as f32,
            )
        })
        .collect();
    let line = if theme::flat() { pal.glyph } else { pal.accent };
    ui.painter()
        .add(egui::Shape::line(pts, Stroke::new(1.5, line)));
}

// ------------------------------------------------------------------ quotas

fn quotas(ui: &mut Ui, input: &ViewInput<'_>, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    match input.load {
        Load::OmpMissing => {
            empty_state(
                ui,
                ICON_TERMINAL,
                pal.warn,
                "omp was not found",
                "OmpDeck reads quotas with `omp usage`. Install omp, or put it on PATH and restart OmpDeck.",
            );
            ui.add_space(12.0);
            if ui
                .vertical_centered(crate::widgets::install_omp_button)
                .inner
            {
                out.push(UsageAction::InstallOmp);
            }
            return;
        }
        Load::Loading => {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
                Spin::new(22.0, pal.accent).paint_at(ui, rect.center());
                ui.label(RichText::new("Reading quotas from omp…").color(pal.weak));
            });
            ui.add_space(16.0);
            return;
        }
        Load::Failed => {
            error_state(ui, input.error.unwrap_or("unknown error"), out);
            return;
        }
        Load::Ready => {}
    }
    let Some(summary) = input.summary else {
        return;
    };
    if let Some(e) = input.error {
        error_strip(ui, e, out);
    }
    if let Some(w) = &summary.warning {
        warning_state(ui, w, out);
        return;
    }
    if summary.cards.is_empty() {
        empty_state(
            ui,
            ICON_KEY_ROUND,
            pal.accent,
            "No accounts with usage data",
            "Log in to a provider with `omp /login` — quotas for Anthropic, Codex, Copilot and others appear here.",
        );
        for u in &summary.orphans {
            unavailable_row(ui, u);
        }
        return;
    }
    let wide =
        ui.available_width() >= 640.0 && summary.cards.iter().filter(|c| !c.idle).count() > 1;
    let active = summary.cards.iter().filter(|c| !c.idle);
    if wide {
        ui.columns(2, |cols| {
            let mut heights = [0.0_f32; 2];
            for c in active {
                let col = usize::from(heights[1] < heights[0]);
                heights[col] += c.weight;
                provider_card(&mut cols[col], c, false, None);
            }
        });
    } else {
        for c in active {
            provider_card(ui, c, false, None);
        }
    }
    if !summary.untouched.is_empty() {
        ui.label(RichText::new(&summary.untouched).small().color(pal.weak));
    }
    for u in &summary.orphans {
        unavailable_row(ui, u);
    }
}
fn warning_state(ui: &mut Ui, w: &NoDataWarning, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    empty_state(
        ui,
        ICON_CIRCLE_ALERT,
        pal.warn,
        &w.title,
        "Every provider fetch failed or is backing off, so there are no quotas to show.",
    );
    for a in &w.accounts {
        unavailable_row(ui, a);
    }
    ui.add_space(4.0);
    ui.vertical_centered(|ui| {
        if DraftButton::new("Retry").show(ui).clicked() {
            out.push(UsageAction::Refresh);
        }
    });
}

fn error_state(ui: &mut Ui, msg: &str, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    empty_state(
        ui,
        ICON_CIRCLE_ALERT,
        pal.danger,
        "Could not read usage",
        msg,
    );
    ui.vertical_centered(|ui| {
        if DraftButton::new("Retry").show(ui).clicked() {
            out.push(UsageAction::Refresh);
        }
    });
}

fn error_strip(ui: &mut Ui, msg: &str, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    Frame::NONE
        .fill(theme::mix(pal.bar, pal.warn, 0.10))
        .stroke(theme::status_stroke(pal, pal.warn, 0.35))
        .corner_radius(theme::rad(8, 12))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(icon_text(ICON_CIRCLE_ALERT, 15.0, pal.warn));
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("Refresh failed — showing the last data. {msg}"))
                            .small()
                            .color(pal.text),
                    )
                    .truncate(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if DraftButton::new("Retry").min_width(72.0).show(ui).clicked() {
                        out.push(UsageAction::Refresh);
                    }
                });
            });
        });
}

fn unavailable_row(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(icon_text(ICON_CIRCLE_ALERT, 13.0, pal.weak));
        ui.add(egui::Label::new(RichText::new(text).small().color(pal.weak)).truncate());
    });
}

/// One provider: status dot + name, then up to four window rows and the extras. A provider with
/// several accounts shows one account at a time: the full card pages through them, the compact
/// one shows [`shown_page`] for `account`.
pub fn provider_card(ui: &mut Ui, c: &CardView, compact: bool, account: Option<&LiveAccount>) {
    let pal = Palette::of_ui(ui);
    let frame = if compact {
        Frame::NONE
    } else {
        card_frame(pal)
    };
    let (index, in_use) = shown_page(ui.ctx(), c, account);
    let Some(page) = c.pages.get(index) else {
        return;
    };
    let paged = c.pages.len() > 1;
    let mut turn_to: Option<usize> = None;
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = if compact { 3.0 } else { 5.0 };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            let (rect, _) = ui.allocate_exact_size(vec2(10.0, 16.0), Sense::hover());
            ui.painter()
                .circle_filled(rect.center(), 4.0, status_color(pal, page.status));
            ui.label(theme::strong(&c.name).size(14.5).color(pal.text_strong));
            if let Some(a) = &c.accounts {
                ui.label(RichText::new(a).small().color(pal.weak));
            }
            if paged && !compact {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let last = c.pages.len() - 1;
                    if page_button(ui, ICON_CHEVRON_RIGHT, "Next account", index < last).clicked() {
                        turn_to = Some(index + 1);
                    }
                    ui.label(
                        RichText::new(format!("{} / {}", index + 1, c.pages.len()))
                            .size(11.5)
                            .color(pal.weak),
                    );
                    if page_button(ui, ICON_CHEVRON_LEFT, "Previous account", index > 0).clicked() {
                        turn_to = Some(index - 1);
                    }
                });
            }
        });
        if paged {
            let who = match (compact, in_use) {
                (_, true) if account.is_some_and(|a| a.pinned) => {
                    format!("{} · pinned", page.account)
                }
                (_, true) => format!("{} · in use", page.account),
                (true, false) => format!("{} · {} / {}", page.account, index + 1, c.pages.len()),
                (false, false) => page.account.clone(),
            };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(icon_text(ICON_KEY_ROUND, 12.0, pal.weak));
                ui.add(egui::Label::new(RichText::new(who).small().color(pal.text)).truncate());
            });
        }
        if page.unlimited {
            ui.label(RichText::new("Unlimited").small().color(pal.weak));
        }
        for w in page.rows.iter().take(CARD_ROWS) {
            window_row(ui, w, compact);
        }
        let more = if compact {
            &page.more_compact
        } else {
            &page.more_full
        };
        if let Some(text) = more {
            ui.label(RichText::new(text).small().color(pal.weak));
        }
        if let Some(r) = &page.reset_credits {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(icon_text(ICON_SPARKLES, 13.0, pal.accent));
                ui.add(egui::Label::new(RichText::new(r).small().color(pal.text)).truncate());
            });
        }
        for a in &c.unavailable {
            unavailable_row(ui, a);
        }
        if !compact {
            for a in &page.daybreak {
                ui.label(RichText::new(a).small().color(pal.weak));
            }
            for n in &page.notes {
                ui.add(egui::Label::new(RichText::new(n).small().color(pal.weak)).truncate());
            }
        }
    });
    if let Some(p) = turn_to.and_then(|i| c.pages.get(i)) {
        let account = p.account.clone();
        ui.ctx()
            .data_mut(|d| d.insert_temp(picked_page(&c.provider), account));
    }
}

/// Memory key of the account last picked in a card's pager (its label, so a reorder keeps it).
fn picked_page(provider: &str) -> Id {
    Id::unique(("usage-account", provider))
}

/// The page a card shows in the status bar: that of `account` (the OAuth account the focused omp
/// session uses) when one matches, else the one last picked in the pager, else the first; and
/// whether it is the session's.
pub fn shown_page(
    ctx: &egui::Context,
    c: &CardView,
    account: Option<&LiveAccount>,
) -> (usize, bool) {
    if let Some(i) = c.in_use(account) {
        return (i, true);
    }
    let picked = ctx.data(|d| d.get_temp::<String>(picked_page(&c.provider)));
    (c.page_index(picked.as_deref()), false)
}

/// A small previous/next chevron of a card's account pager.
fn page_button(ui: &mut Ui, icon: Icon, tip: &str, enabled: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 20.0), sense);
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered() && enabled, 0.1);
    ui.painter()
        .rect_filled(rect, 6, pal.raised.gamma_multiply(0.9 * hov));
    paint_icon(
        ui.painter(),
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        15.0,
        if enabled {
            theme::mix(pal.weak, pal.text_strong, hov)
        } else {
            pal.weak.gamma_multiply(0.35)
        },
    );
    resp.on_hover_text(tip).on_hover_cursor(if enabled {
        CursorIcon::PointingHand
    } else {
        CursorIcon::Default
    })
}

/// `label  [5h]  ......  63% left` over a remaining-bar with `resets in …` on its right.
fn window_row(ui: &mut Ui, w: &QuotaRow, compact: bool) {
    let pal = Palette::of_ui(ui);
    let color = status_color(pal, w.status);
    ui.spacing_mut().item_spacing.y = 3.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add(
            egui::Label::new(RichText::new(&w.label).color(pal.text))
                .truncate()
                .selectable(false),
        );
        if let Some(tag) = &w.tag {
            ui.label(RichText::new(tag).small().color(pal.weak));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if let Some(v) = &w.value {
                if w.measured {
                    ui.label(theme::strong(v).color(color));
                } else {
                    ui.label(RichText::new(v).color(pal.weak));
                }
            }
        });
    });
    if let Some(left) = w.remaining {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let reset_w = if compact { 92.0 } else { 104.0 };
            let bar_w = if w.reset.is_some() {
                (ui.available_width() - reset_w - 10.0).max(40.0)
            } else {
                ui.available_width()
            };
            remaining_bar(ui, bar_w, left, color);
            if let Some(r) = &w.reset {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(r).small().color(pal.weak));
                });
            }
        });
    }
}

fn remaining_bar(ui: &mut Ui, width: f32, left: f32, color: Color32) {
    let pal = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 3, track(pal));
    if left > 0.0 {
        let w = (rect.width() * left).max(4.0).min(rect.width());
        p.rect_filled(
            Rect::from_min_size(rect.min, vec2(w, rect.height())),
            3,
            color,
        );
    }
}

// ------------------------------------------------------------------ chip hover

/// The compact hover card of the status-bar chip, shown while `resp` is hovered. `account`: the
/// OAuth account the focused tab's omp session uses, whose page a multi-account card shows.
pub fn hover(resp: &egui::Response, summary: &Summary, account: Option<&LiveAccount>) {
    crate::card::hover(
        resp,
        312.0,
        false,
        &crate::card::Header::new(ICON_GAUGE, "Usage"),
        |ui| {
            crate::card::kbd(ui, "Ctrl+Shift+U");
        },
        |ui| {
            let pal = Palette::of_ui(ui);
            ui.spacing_mut().item_spacing.y = 6.0;
            let mut first = true;
            for c in summary.cards.iter().filter(|c| !c.idle) {
                if !first {
                    let (rect, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                    ui.painter().rect_filled(rect, 0, pal.stroke);
                }
                first = false;
                provider_card(ui, c, true, account);
            }
            if first {
                ui.label(RichText::new("Nothing used yet").small().color(pal.weak));
            }
            if !summary.untouched.is_empty() {
                ui.label(RichText::new(&summary.untouched).small().color(pal.weak));
            }
            crate::card::footer_hints(ui, &[("click", "full Usage view")]);
        },
    );
}

// ------------------------------------------------------------------ heatmap

fn level_color(pal: &Palette, level: u8) -> Color32 {
    if level == 0 {
        return theme::mix(
            pal.surface,
            pal.weak,
            if pal.bg.r() < 128 { 0.20 } else { 0.28 },
        );
    }
    let t = f32::from(level.min(4)) / 4.0;
    let top = if theme::flat() {
        pal.accent
    } else {
        theme::mix(pal.accent_a, pal.accent_b, t)
    };
    theme::mix(theme::mix(pal.surface, top, 0.22), top, t)
}

fn activity(ui: &mut Ui, input: &ViewInput<'_>) {
    let pal = Palette::of_ui(ui);
    if let Some(e) = input.activity_error {
        ui.label(
            RichText::new(format!("Could not read omp's activity data: {e}"))
                .small()
                .color(pal.danger),
        );
        return;
    }
    let Some(a) = input.activity else {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
            Spin::new(16.0, pal.accent).paint_at(ui, rect.center());
            ui.label(RichText::new("Reading activity…").small().color(pal.weak));
        });
        return;
    };
    if a.empty {
        card_frame(pal).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(icon_text(ICON_ACTIVITY, 16.0, pal.weak));
            ui.label(
                RichText::new(
                    "No activity yet — omp records your requests in stats.db as you work.",
                )
                .color(pal.weak),
            );
        });
        return;
    }
    card_frame(pal).show(ui, |ui| {
        ui.set_width(ui.available_width());
        heatmap(ui, &a.heatmap);
    });
}

fn heatmap(ui: &mut Ui, hm: &Heatmap) {
    let pal = Palette::of_ui(ui);
    let label_w = 14.0;
    let avail = ui.available_width();
    let cs = ((avail - label_w - 6.0 - GRID_GAP * (hm.weeks as f32 - 1.0)) / hm.weeks as f32)
        .clamp(7.0, 16.0);
    let step = cs + GRID_GAP;
    let top = 15.0;
    let size = vec2(label_w + 6.0 + step * hm.weeks as f32, top + step * 7.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let origin = pos2(rect.left() + label_w + 6.0, rect.top() + top);
    let hover = resp.hover_pos().and_then(|p| {
        let (dx, dy) = (p.x - origin.x, p.y - origin.y);
        if dx < 0.0 || dy < 0.0 {
            return None;
        }
        let (week, row) = ((dx / step) as usize, (dy / step) as usize);
        (week < hm.weeks && row < 7 && hm.cells[row][week].is_some()).then_some((row, week))
    });
    let p = ui.painter();
    let small = theme::font(10.5);
    for (row, letter) in DAY_LETTERS.iter().enumerate() {
        p.text(
            pos2(rect.left(), origin.y + step * row as f32 + cs / 2.0),
            Align2::LEFT_CENTER,
            *letter,
            small.clone(),
            pal.weak.gamma_multiply(0.8),
        );
    }
    let mut last_label_x = f32::NEG_INFINITY;
    for week in 0..hm.weeks {
        let x = origin.x + step * week as f32;
        if let Some(Some(name)) = hm.month_labels.get(week)
            && x - last_label_x >= 26.0
        {
            p.text(
                pos2(x, rect.top() + 1.0),
                Align2::LEFT_TOP,
                *name,
                small.clone(),
                pal.weak,
            );
            last_label_x = x;
        }
        for row in 0..7 {
            let Some(level) = hm.cells[row][week] else {
                continue;
            };
            let r = Rect::from_min_size(pos2(x, origin.y + step * row as f32), vec2(cs, cs));
            p.rect_filled(r, (cs * 0.22).round() as u8, level_color(pal, level));
        }
    }
    if let Some((row, week)) = hover {
        let r = Rect::from_min_size(
            pos2(origin.x + step * week as f32, origin.y + step * row as f32),
            vec2(cs, cs),
        );
        p.rect_stroke(
            r.expand(1.0),
            (cs * 0.3).round() as u8,
            Stroke::new(1.5, pal.text_strong),
            StrokeKind::Outside,
        );
        cell_tip(&resp, hm, row, week);
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        ui.label(
            RichText::new(crate::usage_fmt::streak_text(
                hm.active_days,
                hm.longest_streak,
            ))
            .small()
            .color(pal.weak),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new("More").small().color(pal.weak));
            for level in (0..=4).rev() {
                let (r, _) = ui.allocate_exact_size(vec2(11.0, 11.0), Sense::hover());
                ui.painter().rect_filled(r, 2, level_color(pal, level));
            }
            ui.label(RichText::new("Less").small().color(pal.weak));
        });
    });
}

fn cell_tip(resp: &egui::Response, hm: &Heatmap, row: usize, week: usize) {
    use crate::badge::{self, Hue};
    use crate::card::{self, Header};
    const KEY_W: f32 = 84.0;
    let t = hm.totals[row][week];
    let date = long_date(hm.date_at(row, week));
    let head = Header::new(ICON_ACTIVITY, &date);
    card::hover(
        resp,
        260.0,
        true,
        &head,
        |ui| {
            if t.requests == 0 && t.cost <= 0.0 {
                badge::show(ui, None, "Idle", Hue::Blue, 12.0);
            }
        },
        |ui| {
            let pal = Palette::of_ui(ui);
            if t.requests == 0 && t.cost <= 0.0 {
                ui.label(RichText::new("No activity").color(pal.weak));
                return;
            }
            card::kv(ui, KEY_W, "Cost", |ui| {
                badge::show(ui, None, &money(t.cost), Hue::Amber, 12.0);
            });
            card::kv(ui, KEY_W, "Requests", |ui| {
                ui.label(RichText::new(requests(t.requests)).color(pal.text_strong));
            });
            card::kv(ui, KEY_W, "Tokens", |ui| {
                badge::show(
                    ui,
                    None,
                    &crate::statusbar::group_digits(t.tokens),
                    Hue::Teal,
                    12.0,
                );
            });
        },
    );
}

// ------------------------------------------------------------------ saved resets

/// omp's `/usage reset`: the saved rate-limit resets of the stored accounts, each with a Drafted
/// Spend button (the app asks first). The list is read when the page first shows (the providers
/// are asked, which takes seconds).
fn resets(ui: &mut Ui, st: &mut ViewState, input: &ViewInput<'_>, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    if !st.resets.requested {
        out.push(UsageAction::LoadResets);
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::Label::new(
                RichText::new(
                    "Rate-limit resets your Codex and Claude plans have banked. Spending one \
                     clears the limit it covers right away.",
                )
                .small()
                .color(pal.weak),
            )
            .wrap(),
        );
    });
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if st.resets.loading {
            ui.add(Spin::new(15.0, pal.accent));
            ui.label(
                RichText::new("Asking the providers…")
                    .small()
                    .color(pal.weak),
            );
        } else if DraftButton::new("Reload")
            .icon(ICON_REFRESH_CW)
            .min_width(96.0)
            .show(ui)
            .on_hover_text("Ask the providers again")
            .clicked()
        {
            out.push(UsageAction::LoadResets);
        }
    });
    if let Some(e) = &st.resets.error {
        Frame::NONE
            .fill(theme::mix(pal.bar, pal.danger, 0.10))
            .stroke(theme::status_stroke(pal, pal.danger, 0.35))
            .corner_radius(theme::rad(8, 12))
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(icon_text(ICON_CIRCLE_ALERT, 15.0, pal.danger));
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("Could not read the saved resets. {e}"))
                                .small()
                                .color(pal.text),
                        )
                        .wrap(),
                    );
                });
            });
    }
    let Some(list) = &st.resets.accounts else {
        return;
    };
    let rows = crate::usage_resets::rows(list, input.session);
    if rows.is_empty() {
        empty_state(
            ui,
            ICON_KEY_ROUND,
            pal.accent,
            "No accounts",
            crate::usage_resets::NO_ACCOUNTS,
        );
        return;
    }
    if let Some(msg) = crate::usage_resets::empty_message(list) {
        Frame::NONE
            .fill(theme::mix(pal.bar, pal.accent, 0.08))
            .stroke(Stroke::new(1.0, pal.stroke))
            .corner_radius(theme::rad(8, 12))
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(icon_text(crate::icons::ICON_INFO, 15.0, pal.accent));
                    ui.add(egui::Label::new(RichText::new(msg).color(pal.text)).wrap());
                });
            });
    }
    let busy = st.resets.spending.is_some();
    for r in &rows {
        reset_card(
            ui,
            r,
            busy,
            st.resets.is_spending(&r.provider, &r.credential),
            out,
        );
    }
}

fn reset_card(
    ui: &mut Ui,
    r: &crate::usage_resets::AccountRow,
    busy: bool,
    spending: bool,
    out: &mut Vec<UsageAction>,
) {
    let pal = Palette::of_ui(ui);
    card_frame(pal).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            chip(ui, &r.provider_label, pal.accent);
            ui.add(
                egui::Label::new(theme::strong(&r.label).size(14.5).color(pal.text_strong))
                    .truncate(),
            );
            if r.in_use {
                chip(ui, "In use", pal.ok);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if spending {
                    ui.add(Spin::new(17.0, pal.accent));
                    return;
                }
                let tip = if !r.spendable {
                    "This account has no saved reset"
                } else if busy {
                    "A reset is being spent"
                } else {
                    "Spend one saved reset (asks first)"
                };
                if DraftButton::new("Spend 1 reset")
                    .kind(DraftKind::Primary)
                    .enabled(r.spendable && !busy)
                    .min_width(130.0)
                    .show(ui)
                    .on_hover_text(tip)
                    .on_disabled_hover_text(tip)
                    .clicked()
                {
                    out.push(UsageAction::SpendReset {
                        provider: r.provider.clone(),
                        credential: r.credential.clone(),
                        question: r.spend_question(),
                    });
                }
            });
        });
        crate::card::kv(ui, 130.0, "Saved", |ui| {
            ui.label(theme::strong(r.available.to_string()).color(pal.text_strong));
        });
        crate::card::kv(ui, 130.0, "Redeemable now", |ui| {
            let color = if r.redeemable > 0 { pal.ok } else { pal.weak };
            ui.label(theme::strong(r.redeemable.to_string()).color(color));
        });
        if let Some(d) = &r.soonest {
            crate::card::kv(ui, 130.0, "Soonest expiry", |ui| {
                ui.label(theme::strong(d).color(pal.text_strong));
            });
        }
        if let Some(why) = &r.reason {
            crate::card::kv(ui, 130.0, "Unavailable", |ui| {
                ui.add(egui::Label::new(RichText::new(why).color(pal.warn)).wrap());
            });
        }
        if let Some(e) = &r.error {
            crate::card::kv(ui, 130.0, "Error", |ui| {
                ui.add(egui::Label::new(RichText::new(e).color(pal.danger)).wrap());
            });
        }
    });
}

/// A small bordered tag.
fn chip(ui: &mut Ui, text: &str, color: Color32) {
    Frame::NONE
        .fill(color.gamma_multiply(0.16))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
        .corner_radius(theme::rad(4, 9))
        .inner_margin(Margin::symmetric(7, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().strong().color(color));
        });
}

// ------------------------------------------------------------------ accounts

const ROUTING_NOTE: &str = "omp picks an account for each session. Accounts that are \
rate-limited, used up, inside their reserve or past 85 % of their 5-hour window step aside first; \
among the rest, the higher one here goes first. A session keeps its account until that reaches \
its reserve, or a higher account has more than its reclaim level left. A pinned tab stays on its \
account until that is rate-limited.";

const KEYS_NOTE: &str = "omp chooses among API keys by itself on every request: used-up or \
rate-limited keys are skipped and, where omp measures quotas, the key with more room left goes \
first. Account order and pinning only work for OAuth logins.";

fn accounts(ui: &mut Ui, st: &mut ViewState, input: &ViewInput<'_>, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    match input.load {
        Load::OmpMissing | Load::Loading | Load::Failed => {
            quotas(ui, input, out);
            return;
        }
        Load::Ready => {}
    }
    let Some(summary) = input.summary else {
        return;
    };
    if let Some((provider, at)) = &st.saved
        && *at != Arc::as_ptr(summary) as usize
    {
        st.drafts.remove(provider);
        st.saved = None;
    }
    if let Some(e) = input.error {
        error_strip(ui, e, out);
    }
    // The routing note is about OAuth accounts; API-key groups carry their own.
    if !summary.accounts.is_empty() || summary.keys.is_empty() {
        ui.add(egui::Label::new(RichText::new(ROUTING_NOTE).small().color(pal.weak)).wrap());
    }
    if summary.accounts.is_empty() && summary.keys.is_empty() {
        empty_state(
            ui,
            ICON_KEY_ROUND,
            pal.accent,
            "One account per provider",
            "Account order and pinning apply once a provider has two or more OAuth logins. Add another with `omp login`.",
        );
        return;
    }
    for g in &summary.accounts {
        account_group(ui, st, input, g, out);
    }
    for g in &summary.keys {
        key_group(ui, g);
    }
}

/// One provider's API keys: listed with their quotas, without order or pin (omp offers neither
/// for API keys).
fn key_group(ui: &mut Ui, g: &KeyGroup) {
    let pal = Palette::of_ui(ui);
    card_frame(pal).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(theme::strong(&g.name).size(14.5).color(pal.text_strong));
            ui.label(
                RichText::new(format!("{} API keys · omp picks", g.rows.len()))
                    .small()
                    .color(pal.weak),
            );
        });
        ui.add(egui::Label::new(RichText::new(KEYS_NOTE).small().color(pal.weak)).wrap());
        for (i, row) in g.rows.iter().enumerate() {
            ui.push_id(("key-row", &row.label, i), |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(icon_text(ICON_KEY_ROUND, 12.0, pal.weak));
                    ui.add(
                        egui::Label::new(RichText::new(&row.label).color(pal.text_strong))
                            .truncate(),
                    );
                    if let Some(n) = &row.note {
                        ui.add(
                            egui::Label::new(RichText::new(n).small().color(pal.weak)).truncate(),
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        for p in row.pills.iter().rev() {
                            ui.label(
                                RichText::new(format!("{} {}", p.tag, p.text))
                                    .small()
                                    .color(status_color(pal, p.status)),
                            );
                        }
                    });
                });
            });
        }
    });
}

/// One provider: its accounts top first, each movable, with its reserve and reclaim levels and,
/// for the focused tab's provider, the pin.
fn account_group(
    ui: &mut Ui,
    st: &mut ViewState,
    input: &ViewInput<'_>,
    g: &AccountGroup,
    out: &mut Vec<UsageAction>,
) {
    let pal = Palette::of_ui(ui);
    let mut rows = merged_draft(g, st.drafts.get(&g.provider).map(Vec::as_slice));
    let original: Vec<DraftRow> = g.rows.iter().map(DraftRow::of).collect();
    let saving = st.saving.as_deref() == Some(g.provider.as_str());
    let session = input.session.filter(|s| s.provider == g.provider);
    let pin_busy = input.pin_busy(st);
    let mut discard = false;
    card_frame(pal).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(theme::strong(&g.name).size(14.5).color(pal.text_strong));
            let mode = if g.has_policies {
                "your order"
            } else {
                "omp's automatic order"
            };
            ui.label(RichText::new(mode).small().color(pal.weak));
            if g.has_policies {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let reset = ui
                        .add_enabled(
                            !saving,
                            egui::Button::new(RichText::new("Reset to automatic").small()),
                        )
                        .on_hover_text(
                            "Remove the rules for this provider from omp's config: omp orders its accounts by itself again",
                        );
                    if reset.clicked() {
                        out.push(UsageAction::SaveAccounts {
                            provider: g.provider.clone(),
                            name: g.name.clone(),
                            policies: Vec::new(),
                        });
                    }
                });
            }
        });
        if let Some(s) = session {
            let shown = g
                .rows
                .iter()
                .find(|r| r.identity.same_account(&s.identity))
                .map_or_else(
                    || s.identity.email.clone().unwrap_or_default(),
                    |r| r.label.clone(),
                );
            let how = if s.pinned {
                "pinned: it stays there until that account is rate-limited"
            } else {
                "chosen by omp"
            };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(icon_text(ICON_TERMINAL, 12.0, pal.weak));
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("Focused tab: {shown} · {how}"))
                            .small()
                            .color(pal.text),
                    )
                    .truncate(),
                );
            });
        }
        let n = rows.len();
        let mut swap: Option<usize> = None;
        for (i, draft) in rows.iter_mut().enumerate() {
            let Some(row) = g.rows.iter().find(|r| r.identity == draft.identity) else {
                continue;
            };
            ui.push_id(("account-row", &row.label, i), |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if page_button(ui, ICON_ARROW_UP, "Move up", i > 0 && !saving).clicked() {
                        swap = Some(i - 1);
                    }
                    if page_button(ui, ICON_ARROW_DOWN, "Move down", i + 1 < n && !saving).clicked()
                    {
                        swap = Some(i);
                    }
                    account_label(ui, i, row);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if let Some(s) = session {
                            pin_cell(ui, s, row, pin_busy, out);
                        }
                        for p in row.pills.iter().rev() {
                            ui.label(
                                RichText::new(format!("{} {}", p.tag, p.text))
                                    .small()
                                    .color(status_color(pal, p.status)),
                            );
                        }
                    });
                });
                if g.usage_capable {
                    levels(ui, draft, input.global_reserve, !saving);
                }
            });
        }
        if let Some(i) = swap {
            rows.swap(i, i + 1);
        }
        let dirty = rows != original;
        if dirty || saving {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let unkeyed = g.rows.iter().filter(|r| r.key.is_none()).count();
                if unkeyed > 0 {
                    ui.label(icon_text(ICON_CIRCLE_ALERT, 13.0, pal.warn));
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                "{unkeyed} account{} cannot be told apart and stay{} automatic, below the rest",
                                if unkeyed == 1 { "" } else { "s" },
                                if unkeyed == 1 { "s" } else { "" },
                            ))
                            .small()
                            .color(pal.weak),
                        )
                        .truncate(),
                    );
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if saving {
                        let (rect, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                        Spin::new(16.0, pal.accent).paint_at(ui, rect.center());
                        ui.label(RichText::new("Saving…").small().color(pal.weak));
                        return;
                    }
                    let save = DraftButton::new("Save order")
                        .kind(DraftKind::Primary)
                        .show(ui)
                        .on_hover_text("Write these rules to auth.accountPolicies in omp's config");
                    if save.clicked() {
                        out.push(UsageAction::SaveAccounts {
                            provider: g.provider.clone(),
                            name: g.name.clone(),
                            policies: account_policies(g, &rows, input.global_reserve),
                        });
                    }
                    if DraftButton::new("Discard").show(ui).clicked() {
                        discard = true;
                    }
                });
            });
        }
    });
    if discard || rows == original {
        st.drafts.remove(&g.provider);
    } else {
        st.drafts.insert(g.provider.clone(), rows);
    }
}

impl ViewInput<'_> {
    /// No pin can be sent: the focused tab has no live session, or a pin is in flight.
    fn pin_busy(&self, st: &ViewState) -> bool {
        st.pinning || self.session.is_none()
    }
}

/// `2.  me@example.test` with the account's state under omp's rules and why it has no usage.
fn account_label(ui: &mut Ui, i: usize, row: &AccountRow) {
    let pal = Palette::of_ui(ui);
    ui.label(RichText::new(format!("{}.", i + 1)).color(pal.weak));
    let label = ui.add(
        egui::Label::new(RichText::new(&row.label).color(if row.key.is_some() {
            pal.text_strong
        } else {
            pal.weak
        }))
        .truncate(),
    );
    if row.key.is_none() {
        label.on_hover_text(
            "Another account has the same identity, so no rule can single this one out: omp keeps ranking it by itself, below the accounts with rules.",
        );
    }
    let badge = match row.state {
        Some(PolicyState::InsideReserve) => Some(("inside reserve", pal.warn)),
        Some(PolicyState::Exhausted) => Some(("used up", pal.danger)),
        _ => None,
    };
    if let Some((text, color)) = badge {
        ui.label(RichText::new(text).small().color(color));
    }
    if let Some(n) = &row.note {
        ui.add(egui::Label::new(RichText::new(n).small().color(pal.weak)).truncate());
    }
}

/// "this tab" / Pin / Unpin for the focused tab's session.
fn pin_cell(
    ui: &mut Ui,
    s: &LiveAccount,
    row: &AccountRow,
    busy: bool,
    out: &mut Vec<UsageAction>,
) {
    let pal = Palette::of_ui(ui);
    let in_use = row.identity.same_account(&s.identity);
    let small = |t: &str| egui::Button::new(RichText::new(t).small());
    if in_use && s.pinned {
        let unpin = ui
            .add_enabled(!busy, small("Unpin"))
            .on_hover_text("Let omp choose this tab's account again");
        if unpin.clicked() {
            out.push(UsageAction::Pin {
                provider: s.provider.clone(),
                credential: None,
                label: None,
            });
        }
    } else if let Some(c) = s
        .credentials
        .iter()
        .find(|c| c.identity.same_account(&row.identity))
    {
        let pin = ui
            .add_enabled(!busy, small("Pin"))
            .on_hover_text("Keep the focused tab on this account (omp's /session pin)");
        if pin.clicked() {
            out.push(UsageAction::Pin {
                provider: s.provider.clone(),
                credential: Some(c.id.clone()),
                label: Some(row.label.clone()),
            });
        }
    }
    if in_use {
        let text = RichText::new("this tab").small().color(pal.accent);
        if s.pinned {
            ui.label(icon_text(ICON_LOCK, 12.0, pal.accent))
                .on_hover_text("Pinned");
        }
        ui.label(text);
    }
}

/// `Reserve [10] %  ·  Reclaim above [60] %` for one account.
fn levels(ui: &mut Ui, row: &mut DraftRow, global: u32, enabled: bool) {
    let pal = Palette::of_ui(ui);
    let caption = |ui: &mut Ui, t: &str, tip: &str| {
        ui.label(RichText::new(t).small().color(pal.weak))
            .on_hover_text(tip);
    };
    let clear = |ui: &mut Ui, tip: &str| {
        ui.add_enabled(
            enabled,
            egui::Button::new(icon_text(ICON_X, 12.0, pal.weak)).frame(false),
        )
        .on_hover_text(tip)
        .clicked()
    };
    ui.horizontal(|ui| {
        ui.add_enabled_ui(enabled, |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            // Line the controls up under the account name, past the two arrows.
            ui.add_space(54.0);
            caption(
                ui,
                "Reserve",
                "Below this much left the account steps behind the others",
            );
            let reserve = row.reserve.unwrap_or(global);
            if let Some(v) = stepper(ui, "reserve", reserve as f32, (0.0, 99.0), 5.0, "%") {
                row.reserve = Some(v as u32);
            }
            if row.reserve.is_some() {
                if clear(ui, &format!("Use the default reserve ({global} %)")) {
                    row.reserve = None;
                }
            } else {
                caption(
                    ui,
                    "default",
                    "omp's retry.usageReservePct, shared by accounts without their own",
                );
            }
            ui.add_space(12.0);
            caption(
                ui,
                "Reclaim above",
                "Sessions on a lower account move back here once this much is left",
            );
            let floor = row.reserve.unwrap_or(global);
            match row.reclaim {
                Some(r) => {
                    let lo = (floor + 1).min(100) as f32;
                    if let Some(v) = stepper(ui, "reclaim", r as f32, (lo, 100.0), 5.0, "%") {
                        row.reclaim = Some(v as u32);
                    }
                    if clear(ui, "No reclaim: sessions stay where they are") {
                        row.reclaim = None;
                    }
                }
                None if floor < 100 => {
                    let set = ui
                        .add(egui::Button::new(RichText::new("off").small()))
                        .on_hover_text("Set a reclaim level");
                    if set.clicked() {
                        row.reclaim = Some((floor + 1).max(50));
                    }
                }
                None => {}
            }
        });
    });
    // omp refuses a reclaim at or under the reserve.
    let floor = row.reserve.unwrap_or(global);
    if row.reclaim.is_some_and(|r| r <= floor) {
        row.reclaim = (floor < 100).then_some(floor + 1);
    }
}

// ------------------------------------------------------------------ details

fn details(ui: &mut Ui, input: &ViewInput<'_>, out: &mut Vec<UsageAction>) {
    let pal = Palette::of_ui(ui);
    match input.load {
        Load::OmpMissing | Load::Loading | Load::Failed => {
            quotas(ui, input, out);
            return;
        }
        Load::Ready => {}
    }
    let Some(summary) = input.summary else {
        return;
    };
    if let Some(e) = input.error {
        error_strip(ui, e, out);
    }
    if let Some(w) = &summary.warning {
        warning_state(ui, w, out);
    } else if summary.details.is_empty() {
        empty_state(
            ui,
            ICON_LIST_TREE,
            pal.accent,
            "No limits reported",
            "No logged-in provider reported any usage limits.",
        );
    }
    for b in &summary.details {
        detail_block(ui, b);
    }
    ui.add_space(6.0);
    section(ui, "Top models · last 30 days");
    match input.activity {
        Some(a) if !a.models.is_empty() => models_table(ui, &a.models),
        Some(_) => {
            ui.label(
                RichText::new("No model usage in the last 30 days.")
                    .small()
                    .color(pal.weak),
            );
        }
        None => {
            ui.label(
                RichText::new(if input.activity_loading {
                    "Reading activity…"
                } else {
                    "Activity data is unavailable."
                })
                .small()
                .color(pal.weak),
            );
        }
    }
}

const ROW_H: f32 = 26.0;

fn elided(p: &Painter, text: &str, size: f32, color: Color32, width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(
        text.to_owned(),
        TextFormat {
            font_id: theme::font(size),
            color,
            ..Default::default()
        },
    );
    job.wrap = TextWrapping::truncate_at_width(width.max(8.0));
    p.layout_job(job)
}

fn cell(p: &Painter, text: &str, color: Color32, x: f32, w: f32, mid_y: f32, right: bool) {
    let g = elided(p, text, 13.0, color, w);
    let px = if right { x + w - g.size().x } else { x };
    p.galley(pos2(px, mid_y - g.size().y / 2.0), g, color);
}

fn detail_block(ui: &mut Ui, b: &DetailBlock) {
    let pal = Palette::of_ui(ui);
    card_frame(pal).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal(|ui| {
            ui.label(theme::strong(&b.name).size(14.5).color(pal.text_strong));
            if !b.meta.is_empty() {
                ui.label(RichText::new(&b.meta).small().color(pal.weak));
            }
        });
        ui.add_space(4.0);
        let w = ui.available_width();
        let cols = [w * 0.34, w * 0.27, w * 0.19, w * 0.20];
        let (head, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
        let p = ui.painter();
        let mut x = head.left();
        for (i, title) in ["LIMIT", "ACCOUNT", "LEFT", "RESETS"].iter().enumerate() {
            p.text(
                pos2(x, head.center().y),
                Align2::LEFT_CENTER,
                *title,
                theme::font(10.5),
                pal.weak,
            );
            x += cols[i];
        }
        p.hline(head.x_range(), head.bottom(), Stroke::new(1.0, pal.stroke));
        for r in &b.rows {
            let (row, _) = ui.allocate_exact_size(vec2(w, ROW_H), Sense::hover());
            let p = ui.painter();
            let y = row.center().y;
            let mut x = row.left();
            cell(p, &r.limit, pal.text, x, cols[0] - 10.0, y, false);
            x += cols[0];
            cell(p, &r.account, pal.weak, x, cols[1] - 10.0, y, false);
            x += cols[1];
            let color = status_color(pal, r.status);
            if let Some(f) = r.left_frac {
                let bar = Rect::from_min_size(pos2(x, y - 3.0), vec2(36.0, 6.0));
                p.rect_filled(bar, 3, track(pal));
                if f > 0.0 {
                    p.rect_filled(
                        Rect::from_min_size(bar.min, vec2((bar.width() * f).max(3.0), 6.0)),
                        3,
                        color,
                    );
                }
                cell(p, &r.left, color, x + 44.0, cols[2] - 54.0, y, false);
            } else {
                cell(p, &r.left, pal.text, x, cols[2] - 10.0, y, false);
            }
            x += cols[2];
            cell(p, &r.resets, pal.weak, x, cols[3], y, false);
        }
    });
}

fn models_table(ui: &mut Ui, rows: &[ModelRow]) {
    let pal = Palette::of_ui(ui);
    card_frame(pal).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        let w = ui.available_width();
        let cols = [w * 0.36, w * 0.18, w * 0.14, w * 0.16, w * 0.16];
        let (head, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
        let p = ui.painter();
        let mut x = head.left();
        for (i, title) in ["MODEL", "PROVIDER", "COST", "REQUESTS", "TOKENS"]
            .iter()
            .enumerate()
        {
            let right = i >= 2;
            p.text(
                pos2(if right { x + cols[i] - 10.0 } else { x }, head.center().y),
                if right {
                    Align2::RIGHT_CENTER
                } else {
                    Align2::LEFT_CENTER
                },
                *title,
                theme::font(10.5),
                pal.weak,
            );
            x += cols[i];
        }
        p.hline(head.x_range(), head.bottom(), Stroke::new(1.0, pal.stroke));
        for r in rows.iter().take(12) {
            let (row, _) = ui.allocate_exact_size(vec2(w, ROW_H + 3.0), Sense::hover());
            let p = ui.painter();
            let y = row.center().y - 1.5;
            let mut x = row.left();
            cell(p, &r.model, pal.text, x, cols[0] - 10.0, y, false);
            x += cols[0];
            cell(p, &r.provider, pal.weak, x, cols[1] - 10.0, y, false);
            x += cols[1];
            cell(p, &r.cost, pal.text_strong, x, cols[2] - 10.0, y, true);
            x += cols[2];
            cell(p, &r.requests, pal.text, x, cols[3] - 10.0, y, true);
            x += cols[3];
            cell(p, &r.tokens, pal.weak, x, cols[4] - 10.0, y, true);
            // Share of the busiest model, drawn under the model name.
            let track = Rect::from_min_size(
                pos2(row.left(), row.bottom() - 5.0),
                vec2(cols[0] - 10.0, 3.0),
            );
            p.rect_filled(track, 1.5, pal.surface);
            let bar = Rect::from_min_size(
                track.min,
                vec2(track.width() * r.share.clamp(0.0, 1.0), 3.0),
            );
            p.rect_filled(
                bar,
                1,
                if theme::flat() {
                    pal.glyph.gamma_multiply(0.45)
                } else {
                    pal.accent.gamma_multiply(0.55)
                },
            );
        }
    });
}
