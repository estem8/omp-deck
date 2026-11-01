//! Terminal font families: `term`, `term-bold`, `term-primary` (docs/notes/egui-fonts.md).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};

use egui::epaint::text::Blob;
use egui::epaint::text::{FallbackRequest, FontInsert, FontProvider};
use egui::{FontData, FontDefinitions, FontFamily};
use egui_system_font_provider::SystemFontProvider;

pub(crate) const FAMILY_TERM: &str = "term";
pub(crate) const FAMILY_BOLD: &str = "term-bold";
pub(crate) const FAMILY_PRIMARY: &str = "term-primary";

/// Result of [`install_fonts`].
#[derive(Clone, Debug)]
pub struct FontReport {
    pub primary: Option<PathBuf>,
    pub loaded: Vec<PathBuf>,
    pub missing: Vec<String>,
}

/// Fallback chain after the primary face: (key, family). Each family is resolved through
/// fontconfig, so the files may live anywhere on the system.
#[cfg(unix)]
const FALLBACKS: &[(&str, &str)] = &[
    ("deck-dejavu-mono", "DejaVu Sans Mono"),
    ("deck-source-code-pro", "Source Code Pro"),
    ("deck-symbols2", "Noto Sans Symbols 2"),
    ("deck-symbols-vf", "Noto Sans Symbols"),
    ("deck-cjk-mono-sc", "Noto Sans Mono CJK SC"),
    ("deck-emoji-color", "Noto Color Emoji"),
    ("deck-emoji-mono", "Noto Emoji"),
];

/// Fallback chain after the primary face: (key, family), resolved through the installed-fonts
/// registry. CJK and other scripts come from the system font provider on demand.
#[cfg(windows)]
const FALLBACKS: &[(&str, &str)] = &[
    ("deck-segoe-symbol", "Segoe UI Symbol"),
    ("deck-segoe-emoji", "Segoe UI Emoji"),
    ("deck-segoe-ui", "Segoe UI"),
];

/// Primary face candidates on Windows, first installed wins.
#[cfg(windows)]
const PRIMARY_FAMILIES: &[&str] = &["Cascadia Mono", "Consolas", "Courier New"];

/// File bytes shared process-wide, so repeated installs (or the same file under two families)
/// never read the 32 MB CJK collection twice.
static FONT_BYTES: LazyLock<Mutex<HashMap<PathBuf, Blob>>> = LazyLock::new(Default::default);

fn load_blob(path: &Path) -> Option<Blob> {
    let mut cache = match FONT_BYTES.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some(b) = cache.get(path) {
        return Some(b.clone());
    }
    match std::fs::read(path) {
        Ok(bytes) => {
            let blob: Blob = Arc::new(bytes);
            cache.insert(path.to_path_buf(), blob.clone());
            Some(blob)
        }
        Err(e) => {
            log::debug!("font {} unreadable: {e}", path.display());
            None
        }
    }
}

#[cfg(unix)]
fn fc_match(pattern: &str) -> Option<PathBuf> {
    let out = Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let p = PathBuf::from(s.trim());
    p.is_file().then_some(p)
}

/// Parses `fc-match -f '%{family}\n%{index}\n%{file}'` output; accepts it only when `family`
/// is among the matched face's families (fc-match always answers with *something*).
#[cfg(unix)]
fn parse_family_match(output: &str, family: &str) -> Option<(PathBuf, u32)> {
    let mut lines = output.lines();
    let (fams, index, file) = (lines.next()?, lines.next()?, lines.next()?);
    if !fams
        .split(',')
        .any(|f| f.trim().eq_ignore_ascii_case(family))
    {
        return None;
    }
    // The high 16 bits select a variable-font named instance; the face index is the low half.
    let index = index.trim().parse::<u32>().ok()? & 0xffff;
    let path = PathBuf::from(file.trim());
    path.is_file().then_some((path, index))
}

/// File and face index of the installed font whose family really is `family`.
#[cfg(unix)]
fn fc_family(family: &str) -> Option<(PathBuf, u32)> {
    let out = Command::new("fc-match")
        .args(["-f", "%{family}\n%{index}\n%{file}", family])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_family_match(std::str::from_utf8(&out.stdout).ok()?, family)
}

/// [`SystemFontProvider`] minus the `term-primary` family, which must stay primary-only:
/// `GlyphCache::fits_row` classifies glyphs against it and a discovered fallback would hide
/// which glyphs the primary face lacks.
struct UiSystemFonts(SystemFontProvider);

