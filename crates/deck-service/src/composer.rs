//! Finding omp's composer in the terminal grid (docs/notes/omp-status.md §1.1).
//!
//! The injector must know whether the user has an unsent draft, and whether the cursor is in a
//! composer at all (an omp overlay such as the model picker or the agent hub takes over the
//! screen; text typed then lands in the overlay). The composer is recognised from the frame omp
//! draws around it, which depends on `composer.shape`; only the known frame prefix of each row is
//! removed, so a draft that itself starts with `>` or `│` still counts as text.
//!
//! Shapes (captured from omp 18.12.5, cursor row marked `*`):
//! ```text
//! rail      ▎ draft*                     (every row starts with `▎ `)
//! field     ▐ draft*                  ▌  (like rail, plus a right edge)
//! box       ╭── π > model ───────────╮   `│  ` rows, last row `╰─ `, right edge `│`/`─╯`
//!           ╰─ draft*                ─╯
//! band      π > model ▶──7%────────      status row above, then `╰─ ` and `   ` rows
//!           ╰─ draft*
//! rule      ────────────────────────     `❯ ` then `  ` rows (rule/claude shape also close below)
//!           ❯ draft*
//! pi        ────────────────────────     rows indented by one space between two rules
//!            draft*
//!           ────────────────────────
//! ```
//! An empty composer shows a dim hint (`⇧⇥ to change thinking effort`) far to the right of the
//! cursor row; it is separated from the cursor by a run of blanks.

use deck_term::ScreenText;
use deck_tsp::document::{Document, NodeRef};

/// What the composer holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Composer {
    /// The cursor is not in a recognisable composer (an overlay/picker is open, or omp is not up).
    Missing,
    Empty,
    /// The user's unsent text: the non-blank composer rows joined by ` ↵ `.
    Draft(String),
}

/// Rows scanned above/below the cursor before giving up on finding a frame edge.
const SCAN: usize = 80;
/// A run of at least this many blanks separates the typed text from the empty-composer hint.
const HINT_GAP: &str = "   ";

pub fn inspect(screen: &ScreenText) -> Composer {
    let Some(region) = find(screen) else {
        return Composer::Missing;
    };
    let text = |r: &Row| {
        let mut t = r.text.trim_end().to_owned();
        // The hint is only drawn while the composer is empty, i.e. the cursor sits at the very start.
        if r.row == screen.cursor_row && screen.cursor_col <= region.prefix_cells {
            t = cut_hint(&t);
        }
        t.trim().to_owned()
    };
    let rows: Vec<String> = region
        .rows
        .iter()
        .map(text)
        .filter(|t| !t.is_empty())
        .collect();
    if rows.is_empty() {
        Composer::Empty
    } else {
        Composer::Draft(rows.join(" ↵ "))
    }
}

/// The screen rows below the composer — omp's autocomplete popup while it is open, else the status
/// line and whatever omp draws under the editor — without trailing blank rows; `None` without a
/// composer. The injector compares them to see the popup open and close.
pub fn rows_below(screen: &ScreenText) -> Option<Vec<String>> {
    let last = find(screen)?.rows.last()?.row;
    let mut rows = screen.lines.get(last + 1..).unwrap_or_default().to_vec();
    while rows.last().is_some_and(|r| r.trim().is_empty()) {
        rows.pop();
    }
    Some(rows)
}

/// Whether two [`rows_below`] snapshots differ the way omp's autocomplete popup changes them. omp
/// draws the popup inside its editor, directly under the composer rows (`tui/src/components/
/// editor.ts`), and its status line last (`tui/src/prompt/composer.ts`: `[...runtime roots,
/// statusHost]`), so a popup opening or closing always changes the rows above the status line or
/// their count. The status line also redraws on its own — omp refreshes the git counts on input —
/// and that alone is no popup: read as one, every injection into a repo whose files were changing
/// sent `Esc`, waited for a popup to close and stalled without its Enter.
pub fn rows_below_changed(before: &[String], after: &[String]) -> bool {
    if before.len() != after.len() {
        return true;
    }
    let above_status = before.len().saturating_sub(1);
    before[..above_status] != after[..above_status]
}

