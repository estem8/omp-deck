# egui/epaint 0.37 — rendering a monospace terminal grid

All paths relative to `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/` (`epaint-0.37.0/src/text/…`, `egui-0.37.0/src/…`, `eframe-0.37.0/src/…`).
"EXP" = observed in my throwaway eframe 0.37 app (Xvfb :99, `LIBGL_ALWAYS_SOFTWARE=1`, deleted afterwards). Screenshot: `/tmp/omp-deck-notes/egui-fonts-screenshot.png`.

**BIG CHANGE vs egui ≤0.33 memory: epaint 0.37 has a completely new text stack.** No ab_glyph, no `FontImpl`, no `Fonts::glyph`. Shaping = `harfrust`, outlines/hinting = `skrifa`, rasterisation = `vello_cpu` (epaint Cargo.toml `[dependencies.skrifa]`/`[dependencies.vello_cpu]`; `font_face.rs:155` `vello_cpu::RenderContext::new`, `text_layout.rs:1605` `harfrust::ShapeOptions::new()`).

---------------------------------------------------------------------------------------------------
## 1. Loading fonts

```rust
use egui::{FontData, FontDefinitions, FontFamily, FontId, FontInsert, InsertFontFamily, FontPriority}; // all re-exported at egui root (egui lib.rs:461)
use egui::epaint::text::{LayoutJob, TextFormat, VariationCoords, FontTweak}; // LayoutJob is NOT at egui root (EXP: E0432); use epaint::text or egui::text::LayoutJob
use std::sync::Arc;
```
* `FontDefinitions { pub font_data: BTreeMap<String, Arc<FontData>>, pub families: BTreeMap<FontFamily, Vec<String>> }` (`font_definitions.rs:51,59`). **Yes: `Arc<FontData>`.**
* `FontData { pub font: Blob /*Arc<dyn AsRef<[u8]> + Send + Sync>*/, pub index: u32, pub tweak: FontTweak }` (`font_data.rs:~16-27`). **`index` exists → `.ttc` supported**: `let mut d = FontData::from_owned(bytes); d.index = 2;` (EXP: Noto Sans Mono CJK **SC = index 2** rendered 漢字). Ctors: `from_static(&'static [u8])` `:31`, `from_owned(Vec<u8>)` `:35`, `from_blob(Blob, index: u32)` `:40` (use with a memmap `Arc<Mmap>` to avoid copying the 31 MB CJK .ttc), builder `.tweak(FontTweak)` `:54`. `variation_axes()` lists wght etc.
* `FontFamily::{Proportional, Monospace, Name(Arc<str>)}` (`font_id.rs:~60-80`); `FontFamily::Name("term".into())`. A family **must** be present in `families` or `family_key` panics/logs ("not bound to any fonts"). `FontId::new(size, family)`; `FontId { size, family }` has no weight/italic.
* **Fallback = the `Vec<String>` order** in `families[family]`; no automatic system fallback unless eframe feature `system_fonts` (OFF in our Cargo.toml; `eframe/Cargo.toml:92`, `epi_integration.rs:147-262`). Chars that no font has → tofu (below).
* `ctx.set_fonts(FontDefinitions)` (`context.rs:2505`): replaces everything, active next pass or immediately if no text laid out yet this pass. Call it in the eframe creator (`cc.egui_ctx.set_fonts(..)`). Compares definitions with `==` (reads all bytes) — don't call every frame.
* `ctx.add_font(FontInsert)` (`context.rs:2532`): `FontInsert { name: String, data: FontData, families: Vec<InsertFontFamily{ family, priority: FontPriority::{Highest,Lowest} }> }`; keeps existing fonts; **ignored if `name` already exists**. Also `ctx.add_font_provider(Arc<dyn FontProvider>)` (`context.rs:865`): `Fn(&FallbackRequest{cluster:&str,family:&FontFamily}) -> Option<FontInsert>` called at most once per (family,char) when no installed font has the char → could resolve fonts on demand (e.g. via `fc-match`). Not needed if we hardcode the chain.
* `FontTweak { scale: f32, y_offset_factor: f32, y_offset: f32 (pts), hinting: Option<bool>, hinting_target: HintingTarget, subpixel_binning: Option<bool>, coords: VariationCoords, thin_space_width: f32, tab_size: f32 }` (`font_tweak.rs:3-62`). **No `baseline_offset_factor`.** `scale` and y-offsets are "visual only"... but note `styled_metrics` (`font_face.rs:592-628`) multiplies font size by `tweak.scale` for ascent/advance too, so `scale` DOES change that face's advance.
* Variable fonts work: `TextFormat.coords = VariationCoords::new([(b"wght", 700.0)])` (`text_layout_types.rs:418-433`).