impl FontProvider for UiSystemFonts {
    fn font_for(&self, request: &FallbackRequest<'_>) -> Option<FontInsert> {
        if request.family == &FontFamily::Name(FAMILY_PRIMARY.into()) {
            return None;
        }
        self.0.font_for(request)
    }
}

/// Faces from [`FALLBACKS`] appended (in this order) to egui's `Proportional` and `Monospace`
/// families after their defaults, so UI text can show emoji, symbols and CJK.
#[cfg(unix)]
const UI_FALLBACKS: &[&str] = &[
    "deck-symbols2",
    "deck-symbols-vf",
    "deck-cjk-mono-sc",
    "deck-emoji-color",
    "deck-emoji-mono",
    "deck-dejavu-mono",
];

/// Faces from [`FALLBACKS`] appended (in this order) to egui's `Proportional` and `Monospace`
/// families after their defaults, so UI text can show symbols and emoji.
#[cfg(windows)]
const UI_FALLBACKS: &[&str] = &["deck-segoe-symbol", "deck-segoe-emoji"];

/// A font file and the face index inside it.
type FaceFile = (PathBuf, u32);

/// Faces found for the terminal families.
struct Discovered {
    primary: Option<FaceFile>,
    bold: Option<FaceFile>,
    /// One entry per [`FALLBACKS`] family.
    fallbacks: Vec<Option<FaceFile>>,
    /// What the primary lookup asked for (reported as missing when nothing was found).
    primary_query: &'static str,
}

/// fontconfig: the `monospace` alias (regular and bold) plus one `fc-match` per fallback
/// family, concurrently (these spawns delay the first frame).
#[cfg(unix)]
fn discover() -> Discovered {
    let fallbacks = std::thread::scope(|scope| {
        let handles: Vec<_> = FALLBACKS
            .iter()
            .map(|(_, family)| scope.spawn(|| fc_family(family)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().ok().flatten())
            .collect()
    });
    Discovered {
        primary: fc_match("monospace").map(|p| (p, 0)),
        bold: fc_match("monospace:bold").map(|p| (p, 0)),
        fallbacks,
        primary_query: "fc-match monospace",
    }
}

/// The installed-fonts registry: the first of [`PRIMARY_FAMILIES`] (and its bold face) plus
/// the [`FALLBACKS`] families.
#[cfg(windows)]
fn discover() -> Discovered {
    let faces = windows_fonts::installed();
    let primary_family = PRIMARY_FAMILIES
        .iter()
        .find(|family| installed::find(&faces, family, false).is_some());
    Discovered {
        primary: primary_family.and_then(|family| installed::find(&faces, family, false)),
        bold: primary_family.and_then(|family| installed::find(&faces, family, true)),
        fallbacks: FALLBACKS
            .iter()
            .map(|(_, family)| installed::find(&faces, family, false))
            .collect(),
        primary_query: "Cascadia Mono / Consolas / Courier New",
    }
}

/// Appends the loaded [`UI_FALLBACKS`] faces to `family`, keeping its existing faces first.
fn append_ui_fallbacks(defs: &mut FontDefinitions, family: FontFamily, loaded: &[String]) {
    let list = defs.families.entry(family).or_default();
    for key in UI_FALLBACKS {
        if loaded.iter().any(|k| k == key) && !list.iter().any(|k| k == key) {
            list.push((*key).to_owned());
        }
    }
}

/// Installs the terminal font families into egui (call once, from the eframe creator).
/// Starts from `FontDefinitions::default()`; egui's UI families keep their default faces first
/// and only gain [`UI_FALLBACKS`] at the end (emoji/symbols/CJK in titles). Glyphs the curated
/// chain lacks come on demand from a system font provider that skips `term-primary`. Also sets
/// the text smoothing, see [`smooth_text`].
pub fn install_fonts(ctx: &egui::Context) -> FontReport {
    let mut defs = FontDefinitions::default();
    let mut report = FontReport {
        primary: None,
        loaded: Vec::new(),
        missing: Vec::new(),
    };

    let add = |defs: &mut FontDefinitions,
               report: &mut FontReport,
               key: &str,
               path: &Path,
               index: u32|
     -> bool {
        match load_blob(path) {
            Some(blob) => {
                defs.font_data
                    .insert(key.to_owned(), Arc::new(FontData::from_blob(blob, index)));
                if !report.loaded.iter().any(|p| p == path) {
                    report.loaded.push(path.to_path_buf());
                }
                true
            }
            None => {
                report.missing.push(path.display().to_string());
                false
            }
        }
    };

    let found = discover();
    let primary_key = match &found.primary {
        Some((p, index)) if add(&mut defs, &mut report, "deck-primary", p, *index) => {
            report.primary = Some(p.clone());
            Some("deck-primary".to_owned())
        }
        Some(_) => None,
        None => {
            report.missing.push(found.primary_query.to_owned());
            None
        }
    };
    let bold_key = match &found.bold {
        Some(bold)
            if Some(bold) != found.primary.as_ref()
                && add(&mut defs, &mut report, "deck-primary-bold", &bold.0, bold.1) =>
        {
            Some("deck-primary-bold".to_owned())
        }
        _ => primary_key.clone(),
    };

    let mut chain: Vec<String> = Vec::new();
    for ((key, family), hit) in FALLBACKS.iter().zip(found.fallbacks) {
        match hit {
            Some((path, index)) if add(&mut defs, &mut report, key, &path, index) => {
                chain.push((*key).to_owned());
            }
            _ => report.missing.push((*family).to_owned()),
        }
    }
    // Last resort: egui's bundled monospace stack (Hack + emoji) so the families always resolve.
    let builtin = defs
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let primary = primary_key
        .clone()
        .or_else(|| chain.first().cloned())
        .or_else(|| builtin.first().cloned());

    let build = |first: Option<&String>| -> Vec<String> {
        let mut v: Vec<String> = first.into_iter().cloned().collect();
        for k in chain.iter().chain(builtin.iter()) {
            if !v.contains(k) {
                v.push(k.clone());
            }
        }
        v
    };
    defs.families.insert(
        FontFamily::Name(FAMILY_TERM.into()),
        build(primary.as_ref()),
    );
    defs.families.insert(
        FontFamily::Name(FAMILY_BOLD.into()),
        build(bold_key.as_ref().or(primary.as_ref())),
    );
    defs.families.insert(
        FontFamily::Name(FAMILY_PRIMARY.into()),
        primary.into_iter().collect(),
    );
    // Same `font_data` entries (keyed by name), so no font file is loaded twice.
    append_ui_fallbacks(&mut defs, FontFamily::Proportional, &chain);
    append_ui_fallbacks(&mut defs, FontFamily::Monospace, &chain);

    ctx.set_fonts(defs);
    // Replaces (never appends), so repeated installs keep exactly one provider.
    ctx.set_font_providers(vec![Arc::new(UiSystemFonts(SystemFontProvider::new()))]);
    smooth_text(ctx);
    report
}

/// Softer text than egui's defaults (user-picked from a rendered comparison): unhinted outlines,
/// sub-pixel positioned UI glyphs (terminal cells sit on whole pixels, so they stay unblurred), and
/// on dark styles a lighter coverage curve than egui's heavy `2c − c²`. Light styles keep egui's
/// linear coverage, which is already the light end for dark-on-light text.
fn smooth_text(ctx: &egui::Context) {
    ctx.all_styles_mut(|s| {
        s.visuals.text_options.font_hinting = false;
        s.visuals.text_options.subpixel_binning = true;
    });
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.visuals.text_options.color_transfer_function =
            egui::epaint::FontColorTransferFunction::Gamma(0.75);
    });
}

