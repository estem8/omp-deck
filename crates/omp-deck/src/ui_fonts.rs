//! UI font setup. Flat styles: bundled JetBrains Mono (variable, OFL) as the primary face of the
//! [`NEUTRAL_FAMILY`] family. Neon style: Adwaita Sans (fallback Cantarell, Noto Sans) as the
//! Proportional family. Every bundled face heads a UI text family of its own ([`text_family`]),
//! used by every style when that [`UiFont`] is chosen. [`finalize`] completes the Neutral family
//! and every text family with the whole Proportional chain, so their glyph coverage is a
//! superset of Neon's. Every bundled face also has a face-only preview family
//! ([`preview_family`]). The bundled Lucide icon font backs the `lucide` family (and is a
//! Proportional fallback so glyphs in label strings render). Must run after
//! `deck_term::install_fonts`, which replaces the whole definition set and owns the `term*`
//! families.

use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;
use std::sync::LazyLock;

use deck_core::config::UiFont;
use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
use egui::{Context, FontData, FontDefinitions, FontFamily};

/// Named family of the flat styles' UI text (JetBrains Mono first, then the system faces).
pub const NEUTRAL_FAMILY: &str = "ui-neutral";

/// Every bundled UI face: (choice, font-data key = name of its preview family, bytes).
static FACES: [(UiFont, &str, &[u8]); 18] = [
    (
        UiFont::JetBrainsMono,
        "ui-font-jetbrains-mono",
        include_bytes!("../assets/fonts/JetBrainsMono-VariableFont.ttf"),
    ),
    (
        UiFont::Geist,
        "ui-font-geist",
        include_bytes!("../assets/fonts/ui/Geist-Regular.ttf"),
    ),
    (
        UiFont::Inter,
        "ui-font-inter",
        include_bytes!("../assets/fonts/ui/Inter-Regular.ttf"),
    ),
    (
        UiFont::IbmPlexSans,
        "ui-font-ibm-plex-sans",
        include_bytes!("../assets/fonts/ui/IBMPlexSans-Regular.ttf"),
    ),
    (
        UiFont::Manrope,
        "ui-font-manrope",
        include_bytes!("../assets/fonts/ui/Manrope-Regular.ttf"),
    ),
    (
        UiFont::SpaceGrotesk,
        "ui-font-space-grotesk",
        include_bytes!("../assets/fonts/ui/SpaceGrotesk-Regular.ttf"),
    ),
    (
        UiFont::PlusJakartaSans,
        "ui-font-plus-jakarta-sans",
        include_bytes!("../assets/fonts/ui/PlusJakartaSans-Regular.ttf"),
    ),
    (
        UiFont::Onest,
        "ui-font-onest",
        include_bytes!("../assets/fonts/ui/Onest-Regular.ttf"),
    ),
    (
        UiFont::GeistMono,
        "ui-font-geist-mono",
        include_bytes!("../assets/fonts/ui/GeistMono-Regular.ttf"),
    ),
    (
        UiFont::RedHatMono,
        "ui-font-red-hat-mono",
        include_bytes!("../assets/fonts/ui/RedHatMono-Regular.ttf"),
    ),
    (
        UiFont::MartianMono,
        "ui-font-martian-mono",
        include_bytes!("../assets/fonts/ui/MartianMono-Regular.ttf"),
    ),
    (
        UiFont::Outfit,
        "ui-font-outfit",
        include_bytes!("../assets/fonts/ui/Outfit-Regular.ttf"),
    ),
    (
        UiFont::Sora,
        "ui-font-sora",
        include_bytes!("../assets/fonts/ui/Sora-Regular.ttf"),
    ),
    (
        UiFont::BricolageGrotesque,
        "ui-font-bricolage-grotesque",
        include_bytes!("../assets/fonts/ui/BricolageGrotesque-Regular.ttf"),
    ),
    (
        UiFont::Lexend,
        "ui-font-lexend",
        include_bytes!("../assets/fonts/ui/Lexend-Regular.ttf"),
    ),
    (
        UiFont::Figtree,
        "ui-font-figtree",
        include_bytes!("../assets/fonts/ui/Figtree-Regular.ttf"),
    ),
    (
        UiFont::Urbanist,
        "ui-font-urbanist",
        include_bytes!("../assets/fonts/ui/Urbanist-Regular.ttf"),
    ),
    (
        UiFont::VictorMono,
        "ui-font-victor-mono",
        include_bytes!("../assets/fonts/ui/VictorMono-Regular.ttf"),
    ),
];

/// Font-data key of the face of the flat styles' default UI font.
const NEUTRAL_FACE_KEY: &str = "ui-font-jetbrains-mono";
static LUCIDE: &[u8] = include_bytes!("../assets/fonts/lucide.ttf");
static NERD_SYMBOLS: &[u8] = include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf");