### Recommended definitions (all verified working in EXP)
```rust
fn load(p: &str, index: u32) -> Arc<FontData> { let mut d = FontData::from_owned(std::fs::read(p).unwrap()); d.index = index; Arc::new(d) }
let mut fd = FontDefinitions::empty();            // no Hack/Ubuntu bundled
fd.font_data.insert("noto-mono".into(),      load("/usr/share/fonts/google-noto/NotoSansMono-Regular.ttf", 0));
fd.font_data.insert("noto-mono-bold".into(), load("/usr/share/fonts/google-noto/NotoSansMono-Bold.ttf", 0));
fd.font_data.insert("dejavu-mono".into(),    load("/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf", 0));
fd.font_data.insert("scp".into(),            load("/usr/share/fonts/adobe-source-code-pro-fonts/SourceCodePro-Regular.otf", 0)); // has U+E0A0-E0B3 powerline
fd.font_data.insert("symbols2".into(),       load("/usr/share/fonts/google-noto/NotoSansSymbols2-Regular.ttf", 0)); // braille, ⏵ etc.
fd.font_data.insert("symbols-vf".into(),     load("/usr/share/fonts/google-noto-vf/NotoSansSymbols[wght].ttf", 0));
fd.font_data.insert("cjk-mono-sc".into(),    load("/usr/share/fonts/google-noto-sans-mono-cjk-vf-fonts/NotoSansMonoCJK-VF.ttc", 2)); // 0 JP,1 KR,2 SC,3 TC,4 HK
fd.font_data.insert("emoji-color".into(),    load("/usr/share/fonts/google-noto-color-emoji-fonts/Noto-COLRv1.ttf", 0)); // needs epaint `color_fonts`
fd.font_data.insert("emoji-mono".into(),     load("/usr/share/fonts/google-noto-emoji-fonts/NotoEmoji-Regular.ttf", 0));
let fallbacks = ["dejavu-mono","scp","symbols2","symbols-vf","cjk-mono-sc","emoji-color","emoji-mono"];
let mk = |first: &str| -> Vec<String> { std::iter::once(first).chain(fallbacks).map(String::from).collect() };
fd.families.insert(FontFamily::Name("term".into()),      mk("noto-mono"));
fd.families.insert(FontFamily::Name("term-bold".into()), mk("noto-mono-bold"));
fd.families.insert(FontFamily::Name("term-primary".into()), vec!["noto-mono".into()]); // ONLY for has_glyph() queries (is char in primary face?)
fd.families.insert(FontFamily::Monospace,    vec!["noto-mono".into()]);   // egui requires these two keys to exist
fd.families.insert(FontFamily::Proportional, vec!["noto-mono".into()]);   // (or keep real UI fonts here)
ctx.set_fonts(fd);
```
(If UI needs egui's default proportional font, start from `FontDefinitions::default()` (default_fonts feature: Hack, Ubuntu-Light, egui-icons) and just `insert` the term families.)

---------------------------------------------------------------------------------------------------
## 2. Fonts on this box (exact paths; `fc-match`/`fc-list`, observed)
> **Update:** the paths below are the Fedora dev box's, kept as facts about those fonts. deck-term does not hardcode them: each fallback (`FALLBACKS` in `fonts.rs`) is resolved by family name through `fc-match` (accepted only if the matched face's family list contains that name; `%{index}` masked to its low 16 bits gives the `.ttc` face), so any distro layout works (CBDT `NotoColorEmoji.ttf` on Ubuntu renders in colour via epaint `color_fonts`). Glyphs the curated chain still lacks (e.g. braille from DejaVu Sans in UI titles) come from a `egui_system_font_provider` (fontique) installed in `install_fonts`, wrapped to return nothing for `term-primary` so that family stays primary-only; eframe's own `system_font_fallback` stays off.

