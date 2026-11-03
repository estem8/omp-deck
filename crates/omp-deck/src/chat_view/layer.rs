//! The `layer` region and everything that floats: overlays (sheets, the slash menu), pickers
//! (the model picker), toasts and the image zoom, plus the `image` node itself (blob decode and a
//! texture cache, omp's own mark for `builtin:"omp"`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use egui::text::LayoutJob;
use egui::{
    Align2, Area, Color32, Context, CornerRadius, CursorIcon, Id, Order, Rect, RichText, Sense,
    Stroke, TextureOptions, Ui, UiBuilder, pos2, vec2,
};
use serde_json::Value;

use super::ctx::{Cx, Toast, ToastGalleys};
use super::dock::{self, Tone};
use super::events;
use super::glyphs;
use super::paint;
use super::render;
use super::snap::SNode;
use super::style::LABEL;
use super::text::{self, Base};
use super::transcript::measure_size;

/// Largest image side kept as a texture (px).
const MAX_TEXTURE: u32 = 1800;

// ---- images ---------------------------------------------------------------------------------

/// Decodes `bytes` into an RGBA image no larger than [`MAX_TEXTURE`] a side.
fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width().max(img.height()) > MAX_TEXTURE {
        img.thumbnail(MAX_TEXTURE, MAX_TEXTURE)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    ))
}

/// Missing blobs are not cached: the frame precedes blob delivery. Present but invalid bytes
/// get a negative entry, so decoding (including failed decoding) runs just once per blob.
fn cached_texture(
    cache: &mut HashMap<String, Option<egui::TextureHandle>>,
    id: &str,
    load: impl FnOnce() -> Option<Option<egui::TextureHandle>>,
) -> Option<egui::TextureHandle> {
    if let Some(t) = cache.get(id) {
        return t.clone();
    }
    let t = load()?;
    cache.insert(id.to_owned(), t.clone());
    t
}

fn texture(cx: &Cx, ctx: &Context, id: &str) -> Option<egui::TextureHandle> {
    cx.loc(|l| {
        cached_texture(&mut l.textures, id, || {
            let blob = cx.snap.blob(id)?;
            Some(decode(&blob.bytes).map(|img| {
                ctx.load_texture(format!("chat-blob-{id}"), img, TextureOptions::LINEAR)
            }))
        })
    })
}

fn image_length(v: Option<&Value>, suffix: &str, unit: f32) -> Option<f32> {
    let s = v?.as_str()?;
    let px = if let Some(n) = s.strip_suffix("px") {
        n.parse::<f32>().ok()?
    } else {
        s.strip_suffix(suffix)?.parse::<f32>().ok()? * unit
    };
    (px.is_finite() && px > 0.0).then_some(px)
}

fn image_bounds(max: Option<&Value>, available: f32, cell: egui::Vec2) -> egui::Vec2 {
    let numeric = max.and_then(Value::as_f64).map(|v| v as f32);
    let w = image_length(max.and_then(|v| v.get("w")), "ch", cell.x)
        .or(numeric)
        .unwrap_or(480.0);
    // Tool-result images use omp's 20-line cap even on versions that omit `max`.
    let h = image_length(max.and_then(|v| v.get("h")), "lines", cell.y).unwrap_or(20.0 * cell.y);
    vec2(w.min(available).max(1.0), h.max(1.0))
}

fn image_fit(size: egui::Vec2, bounds: egui::Vec2) -> egui::Vec2 {
    size * (bounds.x / size.x).min(bounds.y / size.y).min(1.0)
}

/// An `image` node: inline, bounded, click to enlarge.
pub fn image(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    if n.str("builtin") == Some("omp") {
        let w = n
            .num("w")
            .map_or(46.0, |w| (w as f32 * 0.4).clamp(24.0, 96.0));
        let (rect, _) = ui.allocate_exact_size(vec2(w, w * 0.85), Sense::hover());
        paint::omp_mark(ui.painter(), rect, ink);
        return;
    }
    let alt = n.str("alt").unwrap_or("image");
    let tex = n.str("blob").and_then(|b| texture(cx, ui.ctx(), b));
    let font = ink.mono_px(ink.mono_size);
    let cell = ui.fonts_mut(|f| vec2(f.glyph_width(&font, 'M'), f.row_height(&font)));
    let bounds = image_bounds(n.get("max"), ui.available_width(), cell);
    // Prefer original dimensions even after upload (large textures are downsampled). This
    // gives the pending blob exactly the same footprint as the decoded image.
    let size = match (n.num("w"), n.num("h")) {
        (Some(w), Some(h)) if w > 0.0 && h > 0.0 => vec2(w as f32, h as f32),
        _ => tex.as_ref().map_or(bounds, egui::TextureHandle::size_vec2),
    };
    let draw = image_fit(size, bounds);
    let (rect, resp) = ui.allocate_exact_size(draw, Sense::click());
    cx.loc(|l| {
        if l.ctx_claim.is_none() && l.ctx_click.is_some_and(|p| rect.contains(p)) {
            l.ctx_claim = Some(super::ctx::Claim::Card(n.id.clone()));
        }
    });
    let Some(tex) = tex else {
        paint::boxed(
            ui.painter(),
            rect,
            6,
            ink.chip(),
            Stroke::new(1.0, ink.line()),
        );
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            alt,
            ink.label(LABEL),
            ink.dim(),
        );
        return;
    };
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
    ui.painter().image(
        tex.id(),
        rect,
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    paint::boxed(
        ui.painter(),
        rect,
        6,
        Color32::TRANSPARENT,
        Stroke::new(1.0, ink.line().lerp_to_gamma(ink.b, hov)),
    );
    paint::pop(ui.painter(), rect, 6, ink, hov);
    let resp = resp.on_hover_cursor(CursorIcon::ZoomIn).on_hover_text(alt);
    let mut opened = false;
    if ui.rect_contains_pointer(rect)
        && let Some(href) = n.str("href")
        && n.obj("actions")
            .and_then(|a| a.get("menu"))
            .and_then(Value::as_array)
            .is_some_and(|menu| menu.iter().any(|a| a.as_str() == Some("open")))
    {
        let button = Rect::from_min_size(rect.right_top() - vec2(26.0, -2.0), vec2(24.0, 24.0));
        if ui
            .put(button, egui::Button::new("↗"))
            .on_hover_text("Open image")
            .clicked()
        {
            cx.open(href.to_owned());
            opened = true;
        }
    }
    if resp.clicked()
        && !opened
        && let Some(blob) = n.str("blob")
    {
        cx.zoom(blob.to_owned());
    }
}