/// The Windows installed-fonts registry format (`HKLM`/`HKCU\SOFTWARE\Microsoft\Windows NT\
/// CurrentVersion\Fonts`): value names list the faces of a file, data is the file name
/// (relative to the system fonts dir) or a full path (per-user fonts). Pure, so it is tested
/// on every platform.
#[cfg(any(windows, test))]
mod installed {
    use std::path::{Path, PathBuf};

    use super::FaceFile;

    /// One installed face.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct Face {
        /// Full face name, e.g. `Consolas Bold`.
        pub(super) name: String,
        pub(super) path: PathBuf,
        /// Face index inside a collection (position in the value name).
        pub(super) index: u32,
    }

    /// Faces of one registry value: `Cambria & Cambria Math (TrueType)` = `cambria.ttc` gives
    /// `Cambria` (index 0) and `Cambria Math` (index 1). A relative file name resolves against
    /// `fonts_dir` (skipped when that is unknown).
    pub(super) fn parse_entry(name: &str, data: &str, fonts_dir: Option<&Path>) -> Vec<Face> {
        let data = data.trim_end_matches('\0').trim();
        if data.is_empty() {
            return Vec::new();
        }
        let file = Path::new(data);
        let path = if file.is_absolute() {
            file.to_path_buf()
        } else {
            match fonts_dir {
                Some(dir) => dir.join(file),
                None => return Vec::new(),
            }
        };
        // Strip the format suffix: `(TrueType)`, `(OpenType)`, …
        let names = match name.trim_end().strip_suffix(')') {
            Some(rest) => rest.rsplit_once(" (").map_or(name, |(faces, _)| faces),
            None => name,
        };
        names
            .split(" & ")
            .map(str::trim)
            .zip(0u32..)
            .filter(|(face, _)| !face.is_empty())
            .map(|(face, index)| Face {
                name: face.to_owned(),
                path: path.clone(),
                index,
            })
            .collect()
    }

    /// The regular (`family` or `family Regular`) or bold (`family Bold`) face of `family`
    /// whose file exists. Earlier entries win (per-user fonts are listed first).
    pub(super) fn find(faces: &[Face], family: &str, bold: bool) -> Option<FaceFile> {
        let style_is = |rest: &str, style: &str| {
            rest.strip_prefix(' ')
                .is_some_and(|s| s.eq_ignore_ascii_case(style))
        };
        faces
            .iter()
            .filter(|face| {
                let name = face.name.as_str();
                let Some(rest) = name
                    .get(..family.len())
                    .filter(|head| head.eq_ignore_ascii_case(family))
                    .map(|_| &name[family.len()..])
                else {
                    return false;
                };
                if bold {
                    style_is(rest, "Bold")
                } else {
                    rest.is_empty() || style_is(rest, "Regular")
                }
            })
            .find(|face| face.path.is_file())
            .map(|face| (face.path.clone(), face.index))
    }
}