* `fc-match -f '%{file}\n' monospace` → `/usr/share/fonts/google-noto/NotoSansMono-Regular.ttf`; `fc-match 'Noto Sans Mono'` → same.
* Noto Sans Mono: static `/usr/share/fonts/google-noto/NotoSansMono-{Thin,ExtraLight,Light,Regular,Medium,SemiBold,Bold,ExtraBold,Black}.ttf` (+Condensed/SemiCondensed/ExtraCondensed variants). **Bold exists** (`NotoSansMono-Bold.ttf`). **No Italic/Oblique** exists. Variable: `/usr/share/fonts/google-noto-vf/NotoSansMono[wght].ttf` (wght axis; advance identical for wght 100/400/700 — EXP: glyph x identical).
* Italic-capable monospace alternatives: `/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono{,-Bold,-Oblique,-BoldOblique}.ttf`; `/usr/share/fonts/source-foundry-hack-fonts/Hack-{Regular,Bold,Italic,BoldItalic}.ttf`; `/usr/share/fonts/liberation-mono-fonts/LiberationMono-{Regular,Bold,Italic,BoldItalic}.ttf`; `/usr/share/fonts/adwaita-mono-fonts/AdwaitaMono-{Regular,Bold,Italic,BoldItalic}.ttf`; `/usr/share/fonts/adobe-source-code-pro-fonts/SourceCodePro-*.otf` (+It). 
* Noto Sans Symbols 2: `/usr/share/fonts/google-noto/NotoSansSymbols2-Regular.ttf` (has braille U+2800–28FF; advance 1.17 cells — NOT monospace). Noto Sans Symbols (v1): only variable `/usr/share/fonts/google-noto-vf/NotoSansSymbols[wght].ttf`. Also `/usr/share/fonts/gdouros-symbola/Symbola.ttf`.
* Emoji: `/usr/share/fonts/google-noto-color-emoji-fonts/Noto-COLRv1.ttf` (**COLRv1, not CBDT**; 5 MB) and monochrome `/usr/share/fonts/google-noto-emoji-fonts/NotoEmoji-Regular.ttf`.
* CJK (all `.ttc`, magic `ttcf`, 5 faces JP=0,KR=1,SC=2,TC=3,HK=4): `/usr/share/fonts/google-noto-sans-mono-cjk-vf-fonts/NotoSansMonoCJK-VF.ttc` (mono, 31.9 MB), `/usr/share/fonts/google-noto-sans-cjk-vf-fonts/NotoSansCJK-VF.ttc`, `/usr/share/fonts/google-noto-serif-cjk-vf-fonts/NotoSerifCJK-VF.ttc`; non-ttc: `/usr/share/fonts/google-droid-sans-fonts/DroidSansFallbackFull.ttf`.
* Powerline (U+E0A0..E0B3): only `adobe-source-code-pro-fonts/SourceCodePro-*.otf` (`fc-list ':charset=e0b0'`). **No Nerd Font installed** (U+F101, U+F015 → no font → tofu). Recommend bundling/installing "Symbols Nerd Font Mono" later; it is a PUA font so just append to the chain.
* Braille U+28FF in: AdwaitaMono, DejaVu Sans (not DejaVu Sans *Mono*), Symbola, NotoSansSymbols2 — **not** in Noto Sans Mono.