/// Name of the UI text family of `FACES[i]`: that face, then the whole Proportional chain.
fn text_family_name(i: usize) -> String {
    format!("ui-text-{}", FACES[i].0.key())
}

/// Index into [`FACES`] of `font`'s bundled face (`None` for `Auto` and `System`).
fn face(font: UiFont) -> Option<usize> {
    FACES.iter().position(|(f, ..)| *f == font)
}

/// UI text family of an explicitly chosen bundled `font` (`None` for `Auto` and `System`).
pub fn text_family(font: UiFont) -> Option<FontFamily> {
    static TEXT: LazyLock<Vec<FontFamily>> = LazyLock::new(|| {
        (0..FACES.len())
            .map(|i| FontFamily::Name(text_family_name(i).into()))
            .collect()
    });
    face(font).map(|i| TEXT[i].clone())
}

/// Family to set `font`'s name in for the picker: its bundled face alone, else (`Auto`,
/// `System`) the family that choice gives the active style.
pub fn preview_family(font: UiFont) -> FontFamily {
    static PREVIEWS: LazyLock<[FontFamily; 18]> =
        LazyLock::new(|| FACES.map(|(_, key, _)| FontFamily::Name(key.into())));
    match face(font) {
        Some(i) => PREVIEWS[i].clone(),
        None => crate::theme::family_for(crate::theme::look(), crate::theme::flat(), font),
    }
}

/// Candidates in preference order: (family, well-known file paths).
#[cfg(unix)]
const CANDIDATES: &[(&str, &[&str])] = &[
    (
        "Adwaita Sans",
        &["/usr/share/fonts/adwaita-sans-fonts/AdwaitaSans-Regular.ttf"],
    ),
    (
        "Cantarell",
        &[
            "/usr/share/fonts/abattis-cantarell-vf-fonts/Cantarell-VF.otf",
            "/usr/share/fonts/abattis-cantarell-fonts/Cantarell-Regular.otf",
        ],
    ),
    (
        "Noto Sans",
        &["/usr/share/fonts/google-noto/NotoSans-Regular.ttf"],
    ),
];

/// Windows system UI faces, newest first (`SegUIVar` ships with Windows 11).
#[cfg(not(unix))]
const CANDIDATES: &[(&str, &[&str])] = &[
    ("Segoe UI Variable", &[r"C:\Windows\Fonts\SegUIVar.ttf"]),
    ("Segoe UI", &[r"C:\Windows\Fonts\segoeui.ttf"]),
    ("Segoe UI Symbol", &[r"C:\Windows\Fonts\seguisym.ttf"]),
];
/// `fc-match` hit whose family really is `family` (fc-match always answers with *something*).
#[cfg(unix)]
fn fc_lookup(family: &str) -> Option<PathBuf> {
    let out = Command::new("fc-match")
        .args(["-f", "%{family}\n%{file}", family])
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let (fam, file) = text.split_once('\n')?;
    fam.split(',')
        .any(|f| f.trim().eq_ignore_ascii_case(family))
        .then(|| PathBuf::from(file.trim()))
}

/// No fontconfig on Windows: only the well-known file paths are probed.
#[cfg(not(unix))]
fn fc_lookup(_family: &str) -> Option<PathBuf> {
    None
}

fn load_one(family: &str, paths: &[&str]) -> Option<Vec<u8>> {
    paths
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .or_else(|| fc_lookup(family))
        .and_then(|p| std::fs::read(p).ok())
}

/// Font bytes of every available system UI candidate, in preference order.
pub struct UiFonts {
    faces: Vec<(&'static str, Vec<u8>)>,
}

impl UiFonts {
    /// Resolves and reads the candidates. Blocking (fc-match, file reads): call before the
    /// event loop starts, never from a frame.
    pub fn load() -> UiFonts {
        let faces = CANDIDATES
            .iter()
            .filter_map(|(family, paths)| {
                let bytes = load_one(family, paths);
                if bytes.is_none() {
                    log::debug!("ui font {family}: not found");
                }
                bytes.map(|b| (*family, b))
            })
            .collect();
        UiFonts { faces }
    }