/// omp's empty-composer hint is `<key> <label>` (bundle `composer-hints.ts`, captured on 18.12.5 for
/// `rail` and `band`: `⇧⇥ to change thinking effort`), drawn right-aligned on the cursor row.
const HINT_LABELS: [&str; 2] = ["to change thinking effort", "to see "];

/// The row text without omp's trailing hint (text that is not exactly that hint stays untouched, so a
/// draft starting with blanks, or one that merely contains such words, is still a draft).
fn cut_hint(text: &str) -> String {
    let t = text.trim_end();
    for label in HINT_LABELS {
        let Some(at) = t.rfind(label) else { continue };
        let tail = &t[at + label.len()..];
        // `to see N running agent(s)`; the fixed labels end the row.
        let ok = if label == "to see " {
            tail.strip_suffix("running agents")
                .or_else(|| tail.strip_suffix("running agent"))
                .is_some_and(|n| {
                    let n = n.trim();
                    !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                })
        } else {
            tail.is_empty()
        };
        if !ok {
            continue;
        }
        let before = t[..at].trim_end();
        // The key is one blank-free token; the head ends with the run of blanks before it.
        let head = match before.rfind(' ') {
            Some(sp) => &before[..=sp],
            None => "",
        };
        if before.len() == head.len() {
            continue; // no key before the label
        }
        let head_trim = head.trim_end();
        let gap = head.len() - head_trim.len();
        if head_trim.is_empty() || gap >= HINT_GAP.len() {
            return head_trim.to_owned();
        }
    }
    text.to_owned()
}

struct Row {
    row: usize,
    /// The row without the frame (prefix and right edge).
    text: String,
}

struct Region {
    rows: Vec<Row>,
    /// Grid columns taken by the frame prefix of the cursor row.
    prefix_cells: usize,
}

fn line(s: &ScreenText, i: usize) -> &str {
    s.lines.get(i).map_or("", String::as_str)
}

fn is_rule(l: &str) -> bool {
    let t = l.trim_end();
    t.chars().count() >= 10 && t.chars().all(|c| c == '─' || c == '━')
}

/// `l` without `prefix` and without up to `spaces` blanks after it.
fn strip_prefix_blanks<'a>(l: &'a str, prefix: &str, spaces: usize) -> &'a str {
    let mut rest = l.strip_prefix(prefix).unwrap_or(l);
    for _ in 0..spaces {
        match rest.strip_prefix(' ') {
            Some(r) => rest = r,
            None => break,
        }
    }
    rest
}

fn find(s: &ScreenText) -> Option<Region> {
    let r = s.cursor_row;
    rail(s, r)
        .or_else(|| field(s, r))
        .or_else(|| boxed(s, r))
        .or_else(|| band(s, r))
        .or_else(|| prompt(s, r))
        .or_else(|| pi(s, r))
}

/// Rows `lo..=hi` that start with `marker`, extended around `r` in both directions.
fn marked_block(s: &ScreenText, r: usize, marker: char) -> (usize, usize) {
    let mut lo = r;
    while lo > 0 && r - lo < SCAN && line(s, lo - 1).starts_with(marker) {
        lo -= 1;
    }
    let mut hi = r;
    while hi - r < SCAN && line(s, hi + 1).starts_with(marker) {
        hi += 1;
    }
    (lo, hi)
}

fn rail(s: &ScreenText, r: usize) -> Option<Region> {
    line(s, r).starts_with('▎').then_some(())?;
    let (lo, hi) = marked_block(s, r, '▎');
    Some(Region {
        rows: (lo..=hi)
            .map(|i| Row {
                row: i,
                text: strip_prefix_blanks(line(s, i), "▎", 1).to_owned(),
            })
            .collect(),
        prefix_cells: 2,
    })
}

fn field(s: &ScreenText, r: usize) -> Option<Region> {
    line(s, r).starts_with('▐').then_some(())?;
    let (lo, hi) = marked_block(s, r, '▐');
    Some(Region {
        rows: (lo..=hi)
            .map(|i| {
                let l = line(s, i).trim_end();
                let l = l.strip_suffix('▌').unwrap_or(l);

                Row {
                    row: i,
                    text: strip_prefix_blanks(l, "▐", 1).to_owned(),
                }
            })
            .collect(),
        prefix_cells: 2,
    })
}