---------------------------------------------------------------------------------------------------
## 3. Rasterizer + fallback behaviour
* Shaper: `harfrust` (`fonts.rs:~509 shape_buffer: harfrust::UnicodeBuffer`, `text_layout.rs:1572-1606 shape_text`) with **default `ShapeOptions::new()`: no way to disable kern/liga/calt** via public API. Outlines: `skrifa` (+ optional autohint `autohint_shaping`), AA raster: `vello_cpu` fill_path (`font_face.rs:153-171`). Output white coverage → atlas.
* Color glyphs: **only if epaint feature `color_fonts`** (`epaint/Cargo.toml:59` → `vello_cpu/png`,`vello_cpu/text`; `font_face.rs:64-68,108,177-290`): supports COLR(v0/v1), sbix, CBDT, EBDT. **egui/eframe do NOT expose it** (grep: no `color_fonts` in egui/eframe Cargo.toml). Enable by adding to our crate: `epaint = { version = "0.37", features = ["color_fonts"] }` (Cargo feature unification; EXP built fine, same 0.37.0 crate). **EXP: 🙂🚀 rendered in full color from Noto-COLRv1.ttf (`Glyph.is_color == true`).** Without the feature, the colored font is rasterised as its (empty/mono) outline. Fallback if undesired: use `NotoEmoji-Regular.ttf` (mono outline, tinted with fg color).
* Chain resolution (`family.rs:~85-165`): per **char** first face in `families[family]` order whose charmap has it (`find_face_for_char_in`), cached in `face_cache`; then `FontProvider::font_for` (none installed); then `GlyphRasterizer`s (none); then **tofu = primary face `.notdef`** (hollow box; advance = notdef advance of primary, EXP 9.60pt), or `MissingGlyphPolicy::Panic` in tests (`fonts.rs:~77`). Text is segmented into per-face runs by grapheme cluster (`text_layout.rs:1504-1570 segment_into_runs`) and shaped per run. There is **no U+FFFD replacement**; tofu is `.notdef`. EXP: U+F101/U+F015 → hollow box in screenshot, `has_glyph == false`.
* Control chars: BEL U+0007 draws tofu (not blank); `\n` with `break_on_newline=false` also tofu (`text_layout_types.rs:72-79`); `\t` is blank with advance `tab_size(4.0) × space` (`text_layout.rs:262-272`); NBSP blank, advance 1 cell. Never feed C0 controls; map to ' ' / draw yourself.

---------------------------------------------------------------------------------------------------
## 4. Metrics
Exact 0.37 API (note `ctx.fonts_mut` — `ctx.fonts` exists too but is `&FontsView` read-only and `glyph_width`/`row_height` need `&mut self` → **use `fonts_mut`**):
```rust
let id = FontId::new(15.0, FontFamily::Name("term".into()));
let (adv_pt, row_h) = ctx.fonts_mut(|f| (f.glyph_width(&id, 'M'), f.row_height(&id)));
//   FontsView::glyph_width(&mut self,&FontId,char)->f32  fonts.rs:355   (points, UNROUNDED: advance_units * size/upem; resolves via fallback chain!)
//   FontsView::row_height(&mut self,&FontId)->f32        fonts.rs:385   (primary face; ascent/descent/gap each round_ui'd to 1/32 pt; NOT pixel-rounded)
//   ctx.fonts_mut<R>(|&mut FontsView| R)  context.rs:1379;  ctx.fonts<R>(|&FontsView|) context.rs:1358 (panics before first pass — call inside ui/update, not in creator)
//   FontsView::has_glyph(&mut self,&FontId,char)->bool  :363;  characters(&FontFamily)->&BTreeMap<char,Vec<String /*face names*/>> :377
```
EXP numbers (Noto Sans Mono, size 16pt): `adv_pt = 9.6` (600/1000 em), `row_height = 21.78125`. At ppp=1: 9.6px; at ppp=1.5: 14.4px.
* **Rounding rules (source-verified + EXP):** inside a layout the pen `cursor_x_px` accumulates the **fractional** advance in physical px, each glyph's x is `round()`ed to an integer px (`text_layout.rs:436,501`, glyph `pos.x = physical_x/ppp`), so a naive 9.6px advance yields glyph deltas **9,10,9,10…** (EXP "NO spacing": deltas {9,10}). Row height in a galley = `round_to_pixel(max(line_height, glyph.line_height))` (`text_layout.rs:1076-1095`), so `row_height()` (21.78 pt) ≠ galley row height (22 px).
* **Fix (EXP-verified uniform deltas = exactly cell_w_px at ppp 1 and 1.5, kern-free):** pick integer cell size in physical px and force it:
```rust
let ppp = ctx.pixels_per_point();
let adv_px   = adv_pt * ppp;
let cell_w_px = adv_px.round();                       // integer px
let cell_w   = cell_w_px / ppp;                       // pts — use for ALL x math
let cell_h_px = (row_h * ppp).ceil();                 // integer px  (or any integer)
let cell_h   = cell_h_px / ppp;
let spacing  = (cell_w_px - adv_px) / ppp;            // TextFormat.extra_letter_spacing (pts)
// TextFormat { extra_letter_spacing: spacing, line_height: Some(cell_h), .. }
```
  `extra_letter_spacing` is added at every cluster boundary except the first glyph of the paragraph (`text_layout.rs:292-297`; EXP: spacing=+0.4 → deltas all 10.0; ppp 1.5: −0.2667 → all 14.0). Alternatively choose font size so `adv_px` is an integer (size_px = cell_w_px/0.6). `line_height: Some(h)` centres glyphs in a taller line like CSS (`text_layout.rs:1100-1115`); keep `h` an integer number of px.
  Recompute whenever `ctx.pixels_per_point()` or font size changes (galleys cache is keyed on ppp, `galley_cache.rs: hash((job, ppp))`; Tessellator warns if ppp changed since layout).
