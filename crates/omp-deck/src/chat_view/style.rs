//! Colours, fonts and sizes of the chat view, all derived from the active OmpDeck style (the 18
//! palettes, dark and light) — nothing here is omp's own palette.

use deck_core::config::UiFont;
use egui::{Color32, Context, FontFamily, FontId, Ui};

use crate::deco;
use crate::theme::{self, Palette};

/// Widest the conversation column gets (px).
pub const MEASURE: f32 = 880.0;
/// Reading size of prose (px) and its line height factor.
pub const PROSE: f32 = 15.0;
pub const PROSE_LINE: f32 = 1.6;
/// Smallest label size (px).
pub const LABEL: f32 = 11.0;

/// Tones omp puts on nodes (`tone` prop).
#[cfg(test)]
pub const TONES: [&str; 9] = [
    "neutral", "accent", "info", "success", "warning", "error", "pending", "muted", "user",
];

/// Everything a frame needs to colour and size chat nodes.
#[derive(Clone)]
pub struct Ink {
    pub p: &'static Palette,
    pub dark: bool,
    /// The accent gradient pair (cyan → violet in Blueprint).
    pub a: Color32,
    pub b: Color32,
    prose: FontFamily,
    mono: FontFamily,
    /// Terminal font size: mono blocks follow it.
    pub mono_size: f32,
    pub reduce_motion: bool,
}

/// The terminal's own font family when installed, else egui's monospace.
fn terminal_family(ctx: &Context) -> FontFamily {
    let term = FontFamily::Name("term".into());
    if ctx.fonts(|f| f.definitions().families.contains_key(&term)) {
        term
    } else {
        FontFamily::Monospace
    }
}

/// Motion is reduced when egui's animation time is zero (the app's reduce-motion switch).
pub fn reduce_motion(ctx: &Context) -> bool {
    ctx.global_style().animation_time <= 0.0
}

impl Ink {
    pub fn new(ui: &Ui, mono_size: f32) -> Ink {
        let p = Palette::of_ui(ui);
        let (a, b) = deco::accent_pair(p);
        let ctx = ui.ctx();
        Ink {
            p,
            dark: ui.visuals().dark_mode,
            a,
            b,
            prose: prose_family(theme::ui_font()),
            mono: terminal_family(ctx),
            mono_size: mono_size.clamp(FONT_MIN, FONT_MAX),
            reduce_motion: reduce_motion(ctx),
        }
    }

    /// Proportional reading face at `size` px.
    pub fn prose(&self, size: f32) -> FontId {
        FontId::new(size, self.prose.clone())
    }

    /// The terminal font at the terminal's size scaled by `k` (never below 9 px).
    pub fn mono(&self, k: f32) -> FontId {
        FontId::new((self.mono_size * k).max(9.0), self.mono.clone())
    }

    /// The terminal font at a fixed `size`.
    pub fn mono_px(&self, size: f32) -> FontId {
        FontId::new(size, self.mono.clone())
    }

    /// Label face (mono where the style sets labels in mono) at `size` px (≥ [`LABEL`]).
    pub fn label(&self, size: f32) -> FontId {
        theme::label_font(size.max(LABEL))
    }

    /// Colour of an omp `tone`; unknown tones read as plain text.
    pub fn tone(&self, tone: &str) -> Color32 {
        match tone {
            "accent" | "info" => self.p.accent,
            "success" => self.p.ok,
            "warning" | "pending" => self.p.warn,
            "error" => self.p.danger,
            "muted" => self.p.weak,
            "user" => self.a,
            _ => self.p.text,
        }
    }

    /// [`Ink::tone`] for a glyph or line drawn thin on the card: the accent tones keep the
    /// contrast floor of [`Ink::accent_text`] (the plain accent is pale on light styles).
    pub fn glyph_tone(&self, tone: &str) -> Color32 {
        match tone {
            "accent" | "info" => self.accent_text(),
            t => self.tone(t),
        }
    }

    pub fn text(&self) -> Color32 {
        self.p.text
    }

    pub fn strong(&self) -> Color32 {
        self.p.text_strong
    }

    pub fn dim(&self) -> Color32 {
        self.p.weak
    }

    /// Very quiet text (timestamps, hints).
    pub fn faint(&self) -> Color32 {
        theme::mix(self.p.weak, self.p.bg, 0.25)
    }

    /// Hairline colour of boxes at rest.
    pub fn line(&self) -> Color32 {
        theme::mix(self.p.stroke, self.b, if self.dark { 0.18 } else { 0.25 })
    }

    /// Body fill of cards and tool bodies.
    pub fn card(&self) -> Color32 {
        theme::mix(self.p.surface, self.b, if self.dark { 0.05 } else { 0.04 })
    }

    /// Header band gradient of cards (left, right), drawn with the slanted end.
    pub fn head(&self) -> (Color32, Color32) {
        let k = if self.dark { 0.20 } else { 0.16 };
        (
            theme::mix(self.p.surface, self.a, k),
            theme::mix(self.p.surface, self.b, k * 0.8),
        )
    }

    /// Fill of the user's bubble (start, end of the diagonal gradient).
    pub fn bubble(&self) -> (Color32, Color32) {
        let k = if self.dark { 0.22 } else { 0.16 };
        (
            theme::mix(self.p.surface, self.b, k),
            theme::mix(self.p.surface, self.a, k * 0.7),
        )
    }