// ---- sheets ---------------------------------------------------------------------------------

/// A floating sheet: styled header band over a body, gradient outline, lift. Padding on all sides.
/// A body taller than `max_body` scrolls.
fn sheet(
    cx: &Cx,
    ui: &mut Ui,
    width: f32,
    max_body: f32,
    head: Option<LayoutJob>,
    right: &dyn Fn(&mut Ui),
    body: &dyn Fn(&mut Ui),
) {
    let ink = cx.ink;
    let pad = 14.0;
    let head_h = if head.is_some() { 38.0 } else { 0.0 };
    let inner_w = width - 2.0 * pad;
    let full = measure_size(ui, inner_w, body);
    let size = vec2(full.x, full.y.min(max_body));
    let h = head_h + size.y + 2.0 * pad;
    let (rect, _) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
    let p = ui.painter();
    let pts = paint::corners_rect(rect, 12.0, 3.0, 12.0, 3.0);
    crate::deco::drop_shadow(
        p,
        rect,
        12,
        14,
        36,
        ink.dark,
        if ink.dark { 170 } else { 80 },
    );
    crate::deco::glow(
        p,
        rect,
        12,
        ink.b.gamma_multiply(if ink.dark { 0.16 } else { 0.10 }),
        24,
    );
    crate::deco::fill(
        p,
        pts.clone(),
        crate::theme::mix(ink.p.surface, ink.p.bg, 0.15),
    );
    if let Some(job) = head {
        let band = Rect::from_min_size(rect.min, vec2(width, head_h));
        let (c0, c1) = ink.head();
        let bpts = vec![
            band.left_top() + vec2(0.0, 12.0),
            pos2(band.left() + 12.0, band.top()),
            band.right_top(),
            pos2(band.right() - 16.0, band.bottom()),
            band.left_bottom(),
        ];
        crate::deco::fill_linear(p, &bpts, c0, c1, vec2(1.0, 0.0));
        paint::hairline(p, ink, band.left() + 12.0, band.right(), band.top(), 0.9);
        p.line_segment(
            [
                pos2(band.left(), band.bottom() - 0.5),
                pos2(band.right() - 16.0, band.bottom() - 0.5),
            ],
            Stroke::new(1.0, ink.line()),
        );
        let g = p.layout_job(job);
        p.with_clip_rect(band.intersect(ui.clip_rect())).galley(
            pos2(band.left() + pad, band.center().y - g.size().y / 2.0),
            g,
            ink.strong(),
        );
        let mut r = ui.new_child(
            UiBuilder::new()
                .id_salt("sheet-right")
                .max_rect(Rect::from_min_max(
                    pos2(band.left() + pad, band.top()),
                    pos2(band.right() - 22.0, band.bottom()),
                ))
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        r.set_clip_rect(band.intersect(ui.clip_rect()));
        right(&mut r);
    }
    crate::deco::gradient_outline(ui.painter(), pts, 1.0, &ink.rule_stops(), vec2(1.0, 0.3));
    let inner = Rect::from_min_size(
        pos2(rect.left() + pad, rect.top() + head_h + pad),
        vec2(inner_w, size.y),
    );
    let mut child = ui.new_child(UiBuilder::new().id_salt("sheet-body").max_rect(inner));
    child.set_width(inner_w.max(0.0));
    child.set_clip_rect(inner.intersect(ui.clip_rect()));
    if full.y > size.y {
        egui::ScrollArea::vertical()
            .id_salt("sheet-scroll")
            .max_height(size.y)
            .auto_shrink([false, false])
            .show(&mut child, body);
    } else {
        body(&mut child);
    }
}

fn head_job(cx: &Cx, n: &SNode, fallback: Option<&str>) -> Option<LayoutJob> {
    let ink = cx.ink;
    let base = Base::new(ink.label(13.0), ink.strong(), ink);
    match render::head_job(cx, n, &base) {
        Some(j) => Some(j),
        None => fallback.map(|t| {
            let mut j = LayoutJob::default();
            j.append(&text::clean(t), 0.0, base.format_plain());
            j
        }),
    }
}

// ---- overlays -------------------------------------------------------------------------------

fn overlay_width(size: Option<&str>, avail: f32) -> f32 {
    let w: f32 = match size {
        Some("sm") => 380.0,
        Some("lg") => 760.0,
        Some("full") => 1100.0,
        _ => 560.0,
    };
    w.min(avail - 32.0).max(240.0)
}

/// Body of an `overlay`: its children.
fn overlay_body(cx: &Cx, ui: &mut Ui, n: &SNode) {
    ui.spacing_mut().item_spacing.y = 8.0;
    let tx = super::ctx::Tx {
        font: cx.ink.prose(14.0),
        color: cx.ink.text(),
        line: Some(21.0),
    };
    cx.with_tx(tx, || render::children(cx, ui, n));
}

/// An overlay/picker/toast met inside the normal flow (a hook modal in the dock): drawn in place
/// as a sheet.
pub fn inline(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let w = ui.available_width();
    match n.kind.as_str() {
        "picker" => picker_sheet(cx, ui, n, w, 360.0),
        _ => sheet(
            cx,
            ui,
            w,
            f32::INFINITY,
            head_job(cx, n, n.str("title")),
            &|_| {},
            &|ui| overlay_body(cx, ui, n),
        ),
    }
}

/// Shows every overlay of the `layer` region, the toasts and the zoomed image over `rect`.
pub fn overlays(cx: &Cx, ctx: &Context, rect: Rect, salt: Id) {
    if let Some(layer) = cx.snap.region("layer") {
        for n in cx
            .snap
            .kids(layer)
            .filter(|n| !n.hidden() && cx.hidden != Some(n.id.as_str()))
        {
            match n.kind.as_str() {
                "overlay" => overlay(cx, ctx, rect, salt, n),
                "picker" => modal_picker(cx, ctx, rect, salt, n),
                _ => {}
            }
        }
    }
    toasts(cx, ctx, rect, salt);
    zoomed(cx, ctx, rect, salt);
}

/// Dims `rect` and reports a click on the backdrop.
fn backdrop(ctx: &Context, rect: Rect, salt: Id, ink: &super::style::Ink) -> bool {
    let id = salt.with("backdrop");
    let mut clicked = false;
    // On Middle, below every sheet (Foreground): a press on an Area raises it among its own
    // order, so a Foreground backdrop would jump above the sheet it dims for.
    Area::new(id)
        .order(Order::Middle)
        .fixed_pos(rect.min)
        .constrain(false)
        .show(ctx, |ui| {
            let (r, resp) = ui.allocate_exact_size(rect.size(), Sense::click());
            ui.painter().rect_filled(
                r,
                0.0,
                Color32::from_black_alpha(if ink.dark { 150 } else { 90 }),
            );
            clicked = resp.clicked();
        });
    clicked
}

fn overlay(cx: &Cx, ctx: &Context, rect: Rect, salt: Id, n: &SNode) {
    let ink = cx.ink;
    let w = overlay_width(n.str("size"), rect.width());
    let modal = n.flag("modal") || n.str("anchor").is_none_or(|a| a == "center");
    let caret_anchor = n.get("anchor").and_then(|a| a.get("caret")).is_some();
    if modal && !caret_anchor && backdrop(ctx, rect, salt.with(&n.id), ink) {
        cx.send(events::action(cx.sf(), &n.id, "close"));
    }
    let (pos, pivot) = if caret_anchor {
        let c = cx
            .loc(|l| l.caret)
            .unwrap_or_else(|| Rect::from_center_size(rect.center_bottom(), vec2(1.0, 1.0)));
        (
            pos2(
                c.left().clamp(
                    rect.left() + 8.0,
                    (rect.right() - w - 8.0).max(rect.left() + 8.0),
                ),
                (c.top() - 10.0).max(rect.top() + 8.0),
            ),
            Align2::LEFT_BOTTOM,
        )
    } else {
        match n.str("anchor") {
            Some("top") => (pos2(rect.center().x, rect.top() + 48.0), Align2::CENTER_TOP),
            Some("bottom") => (
                pos2(rect.center().x, rect.bottom() - 24.0),
                Align2::CENTER_BOTTOM,
            ),
            _ => (rect.center(), Align2::CENTER_CENTER),
        }
    };
    Area::new(salt.with(("overlay", &n.id)))
        .order(Order::Foreground)
        .fixed_pos(pos)
        .pivot(pivot)
        .constrain_to(rect)
        .show(ctx, |ui| {
            ui.set_width(w);
            if caret_anchor {
                menu_sheet(cx, ui, w, n);
            } else {
                // The sheet keeps clear of the pane's edges: a taller body scrolls.
                let max_body = (rect.height() - 2.0 * 40.0 - 38.0 - 28.0).max(120.0);
                sheet(
                    cx,
                    ui,
                    w,
                    max_body,
                    head_job(cx, n, n.str("title")),
                    &|_| {},
                    &|ui| overlay_body(cx, ui, n),
                );
            }
        });
}

/// Tallest the autocomplete menu grows (px) before it scrolls.
const MENU_MAX_H: f32 = 290.0;

/// The slash/file autocomplete: a bare list in a lifted frame.
fn menu_sheet(cx: &Cx, ui: &mut Ui, w: f32, n: &SNode) {
    let ink = cx.ink;
    let body = |ui: &mut Ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let tx = super::ctx::Tx {
            font: ink.prose(13.5),
            color: ink.text(),
            line: None,
        };
        cx.with_tx(tx, || render::children(cx, ui, n));
    };
    let pad = 6.0;
    let size = measure_size(ui, w - 2.0 * pad, &body);
    // Nine rows at most; the highlighted row scrolls into view (`render::item`).
    let h = size.y.min(MENU_MAX_H);
    let (rect, _) = ui.allocate_exact_size(vec2(w, h + 2.0 * pad), Sense::hover());
    let p = ui.painter();
    let pts = paint::corners_rect(rect, 10.0, 3.0, 10.0, 3.0);
    crate::deco::drop_shadow(
        p,
        rect,
        10,
        10,
        26,
        ink.dark,
        if ink.dark { 160 } else { 70 },
    );
    crate::deco::fill(
        p,
        pts.clone(),
        crate::theme::mix(ink.p.surface, ink.p.bg, 0.1),
    );
    crate::deco::gradient_outline(p, pts, 1.0, &ink.rule_stops(), vec2(1.0, 0.3));
    let inner = Rect::from_min_size(rect.min + vec2(pad, pad), vec2(w - 2.0 * pad, h));
    let mut child = ui.new_child(UiBuilder::new().id_salt("menu").max_rect(inner));
    egui::ScrollArea::vertical()
        .id_salt(("menu-scroll", &n.id))
        .max_height(h)
        .auto_shrink([false, false])
        .show(&mut child, body);
}

