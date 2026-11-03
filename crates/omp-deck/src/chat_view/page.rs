//! Pages: the `screen` surface omp opens in the session's place while one of its fullscreen
//! overlays is up (`/git`, the agents hub, `/models`, …; omp's `native/backend.ts` pauses the
//! inline surface meanwhile). omp keeps the keyboard focus in that overlay, so a view that kept
//! drawing the frozen session would look dead while every key and click went to something
//! invisible. The page is drawn from the same node renderers as the session; what omp could not
//! describe arrives as `rows` (ANSI lines) and is drawn too.
//!
//! A strip along the top is OmpDeck's own and always there: it names the page and has "Back to
//! chat", which sends omp an Esc, so a page nothing here can draw is still dismissible.

use egui::{Align, Id, Layout, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2};

use super::ctx::Cx;
use super::dock::{self, Tone};
use super::glyphs;
use super::paint;
use super::render::{self, blank};
use super::snap::SNode;

/// Height of the strip along the top of a page (px).
const STRIP_H: f32 = 36.0;
/// Space between a page's body and the pane's edges (px).
const PAD: f32 = 14.0;
/// Space between a page's panes (px).
const GAP: f32 = 12.0;

/// Width the page's body gets in a pane `rect` wide (what omp's columns are measured from).
pub fn body_width(rect: Rect) -> f32 {
    (rect.width() - 2.0 * PAD).max(220.0)
}

/// Draws the page into `rect`: the strip, then the page's `main` and `dock`.
pub fn show(cx: &Cx, ui: &mut Ui, rect: Rect, id: Id) {
    let strip = Rect::from_min_size(rect.min, vec2(rect.width(), STRIP_H));
    strip_ui(cx, ui, strip, id);
    let body = Rect::from_min_max(
        pos2(rect.left() + PAD, strip.bottom() + 4.0),
        pos2(rect.right() - PAD, rect.bottom() - PAD),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(id.with("page-body"))
            .max_rect(body)
            .layout(Layout::top_down(Align::Min)),
    );
    child.set_clip_rect(body.intersect(ui.clip_rect()));
    child.style_mut().interaction.selectable_labels = false;
    let tx = super::ctx::Tx {
        font: cx.ink.prose(13.5),
        color: cx.ink.text(),
        line: None,
    };
    cx.with_tx(tx, || {
        let Some(main) = cx.snap.region("main") else {
            return;
        };
        // omp's own layouts fill the body and scroll their panes; anything else is a column that
        // scrolls as a whole.
        if cx.snap.kids(main).any(lays_out_itself) {
            render::children(cx, &mut child, main);
        } else {
            egui::ScrollArea::vertical()
                .id_salt(id.with("page-scroll"))
                .auto_shrink([false, false])
                .show(&mut child, |ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    render::children(cx, ui, main);
                    if let Some(dock) = cx.snap.region("dock") {
                        render::children(cx, ui, dock);
                    }
                });
        }
    });
}

/// Whether `n` is a page omp laid out for the whole body (its panes scroll on their own).
fn lays_out_itself(n: &SNode) -> bool {
    n.role_under("omp.app.git")
}

/// What the strip calls the page: `omp.app.<name>` of its first node, else plain "omp".
fn title(cx: &Cx) -> String {
    let name = cx.snap.region("main").and_then(|m| {
        cx.snap.kids(m).find_map(|c| {
            c.role()
                .strip_prefix("omp.app.")
                .and_then(|r| r.split('.').next())
                .filter(|r| !r.is_empty())
        })
    });
    match name {
        Some(n) => {
            let mut cs = n.chars();
            let first: String = cs.next().into_iter().flat_map(char::to_uppercase).collect();
            format!("{first}{}", cs.as_str())
        }
        None => "omp".to_owned(),
    }
}

fn strip_ui(cx: &Cx, ui: &mut Ui, strip: Rect, id: Id) {
    let ink = cx.ink;
    let p = ui.painter();
    p.line_segment(
        [strip.left_bottom(), strip.right_bottom()],
        Stroke::new(1.0, ink.line()),
    );
    let mid = strip.center().y;
    glyphs::paint(
        p,
        "terminal",
        pos2(strip.left() + PAD + 7.0, mid),
        13.0,
        ink.dim(),
    );
    let g = p.layout_no_wrap(title(cx), ink.label(12.5), ink.strong());
    p.galley(
        pos2(strip.left() + PAD + 22.0, mid - g.size().y / 2.0),
        g,
        ink.strong(),
    );
    let mut right = ui.new_child(
        UiBuilder::new()
            .id_salt(id.with("page-strip"))
            .max_rect(strip.shrink2(vec2(PAD, 0.0)))
            .layout(Layout::right_to_left(Align::Center)),
    );
    let back = dock::button(
        cx,
        &mut right,
        id.with("page-back"),
        "Back to chat",
        Some("arrow-left"),
        Tone::Neutral,
    )
    .on_hover_text("Close this page (Esc)");
    if back.clicked() {
        cx.dismiss();
    }
}