/// A box composer's closing row: `╰─ …` (an overlay's frame is `╰──…`) ending in the corner.
fn is_box_bottom(l: &str) -> bool {
    l.starts_with("╰─") && !l.starts_with("╰──") && l.trim_end().ends_with('╯')
}

fn boxed(s: &ScreenText, r: usize) -> Option<Region> {
    let cur = line(s, r);
    if !(cur.starts_with('│') || is_box_bottom(cur)) {
        return None;
    }
    // Down to the closing row, up to the top border; every row between is `│`.
    let mut bottom = r;
    while !is_box_bottom(line(s, bottom)) {
        if bottom - r >= SCAN || !line(s, bottom).starts_with('│') {
            return None;
        }
        bottom += 1;
    }
    let mut top = r;
    while !line(s, top).starts_with('╭') {
        if r - top >= SCAN || top == 0 || !(line(s, top).starts_with('│') || top == r) {
            return None;
        }
        top -= 1;
    }
    let rows = (top + 1..=bottom)
        .map(|i| {
            let l = line(s, i).trim_end();
            if i == bottom {
                let l = l
                    .strip_suffix("─╯")
                    .or_else(|| l.strip_suffix('╯'))
                    .unwrap_or(l);
                Row {
                    row: i,
                    text: strip_prefix_blanks(l, "╰─", 1).to_owned(),
                }
            } else {
                let l = l.strip_suffix('│').unwrap_or(l);
                Row {
                    row: i,
                    text: strip_prefix_blanks(l, "│", 2).to_owned(),
                }
            }
        })
        .collect();
    Some(Region {
        rows,
        prefix_cells: 3,
    })
}

/// Rows from the head row `head` down to the cursor row, where every row in between is a
/// continuation (`cont` prefix); the cursor row itself may be an empty continuation line.
fn head_to_cursor(
    s: &ScreenText,
    r: usize,
    head_ok: impl Fn(&str) -> bool,
    cont_ok: impl Fn(&str) -> bool,
    cont_cells: usize,
) -> Option<usize> {
    // omp draws an empty line of a multi-paragraph draft as a bare empty row (no indent). That is
    // only trusted while the cursor sits on a row with text, so an overlay's blank rows with the
    // cursor on one of them never read as a composer.
    let typing = !line(s, r).trim().is_empty();
    let mut i = r;
    loop {
        let l = line(s, i);
        if head_ok(l) {
            return Some(i);
        }
        let blank_cursor = i == r && l.trim().is_empty() && s.cursor_col == cont_cells;
        let blank_gap = typing && i < r && l.trim().is_empty();
        if i == 0 || r - i >= SCAN || !(cont_ok(l) || blank_cursor || blank_gap) {
            return None;
        }
        i -= 1;
    }
}

fn band(s: &ScreenText, r: usize) -> Option<Region> {
    let band_head = |l: &str| l.starts_with("╰─") && !l.trim_end().ends_with('╯');
    let head = head_to_cursor(s, r, band_head, |l| l.starts_with("   "), 3)?;
    let rows = (head..=r)
        .map(|i| {
            let l = line(s, i);
            let text = if i == head {
                strip_prefix_blanks(l, "╰─", 1)
            } else {
                l.strip_prefix("   ").unwrap_or(l)
            };
            Row {
                row: i,
                text: text.to_owned(),
            }
        })
        .collect();
    Some(Region {
        rows,
        prefix_cells: 3,
    })
}

fn prompt(s: &ScreenText, r: usize) -> Option<Region> {
    let head = head_to_cursor(s, r, |l| l.starts_with('❯'), |l| l.starts_with("  "), 2)?;
    let rows = (head..=r)
        .map(|i| {
            let l = line(s, i);
            let text = if i == head {
                strip_prefix_blanks(l, "❯", 1)
            } else {
                l.strip_prefix("  ").unwrap_or(l)
            };
            Row {
                row: i,
                text: text.to_owned(),
            }
        })
        .collect();
    Some(Region {
        rows,
        prefix_cells: 2,
    })
}