// ---- toasts ---------------------------------------------------------------------------------

/// Fade-in of a toast (seconds).
const TOAST_IN: f32 = 0.18;
/// Fade-out of a toast (seconds).
const TOAST_OUT: f32 = 0.3;
/// Toasts kept on screen at once (the document's ring holds as many).
const TOAST_MAX: usize = 8;

/// The ring's record as a shown toast.
pub fn toast_of(r: &deck_tsp::document::ToastRecord) -> Toast {
    Toast {
        seq: r.seq,
        text: text::clean(&r.text).into(),
        sub: r.sub.as_deref().map(|s| text::clean(s).into()),
        at: r.at,
        ttl: r.ttl,
    }
}

/// Adds the ring's toasts that are not shown yet (by `seq`) and drops the expired ones. The ring
/// is the only source: omp deletes a toast's node the frame after adding it, so the tree never
/// shows one for its time to live.
pub fn merge_toasts(shown: &mut Vec<Toast>, ring: impl IntoIterator<Item = Toast>, now: Instant) {
    shown.retain(|t| now < t.at + t.ttl);
    for t in ring {
        if now < t.at + t.ttl && !shown.iter().any(|s| s.seq == t.seq) {
            shown.push(t);
        }
    }
    shown.sort_by_key(|t| t.seq);
    if shown.len() > TOAST_MAX {
        shown.drain(..shown.len() - TOAST_MAX);
    }
}