/// Reads the installed-fonts registry (small FFI wrappers over advapi32/kernel32).
#[cfg(windows)]
mod windows_fonts {
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ, RegCloseKey,
        RegEnumValueW, RegOpenKeyExW,
    };
    use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

    use super::installed::{Face, parse_entry};

    const FONTS_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts";
    /// Longest registry value name (16383 chars + NUL).
    const MAX_NAME: usize = 16_384;
    /// Font file paths beyond this many UTF-16 units are skipped.
    const MAX_DATA: usize = 2_048;

    /// Every installed face: per-user fonts first, then the machine's.
    pub(super) fn installed() -> Vec<Face> {
        let dir = fonts_dir();
        let mut faces = Vec::new();
        for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
            read_key(root, dir.as_deref(), &mut faces);
        }
        faces
    }

    /// `%WINDIR%\Fonts`.
    fn fonts_dir() -> Option<PathBuf> {
        let mut buf = [0u16; 512];
        // SAFETY: the buffer is valid for `buf.len()` UTF-16 units; the return value is the
        // length written (0 on failure, > len when too small).
        let len = unsafe { GetWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
        (len > 0 && len < buf.len())
            .then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len])).join("Fonts"))
    }

    /// Owned registry key, closed on drop.
    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the handle came from a successful RegOpenKeyExW and is closed once.
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn read_key(root: HKEY, fonts_dir: Option<&std::path::Path>, faces: &mut Vec<Face>) {
        let subkey: Vec<u16> = FONTS_KEY.encode_utf16().chain([0]).collect();
        let mut hkey: HKEY = std::ptr::null_mut();
        // SAFETY: `subkey` is NUL-terminated; `hkey` receives the opened handle.
        let rc = unsafe { RegOpenKeyExW(root, subkey.as_ptr(), 0, KEY_READ, &mut hkey) };
        if rc != ERROR_SUCCESS {
            return;
        }
        let key = Key(hkey);
        let mut name = vec![0u16; MAX_NAME];
        let mut data = vec![0u16; MAX_DATA];
        for index in 0u32.. {
            let mut name_len = name.len() as u32;
            let mut data_bytes = (data.len() * 2) as u32;
            let mut kind = 0u32;
            // SAFETY: every buffer is valid for the length passed alongside it (name in
            // UTF-16 units, data in bytes); the reserved pointer must be null.
            let rc = unsafe {
                RegEnumValueW(
                    key.0,
                    index,
                    name.as_mut_ptr(),
                    &mut name_len,
                    std::ptr::null(),
                    &mut kind,
                    data.as_mut_ptr().cast(),
                    &mut data_bytes,
                )
            };
            match rc {
                ERROR_SUCCESS => {}
                // Oversized path: skip this value.
                ERROR_MORE_DATA => continue,
                // ERROR_NO_MORE_ITEMS or a failure: done.
                _ => break,
            }
            if kind != REG_SZ && kind != REG_EXPAND_SZ {
                continue;
            }
            let name = String::from_utf16_lossy(&name[..name_len as usize]);
            let units = (data_bytes as usize / 2).min(data.len());
            let value = String::from_utf16_lossy(&data[..units]);
            faces.extend(parse_entry(&name, &value, fonts_dir));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_fallbacks_append_after_defaults_only_when_loaded() {
        let mut defs = FontDefinitions::default();
        let before = defs.families[&FontFamily::Proportional].clone();
        let first = UI_FALLBACKS[0];
        let last = UI_FALLBACKS[UI_FALLBACKS.len() - 1];
        let loaded: Vec<String> = [last, "deck-not-a-ui-fallback", first]
            .map(String::from)
            .to_vec();
        append_ui_fallbacks(&mut defs, FontFamily::Proportional, &loaded);
        append_ui_fallbacks(&mut defs, FontFamily::Proportional, &loaded);
        let after = &defs.families[&FontFamily::Proportional];
        assert_eq!(&after[..before.len()], &before[..]);
        assert_eq!(&after[before.len()..], [first, last]);
    }

    #[cfg(unix)]
    #[test]
    fn family_match_requires_the_family_and_masks_instance_bits() {
        let file = std::env::current_exe().unwrap();
        let out = |fams: &str, idx: &str| format!("{fams}\n{idx}\n{}", file.display());
        let hit = parse_family_match(
            &out("Noto Sans Mono CJK SC,Noto", "65538"),
            "noto sans mono cjk sc",
        );
        assert_eq!(hit, Some((file.clone(), 2)));
        assert_eq!(
            parse_family_match(&out("DejaVu Sans", "0"), "Noto Emoji"),
            None
        );
    }

    #[test]
    fn registry_entries_parse_collections_and_paths() {
        use installed::{Face, parse_entry};
        let dir = std::env::temp_dir();
        let face = |name: &str, path: &Path, index| Face {
            name: name.to_owned(),
            path: path.to_path_buf(),
            index,
        };
        assert_eq!(
            parse_entry("Consolas Bold (TrueType)", "consolab.ttf", Some(&dir)),
            [face("Consolas Bold", &dir.join("consolab.ttf"), 0)]
        );
        assert_eq!(
            parse_entry(
                "Cambria & Cambria Math (TrueType)",
                "cambria.ttc\0",
                Some(&dir)
            ),
            [
                face("Cambria", &dir.join("cambria.ttc"), 0),
                face("Cambria Math", &dir.join("cambria.ttc"), 1),
            ]
        );
        // Per-user fonts carry a full path; relative ones need the fonts dir.
        let abs = dir.join("CascadiaMono.ttf");
        let abs_str = abs.to_string_lossy().into_owned();
        assert_eq!(
            parse_entry("Cascadia Mono Regular (TrueType)", &abs_str, None),
            [face("Cascadia Mono Regular", &abs, 0)]
        );
        assert!(parse_entry("Consolas (TrueType)", "consola.ttf", None).is_empty());
        assert!(parse_entry("Consolas (TrueType)", "", Some(&dir)).is_empty());
        // No format suffix: the whole name is the face.
        assert_eq!(
            parse_entry("Segoe UI Emoji", "seguiemj.ttf", Some(&dir)),
            [face("Segoe UI Emoji", &dir.join("seguiemj.ttf"), 0)]
        );
    }

    #[test]
    fn registry_lookup_matches_family_and_style() {
        use installed::{Face, find};
        let real = std::env::current_exe().unwrap();
        let gone = std::env::temp_dir().join("deck-term-no-such-font.ttf");
        let face = |name: &str, path: &PathBuf, index| Face {
            name: name.to_owned(),
            path: path.clone(),
            index,
        };
        let faces = [
            face("Segoe UI Symbol", &real, 0),
            face("Consolas Bold", &real, 1),
            face("Consolas", &gone, 0),
            face("consolas regular", &real, 2),
            face("Segoe UI", &real, 3),
        ];
        // `Consolas` itself is gone: the next regular face with an existing file wins.
        assert_eq!(find(&faces, "Consolas", false), Some((real.clone(), 2)));
        assert_eq!(find(&faces, "Consolas", true), Some((real.clone(), 1)));
        // A longer family name never matches a prefix of it, nor the other way round.
        assert_eq!(find(&faces, "Segoe UI", false), Some((real.clone(), 3)));
        assert_eq!(
            find(&faces, "Segoe UI Symbol", false),
            Some((real.clone(), 0))
        );
        assert_eq!(find(&faces, "Segoe", false), None);
        assert_eq!(find(&faces, "Courier New", false), None);
        assert_eq!(find(&faces, "Segoe UI", true), None);
    }
}
