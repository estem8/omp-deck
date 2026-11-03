//! Markdown of the chat: `md` nodes (assistant answers, user prompts, thinking bodies) and plain
//! strings (`show_text`) drawn as selectable egui text in the "signal edge" language.
//!
//! The text is parsed once per (node id, generation) into an owned block tree ([`Cache`]); every
//! frame only lays that tree out. Paragraphs are single selectable galleys (links are char ranges
//! of the galley's text), code blocks and tables are framed cards with cut corners and an accent
//! hairline, everything is coloured from [`Ink`] only.

use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use egui::epaint::text::VariationCoords;
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Color32, CornerRadius, CursorIcon, FontId, Id, Layout, Margin, Rect, ScrollArea, Sense,
    Shape, Stroke, Ui, UiBuilder, pos2, vec2,
};
use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd, TextMergeStream,
};
use serde_json::Value;

use super::ctx::{Cx, Tx};
use super::highlight;
use super::render::{self, Link};
use super::snap::SNode;
use super::style::{Ink, LABEL, PROSE, PROSE_LINE};
use super::text::{self, Base};
use super::tsel::{Lead, RunOpts, Sep};
use super::{glyphs, paint};
use crate::{deco, theme};

/// Parsed documents kept before the whole cache is dropped.
const MAX_DOCS: usize = 512;
/// How deep containers (quotes, lists, items) nest before deeper ones are merged into their parent.
const MAX_NEST: usize = 16;
/// How long a code block's Copy button shows its check mark.
const COPIED_FOR: std::time::Duration = std::time::Duration::from_millis(1200);
/// Height of a code block's header strip (px).
const CODE_HEAD: f32 = 28.0;
/// Cut of the frames' top-right / bottom-left corners (px).
const CUT: f32 = 9.0;
/// Heading sizes relative to the prose size (h1..h6).
const HEADING_K: [f32; 6] = [1.45, 1.25, 1.1, 1.0, 1.0, 1.0];
/// Line height of headings relative to their size.
const HEADING_LINE: f32 = 1.3;

// ---- parsed tree ----------------------------------------------------------------------------

const BOLD: u8 = 1;
const ITALIC: u8 = 2;
const STRIKE: u8 = 4;
const CODE: u8 = 8;
const LINK: u8 = 16;
/// Image alt text.
const DIM: u8 = 32;

#[derive(Debug, Clone, PartialEq)]
struct Run {
    text: String,
    marks: u8,
    /// Style tokens of a prompt chip (a `marks` entry of the node); the run is drawn as a chip.
    chip: Option<Arc<str>>,
}

/// A chip token of a sent prompt: every occurrence of `t` in the text is drawn as a chip styled by
/// the tokens `s` (omp's `tokenMarks`: skills, mentions, references).
#[derive(Debug, Clone, PartialEq)]
struct Mark {
    t: String,
    s: Arc<str>,
}

/// The usable entries of a node's `marks` prop.
fn marks_of(raw: &[Value]) -> Vec<Mark> {
    raw.iter()
        .filter_map(|m| {
            let t = m
                .get("t")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())?;
            let s = m.get("s").and_then(Value::as_str).unwrap_or("");
            Some(Mark {
                t: t.to_owned(),
                s: s.into(),
            })
        })
        .collect()
}

/// A cheap fingerprint of a node's `marks` prop: the count and the byte lengths of its tokens, no
/// hashing (a changed generation already says the node changed; this guards a reused one).
fn marks_sig(raw: &[Value]) -> u64 {
    raw.iter().fold(raw.len() as u64, |acc, m| {
        ["t", "s"].iter().fold(acc, |a, k| {
            let len = m.get(k).and_then(Value::as_str).map_or(0, str::len);
            a.wrapping_mul(31).wrapping_add(len as u64 + 1)
        })
    })
}

/// Appends `text` as a run of `marks`, joined to the last run when that is the same look.
fn push_run(runs: &mut Vec<Run>, text: &str, marks: u8) {
    match runs.last_mut() {
        Some(r) if r.marks == marks && r.chip.is_none() => r.text.push_str(text),
        _ => runs.push(Run {
            text: text.to_owned(),
            marks,
            chip: None,
        }),
    }
}

/// Appends `seg` (runs of plain text) to `out` with the occurrences of `marks` in their
/// concatenated text as chip runs; the rest keeps the runs' looks.
fn chip_runs(out: &mut Vec<Run>, seg: &[Run], marks: &[Mark]) {
    let text: String = seg.iter().map(|r| r.text.as_str()).collect();
    let mut starts = Vec::with_capacity(seg.len() + 1);
    let mut end = 0;
    for r in seg {
        starts.push(end);
        end += r.text.len();
    }
    starts.push(end);
    let plain = |out: &mut Vec<Run>, from: usize, to: usize| {
        for (r, w) in seg.iter().zip(starts.windows(2)) {
            let (a, b) = (w[0].max(from), w[1].min(to));
            if a < b {
                push_run(out, &text[a..b], r.marks);
            }
        }
    };
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let hit = marks
            .iter()
            .filter_map(|m| rest.find(m.t.as_str()).map(|i| (i, m)))
            .min_by_key(|(i, m)| (*i, std::cmp::Reverse(m.t.len())));
        let Some((i, m)) = hit else {
            plain(out, at, text.len());
            return;
        };
        plain(out, at, at + i);
        let from = at + i;
        let k = starts
            .partition_point(|&s| s <= from)
            .saturating_sub(1)
            .min(seg.len() - 1);
        out.push(Run {
            text: m.t.clone(),
            marks: seg[k].marks,
            chip: Some(Arc::clone(&m.s)),
        });
        at = from + m.t.len();
    }
}