/// Opacity of a toast `age` after it appeared, for a time to live of `ttl`: fades in, holds,
/// fades out over its last [`TOAST_OUT`] seconds (a short toast splits its time between both).
pub fn toast_alpha(age: Duration, ttl: Duration) -> f32 {
    let age = age.as_secs_f32();
    let left = ttl.as_secs_f32() - age;
    ((age / TOAST_IN).min(1.0) * (left / TOAST_OUT).min(1.0)).clamp(0.0, 1.0)
}

fn toast_galleys(cx: &Cx, ui: &Ui, t: &Toast) -> ToastGalleys {
    let ink = cx.ink;
    let ppp = ui.pixels_per_point();
    let colors = [ink.strong(), ink.dim()];
    if let Some(g) = cx.loc(|l| l.toast_galleys.get(&t.seq).cloned())
        && g.ppp == ppp
        && g.colors == colors
    {
        return g;
    }
    let p = ui.painter();
    let g = ToastGalleys {
        ppp,
        colors,
        text: p.layout_no_wrap(t.text.to_string(), ink.prose(13.5), colors[0]),
        sub: t
            .sub
            .as_ref()
            .map(|s| p.layout_no_wrap(s.to_string(), ink.label(12.0), colors[1])),
    };
    cx.loc(|l| l.toast_galleys.insert(t.seq, g.clone()));
    g
}

fn toast_pill(cx: &Cx, ui: &mut Ui, t: &Toast) {
    let ink = cx.ink;
    let c = ink.b;
    let ToastGalleys {
        text: g, sub: sg, ..
    } = toast_galleys(cx, ui, t);
    let w = g.size().x.max(sg.as_ref().map_or(0.0, |s| s.size().x)) + 44.0;
    let h = g.size().y + sg.as_ref().map_or(0.0, |s| s.size().y + 2.0) + 18.0;
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let p = ui.painter();
    let pts = paint::corners_rect(rect, 10.0, 3.0, 10.0, 3.0);
    crate::deco::drop_shadow(p, rect, 10, 8, 20, ink.dark, 150);
    crate::deco::fill(
        p,
        pts.clone(),
        crate::theme::mix(ink.p.surface, ink.p.bg, 0.1),
    );
    crate::deco::gradient_outline(p, pts, 1.0, &[(0.0, c), (1.0, ink.a)], vec2(1.0, 0.2));
    p.line_segment(
        [
            pos2(rect.left() + 12.0, rect.top() + 10.0),
            pos2(rect.left() + 12.0, rect.bottom() - 10.0),
        ],
        Stroke::new(2.5, c),
    );
    let mut y = rect.top() + 9.0;
    let gh = g.size().y;
    p.galley(pos2(rect.left() + 26.0, y), g, ink.strong());
    y += gh + 2.0;
    if let Some(s) = sg {
        p.galley(pos2(rect.left() + 26.0, y), s, ink.dim());
    }
}

fn toasts(cx: &Cx, ctx: &Context, rect: Rect, salt: Id) {
    // One toast at a time (a clone is a few refcounts): the list itself is not copied per frame.
    for i in 0..cx.loc(|l| l.toasts.len()) {
        let Some(t) = cx.loc(|l| l.toasts.get(i).cloned()) else {
            break;
        };
        let age = cx.now.saturating_duration_since(t.at);
        let left = (t.at + t.ttl).saturating_duration_since(cx.now);
        // Moving while it fades; the last wake-up lands on the end of its life.
        cx.animate_in(
            if age.as_secs_f32() < TOAST_IN || left.as_secs_f32() < TOAST_OUT {
                Duration::from_millis(16)
            } else {
                left.saturating_sub(Duration::from_secs_f32(TOAST_OUT))
            },
        );
        let alpha = toast_alpha(age, t.ttl);
        let slide = (1.0 - (age.as_secs_f32() / TOAST_IN).min(1.0)) * -6.0;
        Area::new(salt.with(("toast", t.seq)))
            .order(Order::Tooltip)
            .fixed_pos(pos2(
                rect.center().x,
                rect.top() + 20.0 + i as f32 * 56.0 + slide,
            ))
            .pivot(Align2::CENTER_TOP)
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_opacity(alpha);
                toast_pill(cx, ui, &t);
            });
    }
}

// ---- image zoom -----------------------------------------------------------------------------