/// Rows indented by one blank between two full-width rules.
fn pi(s: &ScreenText, r: usize) -> Option<Region> {
    if is_rule(line(s, r)) {
        return None;
    }
    let inside = |l: &str| l.is_empty() || l.starts_with(' ');
    let mut top = r;
    loop {
        if top == 0 || r - top >= SCAN || !inside(line(s, top)) {
            return None;
        }
        top -= 1;
        if is_rule(line(s, top)) {
            break;
        }
    }
    let mut bottom = r;
    loop {
        if bottom - r >= SCAN || !inside(line(s, bottom)) {
            return None;
        }
        bottom += 1;
        if is_rule(line(s, bottom)) {
            break;
        }
    }
    let rows = (top + 1..bottom)
        .map(|i| Row {
            row: i,
            text: strip_prefix_blanks(line(s, i), "", 1).to_owned(),
        })
        .collect();
    Some(Region {
        rows,
        prefix_cells: 1,
    })
}

/// What omp's TSP document says of its composer: the same three-way answer as [`inspect`] gives
/// for the grid, plus what a `send` event needs. A chat session (a live TSP mirror) has no
/// composer on its grid at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TspComposer {
    /// [`Composer::Missing`] when no sendable `editor` stands in the dock or the cursor is not
    /// in it: an `omp.editor.ask*` (ask, approval) replaces the composer, and any other dialog
    /// takes the surface's focus away from the editor. Otherwise the editor's text.
    pub state: Composer,
    /// `(surface id, editor id)` of the sendable editor.
    pub target: Option<(String, String)>,
    /// The program's hello advertised `send`: an event may submit text atomically.
    pub can_send: bool,
    /// Attachment chips the composer carries (a pasted file drop shows up as one).
    pub chips: usize,
    /// omp's autocomplete popup (an `omp.autocomplete` overlay) is open over the editor: Enter
    /// would pick its entry instead of submitting.
    pub popup: bool,
}

impl TspComposer {
    /// Reads the inline surface's dock. Ids are not stable across omp runs, so the editor is
    /// found by kind and `sendable`, never by id.
    pub fn of(document: &Document) -> TspComposer {
        let can_send = document.program().is_some_and(|p| p.supports("send"));
        let missing = |chips, popup| TspComposer {
            state: Composer::Missing,
            target: None,
            can_send,
            chips,
            popup,
        };
        let Some(surface) = document.inline() else {
            return missing(0, false);
        };
        let mut scan = DockScan::default();
        if let Some(dock) = surface.node("dock") {
            scan.visit(dock);
        }
        let mut layer = DockScan::default();
        if let Some(node) = surface.node("layer") {
            layer.visit(node);
        }
        let popup = layer.popup;
        let Some((editor, text)) = scan.editor else {
            return missing(scan.chips, popup);
        };
        if scan.asking || surface.focus() != Some(editor) {
            return missing(scan.chips, popup);
        }
        TspComposer {
            state: draft_with_chips(text, scan.chips),
            target: Some((surface.id().to_owned(), editor.to_owned())),
            can_send,
            chips: scan.chips,
            popup,
        }
    }

    /// Changes whenever the composer shows something new (text or attachments).
    pub fn fingerprint(&self) -> String {
        match &self.state {
            Composer::Draft(d) => format!("{}\u{0}{d}", self.chips),
            _ => format!("{}\u{0}", self.chips),
        }
    }
}

/// [`draft_of`], except that attachments alone (no text) are a draft too: a send would drop them.
fn draft_with_chips(text: &str, chips: usize) -> Composer {
    match draft_of(text) {
        Composer::Empty if chips > 0 => Composer::Draft(format!("[{chips} attachment(s)]")),
        state => state,
    }
}

/// The editor's text as [`inspect`] reports a draft: non-blank rows joined by ` ↵ `.
fn draft_of(text: &str) -> Composer {
    let mut rows = text.lines().map(str::trim).filter(|r| !r.is_empty());
    let Some(first) = rows.next() else {
        return Composer::Empty;
    };
    let mut draft = first.to_owned();
    for row in rows {
        draft.push_str(" ↵ ");
        draft.push_str(row);
    }
    Composer::Draft(draft)
}