    /// Inline code / quiet chip fill.
    pub fn chip(&self) -> Color32 {
        theme::mix(
            self.p.surface,
            self.p.text,
            if self.dark { 0.08 } else { 0.06 },
        )
    }

    /// Tinted fill of a status colour (badges, approval strip).
    pub fn wash(&self, c: Color32) -> Color32 {
        theme::mix(self.p.surface, c, if self.dark { 0.14 } else { 0.12 })
    }

    /// Hairline of a status colour.
    pub fn edge(&self, c: Color32) -> Color32 {
        theme::mix(self.p.stroke, c, 0.55)
    }

    /// Gradient stops of the accent hairline (a → b).
    /// The second accent as small text: as is on dark, darkened toward the text colour on light so
    /// it stays legible on the pale washes.
    pub fn accent_text(&self) -> Color32 {
        if self.dark {
            self.b
        } else {
            theme::mix(self.b, self.p.text_strong, 0.55)
        }
    }

    pub fn rule_stops(&self) -> [(f32, Color32); 2] {
        [(0.0, self.a), (1.0, self.b)]
    }

    /// `c` pulled toward the strong text colour just far enough to reach WCAG contrast `min`
    /// against the opaque `bg` (unchanged when it already does): for text, outlines and icons
    /// drawn on a tinted surface.
    pub fn legible(&self, c: Color32, bg: Color32, min: f32) -> Color32 {
        (0..=10)
            .map(|i| theme::mix(c, self.p.text_strong, i as f32 / 10.0))
            .find(|c| super::highlight::contrast(*c, bg) >= min)
            .unwrap_or(self.p.text_strong)
    }
}

/// An [`Ink`] from a headless frame (tests).
#[cfg(test)]
pub(crate) fn test_ink() -> Ink {
    test_ink_sized(13.0)
}

/// [`test_ink`] with a terminal font size of `px`.
#[cfg(test)]
pub(crate) fn test_ink_sized(px: f32) -> Ink {
    let ctx = Context::default();
    let mut out = None;
    ctx.run_ui(egui::RawInput::default(), |ui| {
        out = Some(Ink::new(ui, px));
    })
    .textures_delta
    .clear();
    out.expect("ink built")
}

/// The reading face of the conversation: Inter while the UI font is left on Auto (the styles'
/// own chrome faces are mono or display faces that are tiring for paragraphs); an explicit UI
/// font choice is the user's call and prose follows it like the rest of the UI.
fn prose_family(font: UiFont) -> FontFamily {
    if font == UiFont::Auto {
        crate::ui_fonts::text_family(UiFont::Inter).unwrap_or(FontFamily::Proportional)
    } else {
        theme::family_for(theme::look(), theme::flat(), font)
    }
}

/// Terminal font size range of the app (`app.rs` clamps the setting to it); mono blocks follow
/// the terminal size, so the same range applies here.
pub const FONT_MIN: f32 = 8.0;
pub const FONT_MAX: f32 = 40.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn ink() -> Ink {
        crate::chat_view::style::test_ink()
    }

    #[test]
    fn prose_is_inter_only_while_the_ui_font_is_auto() {
        let inter = crate::ui_fonts::text_family(UiFont::Inter).unwrap_or(FontFamily::Proportional);
        assert_eq!(prose_family(UiFont::Auto), inter);
        assert_eq!(
            prose_family(UiFont::System),
            theme::family_for(theme::look(), theme::flat(), UiFont::System)
        );
        assert_eq!(prose_family(UiFont::System), FontFamily::Proportional);
    }

    #[test]
    fn mono_size_follows_the_terminals_range() {
        for (px, want) in [
            (2.0, FONT_MIN),
            (13.0, 13.0),
            (36.0, 36.0),
            (90.0, FONT_MAX),
        ] {
            assert_eq!(test_ink_sized(px).mono_size, want);
        }
    }

    #[test]
    fn every_tone_maps_to_a_theme_colour() {
        let ink = ink();
        for t in TONES {
            let c = ink.tone(t);
            assert_ne!(c, Color32::TRANSPARENT, "{t}");
        }
        assert_eq!(ink.tone("success"), ink.p.ok);
        assert_eq!(ink.tone("error"), ink.p.danger);
        assert_eq!(ink.tone("warning"), ink.p.warn);
        assert_eq!(ink.tone("pending"), ink.p.warn);
        assert_eq!(ink.tone("muted"), ink.p.weak);
        assert_eq!(ink.tone("accent"), ink.p.accent);
        assert_eq!(ink.tone("info"), ink.p.accent);
        assert_eq!(ink.tone("user"), ink.a);
        assert_eq!(ink.tone("neutral"), ink.p.text);
        assert_eq!(ink.tone("no-such-tone"), ink.p.text);
    }

    #[test]
    fn glyph_tones_keep_the_accent_contrast_floor_and_other_tones_are_unchanged() {
        let ink = ink();
        assert_eq!(ink.glyph_tone("accent"), ink.accent_text());
        assert_eq!(ink.glyph_tone("info"), ink.accent_text());
        for t in ["success", "error", "warning", "muted", "user", "neutral"] {
            assert_eq!(ink.glyph_tone(t), ink.tone(t), "{t}");
        }
    }

    #[test]
    fn labels_never_go_below_eleven_px() {
        assert!(ink().label(8.0).size >= LABEL);
    }
}