fn zoomed(cx: &Cx, ctx: &Context, rect: Rect, salt: Id) {
    let Some(blob) = cx.loc(|l| l.zoomed.clone()) else {
        return;
    };
    let Some(tex) = texture(cx, ctx, &blob) else {
        cx.loc(|l| l.zoomed = None);
        return;
    };
    let bounds = ctx.content_rect().intersect(rect).size() - vec2(64.0, 144.0);
    let size = tex.size_vec2();
    let k = (bounds.x.max(1.0) / size.x)
        .min(bounds.y.max(1.0) / size.y)
        .min(4.0);
    let draw = size * k;
    let head = crate::card::Header::new(crate::icons::ICON_APP_WINDOW, "Image preview");
    if crate::card::modal(ctx, salt.with("zoom-img"), draw.x + 32.0, &head, |ui| {
        ui.add(egui::Image::new(&tex).fit_to_exact_size(draw));
    }) {
        cx.loc(|l| l.zoomed = None);
    }
}

// ---- pickers --------------------------------------------------------------------------------

/// The items of a picker after omp's `itemsAdd`/`itemsDel` patches.
#[derive(Default)]
pub struct PickerItems {
    gen_: u64,
    base: u64,
    add: u64,
    del: u64,
    pub items: Vec<Value>,
}

fn hash_value(v: Option<&Value>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if let Some(v) = v {
        v.to_string().hash(&mut h);
    }
    h.finish()
}

fn item_id(v: &Value) -> Option<&str> {
    v.get("id").and_then(Value::as_str)
}

/// Brings the cached item list of picker `n` up to date.
pub fn refresh_items(cache: &mut HashMap<String, PickerItems>, n: &SNode) {
    let e = cache.entry(n.id.clone()).or_default();
    if e.gen_ == n.gen_ && e.gen_ != 0 {
        return;
    }
    e.gen_ = n.gen_;
    let (base, add, del) = (
        hash_value(n.get("items")),
        hash_value(n.get("itemsAdd")),
        hash_value(n.get("itemsDel")),
    );
    if base != e.base {
        e.base = base;
        e.items = n.arr("items").to_vec();
    }
    if add != e.add {
        e.add = add;
        for it in n.arr("itemsAdd") {
            match item_id(it).and_then(|id| e.items.iter().position(|x| item_id(x) == Some(id))) {
                Some(i) => e.items[i] = it.clone(),
                None => e.items.push(it.clone()),
            }
        }
    }
    if del != e.del {
        e.del = del;
        let dels: Vec<&str> = n.arr("itemsDel").iter().filter_map(Value::as_str).collect();
        e.items
            .retain(|x| item_id(x).is_none_or(|id| !dels.contains(&id)));
    }
}

