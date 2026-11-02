//! Shapes of the "signal edge" language the chat view draws: slanted header bands, cut-corner
//! bubbles, accent-pair hairlines, glow, hover pops. Built on `deco`; nothing here keeps state.

use egui::{Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, pos2, vec2};

use crate::deco;

use super::style::Ink;

/// Header band whose right end is cut on the slant (`polygon(0 0,100% 0,calc(100% - s) 100%,0 100%)`).
pub fn slant_band(rect: Rect, s: f32) -> Vec<Pos2> {
    vec![
        rect.left_top(),
        rect.right_top(),
        pos2(rect.right() - s, rect.bottom()),
        rect.left_bottom(),
    ]
}

/// User bubble outline: the top-right and bottom-left corners are cut by `cut` px.
pub fn cut_bubble(rect: Rect, cut: f32) -> Vec<Pos2> {
    let c = cut.min(rect.height() / 2.0).min(rect.width() / 2.0);
    vec![
        rect.left_top(),
        pos2(rect.right() - c, rect.top()),
        pos2(rect.right(), rect.top() + c),
        rect.right_bottom(),
        pos2(rect.left() + c, rect.bottom()),
        pos2(rect.left(), rect.bottom() - c),
    ]
}

/// Parallelogram leaning right (top edge shifted right by `s`).
pub fn parallelogram(rect: Rect, s: f32) -> Vec<Pos2> {
    vec![
        pos2(rect.left() + s, rect.top()),
        rect.right_top(),
        pos2(rect.right() - s, rect.bottom()),
        rect.left_bottom(),
    ]
}

/// The accent hairline along the top of a card, from its left edge to `right`.
pub fn hairline(painter: &Painter, ink: &Ink, x0: f32, x1: f32, y: f32, alpha: f32) {
    let stops = [
        (0.0, ink.a.gamma_multiply(alpha)),
        (1.0, ink.b.gamma_multiply(alpha)),
    ];
    deco::gradient_rule(painter, pos2(x0, y + 0.5), pos2(x1, y + 0.5), 1.0, &stops);
}

/// A hover/focus "pop": a second outline 2 px outside the box in the accent colour.
pub fn pop(painter: &Painter, rect: Rect, radius: u8, ink: &Ink, t: f32) {
    if t <= 0.01 {
        return;
    }
    painter.rect_stroke(
        rect.expand(2.0),
        CornerRadius::same(radius.saturating_add(2)),
        Stroke::new(1.0, ink.p.hover_line.gamma_multiply(0.55 * t)),
        StrokeKind::Outside,
    );
}

/// Pulse 0..=1 of a running thing (a slow breath), constant when motion is reduced.
pub fn breath(ink: &Ink, now: f64) -> f32 {
    if ink.reduce_motion {
        0.6
    } else {
        (0.5 + 0.5 * (now * std::f64::consts::TAU / 2.2).sin()) as f32
    }
}

/// Fill of `pts` with a left→right gradient.
pub fn fill_h(painter: &Painter, pts: &[Pos2], a: Color32, b: Color32) {
    deco::fill_linear(painter, pts, a, b, vec2(1.0, 0.0));
}

/// Rounded rect with a hairline border.
pub fn boxed(painter: &Painter, rect: Rect, radius: u8, fill: Color32, stroke: Stroke) {
    painter.rect(
        rect,
        CornerRadius::same(radius),
        fill,
        stroke,
        StrokeKind::Inside,
    );
}

/// A shape reservation: paint behind content laid out afterwards.
pub fn reserve(painter: &Painter) -> egui::layers::ShapeIdx {
    painter.add(Shape::Noop)
}

/// The omp mark: a slanted accent-pair tile with a π.
pub fn omp_mark(painter: &Painter, rect: Rect, ink: &Ink) {
    let pts = parallelogram(rect, rect.height() * 0.2);
    fill_h(painter, &pts, ink.a, ink.b);
    deco::outline(
        painter,
        pts,
        Stroke::new(1.0, Color32::from_white_alpha(40)),
    );
    painter.text(
        rect.center() + vec2(rect.height() * 0.05, 0.0),
        egui::Align2::CENTER_CENTER,
        "π",
        egui::FontId::new(rect.height() * 0.62, egui::FontFamily::Proportional),
        deco::ink_on_pair(ink.a, ink.b),
    );
}

/// Outline of `rect` with its own radius per corner (top-left, top-right, bottom-right,
/// bottom-left), clockwise from the top-left; a zero radius keeps the corner square.
pub fn corners_rect(rect: Rect, tl: f32, tr: f32, br: f32, bl: f32) -> Vec<Pos2> {
    const STEPS: usize = 6;
    let max = rect.width().min(rect.height()) / 2.0;
    let mut out = Vec::with_capacity(4 * (STEPS + 1));
    // (corner centre offset sign, start angle)
    let corners = [
        (
            tl.min(max),
            rect.left_top(),
            vec2(1.0, 1.0),
            std::f32::consts::PI,
        ),
        (
            tr.min(max),
            rect.right_top(),
            vec2(-1.0, 1.0),
            -std::f32::consts::FRAC_PI_2,
        ),
        (br.min(max), rect.right_bottom(), vec2(-1.0, -1.0), 0.0),
        (
            bl.min(max),
            rect.left_bottom(),
            vec2(1.0, -1.0),
            std::f32::consts::FRAC_PI_2,
        ),
    ];
    for (r, p, sign, a0) in corners {
        if r < 0.5 {
            out.push(p);
            continue;
        }
        let c = p + sign * r;
        for k in 0..=STEPS {
            let a = a0 + std::f32::consts::FRAC_PI_2 * k as f32 / STEPS as f32;
            out.push(c + vec2(a.cos(), a.sin()) * r);
        }
    }
    out
}