* **Bold:** no flag in `TextFormat`/`FontId`. Options: (a) separate family `term-bold` with `NotoSansMono-Bold.ttf` (same advance 600 → grid safe) — **recommended**, one extra family per face; (b) variable font + `TextFormat.coords = VariationCoords::new([(b"wght",700.0)])` with `/usr/share/fonts/google-noto-vf/NotoSansMono[wght].ttf` — EXP works & advance identical for wght 100/400/700; (c) fake bold: paint the same galley twice at x and x+1px (`painter.galley` twice) — fallback only.
* **Italic:** `TextFormat.italics: bool` (synthetic): quad top is shifted right by `0.25 × quad height` (`text_layout.rs:1305-1325`), no new font needed (EXP: rendered slanted). Noto Sans Mono has no real italic; use `term-italic` family from DejaVuSansMono-Oblique/Hack-Italic only if you want true italics (then also bold-italic family).
* **Underline/strikethrough:** `TextFormat.underline: Stroke`, `.strikethrough: Stroke` (`text_layout_types.rs:513-515`), `Stroke::NONE` default; continuous across sections of equal stroke (`add_row_hline`, `text_layout.rs:~1340`). Underline y is at the font's face metrics; for exact SGR 4/9/curly/dotted/double or colored underline (SGR 58) draw yourself with `painter.line_segment`/`rect_filled` at cell-based y (EXP: built-in underline/strike rendered fine, see screenshot row 1).
* `TextFormat.background: Color32` + `expand_bg: f32` (default **1.0** pt — set 0.0) fills glyph-box backgrounds inside the galley (EXP: works; screenshot "bg"). Background height is the row height; for cell backgrounds prefer your own rects (below) so wide chars/gaps are exact.
* TextFormat full field list (`text_layout_types.rs:478-534`): `font_id, extra_letter_spacing, line_height: Option<f32>, color, background, expand_bg, coords: VariationCoords, italics, underline: Stroke, strikethrough: Stroke, valign: Align`. `TextFormat::default()` color is GRAY, valign BOTTOM.
* Antialiasing knobs: `ctx.style_mut(|s| { s.visuals.text_options.subpixel_binning = false; s.visuals.text_options.font_hinting = true; })` (`style.rs:1111`; `TextOptions` in `text/mod.rs:~40-70`, default `subpixel_binning: true` = up to 4 horizontal AA bins/glyph → **disable for a terminal** (blurrier; also redundant since you snap x to integer px; per-font override `FontTweak::subpixel_binning`). Changing options flushes the atlas+all galleys (`fonts.rs:~172-185`). Pixel-snap: egui rounds each galley position to a physical pixel (`tessellator.rs:~2257 round_text_to_pixels`), glyph quads rounded too (`text_layout.rs:1288-1292`) — so **always place at pixel-aligned origins** (`GuiRounding::round_to_pixels(ppp)` / `round_ui()`).
* **As built (user-picked from a rendered A/B/C comparison, 2026-11-01):** `deck_term::fonts::smooth_text` sets `font_hinting = false` + `subpixel_binning = true` on every style and `color_transfer_function = Gamma(0.75)` on the Dark style only (egui's dark default `TwoCoverageMinusCoverageSq` read as too sharp/heavy; Light keeps egui's linear `Off`). Terminal cells sit on whole pixels, so binning picks bin 0 there and the grid stays unblurred; UI text gets sub-pixel spacing (labels may change width by a fraction of a pixel).

---------------------------------------------------------------------------------------------------
## 5/6. Painting API & the recommended strategy
APIs (all exist, EXP-used): 
```rust
ctx.fonts_mut(|f: &mut FontsView| f.layout_job(job: LayoutJob) -> Arc<Galley>)     // memoised, fonts.rs:418
painter.layout_job(job) / painter.layout_no_wrap(String, FontId, Color32) -> Arc<Galley>   // painter.rs:503,517 (not on Context! Context has no layout_job)
painter.galley(pos: Pos2, galley: Arc<Galley>, fallback_color: Color32)           // painter.rs:529  (Shape::galley; Color32::PLACEHOLDER glyph colors → fallback)
painter.galley_with_override_text_color(pos, galley, color)                        // painter.rs:541
painter.rect_filled(rect, CornerRadius::ZERO, color)   // painter.rs:397;  painter.add(Shape::Mesh(Arc<Mesh>)) for custom meshes
ctx.request_screenshot(|Arc<ColorImage>| ..)           // context.rs:4557 (callback; NOT ScreenshotCallback::new)
```
**Galley cache** (`galley_cache.rs`): key = `hash((LayoutJob, ppp))` (whole text + all sections), entries not used during a frame are evicted at next `begin_pass` (`flush_cache`, `:250`). A hit costs hashing the job (+ cloning it by value, because `layout_job(LayoutJob)` takes it by value). A **galley already contains the tessellated mesh** (`Row.visuals.mesh`; `PlacedRow.row: Arc<Row>`), so painting a cached galley is just a vertex copy. Don't hold `Arc<Galley>` across frames yourself: when the glyph atlas passes 80 % fill, `Fonts::begin_pass` clears the atlas **and all galleys** (`fonts.rs:181-184`) → stale UVs. Re-request the galley each frame (hit = cheap).
**EXP benchmark** (debug build opt-level 1, software GL box; layout side only, 50 rows × 200 cols, 8 style sections/row): first frame (miss) **10.4 ms**; subsequent frames (all hits): build jobs **~0.13–0.22 ms** + layout **~0.04 ms** + `painter.galley` ×50 **<0.05 ms**. Per-cell alternative, 10 000 one-char jobs: 10.2 ms first, **3.7 ms every frame (hits)** + 1.5–2.7 ms painter.add → ~100× worse than row jobs. (ppp 1.5 gave the same shape.) Tessellation/GPU upload not included.
**No low-level glyph quad API**: `Fonts::glyph`, `FontImpl`, `font_image` are gone; atlas allocation (`FontsImpl::allocate_glyph`, `GlyphAtlas`) is `pub(crate)`. Public, read-only: `Galley.rows[i].row.glyphs[j]` (`Glyph{chr,pos,advance_width,uv_rect:UvRect{offset,size,min:[u16;2],max:[u16;2]},is_color,..}`) and `row.visuals.mesh` (texel UVs). You *could* build your own `Mesh` with `TextureId::default()` (the font atlas) from a one-char galley's `visuals.mesh` + translation, but it buys nothing over `painter.galley` and has the stale-atlas hazard → not recommended. Custom glyph source (e.g. procedural box drawing as bitmaps) is possible via `GlyphRasterizer` (`ctx.add_glyph_rasterizer(GlyphRasterizer::new(key, |req| Option<RasterizedGlyph>).with_priority(FontPriority::Highest))`, `glyph_rasterizer.rs`), but `rect_filled` is simpler.

**Shaping/grid behaviour verified (EXP, Noto Sans Mono, 67-char string "fi ffi -> == != <= >= AV Wo WAVE === =>> www ::: .. // /* */ !== ||" + extra spacing):** glyph count == char count, all deltas exactly 10.0 px → **no kerning/ligatures in Noto Sans Mono**. Caveat [UNVERIFIED with a ligature font]: since `ShapeOptions::new()` enables default features, a primary font with `calt`/`liga` programming ligatures (Fira Code/JetBrains Mono) *would* shape across cells; ligature clusters emit zero-width continuation glyphs (`text_layout.rs:225-228`) and break grid mapping. Keep the primary font ligature-free (Noto Sans Mono, DejaVu Sans Mono, Hack, Source Code Pro, Liberation Mono are fine). Combining marks: base+mark share one cluster, mark gets advance 0 (EXP "e\u{301}x": x of 'x' = 10 px) → a cell containing base+combining can be one string in one section. ZWJ sequences render as separate emoji glyphs in EXP (not the ligated family glyph) [UNVERIFIED why].
**Fallback-font advance drift (EXP, observed):** advances come from the face that owns the char: braille ⣿ (Symbols2) 11.2 pt = 1.17 cells; CJK 漢 16.0 pt = 1.67 cells (한 14.72); emoji 19.92 pt = 2.08 cells; ✓✗★ 9.63 pt; ⏵ 9.38 pt; powerline (SourceCodePro) 9.60 pt. After such a glyph the pen in a single row-job shifts → **naive one-job-per-row visibly drifts** (screenshot: the word "braille" starts at x=78 in the naive line vs x=70 in the per-cell line). `extra_letter_spacing` cannot fix that (uniform).
**`LayoutJob` with `wrap.max_width = f32::INFINITY`** (the `LayoutJob::default()` wrap, also `layout_no_wrap`): one row per `\n`-paragraph, never wraps; tabs/controls are kept as glyphs (see §3). `break_on_newline` defaults `true`; `halign` LEFT; `keep_trailing_whitespace` only matters with non-left halign. Make each terminal row its own job (no `\n` inside).

### Recommended strategy
1. **One `LayoutJob` per terminal row**, text = the row's chars, one `LayoutSection` per run of equal style (fg color, bold→family, italics, underline/strike strokes). Same `extra_letter_spacing`/`line_height` in every section. Append with `job.append(&str, 0.0, TextFormat)` (leading_space 0). Backgrounds are **not** in the job (see 3).
2. **Cells whose glyph is not in the primary face with exactly 1-cell advance** must be taken out of the job: classify each distinct char once, cache in `HashMap<char, Class>`: `primary = fonts.has_glyph(&FontId::new(sz, Name("term-primary")), c)`; plus wide (`unicode_width`/alacritty `WIDE_CHAR` flag) or glyph_width ≠ adv. For a non-primary/wide/emoji/combining-cluster cell: put `' '` (and for wide chars 2 spaces — or alacritty's spacer cell already is a space) in the row job to keep the pen aligned, and paint the real glyph as a **separate tiny galley at exact `origin.x + col*cell_w`** (`LayoutJob` of that char/cluster, same family so fallback applies, `extra_letter_spacing:0`). For wide glyphs optionally centre: `x += (n_cells*cell_w - glyph_adv)/2` (CJK 1.67 vs 2 cells; emoji 2.08 vs 2 → left-align or scale via `FontTweak.scale ≈ 0.96` on the emoji font — scale does change advance/ascent of that face). These specials are rare so cost is negligible, and they also hit the galley cache (key = char+style).
3. **Backgrounds, cursor, selection**: paint with `painter.rect_filled(Rect::from_min_size(pos2(x0, y0), vec2(n*cell_w, cell_h)), 0.0, color)` (one rect per run of equal bg per row; coords from integer px: `x = origin_px + col*cell_w_px`, divide by ppp; draw rects *before* the galleys). Or accumulate one `Mesh` (`mesh.add_colored_rect(rect, color)`) per frame and `painter.add(Shape::mesh(mesh))` to minimise shape count; `TextFormat.background` inside the job also works but sizes to row/glyph boxes and `expand_bg` defaults to 1.0 pt.
4. **Box drawing U+2500–257F, block elements U+2580–259F, sextants/octants, braille U+2800–28FF, and powerline U+E0B0–E0B3 should be drawn procedurally with `rect_filled`/polygons per cell** (what Konsole/kitty do): EXP shows Noto Sans Mono's own box glyphs are 11×2 px / 2×20 px bitmaps that leave 1–2 px gaps between rows once `line_height` > glyph height and overshoot columns, so lines look broken (screenshot rows 2). Braille from Symbols2 is also 1.17-cell wide. Keep font glyphs as fallback only for the rest.
5. Reuse allocations: keep a `Vec<LayoutJob>`/`String` scratch per row; skip rebuilding rows whose alacritty damage is clean *only if you also keep the previous `Arc<Galley>` for the SAME frame*; otherwise rebuild — cost measured above is ~0.2 ms/frame for 50×200.
6. Snap everything: `origin` to whole pixels (`pos.round_to_pixels(ppp)`; `emath::GuiRounding`), cell size integer px (formula in §4); then galley row k at `origin.y + k*cell_h`.

Snippet:
```rust
fn row_job(cells: &[Cell], fam: &FontFamily, bold_fam: &FontFamily, sz: f32, spacing: f32, cell_h: f32, special: &mut Vec<(usize,String)>) -> LayoutJob {
    let mut job = LayoutJob::default();               // wrap.max_width = INFINITY, break_on_newline = true
    let mut run = String::new(); let mut cur: Option<TextFormat> = None;
    let mut flush = |job: &mut LayoutJob, run: &mut String, cur: &Option<TextFormat>| if let Some(f) = cur { if !run.is_empty() { job.append(run, 0.0, f.clone()); run.clear(); } };
    for (col, c) in cells.iter().enumerate() {
        let f = TextFormat { font_id: FontId::new(sz, if c.bold { bold_fam.clone() } else { fam.clone() }), color: c.fg, italics: c.italic,
                             underline: if c.underline { Stroke::new(1.0, c.fg) } else { Stroke::NONE },
                             strikethrough: if c.strike { Stroke::new(1.0, c.fg) } else { Stroke::NONE },
                             extra_letter_spacing: spacing, line_height: Some(cell_h), ..Default::default() };
        if cur.as_ref() != Some(&f) { flush(&mut job, &mut run, &cur); cur = Some(f); }   // TextFormat: PartialEq
        if c.is_plain_primary_1cell() { run.push(c.ch) } else { run.push(' '); special.push((col, c.cluster_string())) }
    }
    flush(&mut job, &mut run, &cur); job
}
// paint:
let g = painter.layout_job(row_job(..));  painter.galley(origin + vec2(0.0, row as f32 * cell_h), g, Color32::WHITE);
for (col, s) in specials { let g = painter.layout_job(single(s, fmt_without_spacing)); painter.galley(origin + vec2(col as f32*cell_w, row as f32*cell_h), g, Color32::WHITE); }
```
(Color emoji glyphs ignore the section color: `is_color` glyphs keep own colors, only alpha applied — `tessellator.rs:~2290-2330`.)

---------------------------------------------------------------------------------------------------
## 7. Experiment results (screenshot `/tmp/omp-deck-notes/egui-fonts-screenshot.png`)
Setup: eframe 0.37 (`default-features=false, ["default_fonts","glow","wayland","x11"]`) + `epaint = {version="0.37", features=["color_fonts"]}`; `WAYLAND_DISPLAY= DISPLAY=:99 LIBGL_ALWAYS_SOFTWARE=1`, Xvfb `:99 -screen 0 1024x768x24` (started via `setsid nohup`; a plain `( Xvfb & )` died), screenshot through `ctx.request_screenshot` → `image::save_buffer`. Size 16pt, chain = Noto Sans Mono → DejaVu Sans Mono → Source Code Pro → Symbols2 → Symbols[wght] → Noto Sans Mono CJK SC (.ttc idx 2) → Noto-COLRv1 → NotoEmoji. Each test row drawn twice: top (white) = naive single job; bottom (orange) = per-cell galleys at `col*cell_w`; faint vertical lines = cell boundaries.
Per-char results (`has_glyph` in primary `term-primary` family / in full chain, face that supplied it, width in cells):
| glyph | result |
|---|---|
| ASCII, é € » … → ● ─│┌┘╭█▀▄░ | OK from Noto Sans Mono, 1.00 cell (box/block glyphs render but have gaps/overshoot → draw procedurally) |
| ⣿ ⠁ braille | OK, from Symbols2, **1.17 cells** |
| 漢 字 あ 한 (CJK) | OK from Noto Sans Mono CJK SC (.ttc idx 2), 1.67 cells (한 1.53) |
| 🙂 🚀 | OK **in color** (COLRv1, requires `epaint/color_fonts`), 2.08 cells |
| ✓ ✗ ★ | OK from DejaVu Sans Mono (1.003 cells); ⏵ from Symbols2 0.98 cells |
| U+E0A0 U+E0B0 U+E0B1 (powerline) | OK from Source Code Pro (1.00 cell) — screenshot row 6 triangles/branch |
| U+F101 U+F015 (Nerd) | **TOFU** (hollow box, `has_glyph=false`, no font) |
| bold(wght 700 variable) / italics skew / underline / strike / bg | all render; bold keeps advance (x identical across wght 100/400/700) |
| tab / BEL / `\n` (break_on_newline=false) | tab blank adv 4 cells; BEL & `\n` tofu |
Drift: naive row misplaces everything after ⣿/CJK/emoji; per-cell row stays on the grid.
Cleanup done: `/tmp/fonttest`, Xvfb :99 killed; no tmux used.