/// One row of a picker in display order: a group header or an item.
#[derive(Debug, PartialEq)]
pub enum Entry<'a> {
    Group { label: &'a str, count: Option<u64> },
    Item(&'a Value),
}

/// Items in `order` (group headers and item ids), else in sent order.
pub fn ordered<'a>(items: &'a [Value], order: &'a [Value]) -> Vec<Entry<'a>> {
    if order.is_empty() {
        return items.iter().map(Entry::Item).collect();
    }
    let by_id: HashMap<&str, &Value> = items
        .iter()
        .filter_map(|i| Some((item_id(i)?, i)))
        .collect();
    order
        .iter()
        .filter_map(|o| match o {
            Value::String(id) => by_id.get(id.as_str()).map(|v| Entry::Item(v)),
            Value::Object(m) => Some(Entry::Group {
                label: m
                    .get("label")
                    .or_else(|| m.get("group"))
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                count: m.get("count").and_then(Value::as_u64),
            }),
            _ => None,
        })
        .collect()
}

fn modal_picker(cx: &Cx, ctx: &Context, rect: Rect, salt: Id, n: &SNode) {
    if backdrop(ctx, rect, salt.with(&n.id), cx.ink) {
        let esc = n
            .arr("actions")
            .iter()
            .find(|a| {
                a.get("keys").and_then(Value::as_array).is_some_and(|k| {
                    k.iter()
                        .filter_map(Value::as_str)
                        .any(|s| s.eq_ignore_ascii_case("esc") || s.eq_ignore_ascii_case("escape"))
                })
            })
            .and_then(|a| a.get("id").and_then(Value::as_str))
            .unwrap_or("close");
        cx.send(events::action(cx.sf(), &n.id, esc));
    }
    let base: f32 = match n.str("size") {
        Some("lg") => 760.0,
        Some("screen") => 1000.0,
        _ => 580.0,
    };
    let w = base.min(rect.width() - 32.0).max(280.0);
    let max_h = (rect.height() * 0.72).clamp(260.0, 640.0);
    Area::new(salt.with(("picker", &n.id)))
        .order(Order::Foreground)
        .fixed_pos(pos2(
            rect.center().x - w / 2.0,
            rect.top() + (rect.height() * 0.12).max(24.0),
        ))
        .constrain_to(rect)
        .show(ctx, |ui| picker_sheet(cx, ui, n, w, max_h));
}

fn picker_sheet(cx: &Cx, ui: &mut Ui, n: &SNode, w: f32, list_h: f32) {
    let ink = cx.ink;
    cx.loc(|l| refresh_items(&mut l.pickers, n));
    let items: Vec<Value> = cx.loc(|l| {
        l.pickers
            .get(&n.id)
            .map(|e| e.items.clone())
            .unwrap_or_default()
    });
    let title = n.str("title").unwrap_or("Select");
    let head = {
        let mut j = LayoutJob::default();
        let base = Base::new(ink.label(13.0), ink.strong(), ink);
        j.append(&text::clean(title), 0.0, base.format_plain());
        if let Some(s) = n.str("subtitle") {
            let mut d = base.clone();
            d.color = ink.dim();
            j.append(&format!("   {}", text::clean(s)), 0.0, d.format_plain());
        }
        j
    };
    let total = n.num("total").map(|t| t as u64);
    let right = |ui: &mut Ui| {
        if let Some(t) = total {
            ui.label(
                RichText::new(format!("{t}"))
                    .font(ink.label(LABEL))
                    .color(ink.dim()),
            );
        }
    };
    sheet(cx, ui, w, f32::INFINITY, Some(head), &right, &|ui| {
        picker_body(cx, ui, n, &items, list_h)
    });
}

fn picker_body(cx: &Cx, ui: &mut Ui, n: &SNode, items: &[Value], list_h: f32) {
    let ink = cx.ink;
    ui.spacing_mut().item_spacing.y = 8.0;
    // Query line: omp owns the typing; what has been typed shows here.
    let query = n.str("query").unwrap_or_default();
    let placeholder = n.str("placeholder").unwrap_or("Search…");
    {
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(w, 32.0), Sense::hover());
        let p = ui.painter();
        let pts = paint::corners_rect(rect, 8.0, 2.0, 8.0, 2.0);
        crate::deco::fill(p, pts.clone(), ink.chip());
        crate::deco::outline(p, pts, Stroke::new(1.0, ink.p.stroke));
        glyphs::paint(
            p,
            "search",
            pos2(rect.left() + 16.0, rect.center().y),
            14.0,
            ink.dim(),
        );
        let (txt, col) = if query.is_empty() {
            (placeholder, ink.faint())
        } else {
            (query, ink.strong())
        };
        let g = p.layout_no_wrap(text::clean(txt).into_owned(), ink.prose(14.0), col);
        let gx = rect.left() + 32.0;
        p.galley(pos2(gx, rect.center().y - g.size().y / 2.0), g.clone(), col);
        if cx.focused {
            let x = if query.is_empty() {
                gx
            } else {
                gx + g.size().x + 1.0
            };
            p.rect_filled(
                Rect::from_min_max(
                    pos2(x, rect.top() + 7.0),
                    pos2(x + 2.0, rect.bottom() - 7.0),
                ),
                1.0,
                ink.b,
            );
        }
    }
    // Scopes and tabs.
    for (key, act, active) in [
        ("scopes", "scope", n.str("scope")),
        ("tabs", "tab", n.str("tab")),
    ] {
        let chips = n.arr(key);
        if chips.is_empty() {
            continue;
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for c in chips {
                let (id, label) = match c {
                    Value::String(s) => (s.as_str(), s.as_str()),
                    Value::Object(m) => (
                        m.get("id").and_then(Value::as_str).unwrap_or_default(),
                        m.get("label").and_then(Value::as_str).unwrap_or_default(),
                    ),
                    _ => continue,
                };
                let on = active == Some(id);
                let r = dock::button(
                    cx,
                    ui,
                    ui.scope_id().with(("chip", &n.id, act, id)),
                    label,
                    None,
                    if on { Tone::Accent } else { Tone::Neutral },
                );
                if r.clicked() {
                    cx.send(events::action_value(cx.sf(), &n.id, act, Some(id)));
                }
            }
        });
    }
    // Rows.
    let entries = ordered(items, n.arr("order"));
    let selected = n.str("selected");
    let current: Vec<&str> = n.arr("current").iter().filter_map(Value::as_str).collect();
    let columns: Vec<(&str, f32)> = n
        .arr("columns")
        .iter()
        .filter_map(|c| {
            let id = c.get("id")?.as_str()?;
            let min = c.get("min").and_then(Value::as_f64).unwrap_or(7.0) as f32;
            Some((id, (min * 8.0).clamp(48.0, 120.0)))
        })
        .collect();
    let state = n.str("state").unwrap_or("ready");
    if state == "loading" || state == "error" || entries.is_empty() {
        let msg = match state {
            "loading" => n.str("message").unwrap_or("Loading…"),
            "error" => n.str("message").unwrap_or("Something went wrong"),
            _ => n.str("empty").unwrap_or("Nothing matches"),
        };
        ui.horizontal(|ui| {
            if state == "loading" {
                render::spin(cx, ui, 14.0, ink.accent_text());
            }
            let c = if state == "error" {
                ink.p.danger
            } else {
                ink.dim()
            };
            ui.label(RichText::new(msg).font(ink.prose(13.5)).color(c));
        });
    } else {
        egui::ScrollArea::vertical()
            .id_salt(("picker-rows", &n.id))
            .max_height(list_h)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                for e in &entries {
                    match e {
                        Entry::Group { label, count } => {
                            ui.add_space(4.0);
                            let t = match count {
                                Some(c) => format!("{} · {c}", label.to_uppercase()),
                                None => label.to_uppercase(),
                            };
                            ui.label(RichText::new(t).font(ink.label(LABEL)).color(ink.b));
                        }
                        Entry::Item(it) => picker_row(cx, ui, n, it, selected, &current, &columns),
                    }
                }
            });
    }
    // Confirm strip, then the action bar.
    if let Some(c) = n.get("confirm").filter(|c| !c.is_null()) {
        let msg = c
            .get("message")
            .or_else(|| c.get("text"))
            .or_else(|| c.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("Are you sure?");
        let act = c.get("act").and_then(Value::as_str).unwrap_or("confirm");
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(msg)
                    .font(ink.prose(13.5))
                    .color(ink.tone("warning")),
            );
            if dock::button(
                cx,
                ui,
                ui.scope_id().with("confirm"),
                "Confirm",
                None,
                Tone::Danger,
            )
            .clicked()
            {
                cx.send(events::action(cx.sf(), &n.id, act));
            }
            if dock::button(
                cx,
                ui,
                ui.scope_id().with("cancel"),
                "Cancel",
                None,
                Tone::Neutral,
            )
            .clicked()
            {
                cx.send(events::action(cx.sf(), &n.id, "cancel"));
            }
        });
    }
    let actions = n.arr("actions");
    if !actions.is_empty() {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for a in actions
                .iter()
                .filter(|a| a.get("on").and_then(Value::as_bool) != Some(false))
            {
                let (Some(id), label) = (
                    a.get("id").and_then(Value::as_str),
                    a.get("label").and_then(Value::as_str).unwrap_or_default(),
                ) else {
                    continue;
                };
                let tone = if a.get("danger").and_then(Value::as_bool) == Some(true) {
                    Tone::Danger
                } else if a.get("primary").and_then(Value::as_bool) == Some(true) {
                    Tone::Accent
                } else {
                    Tone::Neutral
                };
                if a.get("disabled").and_then(Value::as_bool) == Some(true) {
                    ui.add_enabled_ui(false, |ui| {
                        dock::button(cx, ui, ui.scope_id().with(("act", id)), label, None, tone)
                    });
                } else if dock::button(cx, ui, ui.scope_id().with(("act", id)), label, None, tone)
                    .clicked()
                {
                    cx.send(events::action(cx.sf(), &n.id, id));
                }
            }
        });
    }
}