fn hash_of(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// A line of styled runs; `links` are char ranges of the runs' concatenated text.
#[derive(Debug, Clone, Default, PartialEq)]
struct Inline {
    runs: Vec<Run>,
    links: Vec<Link>,
    /// Chars in all runs.
    len: usize,
}

impl Inline {
    fn push(&mut self, text: &str, marks: u8) {
        if text.is_empty() {
            return;
        }
        self.len += text.chars().count();
        push_run(&mut self.runs, text, marks);
    }

    /// Draws every occurrence of a mark as a chip (the earliest match wins, the longest one on a
    /// tie). Matches the text of the whole line, so a token that emphasis split into several
    /// runs (`@pkg/__init__.py`) is still found; code and links are left alone.
    fn apply_marks(&mut self, marks: &[Mark]) {
        if marks.is_empty() {
            return;
        }
        let open = |r: &Run| r.chip.is_none() && r.marks & (CODE | LINK) == 0;
        let old = std::mem::take(&mut self.runs);
        let mut i = 0;
        while i < old.len() {
            let end = if open(&old[i]) {
                old[i..]
                    .iter()
                    .position(|r| !open(r))
                    .map_or(old.len(), |p| i + p)
            } else {
                i + 1
            };
            if open(&old[i]) {
                chip_runs(&mut self.runs, &old[i..end], marks);
            } else {
                self.runs.extend_from_slice(&old[i..end]);
            }
            i = end;
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Drops trailing whitespace (hard breaks, the newline of HTML lines).
    fn trim_end(&mut self) {
        while let Some(last) = self.runs.last_mut() {
            let kept = last.text.trim_end().len();
            self.len -= last.text[kept..].chars().count();
            last.text.truncate(kept);
            if last.text.is_empty() {
                self.runs.pop();
            } else {
                break;
            }
        }
        let len = self.len;
        self.links.retain(|l| l.from < len);
        for l in &mut self.links {
            l.to = l.to.min(len);
        }
    }

    #[cfg(test)]
    fn plain(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }

    /// The runs as a layout job in `base`; `bold` forces the strong look (headings, table head),
    /// `caret` appends the streaming caret block in that colour.
    fn job(&self, ink: &Ink, base: &Base, bold: bool, caret: Option<Color32>) -> LayoutJob {
        let mut job = LayoutJob::default();
        for r in &self.runs {
            job.append(&r.text, 0.0, run_format(ink, base, r, bold));
        }
        if let Some(c) = caret {
            job.append(" ", 0.0, caret_format(base, c));
        }
        job
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Item {
    task: Option<bool>,
    blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
struct Table {
    aligns: Vec<Alignment>,
    head: Vec<Inline>,
    rows: Vec<Vec<Inline>>,
}

#[derive(Debug, Clone, PartialEq)]
enum Block {
    Para(Inline),
    Heading(u8, Inline),
    List {
        start: Option<u64>,
        items: Vec<Item>,
    },
    Quote(Vec<Block>),
    Code {
        lang: String,
        text: String,
        /// Key of the Copy button's feedback in `Local.copied` (and the scroll area's id, and of
        /// the highlight).
        id: String,
        /// Fingerprint of `text`: the generation of its highlight.
        hash: u64,
    },
    Rule,
    Table(Table),
}

/// One parsed document.
#[derive(Debug, PartialEq)]
struct Doc {
    blocks: Vec<Block>,
    /// Byte length of the source (a cheap guard against a caller reusing a generation).
    src_len: usize,
    /// Fingerprint of the `marks` the text was parsed with.
    marks_sig: u64,
}

/// Parsed documents by node id (or the caller's key), valid for one generation.
#[derive(Default)]
pub struct Cache {
    docs: HashMap<String, (u64, Arc<Doc>)>,
}

impl Cache {
    fn doc(&mut self, key: &str, gen_: u64, text: &str, marks: &[Value]) -> Arc<Doc> {
        let sig = marks_sig(marks);
        if let Some((g, d)) = self.docs.get(key)
            && *g == gen_
            && d.src_len == text.len()
            && d.marks_sig == sig
        {
            return Arc::clone(d);
        }
        if self.docs.len() >= MAX_DOCS {
            self.docs.clear();
        }
        let doc = Arc::new(parse_doc(key, text, marks));
        self.docs.insert(key.to_owned(), (gen_, Arc::clone(&doc)));
        doc
    }
}

// ---- parsing --------------------------------------------------------------------------------

/// The source with the occurrences of the marks backslash-escaped when a token holds a markdown
/// delimiter (`@pkg/__init__.py`): the parser then keeps the token whole instead of reading
/// `__init__` as emphasis. Occurrences inside code are left as written.
fn protect_marks<'a>(src: &'a str, marks: &[Mark], opts: Options) -> Cow<'a, str> {
    const DELIMS: &[char] = &[
        '*', '_', '~', '`', '[', ']', '<', '>', '&', '\\', '!', '#', '|',
    ];
    if !marks.iter().any(|m| m.t.contains(DELIMS)) {
        return Cow::Borrowed(src);
    }
    let mut code: Vec<std::ops::Range<usize>> = Vec::new();
    for (ev, range) in Parser::new_ext(src, opts).into_offset_iter() {
        if matches!(ev, Event::Code(_) | Event::Start(Tag::CodeBlock(_))) {
            code.push(range);
        }
    }
    let mut out = String::with_capacity(src.len() + 16);
    let mut at = 0;
    while at < src.len() {
        let rest = &src[at..];
        let hit = marks
            .iter()
            .filter_map(|m| rest.find(m.t.as_str()).map(|i| (i, m)))
            .min_by_key(|(i, m)| (*i, std::cmp::Reverse(m.t.len())));
        let Some((i, m)) = hit else {
            out.push_str(rest);
            break;
        };
        let from = at + i;
        out.push_str(&src[at..from]);
        if code.iter().any(|r| r.contains(&from)) {
            let step = rest[i..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&src[from..from + step]);
            at = from + step;
            continue;
        }
        for c in m.t.chars() {
            if c.is_ascii_punctuation() {
                out.push('\\');
            }
            out.push(c);
        }
        at = from + m.t.len();
    }
    Cow::Owned(out)
}

fn parse_doc(key: &str, src: &str, marks: &[Value]) -> Doc {
    let clean = text::clean(src);
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let list = marks_of(marks);
    let clean = protect_marks(&clean, &list, opts);
    let mut b = Builder::new(key, list);
    for ev in TextMergeStream::new(Parser::new_ext(&clean, opts)) {
        b.event(ev);
    }
    Doc {
        blocks: b.finish(),
        src_len: src.len(),
        marks_sig: marks_sig(marks),
    }
}

enum Frame {
    Root(Vec<Block>),
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<Item>,
    },
    Item(Item),
}

#[derive(Clone, Copy)]
enum AccKind {
    Para,
    Heading(u8),
    Cell,
}

struct OpenLink {
    url: String,
    from: usize,
    /// Whether the target may be opened (http, https, mailto).
    live: bool,
}

/// The inline content being collected.
struct Acc {
    kind: AccKind,
    /// Started by loose text of a tight list item (no paragraph tag): ends with the next block.
    implicit: bool,
    inline: Inline,
    bold: u8,
    italic: u8,
    strike: u8,
    link: Option<OpenLink>,
    /// Destination and `len` at the start of an image's alt text.
    image: Option<(String, usize)>,
}

impl Acc {
    fn new(kind: AccKind, implicit: bool) -> Acc {
        Acc {
            kind,
            implicit,
            inline: Inline::default(),
            bold: 0,
            italic: 0,
            strike: 0,
            link: None,
            image: None,
        }
    }

    fn marks(&self) -> u8 {
        let mut m = 0;
        if self.bold > 0 {
            m |= BOLD;
        }
        if self.italic > 0 {
            m |= ITALIC;
        }
        if self.strike > 0 {
            m |= STRIKE;
        }
        if self.image.is_some() {
            m |= DIM;
        }
        if self.link.as_ref().is_some_and(|l| l.live) {
            m |= LINK;
        }
        m
    }
}

struct TableAcc {
    aligns: Vec<Alignment>,
    head: Vec<Inline>,
    rows: Vec<Vec<Inline>>,
    cur: Vec<Inline>,
}

struct Builder {
    key: String,
    marks: Arc<[Mark]>,
    frames: Vec<Frame>,
    /// Containers beyond [`MAX_NEST`] (and everything nested in them) that were not pushed.
    skip: usize,
    acc: Option<Acc>,
    code: Option<(String, String)>,
    table: Option<TableAcc>,
    n_code: usize,
}

impl Builder {
    fn new(key: &str, marks: Vec<Mark>) -> Builder {
        Builder {
            key: key.to_owned(),
            marks: marks.into(),
            frames: vec![Frame::Root(Vec::new())],
            skip: 0,
            acc: None,
            code: None,
            table: None,
            n_code: 0,
        }
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush_implicit();
        if self.acc.is_some() {
            self.finish_acc();
        }
        match self.frames.into_iter().next() {
            Some(Frame::Root(b)) => b,
            _ => Vec::new(),
        }
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(end) => self.end(end),
            Event::Text(t) => self.text(&t),
            Event::Code(c) | Event::InlineMath(c) | Event::DisplayMath(c) => {
                let acc = self.acc_mut();
                let marks = acc.marks() | CODE;
                acc.inline.push(&c, marks);
            }
            Event::Html(h) | Event::InlineHtml(h) => {
                if matches!(h.trim(), "<br>" | "<br/>" | "<br />") {
                    self.raw("\n");
                } else {
                    self.text(&h);
                }
            }
            Event::FootnoteReference(l) => self.raw(&format!("[^{l}]")),
            // Chat text keeps its line breaks: a prompt typed over several lines stays that way.
            Event::SoftBreak | Event::HardBreak => self.raw("\n"),
            Event::Rule => {
                self.flush_implicit();
                self.push_block(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some(Frame::Item(item)) = self.frames.last_mut() {
                    item.task = Some(done);
                }
            }
        }
    }

    fn acc_mut(&mut self) -> &mut Acc {
        self.acc
            .get_or_insert_with(|| Acc::new(AccKind::Para, true))
    }

    fn begin(&mut self, kind: AccKind) {
        self.acc = Some(Acc::new(kind, false));
    }

    fn flush_implicit(&mut self) {
        if self.acc.as_ref().is_some_and(|a| a.implicit) {
            self.finish_acc();
        }
    }

    fn finish_acc(&mut self) {
        let Some(mut acc) = self.acc.take() else {
            return;
        };
        acc.inline.apply_marks(&self.marks);
        acc.inline.trim_end();
        match acc.kind {
            AccKind::Para => {
                if !acc.inline.is_empty() {
                    self.push_block(Block::Para(acc.inline));
                }
            }
            AccKind::Heading(level) => {
                if !acc.inline.is_empty() {
                    self.push_block(Block::Heading(level, acc.inline));
                }
            }
            AccKind::Cell => {
                if let Some(t) = &mut self.table {
                    t.cur.push(acc.inline);
                }
            }
        }
    }

    fn push_block(&mut self, b: Block) {
        match self.frames.last_mut() {
            Some(Frame::Root(v) | Frame::Quote(v)) => v.push(b),
            Some(Frame::Item(i)) => i.blocks.push(b),
            Some(Frame::List { items, .. }) => {
                if items.is_empty() {
                    items.push(Item {
                        task: None,
                        blocks: Vec::new(),
                    });
                }
                if let Some(i) = items.last_mut() {
                    i.blocks.push(b);
                }
            }
            None => {}
        }
    }

    /// Opens a container, or merges it into its parent once the nesting limit is reached.
    fn open(&mut self, f: Frame) {
        if self.skip > 0 || self.frames.len() >= MAX_NEST {
            self.skip += 1;
        } else {
            self.frames.push(f);
        }
    }

    /// Closes the innermost container (`None` when it was merged into its parent).
    fn close(&mut self) -> Option<Frame> {
        if self.skip > 0 {
            self.skip -= 1;
            return None;
        }
        if self.frames.len() > 1 {
            self.frames.pop()
        } else {
            None
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.flush_implicit();
                self.begin(AccKind::Para);
            }
            Tag::Heading { level, .. } => {
                self.flush_implicit();
                self.begin(AccKind::Heading(level as u8));
            }
            Tag::HtmlBlock => {
                self.flush_implicit();
                self.begin(AccKind::Para);
            }
            Tag::BlockQuote(_) => {
                self.flush_implicit();
                self.open(Frame::Quote(Vec::new()));
            }
            Tag::CodeBlock(kind) => {
                self.flush_implicit();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_owned()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.flush_implicit();
                self.open(Frame::List {
                    start,
                    items: Vec::new(),
                });
            }
            Tag::Item => {
                self.flush_implicit();
                self.open(Frame::Item(Item {
                    task: None,
                    blocks: Vec::new(),
                }));
            }
            Tag::Table(aligns) => {
                self.flush_implicit();
                self.table = Some(TableAcc {
                    aligns,
                    head: Vec::new(),
                    rows: Vec::new(),
                    cur: Vec::new(),
                });
            }
            Tag::TableCell => self.begin(AccKind::Cell),
            Tag::Emphasis => self.acc_mut().italic += 1,
            Tag::Strong => self.acc_mut().bold += 1,
            Tag::Strikethrough => self.acc_mut().strike += 1,
            Tag::Link { dest_url, .. } => {
                let url = text::clean(&dest_url).into_owned();
                let live = openable(&url);
                let acc = self.acc_mut();
                let from = acc.inline.len;
                acc.link = Some(OpenLink { url, from, live });
            }
            Tag::Image { dest_url, .. } => {
                let url = text::clean(&dest_url).into_owned();
                let acc = self.acc_mut();
                let at = acc.inline.len;
                acc.image = Some((url, at));
            }
            // Not enabled in the parser options; their content flows as ordinary text.
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::TableHead
            | Tag::TableRow
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::HtmlBlock | TagEnd::TableCell => {
                self.finish_acc();
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head = std::mem::take(&mut t.cur);
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.cur);
                    t.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.push_block(Block::Table(Table {
                        aligns: t.aligns,
                        head: t.head,
                        rows: t.rows,
                    }));
                }
            }
            TagEnd::CodeBlock => {
                if let Some((lang, mut text)) = self.code.take() {
                    if text.ends_with('\n') {
                        text.pop();
                    }
                    let id = format!("md:{}:{}", self.key, self.n_code);
                    self.n_code += 1;
                    let hash = hash_of(&text);
                    self.push_block(Block::Code {
                        lang,
                        text,
                        id,
                        hash,
                    });
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush_implicit();
                if let Some(Frame::Quote(blocks)) = self.close() {
                    self.push_block(Block::Quote(blocks));
                }
            }
            TagEnd::List(_) => {
                self.flush_implicit();
                if let Some(Frame::List { start, items }) = self.close() {
                    self.push_block(Block::List { start, items });
                }
            }
            TagEnd::Item => {
                self.flush_implicit();
                if let Some(Frame::Item(item)) = self.close() {
                    match self.frames.last_mut() {
                        Some(Frame::List { items, .. }) => items.push(item),
                        _ => {
                            for b in item.blocks {
                                self.push_block(b);
                            }
                        }
                    }
                }
            }
            TagEnd::Emphasis => {
                let acc = self.acc_mut();
                acc.italic = acc.italic.saturating_sub(1);
            }
            TagEnd::Strong => {
                let acc = self.acc_mut();
                acc.bold = acc.bold.saturating_sub(1);
            }
            TagEnd::Strikethrough => {
                let acc = self.acc_mut();
                acc.strike = acc.strike.saturating_sub(1);
            }
            TagEnd::Link => {
                let acc = self.acc_mut();
                if let Some(l) = acc.link.take()
                    && l.live
                    && acc.inline.len > l.from
                {
                    let to = acc.inline.len;
                    acc.inline.links.push(Link {
                        from: l.from,
                        to,
                        url: l.url,
                    });
                }
            }
            TagEnd::Image => {
                let acc = self.acc_mut();
                if let Some((url, at)) = acc.image.take()
                    && acc.inline.len == at
                {
                    let marks = acc.marks() | DIM;
                    acc.inline.push(&url, marks);
                }
            }
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    /// Text without link detection (line breaks).
    fn raw(&mut self, t: &str) {
        if let Some((_, buf)) = &mut self.code {
            buf.push_str(t);
            return;
        }
        let acc = self.acc_mut();
        let marks = acc.marks();
        acc.inline.push(t, marks);
    }

    /// Text; bare http(s) URLs outside links become links.
    fn text(&mut self, t: &str) {
        if let Some((_, buf)) = &mut self.code {
            buf.push_str(t);
            return;
        }
        let acc = self.acc_mut();
        let marks = acc.marks();
        if acc.link.is_some() || acc.image.is_some() {
            acc.inline.push(t, marks);
            return;
        }
        let mut at = 0;
        for (a, b) in url_ranges(t) {
            acc.inline.push(&t[at..a], marks);
            let from = acc.inline.len;
            acc.inline.push(&t[a..b], marks | LINK);
            acc.inline.links.push(Link {
                from,
                to: acc.inline.len,
                url: t[a..b].to_owned(),
            });
            at = b;
        }
        acc.inline.push(&t[at..], marks);
    }
}

/// Whether omp-deck may open `url` (never `file:`, `javascript:` or bare relative paths).
fn openable(url: &str) -> bool {
    let u = url.trim();
    ["http://", "https://", "mailto:"]
        .iter()
        .any(|p| u.get(..p.len()).is_some_and(|s| s.eq_ignore_ascii_case(p)))
}

/// Byte ranges of the bare `http(s)://` URLs of `s` (trailing punctuation and unbalanced closing
/// brackets excluded).
fn url_ranges(s: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(off) = s[from..].find("http") {
        let start = from + off;
        let rest = &s[start..];
        let scheme = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            from = start + 4;
            continue;
        };
        let boundary = s[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        if !boundary {
            from = start + scheme;
            continue;
        }
        let mut end = start
            + rest
                .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`'))
                .unwrap_or(rest.len());
        while let Some(c) = s[start..end].chars().next_back() {
            let u = &s[start..end];
            let trim = match c {
                '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' => true,
                ')' => u.matches(')').count() > u.matches('(').count(),
                ']' => u.matches(']').count() > u.matches('[').count(),
                '}' => u.matches('}').count() > u.matches('{').count(),
                _ => false,
            };
            if !trim {
                break;
            }
            end -= c.len_utf8();
        }
        if end > start + scheme {
            out.push((start, end));
        }
        from = end.max(start + scheme);
    }
    out
}

// ---- entry points ---------------------------------------------------------------------------

/// A `md` node: its `text` prop, streaming while `stream` is set.
pub fn show(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let text = n.str("text").unwrap_or("");
    let stream = n.flag("stream");
    let marks = n.arr("marks");
    match n.tone() {
        Some(t) if t != "neutral" => {
            let mut tx = cx.tx();
            tx.color = cx.ink.tone(t);
            cx.with_tx(tx, || {
                show_text(cx, ui, &n.id, n.gen_, text, marks, stream);
            });
        }
        _ => show_text(cx, ui, &n.id, n.gen_, text, marks, stream),
    }
}

/// Markdown `text` under the caller's cache `key` (valid for generation `gen_`), its `marks`
/// drawn as chips. `stream` draws the blinking caret at the end and keeps the frame animating.
pub fn show_text(
    cx: &Cx,
    ui: &mut Ui,
    key: &str,
    gen_: u64,
    text: &str,
    marks: &[Value],
    stream: bool,
) {
    let doc = cx.loc(|l| l.md.doc(key, gen_, text, marks));
    if doc.blocks.is_empty() && !stream {
        return;
    }
    let look = Look::new(cx.ink, &cx.tx());
    let caret = if stream { stream_caret(cx, ui) } else { None };
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let used = blocks(cx, ui, &look, 0, &doc.blocks, caret, Flow::LOOSE);
        if let Some(c) = caret
            && !used
        {
            caret_line(cx, ui, &look, c);
        }
    });
}

/// The caret's colour this frame (blinks), `None` when motion is reduced.
fn stream_caret(cx: &Cx, ui: &Ui) -> Option<Color32> {
    if cx.ink.reduce_motion {
        return None;
    }
    cx.animate();
    let on = (ui.input(|i| i.time) * 1.6) as i64 % 2 == 0;
    Some(if on { cx.ink.a } else { Color32::TRANSPARENT })
}

// ---- looks ----------------------------------------------------------------------------------

/// Prose face, size and colour a subtree is drawn in.
#[derive(Clone)]
struct Look {
    font: FontId,
    color: Color32,
}

impl Look {
    /// The current plain-text look when it is a prose face, else the reading face.
    fn new(ink: &Ink, tx: &Tx) -> Look {
        let mono = ink.mono(1.0).family;
        let prose = tx.font.family != mono && tx.font.family != egui::FontFamily::Monospace;
        Look {
            font: if prose {
                tx.font.clone()
            } else {
                ink.prose(PROSE)
            },
            color: tx.color,
        }
    }

    fn size(&self) -> f32 {
        self.font.size
    }

    /// Layout base at `k` × the prose size with `line` × that size as line height.
    fn base(&self, ink: &Ink, k: f32, color: Color32, line: f32) -> Base {
        let size = self.font.size * k;
        Base::new(FontId::new(size, self.font.family.clone()), color, ink).lined(size * line)
    }
}

/// Format of a run of `marks` on top of `base`.
fn run_format(ink: &Ink, base: &Base, run: &Run, bold: bool) -> TextFormat {
    let marks = run.marks;
    let mut f = base.format_plain();
    if (bold || marks & BOLD != 0) && marks & CODE == 0 {
        f.color = ink.strong();
        f.coords = VariationCoords::new([("wght", 640.0_f32)]);
    }
    if marks & ITALIC != 0 {
        f.italics = true;
    }
    if marks & CODE != 0 {
        f.font_id = base.mono.clone();
        f.background = ink.chip();
        f.color = theme::mix(base.color, ink.a, 0.3);
    }
    if marks & DIM != 0 {
        f.color = ink.dim();
    }
    if marks & LINK != 0 {
        f.color = ink.tone("accent");
        f.underline = Stroke::new(1.0, f.color);
    }
    if marks & STRIKE != 0 {
        f.strikethrough = Stroke::new(1.0, f.color);
    }
    if let Some(tokens) = &run.chip {
        let c = chip_color(ink, tokens);
        f.color = c;
        f.background = theme::mix(ink.wash(c), c, 0.16);
        f.expand_bg = 3.0;
        f.coords = VariationCoords::new([("wght", 640.0_f32)]);
    }
    f
}

/// Colour of a chip: the last colouring token of its style (`strong` only sets the weight).
fn chip_color(ink: &Ink, tokens: &str) -> Color32 {
    tokens
        .split_whitespace()
        .filter(|t| *t != "strong")
        .filter_map(|t| text::span_style(ink, t).color)
        .next_back()
        .unwrap_or(ink.a)
}

/// The caret: one space wide block of `c` (the glyph itself is invisible).
fn caret_format(base: &Base, c: Color32) -> TextFormat {
    let mut f = base.format_plain();
    f.color = Color32::TRANSPARENT;
    f.background = c;
    f.expand_bg = 0.0;
    f
}

fn caret_line(cx: &Cx, ui: &mut Ui, look: &Look, c: Color32) {
    let base = look.base(cx.ink, 1.0, look.color, PROSE_LINE);
    let mut job = LayoutJob::default();
    job.append(" ", 0.0, caret_format(&base, c));
    render::para(cx, ui, job, look.color, &[], RunOpts::INERT);
}

// ---- blocks ---------------------------------------------------------------------------------

fn gap(look: &Look, prev: &Block, next: &Block, tight: bool) -> f32 {
    let s = look.size();
    if tight {
        return s * 0.3;
    }
    match (prev, next) {
        (Block::Heading(..), _) => s * 0.4,
        (_, Block::Heading(l, _)) => {
            if *l <= 2 {
                s * 1.1
            } else {
                s * 0.85
            }
        }
        (Block::Code { .. } | Block::Table(_) | Block::Quote(_), _)
        | (_, Block::Code { .. } | Block::Table(_) | Block::Quote(_)) => s * 0.75,
        _ => s * 0.6,
    }
}

/// How a run of blocks follows each other: packed (a list item's) or spaced, and the list marker
/// the first block carries in a copy.
#[derive(Clone, Copy)]
struct Flow {
    tight: bool,
    lead: Lead,
}

impl Flow {
    const LOOSE: Flow = Flow {
        tight: false,
        lead: Lead::None,
    };
}

/// Draws `bs` stacked; `caret` (the streaming caret) goes to the end of the last block. Returns
/// whether the caret was drawn.
fn blocks(
    cx: &Cx,
    ui: &mut Ui,
    look: &Look,
    ld: usize,
    bs: &[Block],
    caret: Option<Color32>,
    flow: Flow,
) -> bool {
    let mut used = false;
    for (i, b) in bs.iter().enumerate() {
        if i > 0 {
            ui.add_space(gap(look, &bs[i - 1], b, flow.tight));
        }
        let last = i + 1 == bs.len();
        let opts = RunOpts::sep(if flow.tight { Sep::Line } else { Sep::Block }).lead(if i == 0 {
            flow.lead
        } else {
            Lead::None
        });
        let u = block(cx, ui, look, ld, b, if last { caret } else { None }, opts);
        if last {
            used = u;
        }
    }
    used
}

fn block(
    cx: &Cx,
    ui: &mut Ui,
    look: &Look,
    ld: usize,
    b: &Block,
    caret: Option<Color32>,
    opts: RunOpts,
) -> bool {
    let ink = cx.ink;
    match b {
        Block::Para(inl) => {
            let base = look.base(ink, 1.0, look.color, PROSE_LINE);
            let job = inl.job(ink, &base, false, caret);
            if job.is_empty() {
                return false;
            }
            let (_, url) = render::para(cx, ui, job, look.color, &inl.links, opts);
            if let Some(u) = url {
                cx.open(u);
            }
            caret.is_some()
        }
        Block::Heading(level, inl) => {
            let l = usize::from((*level).clamp(1, 6));
            let dim = l == 6;
            let color = if dim { ink.dim() } else { ink.strong() };
            let base = look.base(ink, HEADING_K[l - 1], color, HEADING_LINE);
            let job = inl.job(ink, &base, true, caret);
            let (_, url) = render::para(cx, ui, job, color, &inl.links, opts);
            if let Some(u) = url {
                cx.open(u);
            }
            if l <= 2 {
                ui.add_space(look.size() * 0.22);
                let (r, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                let k = if l == 1 { 1.0 } else { 0.7 };
                let stops = [
                    (0.0, ink.a.gamma_multiply(k)),
                    (0.65, ink.b.gamma_multiply(k)),
                    (1.0, ink.b.gamma_multiply(0.0)),
                ];
                deco::gradient_rule(
                    ui.painter(),
                    pos2(r.left(), r.center().y),
                    pos2(r.right(), r.center().y),
                    1.0,
                    &stops,
                );
            }
            caret.is_some()
        }
        Block::List { start, items } => list(cx, ui, look, ld, *start, items, caret),
        Block::Quote(bs) => quote(cx, ui, look, ld, bs, caret),
        Block::Code {
            lang,
            text,
            id,
            hash,
        } => {
            code_block(cx, ui, (lang, *hash), text, id, caret);
            caret.is_some()
        }
        Block::Rule => {
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 9.0), Sense::hover());
            let stops = [
                (0.0, ink.a.gamma_multiply(0.0)),
                (0.2, ink.a.gamma_multiply(0.85)),
                (0.8, ink.b.gamma_multiply(0.85)),
                (1.0, ink.b.gamma_multiply(0.0)),
            ];
            let y = r.center().y;
            deco::gradient_rule(
                ui.painter(),
                pos2(r.left(), y),
                pos2(r.right(), y),
                1.0,
                &stops,
            );
            false
        }
        Block::Table(t) => {
            table(cx, ui, look, t);
            false
        }
    }
}

// ---- lists & quotes -------------------------------------------------------------------------

fn list(
    cx: &Cx,
    ui: &mut Ui,
    look: &Look,
    ld: usize,
    start: Option<u64>,
    items: &[Item],
    caret: Option<Color32>,
) -> bool {
    let ink = cx.ink;
    let size = look.size();
    let line_h = size * PROSE_LINE;
    let indent = size * 0.3;
    let any_task = items.iter().any(|i| i.task.is_some());
    let mw = match start {
        Some(s) => {
            let last = s.saturating_add(items.len() as u64);
            let digits = last.to_string().len() as f32;
            (digits * size * 0.58 + size * 0.95).max(if any_task { size * 1.5 } else { 0.0 })
        }
        None => size * 1.4,
    };
    let mut used = false;
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            ui.add_space(size * 0.3);
        }
        let last = i + 1 == items.len();
        let item_look = if item.task == Some(true) {
            Look {
                font: look.font.clone(),
                color: ink.dim(),
            }
        } else {
            look.clone()
        };
        let lead = match (item.task, start) {
            (Some(done), _) => Lead::Task(done),
            (None, Some(s)) => Lead::Num(s.saturating_add(i as u64)),
            (None, None) => Lead::Bullet,
        };
        let r = ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let (m, _) = ui.allocate_exact_size(vec2(indent + mw, line_h), Sense::hover());
            let at = pos2(m.left() + indent, m.top());
            match item.task {
                Some(done) => {
                    let bs = size * 0.85;
                    let box_rect =
                        Rect::from_min_size(pos2(at.x, at.y + (line_h - bs) / 2.0), vec2(bs, bs));
                    checkbox(ui, ink, box_rect, done);
                }
                None => match start {
                    Some(s) => {
                        let n = s.saturating_add(i as u64);
                        let c = theme::mix(ink.dim(), ink.b, 0.5);
                        let g = ui
                            .painter()
                            .layout_no_wrap(format!("{n}."), look.font.clone(), c);
                        let pos = pos2(
                            m.right() - size * 0.35 - g.size().x,
                            m.center().y - g.size().y / 2.0,
                        );
                        ui.painter().galley(pos, g, c);
                    }
                    None => bullet(ui, ink, pos2(at.x + size * 0.45, m.center().y), ld, size),
                },
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                blocks(
                    cx,
                    ui,
                    &item_look,
                    ld + 1,
                    &item.blocks,
                    if last { caret } else { None },
                    Flow { tight: true, lead },
                )
            })
            .inner
        });
        if last {
            used = r.inner;
        }
    }
    used
}

/// A bullet whose shape follows the list depth: disc, ring, square.
fn bullet(ui: &Ui, ink: &Ink, c: egui::Pos2, ld: usize, size: f32) {
    let p = ui.painter();
    let r = (size * 0.17).max(2.2);
    match ld {
        0 => {
            p.circle_filled(c, r, ink.b);
        }
        1 => {
            p.circle_stroke(c, r, Stroke::new(1.3, ink.b));
        }
        _ => {
            p.rect_filled(
                Rect::from_center_size(c, vec2(r * 1.7, r * 1.7)),
                CornerRadius::same(1),
                ink.dim(),
            );
        }
    }
}

fn checkbox(ui: &Ui, ink: &Ink, rect: Rect, done: bool) {
    let p = ui.painter();
    let (fill, edge) = if done {
        (ink.wash(ink.a), ink.edge(ink.a))
    } else {
        (ink.chip(), ink.line())
    };
    paint::boxed(p, rect, 3, fill, Stroke::new(1.0, edge));
    if done {
        glyphs::paint(p, "check", rect.center(), rect.width() * 0.8, ink.a);
    }
}

fn quote(
    cx: &Cx,
    ui: &mut Ui,
    look: &Look,
    ld: usize,
    bs: &[Block],
    caret: Option<Color32>,
) -> bool {
    let ink = cx.ink;
    let painter = ui.painter().clone();
    let bg = paint::reserve(&painter);
    let x0 = ui.cursor().left();
    let w = ui.available_width().min(4000.0);
    let inner = Look {
        font: look.font.clone(),
        color: ink.dim(),
    };
    let r = ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(16.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add_space(4.0);
            let used = blocks(cx, ui, &inner, ld, bs, caret, Flow::LOOSE);
            ui.add_space(4.0);
            used
        })
        .inner
    });
    let rr = r.response.rect;
    let full = Rect::from_min_max(pos2(x0, rr.top()), pos2(x0 + w, rr.bottom()));
    painter.set(
        bg,
        Shape::rect_filled(
            full,
            CornerRadius {
                nw: 0,
                ne: 6,
                sw: 0,
                se: 6,
            },
            ink.chip().gamma_multiply(0.5),
        ),
    );
    deco::gradient_rule(
        &painter,
        pos2(x0 + 1.0, full.top()),
        pos2(x0 + 1.0, full.bottom()),
        2.0,
        &ink.rule_stops(),
    );
    r.inner
}

// ---- code blocks ----------------------------------------------------------------------------

fn code_block(
    cx: &Cx,
    ui: &mut Ui,
    (lang, hash): (&str, u64),
    code: &str,
    id: &str,
    caret: Option<Color32>,
) {
    let ink = cx.ink;
    let painter = ui.painter().clone();
    let bg = paint::reserve(&painter);
    let w = ui.available_width();
    let top_left = ui.cursor().min;
    let (head, _) = ui.allocate_exact_size(vec2(w, CODE_HEAD), Sense::hover());

    // Header strip: slanted gradient band, language mark and label, Copy button.
    let band_right = head.right() - CUT;
    let band = paint::slant_band(
        Rect::from_min_max(head.min + vec2(1.0, 1.0), pos2(band_right, head.bottom())),
        10.0,
    );
    let (h0, h1) = ink.head();
    paint::fill_h(&painter, &band, h0, h1);
    paint::hairline(
        &painter,
        ink,
        head.left() + 1.0,
        band_right,
        head.top(),
        0.9,
    );
    painter.line_segment(
        [
            pos2(head.left() + 1.0, head.bottom()),
            pos2(head.right() - 1.0, head.bottom()),
        ],
        Stroke::new(1.0, ink.line()),
    );
    let mark = Rect::from_min_size(
        pos2(head.left() + 12.0, head.center().y - 5.0),
        vec2(9.0, 10.0),
    );
    paint::fill_h(&painter, &paint::parallelogram(mark, 3.0), ink.a, ink.b);
    let label = if lang.is_empty() { "code" } else { lang };
    let g = painter.layout_no_wrap(label.to_owned(), ink.label(LABEL), ink.dim());
    painter.galley(
        pos2(mark.right() + 8.0, head.center().y - g.size().y / 2.0),
        g,
        ink.dim(),
    );
    let mut btn = ui.new_child(
        UiBuilder::new()
            .id_salt(("md-copy", id))
            .max_rect(Rect::from_min_max(
                pos2(head.left() + 10.0, head.top()),
                pos2(head.right() - CUT - 10.0, head.bottom()),
            ))
            .layout(Layout::right_to_left(Align::Center)),
    );
    copy_button(cx, &mut btn, id, code);

    // Body.
    let body = egui::Frame::NONE
        .inner_margin(Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            code_text(cx, ui, (lang, hash), id, code, caret);
        });

    let total = Rect::from_min_max(top_left, pos2(top_left.x + w, body.response.rect.bottom()));
    painter.set(
        bg,
        Shape::Vec(vec![
            deco::drop_shadow_shape(total, 8, 2, 8, ink.dark, 46),
            Shape::convex_polygon(
                paint::cut_bubble(total, CUT),
                ink.card(),
                Stroke::new(1.0, ink.line()),
            ),
        ]),
    );
}

/// The code as unwrapped mono text with syntax colours (once the worker has them); a horizontal
/// scroll area only when a line is too long.
fn code_text(
    cx: &Cx,
    ui: &mut Ui,
    (lang, hash): (&str, u64),
    id: &str,
    code: &str,
    caret: Option<Color32>,
) {
    let ink = cx.ink;
    let font = ink.mono(1.0);
    let line = font.size * 1.5;
    let mut fmt = TextFormat::simple(font, ink.text());
    fmt.line_height = Some(line);
    let hl = if lang.is_empty() {
        None
    } else {
        cx.loc(|l| {
            l.hl.get(ui.ctx(), id, hash, || {
                highlight::source_of_text(lang, "", code)
            })
        })
    };
    let colors = highlight::colors(ui.ctx(), ink, ink.card());
    let mut job = LayoutJob::default();
    highlight::push_text(&mut job, code, hl.as_ref(), &colors, &fmt);
    if let Some(c) = caret {
        fmt.color = Color32::TRANSPARENT;
        fmt.background = c;
        fmt.expand_bg = 0.0;
        job.append(" ", 0.0, fmt);
    }
    let galley = ui.painter().layout_job(job);
    let color = ink.text();
    if galley.size().x <= ui.available_width() + 0.5 {
        code_galley(cx, ui, galley, color, line);
    } else {
        ScrollArea::horizontal()
            .id_salt(Id::unique(id))
            .auto_shrink([false, true])
            .show(ui, |ui| code_galley(cx, ui, galley, color, line));
    }
}

fn code_galley(cx: &Cx, ui: &mut Ui, galley: Arc<egui::Galley>, color: Color32, min_h: f32) {
    let size = galley.size().max(vec2(1.0, min_h));
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let seq = cx.seqs(ui, 1);
    cx.text_run(ui, seq, &galley, rect.min, color, RunOpts::BLOCK);
}

fn copy_button(cx: &Cx, ui: &mut Ui, id: &str, code: &str) {
    let ink = cx.ink;
    let copied = cx.loc(|l| {
        l.copied
            .get(id)
            .is_some_and(|t| cx.now.saturating_duration_since(*t) < COPIED_FOR)
    });
    if copied {
        cx.animate();
    }
    let painter = ui.painter().clone();
    let ok = ink.tone("success");
    let text = if copied { "Copied" } else { "Copy" };
    let g = painter.layout_no_wrap(text.to_owned(), ink.label(LABEL), ink.dim());
    let icon = 12.0;
    let (rect, resp) = ui.allocate_exact_size(
        vec2(8.0 + icon + 5.0 + g.size().x + 8.0, 20.0),
        Sense::click(),
    );
    let hot = ui.ctx().animate_bool(resp.id.with("pop"), resp.hovered());
    let (fill, edge, fg) = if copied {
        (ink.wash(ok), ink.edge(ok), ok)
    } else if resp.hovered() {
        (ink.chip(), ink.p.hover_line, ink.text())
    } else {
        (Color32::TRANSPARENT, ink.line(), ink.dim())
    };
    paint::boxed(&painter, rect, 4, fill, Stroke::new(1.0, edge));
    if !copied {
        paint::pop(&painter, rect, 4, ink, hot);
    }
    glyphs::paint(
        &painter,
        if copied { "check" } else { "copy" },
        pos2(rect.left() + 8.0 + icon / 2.0, rect.center().y),
        icon,
        fg,
    );
    painter.galley(
        pos2(
            rect.left() + 8.0 + icon + 5.0,
            rect.center().y - g.size().y / 2.0,
        ),
        g,
        fg,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if resp.clicked() {
        cx.copy(code.to_owned());
        cx.loc(|l| {
            l.copied.insert(id.to_owned(), cx.now);
        });
    }
}

// ---- tables ---------------------------------------------------------------------------------

/// Column widths for `natural` widths in `avail`: natural when they fit, else the columns that
/// need less than their fair share keep theirs and the wider ones split the rest (never below
/// `min`).
fn fit_columns(natural: &[f32], avail: f32, min: f32) -> Vec<f32> {
    let n = natural.len();
    if natural.iter().sum::<f32>() <= avail {
        return natural.to_vec();
    }
    let mut out = vec![0.0; n];
    let mut fixed = vec![false; n];
    let mut room = avail;
    let mut free = n;
    loop {
        let share = if free > 0 {
            (room / free as f32).max(min)
        } else {
            0.0
        };
        let mut changed = false;
        for i in 0..n {
            if !fixed[i] && natural[i] <= share {
                fixed[i] = true;
                out[i] = natural[i];
                room -= natural[i];
                free -= 1;
                changed = true;
            }
        }
        if !changed || free == 0 {
            let share = if free > 0 {
                (room / free as f32).max(min)
            } else {
                0.0
            };
            for i in 0..n {
                if !fixed[i] {
                    out[i] = share;
                }
            }
            return out;
        }
    }
}

fn table(cx: &Cx, ui: &mut Ui, look: &Look, t: &Table) {
    let ink = cx.ink;
    let size = look.size();
    let cols = t
        .aligns
        .len()
        .max(t.head.len())
        .max(t.rows.iter().map(Vec::len).max().unwrap_or(0));
    if cols == 0 {
        return;
    }
    let (px, py) = (10.0_f32, 6.0_f32);
    let head_base = Base::new(ink.label(size * 0.8), ink.strong(), ink).lined(size * 1.3);
    let body_base = look.base(ink, 0.93, look.color, PROSE_LINE);
    let cell_job = |inl: &Inline, header: bool, wrap: f32| -> LayoutJob {
        let base = if header { &head_base } else { &body_base };
        let mut job = inl.job(ink, base, header, None);
        job.wrap.max_width = wrap;
        job
    };
    let painter = ui.painter().clone();

    // Column widths from the unwrapped text.
    let mut natural = vec![28.0_f32; cols];
    let rows_all = std::iter::once((&t.head, true)).chain(t.rows.iter().map(|r| (r, false)));
    for (row, header) in rows_all {
        for (c, inl) in row.iter().enumerate().take(cols) {
            let w = painter
                .layout_job(cell_job(inl, header, f32::INFINITY))
                .size()
                .x;
            natural[c] = natural[c].max(w + 2.0 * px);
        }
    }
    let widths = fit_columns(&natural, ui.available_width(), 56.0);

    // Row heights from the wrapped text.
    let has_head = !t.head.is_empty();
    let row_h = |row: &[Inline], header: bool| -> f32 {
        let mut h = 0.0_f32;
        for (c, inl) in row.iter().enumerate().take(cols) {
            let g = painter.layout_job(cell_job(inl, header, widths[c] - 2.0 * px));
            h = h.max(g.size().y);
        }
        (h + 2.0 * py).max(size * 1.6 + 2.0 * py * 0.5)
    };
    let mut heights = Vec::with_capacity(t.rows.len() + 1);
    if has_head {
        heights.push(row_h(&t.head, true));
    }
    for r in &t.rows {
        heights.push(row_h(r, false));
    }

    let total_w: f32 = widths.iter().sum();
    let total_h: f32 = heights.iter().sum();
    let (rect, _) = ui.allocate_exact_size(vec2(total_w, total_h), Sense::hover());
    if !cx.visible(ui, rect) {
        // The cells are runs even when nothing of them is on screen: number them all.
        let cells = std::iter::once(&t.head)
            .filter(|_| has_head)
            .chain(t.rows.iter())
            .flat_map(|r| r.iter().take(cols))
            .filter(|inl| !inl.is_empty())
            .count();
        cx.seqs(ui, u32::try_from(cells).unwrap_or(u32::MAX));
        return;
    }

    // Frame: shadow, fill, header band (clipped by the cut corner), grid, outline.
    let outline = paint::cut_bubble(rect, CUT);
    painter.add(deco::drop_shadow_shape(rect, 8, 2, 8, ink.dark, 38));
    paint::fill_h(&painter, &outline, ink.card(), ink.card());
    if has_head {
        let hb = rect.top() + heights[0];
        let band = [
            rect.left_top(),
            pos2(rect.right() - CUT, rect.top()),
            pos2(rect.right(), rect.top() + CUT),
            pos2(rect.right(), hb),
            pos2(rect.left(), hb),
        ];
        let (h0, h1) = ink.head();
        paint::fill_h(&painter, &band, h0, h1);
    }
    let line = Stroke::new(1.0, ink.line());
    let mut y = rect.top();
    for (i, h) in heights.iter().enumerate() {
        y += h;
        if i + 1 == heights.len() {
            break;
        }
        if has_head && i == 0 {
            paint::hairline(
                &painter,
                ink,
                rect.left() + 1.0,
                rect.right() - 1.0,
                y - 1.0,
                1.0,
            );
        } else {
            painter.line_segment(
                [pos2(rect.left() + 1.0, y), pos2(rect.right() - 1.0, y)],
                line,
            );
        }
    }
    let mut x = rect.left();
    for w in widths.iter().take(cols - 1) {
        x += w;
        painter.line_segment(
            [pos2(x, rect.top() + 1.0), pos2(x, rect.bottom() - 1.0)],
            Stroke::new(1.0, ink.line().gamma_multiply(0.7)),
        );
    }
    deco::outline(&painter, outline, line);

    // Cells (child uis: the parent's cursor stays below the whole table).
    let tid = ui.auto_id_with("md-table");
    let rows_all = std::iter::once((&t.head, true))
        .filter(|_| has_head)
        .chain(t.rows.iter().map(|r| (r, false)));
    let mut y = rect.top();
    for (r, ((row, header), h)) in rows_all.zip(&heights).enumerate() {
        let mut x = rect.left();
        for (c, inl) in row.iter().enumerate().take(cols) {
            let w = widths[c];
            if !inl.is_empty() {
                let align = match t.aligns.get(c) {
                    Some(Alignment::Center) => Align::Center,
                    Some(Alignment::Right) => Align::Max,
                    _ => Align::Min,
                };
                let cell = Rect::from_min_size(
                    pos2(x + px, y + py),
                    vec2(w - 2.0 * px, (h - 2.0 * py).max(1.0)),
                );
                let color = if header { ink.strong() } else { look.color };
                let job = cell_job(inl, header, w - 2.0 * px);
                let mut child = ui.new_child(
                    UiBuilder::new()
                        .id_salt((tid, r, c))
                        .max_rect(cell)
                        .layout(Layout::top_down(align)),
                );
                let sep = if c == 0 { Sep::Line } else { Sep::Tab };
                let (_, url) =
                    render::para(cx, &mut child, job, color, &inl.links, RunOpts::sep(sep));
                if let Some(u) = url {
                    cx.open(u);
                }
            }
            x += w;
        }
        y += h;
    }
}

// ---- tests ----------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serde_json::json;

    use super::*;
    use crate::chat_view::ctx::Local;
    use crate::chat_view::snap::Snap;

    const DOC: &str = "\
# Title

Some **bold** text with a [link](https://example.com/a) and https://rust-lang.org/x_y, plus `code`.

- one
- [x] two
  - nested

1. first
2. second

> quoted

```rust
fn main() {}
```

| a | b |
|:--|--:|
| 1 | 2 |

---
";

    fn kind(b: &Block) -> &'static str {
        match b {
            Block::Para(_) => "para",
            Block::Heading(..) => "heading",
            Block::List { .. } => "list",
            Block::Quote(_) => "quote",
            Block::Code { .. } => "code",
            Block::Rule => "rule",
            Block::Table(_) => "table",
        }
    }

    fn ink() -> Ink {
        crate::chat_view::style::test_ink()
    }

    fn parse(key: &str, src: &str) -> Doc {
        parse_doc(key, src, &[])
    }

    fn chips() -> Vec<Value> {
        vec![
            json!({"t": "/skill:plan", "s": "customMessageLabel strong"}),
            json!({"t": "@src/lib.rs", "s": "statusLineModel strong"}),
        ]
    }

    /// (text, is a chip) of the runs of the first paragraph.
    fn runs(doc: &Doc) -> Vec<(String, bool)> {
        let Block::Para(p) = &doc.blocks[0] else {
            panic!("para");
        };
        p.runs
            .iter()
            .map(|r| (r.text.clone(), r.chip.is_some()))
            .collect()
    }

    #[test]
    fn marks_become_chip_runs() {
        let doc = parse_doc("k", "run /skill:plan on @src/lib.rs now", &chips());
        assert_eq!(
            runs(&doc),
            [
                ("run ".to_owned(), false),
                ("/skill:plan".to_owned(), true),
                (" on ".to_owned(), false),
                ("@src/lib.rs".to_owned(), true),
                (" now".to_owned(), false),
            ]
        );
        let Block::Para(p) = &doc.blocks[0] else {
            panic!("para");
        };
        assert_eq!(p.len, "run /skill:plan on @src/lib.rs now".chars().count());
        // Two marks in a row stay two chips.
        let doc = parse_doc("k", "/skill:plan/skill:plan", &chips());
        assert_eq!(runs(&doc).iter().filter(|r| r.1).count(), 2);
    }

    #[test]
    fn a_mark_that_emphasis_splits_is_still_one_chip() {
        let marks = [json!({"t": "@pkg/__init__.py", "s": "accent"})];
        // `__init__` reads as strong emphasis: the token reaches the builder in three pieces.
        let doc = parse_doc("k", "open @pkg/__init__.py now", &marks);
        assert_eq!(
            runs(&doc),
            [
                ("open ".to_owned(), false),
                ("@pkg/__init__.py".to_owned(), true),
                (" now".to_owned(), false),
            ]
        );
        let Block::Para(p) = &doc.blocks[0] else {
            panic!("para");
        };
        assert_eq!(p.len, "open @pkg/__init__.py now".chars().count());
        // The look of the text around the chip survives.
        let doc = parse_doc(
            "k",
            "**bold** @a_b.rs _it_",
            &[json!({"t": "@a_b.rs", "s": "x"})],
        );
        let Block::Para(p) = &doc.blocks[0] else {
            panic!("para");
        };
        assert!(
            p.runs
                .iter()
                .any(|r| r.text == "bold" && r.marks & BOLD != 0)
        );
        assert!(
            p.runs
                .iter()
                .any(|r| r.text == "it" && r.marks & ITALIC != 0)
        );
        assert!(
            p.runs
                .iter()
                .any(|r| r.text == "@a_b.rs" && r.chip.is_some())
        );
    }

    #[test]
    fn protecting_a_token_leaves_code_as_written() {
        let marks = [json!({"t": "@pkg/__init__.py", "s": "accent"})];
        let doc = parse_doc("k", "see `@pkg/__init__.py`", &marks);
        let Block::Para(p) = &doc.blocks[0] else {
            panic!("para");
        };
        assert!(
            p.runs
                .iter()
                .any(|r| r.text == "@pkg/__init__.py" && r.marks & CODE != 0 && r.chip.is_none())
        );
        let doc = parse_doc("k", "```\n@pkg/__init__.py\n```", &marks);
        let Block::Code { text, .. } = &doc.blocks[0] else {
            panic!("code");
        };
        assert_eq!(text.trim_end(), "@pkg/__init__.py");
    }

    #[test]
    fn marks_do_not_touch_code_spans_or_links_and_the_longest_wins() {
        let doc = parse_doc("k", "`/skill:plan` [/skill:plan](https://a.b) x", &chips());
        assert!(runs(&doc).iter().all(|r| !r.1));
        let marks = [
            json!({"t": "@a", "s": "accent"}),
            json!({"t": "@a.rs", "s": "accent"}),
        ];
        let doc = parse_doc("k", "see @a.rs", &marks);
        assert_eq!(
            runs(&doc),
            [("see ".to_owned(), false), ("@a.rs".to_owned(), true)]
        );
        // Nothing without marks, and an entry without text is ignored.
        let doc = parse_doc("k", "see @a.rs", &[json!({"s": "accent"})]);
        assert_eq!(runs(&doc), [("see @a.rs".to_owned(), false)]);
    }

    #[test]
    fn a_chip_is_drawn_in_its_tone_on_a_wash_and_strong() {
        let ink = ink();
        let doc = parse_doc("k", "go /skill:plan @src/lib.rs", &chips());
        let Block::Para(p) = &doc.blocks[0] else {
            panic!("para");
        };
        let base = Base::new(ink.prose(PROSE), ink.text(), &ink);
        let fmt = |i: usize| run_format(&ink, &base, &p.runs[i], false);
        let (skill, mention, plain) = (fmt(1), fmt(3), fmt(0));
        assert_eq!(skill.color, ink.tone("user"));
        assert_eq!(mention.color, ink.tone("accent"));
        let user = ink.tone("user");
        assert_eq!(skill.background, theme::mix(ink.wash(user), user, 0.16));
        assert_ne!(skill.coords, plain.coords, "chips are strong");
        assert_eq!(plain.background, Color32::TRANSPARENT);
    }

    #[test]
    fn the_cache_reparses_when_the_marks_change() {
        let mut c = Cache::default();
        let a = c.doc("k", 1, "run /skill:plan", &[]);
        assert!(Arc::ptr_eq(&a, &c.doc("k", 1, "run /skill:plan", &[])));
        let b = c.doc("k", 1, "run /skill:plan", &chips());
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(&b, &c.doc("k", 1, "run /skill:plan", &chips())));
        assert_eq!(runs(&b).iter().filter(|r| r.1).count(), 1);
    }

    #[test]
    fn code_blocks_carry_a_fingerprint_of_their_text() {
        let hash = |src: &str| {
            let doc = parse("k", src);
            let Block::Code { hash, .. } = &doc.blocks[0] else {
                panic!("code");
            };
            *hash
        };
        assert_eq!(
            hash("```rust\nfn a() {}\n```"),
            hash("```rust\nfn a() {}\n```")
        );
        assert_ne!(
            hash("```rust\nfn a() {}\n```"),
            hash("```rust\nfn b() {}\n```")
        );
    }

    fn slice(plain: &str, l: &Link) -> String {
        plain.chars().skip(l.from).take(l.to - l.from).collect()
    }

    #[test]
    fn a_document_parses_into_the_expected_blocks() {
        let doc = parse("k", DOC);
        let kinds: Vec<_> = doc.blocks.iter().map(kind).collect();
        assert_eq!(
            kinds,
            [
                "heading", "para", "list", "list", "quote", "code", "table", "rule"
            ]
        );
        let Block::Heading(1, h) = &doc.blocks[0] else {
            panic!("heading");
        };
        assert_eq!(h.plain(), "Title");
        let Block::List { start: None, items } = &doc.blocks[2] else {
            panic!("bullets");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].task, None);
        assert_eq!(items[1].task, Some(true));
        assert_eq!(
            items[1].blocks.iter().map(kind).collect::<Vec<_>>(),
            ["para", "list"]
        );
        let Block::List {
            start: Some(1),
            items,
        } = &doc.blocks[3]
        else {
            panic!("numbered");
        };
        assert_eq!(items.len(), 2);
        let Block::Code { lang, text, .. } = &doc.blocks[5] else {
            panic!("code");
        };
        assert_eq!((lang.as_str(), text.as_str()), ("rust", "fn main() {}"));
        let Block::Table(t) = &doc.blocks[6] else {
            panic!("table");
        };
        assert_eq!(t.head.len(), 2);
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.aligns, [Alignment::Left, Alignment::Right]);
    }

    #[test]
    fn link_ranges_are_char_offsets_of_the_paragraph_text() {
        let doc = parse("k", DOC);
        let Block::Para(p) = &doc.blocks[1] else {
            panic!("para");
        };
        let plain = p.plain();
        assert_eq!(
            plain,
            "Some bold text with a link and https://rust-lang.org/x_y, plus code."
        );
        let got: Vec<_> = p
            .links
            .iter()
            .map(|l| (slice(&plain, l), l.url.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("link".to_owned(), "https://example.com/a"),
                (
                    "https://rust-lang.org/x_y".to_owned(),
                    "https://rust-lang.org/x_y"
                ),
            ]
        );
        // The layout job concatenates the same text, so the ranges hold for the galley too.
        let ink = ink();
        let base = Base::new(ink.prose(PROSE), ink.text(), &ink);
        let job = p.job(&ink, &base, false, None);
        assert_eq!(job.text, plain);
        assert_eq!(slice(&job.text, &p.links[0]), "link");
    }

    #[test]
    fn ranges_survive_multibyte_text_before_a_link() {
        let doc = parse("k", "héllo ✓ [ok](https://a.b/c) done");
        let Block::Para(p) = &doc.blocks[0] else {
            panic!("para");
        };
        assert_eq!(slice(&p.plain(), &p.links[0]), "ok");
    }

    #[test]
    fn only_web_and_mail_targets_are_clickable() {
        let doc = parse(
            "k",
            "[a](javascript:alert(1)) [b](src/main.rs) [c](mailto:x@y.z) (see https://a.b/c_(d)), end.",
        );
        let Block::Para(p) = &doc.blocks[0] else {
            panic!("para");
        };
        let urls: Vec<_> = p.links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(urls, ["mailto:x@y.z", "https://a.b/c_(d)"]);
    }

    #[test]
    fn images_become_dim_alt_text() {
        let doc = parse("k", "![the alt](pic.png) and ![](only.png)");
        let Block::Para(p) = &doc.blocks[0] else {
            panic!("para");
        };
        assert_eq!(p.plain(), "the alt and only.png");
        assert!(
            p.runs
                .iter()
                .any(|r| r.marks & DIM != 0 && r.text == "the alt")
        );
    }

    #[test]
    fn nesting_is_bounded() {
        let src = format!("{}deep", "> ".repeat(60));
        let doc = parse("k", &src);
        fn depth(bs: &[Block]) -> usize {
            bs.iter()
                .map(|b| match b {
                    Block::Quote(c) => 1 + depth(c),
                    _ => 0,
                })
                .max()
                .unwrap_or(0)
        }
        assert!(depth(&doc.blocks) <= MAX_NEST);
        assert!(!doc.blocks.is_empty());
    }

    #[test]
    fn an_unterminated_fence_is_a_code_block_while_streaming() {
        let doc = parse("k", "text\n\n```py\nprint(1)\nprint(");
        assert_eq!(
            doc.blocks.iter().map(kind).collect::<Vec<_>>(),
            ["para", "code"]
        );
        let Block::Code { text, .. } = &doc.blocks[1] else {
            panic!("code");
        };
        assert_eq!(text, "print(1)\nprint(");
    }

    #[test]
    fn the_cache_reparses_only_when_the_generation_moves() {
        let mut c = Cache::default();
        let a = c.doc("k", 1, "one", &[]);
        assert!(Arc::ptr_eq(&a, &c.doc("k", 1, "one", &[])));
        let b = c.doc("k", 2, "one two", &[]);
        assert!(!Arc::ptr_eq(&a, &b));
        for i in 0..=MAX_DOCS {
            c.doc(&format!("n{i}"), 1, "x", &[]);
        }
        assert!(c.docs.len() <= MAX_DOCS);
    }

    #[test]
    fn fitted_columns_share_the_width_fairly() {
        assert_eq!(fit_columns(&[100.0, 200.0], 400.0, 40.0), [100.0, 200.0]);
        let w = fit_columns(&[100.0, 600.0], 400.0, 40.0);
        assert_eq!(w[0], 100.0);
        assert!((w[1] - 300.0).abs() < 0.01);
        let w = fit_columns(&[500.0, 500.0, 500.0], 600.0, 40.0);
        assert!(w.iter().all(|x| (x - 200.0).abs() < 0.01));
    }

    /// Streaming only adds the caret and keeps the frame animating; the parse is the same.
    #[test]
    fn streaming_does_not_change_the_parse() {
        let ctx = egui::Context::default();
        crate::ui_fonts::install(&ctx, crate::ui_fonts::UiFonts::none());
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        crate::ui_fonts::finalize(&ctx);

        let snap = Snap::default();
        let mut loc = Local::default();
        let mut drawn = Vec::new();
        for stream in [false, true] {
            let key = if stream { "s1" } else { "s0" };
            let mut animating = false;
            for _ in 0..2 {
                ctx.run_ui(egui::RawInput::default(), |ui| {
                    let ink = Ink::new(ui, 13.0);
                    let cx = Cx::new(&snap, &ink, &mut loc, Instant::now(), false, false);
                    show_text(&cx, ui, key, 1, DOC, &[], stream);
                    animating = cx.into_out().1.is_some();
                })
                .textures_delta
                .clear();
            }
            assert_eq!(animating, stream);
            let doc = loc.md.docs.get(key).map(|(_, d)| Arc::clone(d));
            drawn.push(doc.expect("parsed"));
        }
        // The parse depends on the text (and the cache key it derives ids from), not on `stream`.
        assert_eq!(*drawn[0], parse("s0", DOC));
        assert_eq!(*drawn[1], parse("s1", DOC));
    }
}