    /// No system faces (headless tests: the bundled faces only).
    #[cfg(test)]
    pub fn none() -> UiFonts {
        UiFonts { faces: Vec::new() }
    }
}

fn neutral_family() -> FontFamily {
    FontFamily::Name(NEUTRAL_FAMILY.into())
}

/// Installs bundled UI and icon faces, plus available system UI faces. Nerd Font Symbols supplies
/// private-use glyphs commonly present in session titles; Lucide supplies OmpDeck's app icons.
/// The first system face is the primary Proportional face; all later faces become glyph fallbacks.
pub fn install(ctx: &Context, fonts: UiFonts) {
    for (i, (_, key, bytes)) in FACES.iter().enumerate() {
        let mut families = vec![
            InsertFontFamily {
                family: FontFamily::Name((*key).into()),
                priority: FontPriority::Highest,
            },
            InsertFontFamily {
                family: FontFamily::Name(text_family_name(i).into()),
                priority: FontPriority::Highest,
            },
        ];
        if i == 0 {
            families.push(InsertFontFamily {
                family: neutral_family(),
                priority: FontPriority::Highest,
            });
        }
        ctx.add_font(FontInsert::new(key, FontData::from_static(bytes), families));
    }
    for (i, (family, bytes)) in fonts.faces.into_iter().enumerate() {
        let priority = if i == 0 {
            FontPriority::Highest
        } else {
            FontPriority::Lowest
        };
        log::info!("ui font {family} ({priority:?})");
        ctx.add_font(FontInsert::new(
            &format!("ui-{i}"),
            FontData::from_owned(bytes),
            vec![InsertFontFamily {
                family: FontFamily::Proportional,
                priority,
            }],
        ));
    }
    let mut lucide = FontData::from_static(LUCIDE);
    lucide.tweak.y_offset_factor = 0.0;
    ctx.add_font(FontInsert::new(
        "lucide",
        lucide,
        vec![
            InsertFontFamily {
                family: FontFamily::Proportional,
                priority: FontPriority::Lowest,
            },
            InsertFontFamily {
                family: FontFamily::Name(crate::icons::FONT_FAMILY.into()),
                priority: FontPriority::Highest,
            },
        ],
    ));
    ctx.add_font(FontInsert::new(
        "nerd-font-symbols",
        FontData::from_static(NERD_SYMBOLS),
        vec![InsertFontFamily {
            family: FontFamily::Proportional,
            priority: FontPriority::Lowest,
        }],
    ));
}

/// `first` followed by every other face of the Proportional chain `prop`, in order.
fn chain(first: &str, prop: &[String]) -> Vec<String> {
    std::iter::once(first.to_owned())
        .chain(prop.iter().filter(|k| *k != first).cloned())
        .collect()
}

/// Makes the Neutral family and every bundled face's UI text family that face followed by the full
/// Proportional chain (system UI faces, deck-term symbols, emoji, Lucide, Nerd Font Symbols), so no
/// bundled choice covers fewer glyphs than the default. Call once after queued font inserts settle.
/// Font choices are prepared once; runtime `set_fonts` would rebuild glyph atlases under cached galleys.
pub fn finalize(ctx: &Context) {
    let mut defs: FontDefinitions = ctx.fonts(|f| f.definitions().clone());
    let prop = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut changed = false;
    let wanted = std::iter::once((neutral_family(), NEUTRAL_FACE_KEY)).chain(
        FACES
            .iter()
            .enumerate()
            .map(|(i, (_, key, _))| (FontFamily::Name(text_family_name(i).into()), *key)),
    );
    for (family, first) in wanted {
        let chain = chain(first, &prop);
        if defs.families.get(&family) != Some(&chain) {
            defs.families.insert(family, chain);
            changed = true;
        }
    }
    if changed {
        ctx.set_fonts(defs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_choice_has_one_face() {
        for font in UiFont::ALL {
            let bundled = FACES.iter().filter(|(f, ..)| *f == font).count();
            let expected = usize::from(!matches!(font, UiFont::Auto | UiFont::System));
            assert_eq!(bundled, expected, "{font:?}");
        }
        assert_eq!(
            (FACES[0].0, FACES[0].1),
            (UiFont::JetBrainsMono, NEUTRAL_FACE_KEY)
        );
    }

    /// Every bundled face leads its UI text family and the whole Proportional chain follows it,
    /// so an explicit font never covers fewer glyphs than the style default.
    #[test]
    fn text_families_are_the_face_then_the_proportional_chain() {
        let ctx = Context::default();
        install(&ctx, UiFonts::none());
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        finalize(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        let defs = ctx.fonts(|f| f.definitions().clone());
        let prop = &defs.families[&FontFamily::Proportional];
        let neutral = &defs.families[&neutral_family()];
        assert_eq!(neutral[0], NEUTRAL_FACE_KEY);
        assert_eq!(&neutral[1..], prop.as_slice());
        for (font, key, _) in &FACES {
            let Some(family) = text_family(*font) else {
                panic!("{font:?} has no text family");
            };
            let chain = &defs.families[&family];
            assert_eq!(chain[0], *key);
            assert_eq!(&chain[1..], prop.as_slice(), "{font:?}");
        }
    }
    #[test]
    fn nerd_font_private_use_glyphs_resolve_through_proportional_fallback() {
        let ctx = Context::default();
        install(&ctx, UiFonts::none());
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        finalize(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        let width = ctx.fonts_mut(|fonts| {
            fonts.glyph_width(
                &egui::FontId::new(14.0, FontFamily::Proportional),
                '\u{f082b}',
            )
        });
        assert!(width > 0.0, "Nerd Font U+F082B must resolve to a glyph");
    }
}