/// Nodes of a page with a look of their own; `true` when handled.
pub fn role_node(cx: &Cx, ui: &mut Ui, n: &SNode) -> bool {
    if n.kind == "button" {
        button(cx, ui, n);
        return true;
    }
    match n.role() {
        "omp.app.git" if n.kind == "row" => split(cx, ui, n),
        _ => return false,
    }
    true
}

/// omp's `button`: icon and/or text, `actions.click`, tooltip from `title`/`aria`. `variant`
/// `quiet` draws no box at rest; a role ending in `.set` marks a toggle that is on.
fn button(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let label = super::text::prop(n, "text").unwrap_or_default();
    let icon = n.str("icon");
    let on = n.role().ends_with(".set") || n.flag("on") || n.flag("pressed");
    let disabled = n.flag("disabled");
    let font = ink.label(12.0);
    let color = if disabled {
        ink.faint()
    } else if on {
        ink.accent_text()
    } else {
        ink.text()
    };
    let g = (!label.is_empty()).then(|| ui.painter().layout_no_wrap(label, font, color));
    let icon_w = if icon.is_some() { 14.0 } else { 0.0 };
    let gap = if icon.is_some() && g.is_some() {
        6.0
    } else {
        0.0
    };
    let w = icon_w + gap + g.as_ref().map_or(0.0, |g| g.size().x) + 16.0;
    let size = vec2(w.max(26.0), 26.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let id = ui.scope_id().with(("page-button", n.id.as_str()));
    let resp = ui.interact(
        rect,
        id,
        if disabled {
            Sense::hover()
        } else {
            Sense::click()
        },
    );
    let hov = ui
        .ctx()
        .animate_bool_with_time(id.with("h"), resp.hovered() && !disabled, 0.08);
    let quiet = n.str("variant") == Some("quiet") && !on;
    let p = ui.painter();
    if on {
        paint::boxed(
            p,
            rect,
            5,
            ink.wash(ink.p.accent),
            Stroke::new(1.0, ink.edge(ink.p.accent)),
        );
    } else if !quiet {
        paint::boxed(p, rect, 5, ink.chip(), Stroke::new(1.0, ink.p.stroke));
    } else if hov > 0.0 {
        p.rect_filled(rect, 5.0, ink.p.hover_bg.gamma_multiply(hov));
    }
    paint::pop(p, rect, 5, ink, hov);
    let total = icon_w + gap + g.as_ref().map_or(0.0, |g| g.size().x);
    let mut x = rect.center().x - total / 2.0;
    if let Some(ic) = icon {
        glyphs::paint(p, ic, pos2(x + icon_w / 2.0, rect.center().y), 13.0, color);
        x += icon_w + gap;
    }
    if let Some(g) = g {
        let at = pos2(x, rect.center().y - g.size().y / 2.0);
        p.galley(at, g, color);
    }
    let tip = n.str("title").or_else(|| n.str("aria"));
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let resp = match tip {
        Some(t) => resp.on_hover_text(super::text::clean(t).into_owned()),
        None => resp,
    };
    if resp.clicked()
        && !disabled
        && let Some(act) = n.click()
    {
        cx.click(n, act);
    }
}

/// omp's git page: the diff (`main`) beside the changes and the commit form (`side`), each pane
/// a framed card that scrolls on its own. A first child that is a bar stays above the scroll.
fn split(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let kids: Vec<&SNode> = cx.snap.kids(n).filter(|c| !blank(cx, c)).collect();
    let rect = ui.available_rect_before_wrap();
    if kids.len() < 2 || !rect.height().is_finite() || rect.width() < 520.0 {
        render::children(cx, ui, n);
        return;
    }
    let side_w = (rect.width() * 0.30).clamp(280.0, 420.0);
    let main = Rect::from_min_max(rect.min, pos2(rect.right() - side_w - GAP, rect.bottom()));
    let side = Rect::from_min_max(pos2(rect.right() - side_w, rect.top()), rect.max);
    pane(cx, ui, main, kids[0]);
    pane(cx, ui, side, kids[1]);
    ui.allocate_rect(rect, Sense::hover());
}

fn pane(cx: &Cx, ui: &mut Ui, rect: Rect, n: &SNode) {
    let ink = cx.ink;
    paint::boxed(
        ui.painter(),
        rect,
        8,
        ink.card(),
        Stroke::new(1.0, ink.line()),
    );
    let inner = rect.shrink(10.0);
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(("page-pane", &n.id))
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
    );
    child.set_clip_rect(rect.shrink(1.0).intersect(ui.clip_rect()));
    child.spacing_mut().item_spacing.y = 8.0;
    let kids: Vec<&SNode> = cx.snap.kids(n).collect();
    // A bar stays put; everything below it scrolls.
    let (bar, rest) = match kids.split_first() {
        Some((b, rest)) if b.role().ends_with(".bar") => (Some(*b), rest),
        _ => (None, &kids[..]),
    };
    if let Some(b) = bar {
        render::node(cx, &mut child, b);
    }
    egui::ScrollArea::vertical()
        .id_salt(("page-pane-scroll", &n.id))
        .auto_shrink([false, false])
        .show(&mut child, |ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            for c in rest {
                render::node(cx, ui, c);
            }
        });
}