#[derive(Default)]
struct DockScan<'a> {
    /// The first sendable editor: its id and text.
    editor: Option<(&'a str, &'a str)>,
    asking: bool,
    popup: bool,
    chips: usize,
}

impl<'a> DockScan<'a> {
    fn visit(&mut self, node: NodeRef<'a>) {
        let role = node.str_prop("role").unwrap_or_default();
        self.asking |= role.starts_with("omp.editor.ask");
        self.popup |= role == "omp.autocomplete";
        if role == "omp.composer.chip" {
            self.chips += 1;
        }
        if self.editor.is_none()
            && node.kind() == "editor"
            && node.prop("sendable") == Some(&serde_json::Value::Bool(true))
        {
            self.editor = Some((node.id(), node.str_prop("text").unwrap_or_default()));
        }
        for child in node.children() {
            self.visit(child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HINT: &str = "⇧⇥ to change thinking effort";

    fn screen(lines: &[&str], cursor_row: usize, cursor_col: usize) -> ScreenText {
        ScreenText {
            lines: lines.iter().map(|l| (*l).to_owned()).collect(),
            cursor_row,
            cursor_col,
        }
    }

    /// `head` + blanks + the hint, as omp draws an empty composer row.
    fn hinted(head: &str, tail: &str) -> String {
        format!("{head}{}{HINT}{tail}", " ".repeat(60))
    }

    const STATUS: &str = " π · ◒ Fake One · 🗑 w3f/proj · ◫ 7.4%/100K ⟲";
    const SPLASH: &[&str] = &["", "   ████████", "   v18.12.5", "", "  Tip: Try /omfg", ""];

    fn with(prefix: &[&str], rows: &[&str], suffix: &[&str]) -> Vec<String> {
        prefix
            .iter()
            .chain(rows)
            .chain(suffix)
            .map(|s| (*s).to_owned())
            .collect()
    }

    fn st(lines: Vec<String>, cursor_row: usize, cursor_col: usize) -> ScreenText {
        ScreenText {
            lines,
            cursor_row,
            cursor_col,
        }
    }

    // ---- rail (the user's shape): captured with `composer.shape rail` on omp 18.12.5

    #[test]
    fn a_draft_starting_with_blanks_is_a_draft_even_with_the_cursor_at_home() {
        for (shape, head, col) in [("rail", "▎", 2), ("band", "╰─", 3)] {
            let draft = format!("{head}     indented draft");
            let mut l: Vec<String> = SPLASH.iter().map(|s| (*s).to_owned()).collect();
            if shape == "band" {
                l.push(BAND_STATUS.to_owned());
            }
            let row = l.len();
            l.push(draft);
            assert_eq!(
                inspect(&st(l, row, col)),
                Composer::Draft("indented draft".to_owned()),
                "{shape}"
            );
        }
    }

    #[test]
    fn the_running_agents_hint_counts_as_a_hint() {
        let row = format!("▎{}⌥← to see 2 running agents", " ".repeat(60));
        let l = with(SPLASH, &[&row, "", STATUS], &[]);
        assert_eq!(inspect(&st(l, 6, 2)), Composer::Empty);
        let row = format!("▎{}⇧⇥ to see agents", " ".repeat(60));
        let l = with(SPLASH, &[&row, "", STATUS], &[]);
        assert_ne!(inspect(&st(l, 6, 2)), Composer::Empty);
    }

    #[test]
    fn rail_empty_composer_with_hint_is_empty() {
        let hint = hinted("▎", "");
        let l = with(SPLASH, &[&hint, "", STATUS], &[]);
        assert_eq!(inspect(&st(l, 6, 2)), Composer::Empty);
    }

    #[test]
    fn rail_single_and_gt_drafts() {
        let l = with(SPLASH, &["▎ hello", "", STATUS], &[]);
        assert_eq!(inspect(&st(l, 6, 7)), Composer::Draft("hello".to_owned()));
        // A draft that looks like omp's own prompt glyph is still the user's text.
        let l = with(SPLASH, &["▎ > quote", "", STATUS], &[]);
        assert_eq!(inspect(&st(l, 6, 9)), Composer::Draft("> quote".to_owned()));
        let l = with(SPLASH, &["▎ >", "", STATUS], &[]);
        assert_eq!(inspect(&st(l, 6, 4)), Composer::Draft(">".to_owned()));
    }

    #[test]
    fn rail_multiline_with_cursor_on_empty_last_line_is_a_draft() {
        let l = with(SPLASH, &["▎ line one", "▎ line two", "▎", "", STATUS], &[]);
        assert_eq!(
            inspect(&st(l, 8, 2)),
            Composer::Draft("line one ↵ line two".to_owned())
        );
    }

    #[test]
    fn rail_cursor_at_home_of_a_draft_with_hintless_text_is_a_draft() {
        let l = with(SPLASH, &["▎ hello", "", STATUS], &[]);
        assert_eq!(inspect(&st(l, 6, 2)), Composer::Draft("hello".to_owned()));
    }

    #[test]
    fn rail_overlay_open_is_missing() {
        // Model picker captured over a rail composer; omp hides the cursor (grid cursor at 0,0).
        let l = with(
            SPLASH,
            &[
                "▎",
                "❯ 🖥  computer          Computer: off          █",
                "╭─ Switch Model ─────────────────────────────╮",
                "│  🔍 >                                       │",
                "│ ❯ fake/fake-1 ●                    100k ◫  │",
                "╰─────────────────────────────────────────────╯",
            ],
            &[],
        );
        assert_eq!(inspect(&st(l.clone(), 0, 0)), Composer::Missing);
        // Cursor parked in the picker's search row.
        assert_eq!(inspect(&st(l, 9, 8)), Composer::Missing);
    }

    #[test]
    fn rail_slash_popup_still_sees_the_draft() {
        let l = with(
            SPLASH,
            &[
                "▎ /co",
                "❯ 🖥  computer          Computer: off          █",
                "  📡 collab            Collab: off             │",
            ],
            &[],
        );
        assert_eq!(inspect(&st(l, 6, 7)), Composer::Draft("/co".to_owned()));
    }

    // ---- band (default shape)

    const BAND_STATUS: &str = " π > ◒ Fake One > 🗑 w3f/proj ▶──────7%────────────────╎─────100K─";

    #[test]
    fn band_empty_draft_multiline() {
        let hint = hinted("╰─", "");
        let l = with(SPLASH, &[BAND_STATUS, &hint], &[]);
        assert_eq!(inspect(&st(l, 7, 3)), Composer::Empty);

        let l = with(SPLASH, &[BAND_STATUS, "╰─ hello"], &[]);
        assert_eq!(inspect(&st(l, 7, 8)), Composer::Draft("hello".to_owned()));

        let l = with(
            SPLASH,
            &[BAND_STATUS, "╰─ line one", "   line two", ""],
            &[],
        );
        assert_eq!(
            inspect(&st(l, 9, 3)),
            Composer::Draft("line one ↵ line two".to_owned())
        );

        let l = with(SPLASH, &[BAND_STATUS, "╰─ line one", "   >"], &[]);
        assert_eq!(
            inspect(&st(l, 8, 4)),
            Composer::Draft("line one ↵ >".to_owned())
        );
    }

    /// An inserted multi-paragraph review (omp 18.12.6 `band`, as shown by a real tab): the blank
    /// line of the draft is a bare empty row without the `   ` indent.
    #[test]
    fn band_draft_with_a_blank_paragraph_break_is_a_draft() {
        let l = with(
            SPLASH,
            &[
                BAND_STATUS,
                "╰─ Review comments on my uncommitted changes. Paths are relative to the repository",
                "   root. Line numbers refer to the file after the change.",
                "",
                "   1. General",
                "   general note one",
            ],
            &[],
        );
        assert_eq!(
            inspect(&st(l.clone(), 11, 20)),
            Composer::Draft(
                "Review comments on my uncommitted changes. Paths are relative to the repository ↵ \
                 root. Line numbers refer to the file after the change. ↵ 1. General ↵ \
                 general note one"
                    .to_owned()
            )
        );
        // A blank row with the cursor on it is not a draft: an overlay's blank rows stay Missing.
        let mut l = with(SPLASH, &[BAND_STATUS, "╰─", "", "", ""], &[]);
        l.push("╭─ Switch Model ──────────────╮".to_owned());
        assert_eq!(inspect(&st(l, 10, 3)), Composer::Missing);
    }

    #[test]
    fn band_overlay_open_is_missing() {
        let l = with(
            SPLASH,
            &[
                BAND_STATUS,
                "╰─",
                "",
                "",
                "",
                "╭─ Switch Model ──────────────╮",
                "│  🔍 >                       │",
                "╰─────────────────────────────╯",
            ],
            &[],
        );
        assert_eq!(inspect(&st(l.clone(), 0, 0)), Composer::Missing);
        assert_eq!(inspect(&st(l, 12, 8)), Composer::Missing);
    }

    // ---- other shapes (frames captured, cursor derived)

    #[test]
    fn box_shape() {
        let top = "╭── π > ◒ Fake One > 🗑 w3f/proj ▶────7%────────────────────╮";
        let hint = hinted("╰─", " ─╯");
        let l = with(SPLASH, &[top, &hint], &[]);
        assert_eq!(inspect(&st(l, 7, 3)), Composer::Empty);
        let l = with(
            SPLASH,
            &[
                top,
                "│  hello                      │",
                "╰─ two                       ─╯",
            ],
            &[],
        );
        assert_eq!(
            inspect(&st(l, 8, 6)),
            Composer::Draft("hello ↵ two".to_owned())
        );
        // The same frame glyphs in an overlay (`╰──…╯` bottom) are not a composer.
        let l = with(
            SPLASH,
            &["╭─ Pick ───╮", "│  🔍 >     │", "╰───────────╯"],
            &[],
        );
        assert_eq!(inspect(&st(l, 7, 8)), Composer::Missing);
    }

    #[test]
    fn rule_and_borderless_shape() {
        let rule = "─".repeat(60);
        let hint = hinted("❯", "");
        let l = with(SPLASH, &[&rule, &hint, STATUS], &[]);
        assert_eq!(inspect(&st(l, 7, 2)), Composer::Empty);
        let l = with(SPLASH, &[&rule, "❯ hello", "  two", STATUS], &[]);
        assert_eq!(
            inspect(&st(l, 8, 5)),
            Composer::Draft("hello ↵ two".to_owned())
        );
        let l = with(SPLASH, &["❯ hello", "  two", STATUS], &[]);
        assert_eq!(
            inspect(&st(l, 7, 5)),
            Composer::Draft("hello ↵ two".to_owned())
        );
    }

    #[test]
    fn field_shape() {
        let hint = hinted("▐", " ▌");
        let l = with(SPLASH, &[&hint, "", STATUS], &[]);
        assert_eq!(inspect(&st(l, 6, 2)), Composer::Empty);
        let l = with(
            SPLASH,
            &["▐ hello          ▌", "▐ two            ▌", "", STATUS],
            &[],
        );
        assert_eq!(
            inspect(&st(l, 7, 5)),
            Composer::Draft("hello ↵ two".to_owned())
        );
    }

    #[test]
    fn pi_shape() {
        let rule = "─".repeat(60);
        let hint = hinted("", "");
        let l = with(SPLASH, &[&rule, &hint, &rule, STATUS], &[]);
        assert_eq!(inspect(&st(l, 7, 1)), Composer::Empty);
        let l = with(SPLASH, &[&rule, " hello", " two", &rule, STATUS], &[]);
        assert_eq!(
            inspect(&st(l, 8, 4)),
            Composer::Draft("hello ↵ two".to_owned())
        );
    }

    #[test]
    fn unrelated_screen_is_missing() {
        assert_eq!(inspect(&screen(&[], 0, 0)), Composer::Missing);
        assert_eq!(
            inspect(&screen(&["$ ls", "foo bar"], 1, 3)),
            Composer::Missing
        );
    }

    /// An open inline surface with `dock` and `layer` children (JSON node objects), `focus` on
    /// the given node, and a program that advertises `features`.
    fn tsp_document(
        dock: serde_json::Value,
        layer: serde_json::Value,
        focus: Option<&str>,
        features: &[&str],
    ) -> Document {
        use deck_tsp::document::Program;
        use deck_tsp::stream::{Message, Verb};
        let msg = |verb, body: serde_json::Value| Message {
            verb,
            params: Vec::new(),
            body: serde_json::to_vec(&body).expect("json"),
        };
        let mut doc = Document::new();
        doc.set_program(Program {
            app: Some("omp".to_owned()),
            ver: None,
            features: features.iter().map(|f| (*f).to_owned()).collect(),
        });
        let now = std::time::Instant::now();
        let open = serde_json::json!({"id": "s", "mode": "inline", "role": "omp.session"});
        doc.apply(&msg(Verb::Open, open), now);
        let mut ops = vec![
            serde_json::json!(["add", "dock", "s", null, {"id": "dock", "k": "col"}]),
            serde_json::json!(["add", "layer", "s", null, {"id": "layer", "k": "col"}]),
        ];
        for (region, nodes) in [("dock", dock), ("layer", layer)] {
            for node in nodes.as_array().expect("array") {
                ops.push(serde_json::json!(["add", node["id"], region, null, node]));
            }
        }
        ops.push(serde_json::json!(["focus", focus]));
        let frame = serde_json::json!({"sf": "s", "s": 1, "ops": ops});
        let applied = doc.apply(&msg(Verb::Frame, frame), now);
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        doc
    }

    fn editor(text: &str, sendable: bool) -> serde_json::Value {
        serde_json::json!({"id": "ed", "k": "editor", "p": {"text": text, "sendable": sendable}})
    }

    #[test]
    fn the_tsp_composer_is_the_docks_sendable_editor_in_focus() {
        let of = |dock, layer, focus: Option<&str>| {
            TspComposer::of(&tsp_document(dock, layer, focus, &["edit", "send"]))
        };
        let none = serde_json::json!([]);
        let target = Some(("s".to_owned(), "ed".to_owned()));

        let empty = of(
            serde_json::json!([editor("", true)]),
            none.clone(),
            Some("ed"),
        );
        assert_eq!(empty.state, Composer::Empty);
        assert_eq!(empty.target, target);
        assert!(empty.can_send && !empty.popup && empty.chips == 0);

        // Blank rows are no draft; a draft reads like the grid's: rows joined by ` ↵ `.
        let blank = of(
            serde_json::json!([editor("  \n ", true)]),
            none.clone(),
            Some("ed"),
        );
        assert_eq!(blank.state, Composer::Empty);
        let draft = of(
            serde_json::json!([editor("first\n\n second ", true)]),
            none.clone(),
            Some("ed"),
        );
        assert_eq!(draft.state, Composer::Draft("first ↵ second".to_owned()));

        // An editor that cannot send (omp is not wired yet) is no composer; nor is one that does
        // not hold the cursor (an overlay took the focus).
        for (ed, focus) in [(editor("", false), Some("ed")), (editor("", true), None)] {
            let c = of(serde_json::json!([ed]), none.clone(), focus);
            assert_eq!((c.state, c.target), (Composer::Missing, None));
        }
        // An ask dialog replaces the composer, whatever else the dock holds.
        let ask = serde_json::json!({"id": "ask", "k": "col", "p": {"role": "omp.editor.ask"}});
        let c = of(
            serde_json::json!([editor("", true), ask]),
            none.clone(),
            Some("ed"),
        );
        assert_eq!(c.state, Composer::Missing);

        // Attachment chips and the autocomplete popup are seen wherever they sit in their region.
        let chip = serde_json::json!({"id": "c1", "k": "card", "p": {"role": "omp.composer.chip"}});
        let chips = serde_json::json!({"id": "m", "k": "row", "c": [chip]});
        let popup =
            serde_json::json!({"id": "pop", "k": "overlay", "p": {"role": "omp.autocomplete"}});
        let c = of(
            serde_json::json!([editor("/ef", true), chips]),
            serde_json::json!([popup]),
            Some("ed"),
        );
        assert_eq!((c.chips, c.popup), (1, true));
        assert_ne!(c.fingerprint(), empty.fingerprint());

        // `send` must be in the hello.
        let doc = tsp_document(
            serde_json::json!([editor("", true)]),
            none,
            Some("ed"),
            &["edit"],
        );
        assert!(!TspComposer::of(&doc).can_send);
    }
}