fn picker_row(
    cx: &Cx,
    ui: &mut Ui,
    n: &SNode,
    it: &Value,
    selected: Option<&str>,
    current: &[&str],
    columns: &[(&str, f32)],
) {
    let ink = cx.ink;
    let Some(id) = item_id(it) else { return };
    let disabled = it.get("disabled").and_then(Value::as_bool) == Some(true);
    let is_sel = selected == Some(id);
    let is_cur = current.contains(&id);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 30.0), Sense::hover());
    let resp = ui.interact(
        rect,
        ui.scope_id().with(("prow", &n.id, id)),
        if disabled {
            Sense::hover()
        } else {
            Sense::click()
        },
    );
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("h"), resp.hovered() && !disabled, 0.08);
    let p = ui.painter();
    if is_sel {
        p.rect(
            rect,
            CornerRadius::same(4),
            ink.wash(ink.b),
            Stroke::new(1.0, ink.edge(ink.b)),
            egui::StrokeKind::Inside,
        );
        p.rect_filled(
            Rect::from_min_max(rect.left_top(), pos2(rect.left() + 3.0, rect.bottom())),
            1.0,
            ink.b,
        );
    } else if hov > 0.0 {
        p.rect_filled(rect, 4.0, ink.p.hover_bg.gamma_multiply(hov));
        paint::pop(p, rect, 4, ink, hov);
    }
    let cy = rect.center().y;
    let mut x = rect.left() + 12.0;
    if let Some(tone) = it.get("dot").and_then(Value::as_str) {
        p.circle_filled(pos2(x + 3.0, cy), 3.0, ink.tone(tone));
        x += 14.0;
    } else if let Some(ic) = it.get("icon").and_then(Value::as_str) {
        glyphs::paint(p, ic, pos2(x + 7.0, cy), 14.0, ink.dim());
        x += 22.0;
    }
    let col = if disabled {
        ink.faint()
    } else if is_sel {
        ink.strong()
    } else {
        ink.text()
    };
    let label = text::clean(it.get("label").and_then(Value::as_str).unwrap_or(id)).into_owned();
    let mono = it.get("mono").and_then(Value::as_bool) == Some(true);
    let font = if mono {
        ink.mono_px(13.0)
    } else {
        ink.prose(14.0)
    };
    // Right side first (facts), so the label can truncate against it.
    let mut rx = rect.right() - 12.0;
    if is_cur {
        glyphs::paint(p, "check", pos2(rx - 6.0, cy), 14.0, ink.tone("success"));
        rx -= 22.0;
    }
    if let Some(facts) = it.get("facts").and_then(Value::as_object) {
        for (cid, cw) in columns.iter().rev() {
            if let Some(v) = facts.get(*cid) {
                let s = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(num) => num.to_string(),
                    _ => continue,
                };
                let g = p.layout_no_wrap(s, ink.label(12.0), ink.dim());
                p.galley(pos2(rx - g.size().x, cy - g.size().y / 2.0), g, ink.dim());
                rx -= cw;
            }
        }
    }
    for b in it
        .get("badges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
    {
        let t = b
            .as_str()
            .or_else(|| b.get("text").and_then(Value::as_str))
            .unwrap_or_default();
        let g = p.layout_no_wrap(t.to_owned(), ink.label(LABEL), ink.dim());
        let r = Rect::from_center_size(
            pos2(rx - g.size().x / 2.0 - 5.0, cy),
            g.size() + vec2(10.0, 4.0),
        );
        paint::boxed(p, r, 3, ink.chip(), Stroke::new(1.0, ink.p.stroke));
        p.galley(r.center() - g.size() / 2.0, g, ink.dim());
        rx = r.left() - 6.0;
    }
    let mut job = LayoutJob::simple_singleline(label, font, col);
    job.wrap.max_width = (rx - x - 8.0).max(30.0);
    job.wrap.max_rows = 1;
    let g = p.layout_job(job);
    let gw = g.size().x;
    p.galley(pos2(x, cy - g.size().y / 2.0), g, col);
    if let Some(d) = it.get("detail").and_then(Value::as_str) {
        let avail = rx - x - gw - 18.0;
        if avail > 40.0 {
            let mut j = LayoutJob::simple_singleline(
                text::clean(d).into_owned(),
                ink.label(12.0),
                ink.dim(),
            );
            j.wrap.max_width = avail;
            j.wrap.max_rows = 1;
            let g = p.layout_job(j);
            p.galley(pos2(x + gw + 10.0, cy - g.size().y / 2.0), g, ink.dim());
        }
    }
    if !disabled {
        let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
        if resp.double_clicked() || (resp.clicked() && is_sel) {
            cx.send(events::activate(cx.sf(), &n.id, id));
        } else if resp.clicked() {
            cx.send(events::select(cx.sf(), &n.id, id));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn webp_blob_decodes_into_a_texture() {
        let mut bytes = Vec::new();
        let img = image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255]));
        img.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::WebP,
        )
        .expect("webp encodes");
        let ctx = Context::default();
        let mut cache = HashMap::new();
        let tex = cached_texture(&mut cache, "webp", || {
            Some(decode(&bytes).map(|img| ctx.load_texture("webp", img, TextureOptions::LINEAR)))
        })
        .expect("webp uploads");
        assert_eq!(tex.size(), [3, 2]);
    }

    #[test]
    fn image_limits_use_character_and_line_units_and_reserve_the_aspect_ratio() {
        let max = json!({"w": "12ch", "h": "4lines"});
        let bounds = image_bounds(Some(&max), 800.0, vec2(8.0, 20.0));
        assert_eq!(bounds, vec2(96.0, 80.0));
        let draw = image_fit(vec2(333.0, 200.0), bounds);
        assert_eq!(draw.x, 96.0);
        assert!((draw.y - 57.65766).abs() < 0.001);
        assert_eq!(
            image_bounds(
                Some(&json!({"w": "120px", "h": "80px"})),
                100.0,
                vec2(8.0, 20.0)
            ),
            vec2(100.0, 80.0)
        );
        assert_eq!(
            image_bounds(Some(&json!(320)), 800.0, vec2(8.0, 20.0)),
            vec2(320.0, 400.0)
        );
    }

    #[test]
    fn invalid_blob_is_decoded_once_but_a_missing_blob_is_retried() {
        let mut cache = HashMap::new();
        let mut attempts = 0;
        assert!(cached_texture(&mut cache, "blob", || None).is_none());
        assert!(!cache.contains_key("blob"));
        for _ in 0..3 {
            assert!(
                cached_texture(&mut cache, "blob", || {
                    attempts += 1;
                    Some(decode(b"invalid").map(|_| unreachable!("invalid bytes")))
                })
                .is_none()
            );
        }
        assert_eq!(attempts, 1);
        assert!(matches!(cache.get("blob"), Some(None)));
    }

    #[test]
    fn picker_order_interleaves_group_headers_and_skips_unknown_ids() {
        let items = vec![
            json!({"id": "a/x"}),
            json!({"id": "a/y"}),
            json!({"id": "b/z"}),
        ];
        let order = vec![
            json!({"group": "a", "label": "Anthropic", "count": 2}),
            json!("a/x"),
            json!("a/y"),
            json!("gone"),
            json!({"group": "b", "label": "Other"}),
            json!("b/z"),
        ];
        let e = ordered(&items, &order);
        assert_eq!(e.len(), 5);
        assert_eq!(
            e[0],
            Entry::Group {
                label: "Anthropic",
                count: Some(2)
            }
        );
        assert_eq!(e[1], Entry::Item(&items[0]));
        assert_eq!(
            e[3],
            Entry::Group {
                label: "Other",
                count: None
            }
        );
        assert_eq!(ordered(&items, &[]).len(), 3);
    }

    #[test]
    fn decode_rejects_garbage_and_reads_a_png() {
        assert!(decode(b"not an image").is_none());
        let mut png = Vec::new();
        let img = image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255]));
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("png encodes");
        let c = decode(&png).expect("png decodes");
        assert_eq!(c.size, [3, 2]);
    }

    fn toast(seq: u64, at: Instant, ms: u64) -> Toast {
        Toast {
            seq,
            text: format!("t{seq}").into(),
            sub: None,
            at,
            ttl: Duration::from_millis(ms),
        }
    }

    #[test]
    fn the_ring_is_merged_by_seq_so_a_repeat_read_never_doubles_a_toast() {
        let t0 = Instant::now();
        let mut shown = Vec::new();
        merge_toasts(&mut shown, [toast(1, t0, 2400), toast(2, t0, 2400)], t0);
        // The next sync reads the same ring, plus a new toast.
        let later = t0 + Duration::from_millis(100);
        merge_toasts(
            &mut shown,
            [
                toast(1, t0, 2400),
                toast(2, t0, 2400),
                toast(3, later, 2400),
            ],
            later,
        );
        assert_eq!(shown.iter().map(|t| t.seq).collect::<Vec<_>>(), [1, 2, 3]);
        // A toast the ring no longer has (omp deleted its node) still shows until it expires.
        let mid = t0 + Duration::from_millis(2000);
        merge_toasts(&mut shown, [toast(3, later, 2400)], mid);
        assert_eq!(shown.iter().map(|t| t.seq).collect::<Vec<_>>(), [1, 2, 3]);
        let end = t0 + Duration::from_millis(2450);
        merge_toasts(&mut shown, [], end);
        assert_eq!(shown.iter().map(|t| t.seq).collect::<Vec<_>>(), [3]);
        // Expired entries of the ring are not adopted, and the stack is capped.
        let mut shown = Vec::new();
        let ring: Vec<Toast> = (0..12).map(|s| toast(s, t0, 60_000)).collect();
        merge_toasts(&mut shown, ring, t0);
        assert_eq!(shown.len(), TOAST_MAX);
        assert_eq!(shown[0].seq, 4, "the oldest give way");
        let mut shown = Vec::new();
        merge_toasts(&mut shown, [toast(9, t0, 100)], t0 + Duration::from_secs(1));
        assert!(shown.is_empty());
    }

    #[test]
    fn a_toast_fades_in_holds_and_fades_out_over_its_life() {
        let ttl = Duration::from_millis(2400);
        let a = |ms: u64| toast_alpha(Duration::from_millis(ms), ttl);
        assert!(a(0) < 0.01, "starts transparent");
        assert!((a(90) - 0.5).abs() < 0.01, "half-way through the fade-in");
        assert!((a(180) - 1.0).abs() < 1e-4 && (a(1200) - 1.0).abs() < 1e-4);
        assert!(
            (a(2250) - 0.5).abs() < 0.01,
            "half-way through the fade-out"
        );
        assert!(
            a(2400) < 0.01 && a(5000) < 0.01,
            "gone at and after its end"
        );
        // A very short toast never exceeds 1 and never goes negative.
        let short = toast_alpha(Duration::from_millis(50), Duration::from_millis(100));
        assert!((0.0..=1.0).contains(&short));
    }
}
