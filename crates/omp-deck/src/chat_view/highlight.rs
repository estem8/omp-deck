//! Syntax highlighting of the chat's code: `code` nodes, fenced code of `md` and the lines of
//! `diff` nodes.
//!
//! syntect parses; it never colours. Each token is classified into a small semantic set ([`Tok`])
//! and the set is mapped onto the active style's terminal palette ([`TokColors`], the same 16 ANSI
//! colours the chat's ANSI text uses), so the colours follow the style and its dark/light class and
//! nothing here is a syntect theme.
//!
//! The syntax set loads and the parsing runs on one worker thread ([`Cache::get`] never blocks the
//! UI): a request for `(key, generation)` returns the latest result available — nothing the first
//! time, the previous generation's while a newer one is computed — and the worker repaints the
//! frame when it lands. A result of an older generation is validated line by line by hash before
//! it is applied, so it never colours a line whose text changed; one of the asked generation
//! needs no check ([`Hl`]). A re-request of a text that grew (a streaming code block) resumes the
//! parse from the last checkpoint before the first changed line. A request superseded while it
//! parses is abandoned at its next checkpoint, and one past its time budget keeps the lines
//! done: what is left stays plain.

use std::cell::Cell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, Context};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use super::ansi;
use super::style::Ink;
use crate::theme::mix;

/// Lines longer than this are left plain (a minified bundle would stall the worker).
const MAX_LINE_BYTES: usize = 1000;
/// Lines highlighted of one block; the rest stays plain (the blocks draw at most 2000 rows).
const MAX_LINES: usize = 3000;
/// A parse state is kept every this many lines, to resume a re-parse from.
const CHECKPOINT_EVERY: usize = 32;
/// Results kept per view; past it the least recently used of those the frame does not draw go.
const MAX_SLOTS: usize = 192;
/// Longest the worker parses one request before it settles for the lines done so far.
const BUDGET: Duration = Duration::from_millis(200);

// ---- tokens ---------------------------------------------------------------------------------

/// What a token of source text is, independent of any colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tok {
    Plain,
    Comment,
    Keyword,
    Operator,
    Punct,
    Str,
    Escape,
    Number,
    Const,
    Func,
    Type,
    Tag,
    Attr,
    Key,
    Macro,
    Err,
}

impl Tok {
    pub const ALL: [Tok; 16] = [
        Tok::Plain,
        Tok::Comment,
        Tok::Keyword,
        Tok::Operator,
        Tok::Punct,
        Tok::Str,
        Tok::Escape,
        Tok::Number,
        Tok::Const,
        Tok::Func,
        Tok::Type,
        Tok::Tag,
        Tok::Attr,
        Tok::Key,
        Tok::Macro,
        Tok::Err,
    ];
}

/// Scope prefix → token, most specific first: a scope takes the first row it matches, and a token's
/// scopes are tried innermost first (so `punctuation.definition.string` inside a string is a string).
const TABLE: &[(&str, Tok)] = &[
    ("comment", Tok::Comment),
    ("punctuation.definition.comment", Tok::Comment),
    ("string.regexp", Tok::Escape),
    ("constant.character.escape", Tok::Escape),
    ("constant.other.placeholder", Tok::Escape),
    ("punctuation.section.interpolation", Tok::Escape),
    ("punctuation.definition.template-expression", Tok::Escape),
    ("string", Tok::Str),
    ("punctuation.definition.string", Tok::Str),
    ("constant.numeric", Tok::Number),
    ("constant.other.color", Tok::Number),
    ("constant.language", Tok::Const),
    ("constant", Tok::Const),
    ("support.constant", Tok::Const),
    ("variable.language", Tok::Const),
    ("variable.other.constant", Tok::Const),
    ("keyword.operator", Tok::Operator),
    ("keyword", Tok::Keyword),
    ("storage.type.annotation", Tok::Macro),
    ("storage.type", Tok::Keyword),
    ("storage", Tok::Keyword),
    ("variable.annotation", Tok::Macro),
    ("punctuation.definition.annotation", Tok::Macro),
    ("support.macro", Tok::Macro),
    ("entity.name.macro", Tok::Macro),
    ("entity.name.function.preprocessor", Tok::Macro),
    ("entity.name.function", Tok::Func),
    ("support.function", Tok::Func),
    ("variable.function", Tok::Func),
    ("support.type.property-name", Tok::Key),
    ("entity.name.type", Tok::Type),
    ("entity.name.class", Tok::Type),
    ("entity.name.struct", Tok::Type),
    ("entity.name.enum", Tok::Type),
    ("entity.name.trait", Tok::Type),
    ("entity.name.impl", Tok::Type),
    ("entity.name.interface", Tok::Type),
    ("entity.name.union", Tok::Type),
    ("entity.name.namespace", Tok::Type),
    ("entity.name.module", Tok::Type),
    ("entity.other.inherited-class", Tok::Type),
    ("support.type", Tok::Type),
    ("support.class", Tok::Type),
    ("entity.name.tag.yaml", Tok::Key),
    ("entity.name.tag.toml", Tok::Key),
    ("entity.name.tag", Tok::Tag),
    ("entity.name.section", Tok::Func),
    ("entity.name.table", Tok::Func),
    ("entity.other.attribute-name", Tok::Attr),
    ("variable.parameter.option", Tok::Attr),
    ("variable.parameter", Tok::Plain),
    ("markup.inserted", Tok::Str),
    ("markup.deleted", Tok::Tag),
    ("markup.heading", Tok::Func),
    ("markup.raw", Tok::Str),
    ("markup.underline", Tok::Attr),
    ("punctuation", Tok::Punct),
    ("invalid", Tok::Err),
];

/// Scopes whose strings and bare words are a mapping/object key.
const KEY_SCOPES: &[&str] = &[
    "meta.object-literal.key",
    "meta.structure.dictionary.key",
    "meta.mapping.key",
];

/// Words that `storage.type` marks in several languages but that name a type, not a keyword.
const PRIMITIVES: &[&str] = &[
    "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize", "f32",
    "f64", "bool", "char", "str", "int", "long", "short", "float", "double", "void", "byte",
    "unsigned", "signed", "string", "number", "boolean", "any", "object", "size_t", "uint8_t",
    "uint16_t", "uint32_t", "uint64_t", "int8_t", "int16_t", "int32_t", "int64_t",
];

struct Table {
    /// Scope, token, whether the row is the bare `storage.type`.
    rows: Vec<(Scope, Tok, bool)>,
    keys: Vec<Scope>,
}

impl Table {
    fn new() -> Table {
        Table {
            rows: TABLE
                .iter()
                .filter_map(|(s, t)| Some((Scope::new(s).ok()?, *t, *s == "storage.type")))
                .collect(),
            keys: KEY_SCOPES
                .iter()
                .filter_map(|s| Scope::new(s).ok())
                .collect(),
        }
    }

    /// The token of the text `word` under `stack`.
    fn classify(&self, stack: &ScopeStack, word: &str) -> Tok {
        let mut found = None;
        let mut in_key = false;
        for scope in stack.as_slice().iter().rev() {
            if self.keys.iter().any(|k| k.is_prefix_of(*scope)) {
                in_key = true;
            }
            if found.is_none()
                && let Some((_, tok, storage)) =
                    self.rows.iter().find(|(p, _, _)| p.is_prefix_of(*scope))
            {
                found = Some(if *storage && scope.len() <= 3 && is_primitive(word) {
                    Tok::Type
                } else {
                    *tok
                });
            }
        }
        match found {
            None | Some(Tok::Str) if in_key => Tok::Key,
            // `in`, `as`, `and`, `instanceof`: operators of the grammar spelt as words read as
            // keywords.
            Some(Tok::Operator)
                if !word.is_empty() && word.bytes().all(|b| b.is_ascii_alphabetic()) =>
            {
                Tok::Keyword
            }
            Some(t) => t,
            None => Tok::Plain,
        }
    }
}

fn is_primitive(word: &str) -> bool {
    PRIMITIVES.contains(&word)
}

// ---- results --------------------------------------------------------------------------------

/// A run of one token ending at byte `end` (it starts where the previous one ends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub end: u32,
    pub tok: Tok,
}

#[derive(Clone)]
struct Line {
    hash: u64,
    /// The parse state was reset before this line (a new hunk).
    reset: bool,
    spans: Arc<[Span]>,
}

#[derive(Clone)]
struct Checkpoint {
    line: usize,
    parse: ParseState,
    stack: ScopeStack,
}

/// The highlight of one text: the tokens of each line.
#[derive(Default)]
pub struct Spans {
    syntax: Option<String>,
    lines: Vec<Line>,
    cps: Vec<Checkpoint>,
}

impl Spans {
    /// The tokens of line `i` when it still reads `text` (else — or when the line is past the
    /// highlighted range — `None`: draw it plain).
    pub fn line(&self, i: usize, text: &str) -> Option<&[Span]> {
        let l = self.lines.get(i)?;
        (l.hash == hash_line(text)).then_some(&l.spans[..])
    }

    /// Whether nothing is coloured (unknown language).
    #[cfg(test)]
    pub fn is_plain(&self) -> bool {
        self.syntax.is_none()
    }

    #[cfg(test)]
    fn toks(&self, i: usize, text: &str) -> Vec<(String, Tok)> {
        let Some(spans) = self.line(i, text) else {
            return Vec::new();
        };
        let mut from = 0usize;
        spans
            .iter()
            .map(|s| {
                let r = (text[from..s.end as usize].to_owned(), s.tok);
                from = s.end as usize;
                r
            })
            .collect()
    }
}

fn hash_line(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

// ---- parsing --------------------------------------------------------------------------------

/// A text to highlight: its lines (without line ends), where the parse state restarts (`resets`,
/// ascending line indices; line 0 always starts fresh) and the language hints.
pub struct Source {
    pub lang: String,
    pub path: String,
    pub lines: Vec<String>,
    pub resets: Vec<u32>,
}

/// The syntax for a fence/`lang` tag or, failing that, a file path (`None`: plain text).
fn resolve<'a>(set: &'a SyntaxSet, lang: &str, path: &str) -> Option<&'a SyntaxReference> {
    let tag = lang
        .trim()
        .split(|c: char| c == ',' || c == '{' || c.is_whitespace())
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let plain = set.find_syntax_plain_text();
    let found = (!tag.is_empty())
        .then(|| {
            let token = alias(&tag);
            set.find_syntax_by_token(token)
                .or_else(|| set.find_syntax_by_extension(token))
        })
        .flatten()
        .or_else(|| {
            let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
            if file.is_empty() {
                return None;
            }
            set.find_syntax_by_extension(file).or_else(|| {
                file.rsplit_once('.').and_then(|(_, ext)| {
                    set.find_syntax_by_extension(alias(&ext.to_ascii_lowercase()))
                })
            })
        });
    found.filter(|s| s.name != plain.name)
}

/// Names the bundled syntaxes know under another token.
fn alias(tag: &str) -> &str {
    match tag {
        "node" | "nodejs" => "js",
        "shell" | "sh" | "bash" | "zsh" | "fish" | "shellscript" | "console" | "shell-session" => {
            "sh"
        }
        "yml" => "yaml",
        "jsonc" | "json5" | "jsonl" | "ndjson" => "json",
        "c++" | "cc" | "cxx" | "hpp" | "hxx" => "cpp",
        "golang" => "go",
        "csharp" => "cs",
        "py3" | "python3" => "py",
        "rust" => "rs",
        "ruby" => "rb",
        "make" | "mk" => "makefile",
        "patch" => "diff",
        "text" | "txt" | "plain" | "plaintext" | "log" | "output" | "none" => "",
        other => other,
    }
}

/// Highlights `src` (reusing what `prev` still holds of an earlier version of the same text).
/// `stop` is asked every [`CHECKPOINT_EVERY`] lines; once it says so the lines parsed so far are
/// the result and the rest stays plain.
fn highlight(
    set: &SyntaxSet,
    table: &Table,
    src: &Source,
    prev: Option<&Spans>,
    stop: &mut dyn FnMut() -> bool,
) -> Spans {
    let Some(syntax) = resolve(set, &src.lang, &src.path) else {
        return Spans::default();
    };
    let total = src.lines.len().min(MAX_LINES);
    let reset_at = |i: usize| i == 0 || src.resets.binary_search(&(i as u32)).is_ok();

    let mut out = Spans {
        syntax: Some(syntax.name.clone()),
        lines: Vec::with_capacity(total),
        cps: Vec::new(),
    };
    let mut start = 0usize;
    let mut parse = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    if let Some(p) = prev.filter(|p| p.syntax.as_deref() == Some(syntax.name.as_str())) {
        let same = (0..total.min(p.lines.len()))
            .take_while(|&i| {
                p.lines[i].hash == hash_line(&src.lines[i]) && p.lines[i].reset == reset_at(i)
            })
            .count();
        // A checkpoint holds the state after its line's reset: it serves the line right after
        // the shared prefix only when that line resets (or not) as it did, else its state is
        // the other one's.
        let usable = |c: &&Checkpoint| {
            c.line < same
                || (c.line == same
                    && p.lines
                        .get(c.line)
                        .is_some_and(|l| l.reset == reset_at(c.line)))
        };
        if let Some(cp) = p.cps.iter().rev().find(usable) {
            start = cp.line;
            parse = cp.parse.clone();
            stack = cp.stack.clone();
            out.lines.extend_from_slice(&p.lines[..start]);
            out.cps
                .extend(p.cps.iter().filter(|c| c.line < start).cloned());
        }
    }

    let mut buf = String::new();
    for i in start..total {
        if i > start && i % CHECKPOINT_EVERY == 0 && stop() {
            break;
        }
        let text = src.lines[i].as_str();
        let reset = reset_at(i);
        if reset {
            parse = ParseState::new(syntax);
            stack = ScopeStack::new();
        }
        if i % CHECKPOINT_EVERY == 0 {
            out.cps.push(Checkpoint {
                line: i,
                parse: parse.clone(),
                stack: stack.clone(),
            });
        }
        let hash = hash_line(text);
        if text.len() > MAX_LINE_BYTES {
            out.lines.push(Line {
                hash,
                reset,
                spans: Arc::from(Vec::new()),
            });
            continue;
        }
        buf.clear();
        buf.push_str(text);
        buf.push('\n');
        let Ok(ops) = parse.parse_line(&buf, set) else {
            // A grammar that does not hold together (a context it names is missing): this line
            // and the rest stay plain. Regex search errors are no-matches to syntect, not this.
            break;
        };
        let mut spans: Vec<Span> = Vec::new();
        let mut last = 0usize;
        let mut emit = |from: usize, to: usize, stack: &ScopeStack| {
            let to = to.min(text.len());
            if to <= from {
                return;
            }
            let tok = table.classify(stack, &text[from..to]);
            match spans.last_mut() {
                Some(s) if s.tok == tok => s.end = to as u32,
                _ => spans.push(Span {
                    end: to as u32,
                    tok,
                }),
            }
        };
        for (at, op) in ops {
            emit(last, at, &stack);
            last = last.max(at);
            if stack.apply(&op).is_err() {
                break;
            }
        }
        emit(last, text.len(), &stack);
        out.lines.push(Line {
            hash,
            reset,
            spans: spans.into(),
        });
    }
    out
}

// ---- worker ---------------------------------------------------------------------------------

struct Slot {
    /// The generation last asked for.
    gen_: u64,
    /// The frame this slot was last asked for in.
    used: u64,
    /// The newest text asked for, until the worker takes it.
    want: Option<Source>,
    /// Latest result, with the generation it was parsed for (an earlier one while a newer is
    /// computed).
    result: Option<(u64, Arc<Spans>)>,
    queued: bool,
}

impl Slot {
    fn hl(&self) -> Option<Hl> {
        self.result.as_ref().map(|(g, spans)| Hl {
            spans: Arc::clone(spans),
            exact: *g == self.gen_,
        })
    }
}

#[derive(Default)]
struct Slots {
    map: HashMap<String, Slot>,
}

impl Slots {
    /// Makes room for one more slot: drops the least recently used among those not asked for in
    /// `frame`. Slots the current frame still draws stay, so a frame that shows more blocks than
    /// [`MAX_SLOTS`] never evicts what it is about to need again.
    fn make_room(&mut self, frame: u64) {
        let excess = (self.map.len() + 1).saturating_sub(MAX_SLOTS);
        if excess == 0 {
            return;
        }
        let mut stale: Vec<(u64, &String)> = self
            .map
            .iter()
            .filter(|(_, v)| v.used < frame)
            .map(|(k, v)| (v.used, k))
            .collect();
        stale.sort_unstable();
        let drop: Vec<String> = stale
            .into_iter()
            .take(excess)
            .map(|(_, k)| k.clone())
            .collect();
        for k in drop {
            self.map.remove(&k);
        }
    }
}

struct Job {
    slots: Arc<Mutex<Slots>>,
    key: String,
    ctx: Context,
}

fn lock(m: &Mutex<Slots>) -> MutexGuard<'_, Slots> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The worker's queue; `None` when the thread could not start (everything then stays plain).
fn queue() -> Option<&'static Sender<Job>> {
    static TX: LazyLock<Option<Sender<Job>>> = LazyLock::new(|| {
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name("chat-highlight".into())
            .spawn(move || {
                // Loaded here, never on the UI thread.
                let set = two_face::syntax::extra_newlines();
                let table = Table::new();
                for job in rx {
                    let _ = catch_unwind(AssertUnwindSafe(|| run(&set, &table, &job)));
                    job.ctx.request_repaint();
                }
            })
            .ok()
            .map(|_| tx)
    });
    TX.as_ref()
}

fn run(set: &SyntaxSet, table: &Table, job: &Job) {
    let (src, prev, gen_) = {
        let mut s = lock(&job.slots);
        let Some(slot) = s.map.get_mut(&job.key) else {
            return;
        };
        slot.queued = false;
        let Some(src) = slot.want.take() else {
            return;
        };
        (
            src,
            slot.result.as_ref().map(|(_, r)| Arc::clone(r)),
            slot.gen_,
        )
    };
    let started = Instant::now();
    let stale = Cell::new(false);
    let mut stop = || {
        if started.elapsed() > BUDGET {
            return true;
        }
        // A newer generation is waiting: its job resumes from what this one got through.
        let newer = lock(&job.slots)
            .map
            .get(&job.key)
            .is_none_or(|slot| slot.gen_ != gen_);
        stale.set(newer);
        newer
    };
    let done = Arc::new(highlight(set, table, &src, prev.as_deref(), &mut stop));
    let mut s = lock(&job.slots);
    if let Some(slot) = s.map.get_mut(&job.key) {
        // A result abandoned for a newer text only replaces a shorter one.
        let keep = stale.get()
            && slot
                .result
                .as_ref()
                .is_some_and(|(_, r)| r.lines.len() >= done.lines.len());
        if !keep {
            slot.result = Some((gen_, done));
        }
    }
}

/// A highlight handed to a drawing pass.
#[derive(Clone)]
pub struct Hl {
    spans: Arc<Spans>,
    /// Parsed from the very text asked for: its lines need no check against what is drawn.
    exact: bool,
}

impl Hl {
    /// The tokens of line `i` of the text, `text` being what is drawn for it (`None`: plain — the
    /// line is past the parsed range, or this is an older result and the line changed since).
    pub fn line(&self, i: usize, text: &str) -> Option<&[Span]> {
        if self.exact {
            self.spans.lines.get(i).map(|l| &l.spans[..])
        } else {
            self.spans.line(i, text)
        }
    }
}

/// Highlight results of one chat view, by the caller's key (a node id and, for several blocks in
/// one node, a suffix).
#[derive(Default, Clone)]
pub struct Cache {
    slots: Arc<Mutex<Slots>>,
}

impl Cache {
    /// The latest highlight available for `key`, asking the worker for generation `gen_` of it when
    /// that was not asked yet (`source` runs then and only then). `None` until a first result.
    pub fn get(
        &self,
        ctx: &Context,
        key: &str,
        gen_: u64,
        source: impl FnOnce() -> Source,
    ) -> Option<Hl> {
        let mut s = lock(&self.slots);
        let frame = ctx.cumulative_frame_nr();
        if let Some(slot) = s.map.get_mut(key) {
            slot.used = frame;
            if slot.gen_ == gen_ {
                return slot.hl();
            }
        } else {
            s.make_room(frame);
        }
        let src = source();
        let slot = s.map.entry(key.to_owned()).or_insert(Slot {
            gen_,
            used: frame,
            want: None,
            result: None,
            queued: false,
        });
        slot.gen_ = gen_;
        slot.want = Some(src);
        let result = slot.hl();
        if !slot.queued {
            match queue() {
                Some(tx) => {
                    slot.queued = true;
                    let job = Job {
                        slots: Arc::clone(&self.slots),
                        key: key.to_owned(),
                        ctx: ctx.clone(),
                    };
                    if tx.send(job).is_err() {
                        slot.queued = false;
                        slot.want = None;
                    }
                }
                None => slot.want = None,
            }
        }
        result
    }

    /// Slots held.
    #[cfg(test)]
    fn len(&self) -> usize {
        lock(&self.slots).map.len()
    }
}

// ---- colours --------------------------------------------------------------------------------

/// Colours of every [`Tok`] for one style over one background.
#[derive(Clone)]
pub struct TokColors {
    c: [Color32; Tok::ALL.len()],
}

/// Contrast ratio (WCAG) of two opaque colours.
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let lum = |c: Color32| {
        let lin = |v: u8| {
            let v = f32::from(v) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
    };
    let (la, lb) = (lum(a), lum(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Perceptual (OKLab) coordinates of a colour: lightness, and the a/b chroma axes.
fn oklab(c: Color32) -> [f32; 3] {
    let lin = |v: u8| {
        let v = f32::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (lin(c.r()), lin(c.g()), lin(c.b()));
    let l = (0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_84 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// The colour of OKLab `[l, a, b]`, its chroma pulled in until it fits the sRGB gamut.
fn from_oklab([l, a, b]: [f32; 3]) -> Color32 {
    let enc = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        let v = if v <= 0.003_130_8 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (v * 255.0).round() as u8
    };
    for step in 0..12 {
        let k = 0.9_f32.powi(step);
        let (a, b) = (a * k, b * k);
        let l_ = (l + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
        let m_ = (l - 0.105_561_35 * a - 0.063_854_17 * b).powi(3);
        let s_ = (l - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
        let rgb = [
            4.076_741_7 * l_ - 3.307_711_6 * m_ + 0.230_969_93 * s_,
            -1.268_438 * l_ + 2.609_757_4 * m_ - 0.341_319_4 * s_,
            -0.004_196_086 * l_ - 0.703_418_6 * m_ + 1.707_614_7 * s_,
        ];
        if rgb.iter().all(|v| (-0.002..=1.002).contains(v)) || step == 11 {
            return Color32::from_rgb(enc(rgb[0]), enc(rgb[1]), enc(rgb[2]));
        }
    }
    Color32::GRAY
}

/// Perceptual distance of two colours (OKLab Euclidean; about 0.02 is just noticeable).
pub fn apart(a: Color32, b: Color32) -> f32 {
    let (x, y) = (oklab(a), oklab(b));
    ((x[0] - y[0]).powi(2) + (x[1] - y[1]).powi(2) + (x[2] - y[2]).powi(2)).sqrt()
}

/// Smallest chroma a colour needs to read as a hue next to the text.
const MIN_CHROMA: f32 = 0.10;
/// Nearest a palette colour may sit to the text colour.
const MIN_APART: f32 = 0.12;
/// Widest hue drift (degrees) of a palette colour from its token's canonical hue.
const HUE_TOL: f32 = 24.0;

/// `c` when it already reads as `hue` next to `text` (OKLCH degrees: red 30, orange 55, yellow 105, green 145,
/// cyan 195, blue 255, violet 300), else the same lightness turned to that hue. Palettes of
/// near-monochrome styles keep their magenta and cyan too close to the text to tell tokens apart.
fn steer(c: Color32, hue: f32, text: Color32, dark: bool) -> Color32 {
    let [_, a, b] = oklab(c);
    let chroma = a.hypot(b);
    let h = b.atan2(a).to_degrees().rem_euclid(360.0);
    let drift = (h - hue + 180.0).rem_euclid(360.0) - 180.0;
    if chroma >= MIN_CHROMA && drift.abs() <= HUE_TOL && apart(c, text) >= MIN_APART {
        return c;
    }
    rehue(c, hue, dark)
}

/// `c` turned to `hue` at a chroma that reads and a lightness that leaves room for it (a pale or,
/// in light styles, dark colour has none) and still clears the contrast floor against the block.
fn rehue(c: Color32, hue: f32, dark: bool) -> Color32 {
    let [l, a, b] = oklab(c);
    let chroma = a.hypot(b).clamp(0.14, 0.18);
    let r = hue.to_radians();
    let l = if dark {
        l.clamp(0.66, 0.80)
    } else {
        l.clamp(0.44, 0.54)
    };
    from_oklab([l, chroma * r.cos(), chroma * r.sin()])
}

/// `c`, pulled toward `toward` just far enough to reach contrast `min` against every `bg`.
fn legible(c: Color32, bgs: &[Color32], min: f32, toward: Color32) -> Color32 {
    let ok = |c: Color32| bgs.iter().all(|b| contrast(c, *b) >= min);
    (0..=10)
        .map(|i| mix(c, toward, i as f32 / 10.0))
        .find(|c| ok(*c))
        .unwrap_or(toward)
}

impl TokColors {
    /// The mapping onto `ink`'s terminal palette, legible over `bg` and over the add/delete
    /// washes drawn on it. Dark styles take the bright half of the palette, light styles the
    /// normal half; whatever still falls short of its contrast is pulled toward the strong text.
    pub fn new(ink: &Ink, bg: Color32) -> TokColors {
        let pal = ansi::palette16(ink);
        let p = ink.p;
        let pick = |i: usize| if ink.dark { pal[i + 8] } else { pal[i] };
        let text = ink.text();
        let orange = mix(pick(3), pick(1), 0.5);
        let bgs = [bg, mix(bg, p.ok, 0.16), mix(bg, p.danger, 0.16)];
        let fin = |c: Color32| legible(c, &bgs, 4.5, p.text_strong);
        let string = steer(pick(2), 145.0, text, ink.dark);
        // Green and cyan palettes can leave strings and types a shade apart: turn the type.
        let typ = steer(pick(6), 195.0, text, ink.dark);
        let typ = if apart(fin(typ), fin(string)) < 0.09 {
            rehue(typ, 215.0, ink.dark)
        } else {
            typ
        };
        let mut c = [text; Tok::ALL.len()];
        for tok in Tok::ALL {
            let (color, min) = match tok {
                Tok::Plain => (text, 4.5),
                Tok::Comment => (mix(p.weak, text, 0.12), 3.0),
                Tok::Keyword => (steer(pick(5), 305.0, text, ink.dark), 4.5),
                Tok::Operator => (mix(text, p.weak, 0.3), 4.0),
                Tok::Punct => (mix(text, p.weak, 0.5), 3.5),
                Tok::Str => (string, 4.5),
                Tok::Escape => (if ink.dark { pal[14] } else { pal[6] }, 4.5),
                Tok::Number => (steer(orange, 55.0, text, ink.dark), 4.5),
                Tok::Const => (pick(3), 4.5),
                Tok::Func => (steer(pick(4), 255.0, text, ink.dark), 4.5),
                Tok::Type => (typ, 4.5),
                Tok::Tag => (pick(1), 4.5),
                Tok::Attr => (mix(pick(3), pick(2), 0.5), 4.5),
                Tok::Key => (
                    steer(mix(pick(4), pick(6), 0.35), 225.0, text, ink.dark),
                    4.5,
                ),
                Tok::Macro => (mix(pick(4), pick(5), 0.5), 4.5),
                Tok::Err => (pick(1), 4.5),
            };
            c[tok as usize] = legible(color, &bgs, min, p.text_strong);
        }
        TokColors { c }
    }

    pub fn get(&self, tok: Tok) -> Color32 {
        self.c[tok as usize]
    }
}

/// What a [`TokColors`] is derived from: dark/light and the palette's colours.
type ColorsKey = (bool, [Color32; 10]);

/// The [`TokColors`] of `ink` over `bg`, derived once per (style, background) and kept in egui's
/// memory (blocks ask every frame).
pub fn colors(ctx: &Context, ink: &Ink, bg: Color32) -> Arc<TokColors> {
    let p = ink.p;
    let key: ColorsKey = (
        ink.dark,
        [
            bg,
            p.text,
            p.text_strong,
            p.weak,
            p.accent,
            p.ok,
            p.warn,
            p.danger,
            ink.a,
            ink.b,
        ],
    );
    let id = egui::Id::NULL.with(("omp_deck::chat::tok-colors", bg));
    if let Some((k, c)) = ctx.data(|d| d.get_temp::<(ColorsKey, Arc<TokColors>)>(id))
        && k == key
    {
        return c;
    }
    let c = Arc::new(TokColors::new(ink, bg));
    ctx.data_mut(|d| d.insert_temp(id, (key, Arc::clone(&c))));
    c
}

// ---- layout ---------------------------------------------------------------------------------

/// Appends `text` (one line, no line end) to `job` in `fmt`, coloured by `spans` when there are
/// any (plain tokens keep `fmt`'s colour).
pub fn push_line(
    job: &mut LayoutJob,
    text: &str,
    spans: Option<&[Span]>,
    colors: &TokColors,
    fmt: &TextFormat,
) {
    push_line_after(job, text, spans, 0, colors, fmt);
}

/// [`push_line`] without the first `skip` bytes of the line (a char boundary).
pub fn push_line_after(
    job: &mut LayoutJob,
    text: &str,
    spans: Option<&[Span]>,
    skip: usize,
    colors: &TokColors,
    fmt: &TextFormat,
) {
    let Some(spans) = spans.filter(|s| !s.is_empty()) else {
        job.append(text.get(skip..).unwrap_or(""), 0.0, fmt.clone());
        return;
    };
    let mut from = skip;
    let mut f = fmt.clone();
    for s in spans {
        let to = (s.end as usize).min(text.len());
        let Some(piece) = text.get(from..to).filter(|p| !p.is_empty()) else {
            continue;
        };
        f.color = if s.tok == Tok::Plain {
            fmt.color
        } else {
            colors.get(s.tok)
        };
        job.append(piece, 0.0, f.clone());
        from = to;
    }
    if let Some(rest) = text.get(from..).filter(|r| !r.is_empty()) {
        job.append(rest, 0.0, fmt.clone());
    }
}

/// Appends multi-line `text` to `job`, each line coloured by line `i` of `spans`.
pub fn push_text(
    job: &mut LayoutJob,
    text: &str,
    spans: Option<&Hl>,
    colors: &TokColors,
    fmt: &TextFormat,
) {
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            job.append("\n", 0.0, fmt.clone());
        }
        let toks = spans.and_then(|s| s.line(i, line));
        push_line(job, line, toks, colors, fmt);
    }
}

/// A [`Source`] of `text`'s lines (one fresh parse).
pub fn source_of_text(lang: &str, path: &str, text: &str) -> Source {
    Source {
        lang: lang.to_owned(),
        path: path.to_owned(),
        lines: text.split('\n').map(str::to_owned).collect(),
        resets: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> SyntaxSet {
        two_face::syntax::extra_newlines()
    }

    fn run_on(src: &Source, prev: Option<&Spans>) -> Spans {
        highlight(&set(), &Table::new(), src, prev, &mut || false)
    }

    fn toks(lang: &str, text: &str) -> Vec<(String, Tok)> {
        let src = source_of_text(lang, "", text);
        let s = run_on(&src, None);
        s.toks(0, text.split('\n').next().unwrap_or(""))
    }

    fn tok_of(v: &[(String, Tok)], word: &str) -> Tok {
        v.iter()
            .find(|(t, _)| t.trim() == word)
            .map_or(Tok::Plain, |(_, k)| *k)
    }

    #[test]
    fn rust_tokens_get_their_semantic_class() {
        let v = toks(
            "rust",
            "pub fn go(x: u32) -> String { let s = \"a\"; // note",
        );
        assert_eq!(tok_of(&v, "fn"), Tok::Keyword);
        assert_eq!(tok_of(&v, "go"), Tok::Func);
        assert_eq!(
            tok_of(&v, "u32"),
            Tok::Type,
            "a primitive named by storage.type"
        );
        assert_eq!(tok_of(&v, "let"), Tok::Keyword);
        assert_eq!(tok_of(&v, "\"a\""), Tok::Str);
        assert!(
            v.iter()
                .any(|(t, k)| t.contains("note") && *k == Tok::Comment)
        );
        assert_eq!(tok_of(&v, "->"), Tok::Punct);
    }

    #[test]
    fn bash_commands_and_options_are_told_apart() {
        let v = toks("bash", "grep -rn \"x $HOME\" src | wc -l");
        assert_eq!(tok_of(&v, "grep"), Tok::Func);
        assert!(v.iter().any(|(t, k)| t.contains("rn") && *k == Tok::Attr));
        assert!(v.iter().any(|(t, k)| t.contains("x ") && *k == Tok::Str));
        assert_eq!(tok_of(&v, "|"), Tok::Operator);
    }

    #[test]
    fn json_keys_differ_from_string_values() {
        let v = toks("json", "{\"k\": \"v\", \"n\": 12, \"b\": true}");
        assert_eq!(tok_of(&v, "\"k\""), Tok::Key);
        assert_eq!(tok_of(&v, "\"v\""), Tok::Str);
        assert_eq!(tok_of(&v, "12"), Tok::Number);
        assert_eq!(tok_of(&v, "true"), Tok::Const);
    }

    #[test]
    fn unknown_language_falls_back_to_plain() {
        for (lang, path) in [
            ("klingon", ""),
            ("", ""),
            ("text", "notes.txt"),
            ("", "data.unknownext"),
        ] {
            let src = source_of_text(lang, path, "let x = 1;\nfn y() {}");
            let s = run_on(&src, None);
            assert!(s.is_plain(), "{lang:?} {path:?}");
            assert!(s.line(0, "let x = 1;").is_none());
        }
    }

    #[test]
    fn the_language_comes_from_the_tag_else_the_path() {
        let set = set();
        let name = |l: &str, p: &str| resolve(&set, l, p).map(|s| s.name.clone());
        assert_eq!(name("rust", "").as_deref(), Some("Rust"));
        assert_eq!(name("rs,no_run", "").as_deref(), Some("Rust"));
        assert_eq!(name("", "src/main.rs").as_deref(), Some("Rust"));
        assert_eq!(name("", "C:\\x\\a.py").as_deref(), Some("Python"));
        assert_eq!(name("typescript", "").as_deref(), Some("TypeScript"));
        assert_eq!(name("ts", "").as_deref(), Some("TypeScript"));
        assert_eq!(name("", "src/app.tsx").as_deref(), Some("TypeScriptReact"));
        assert_eq!(name("javascript", "").as_deref(), Some("JavaScript"));
        assert_eq!(name("toml", "").as_deref(), Some("TOML"));
        assert_eq!(name("", "Cargo.toml").as_deref(), Some("TOML"));
        assert_eq!(name("sh", "").as_deref(), Some("Bourne Again Shell (bash)"));
        assert_eq!(name("", "Makefile").as_deref(), Some("Makefile"));
        // The tag wins over the path.
        assert_eq!(name("python", "a.rs").as_deref(), Some("Python"));
        assert_eq!(name("", "README"), None);
    }

    #[test]
    fn a_stale_line_is_not_coloured() {
        let src = source_of_text("rust", "", "let a = 1;");
        let s = run_on(&src, None);
        assert!(s.line(0, "let a = 1;").is_some());
        assert!(s.line(0, "let a = 2;").is_none());
        assert!(s.line(1, "").is_none());
    }

    #[test]
    fn an_appended_text_resumes_and_matches_a_fresh_parse() {
        let mut lines: Vec<String> = (0..100)
            .map(|i| {
                if i == 40 {
                    "/* a block".to_owned()
                } else if i == 45 {
                    "still */ let k = 1;".to_owned()
                } else {
                    format!("let v{i} = \"s\";")
                }
            })
            .collect();
        let mk = |lines: &[String]| Source {
            lang: "rust".into(),
            path: String::new(),
            lines: lines.to_vec(),
            resets: Vec::new(),
        };
        let set = set();
        let table = Table::new();
        let first = highlight(&set, &table, &mk(&lines), None, &mut || false);
        assert!(first.cps.len() >= 3);
        // Grow the tail and rewrite a line after the last checkpoint but one.
        lines.push("let tail = 3;".into());
        lines[70] = "fn changed() {}".into();
        let src = mk(&lines);
        let resumed = highlight(&set, &table, &src, Some(&first), &mut || false);
        let fresh = highlight(&set, &table, &src, None, &mut || false);
        assert_eq!(resumed.lines.len(), fresh.lines.len());
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(resumed.toks(i, line), fresh.toks(i, line), "line {i}");
        }
        // The block comment state at line 41 survived the resume.
        assert_eq!(resumed.toks(41, &lines[41]), fresh.toks(41, &lines[41]));
    }

    #[test]
    fn a_dropped_reset_at_a_checkpoint_line_resumes_from_the_state_before_it() {
        let mut lines: Vec<String> = (0..100).map(|i| format!("let v{i} = 1;")).collect();
        lines[60] = "/* a comment".into();
        lines[70] = "closed */".into();
        let mk = |resets: Vec<u32>| Source {
            lang: "rust".into(),
            path: String::new(),
            lines: lines.clone(),
            resets,
        };
        let set = set();
        let table = Table::new();
        // The first parse restarts at line 64 (a checkpoint line); the second does not, so line
        // 64 is still inside the comment.
        let first = highlight(&set, &table, &mk(vec![64]), None, &mut || false);
        let src = mk(Vec::new());
        let resumed = highlight(&set, &table, &src, Some(&first), &mut || false);
        let fresh = highlight(&set, &table, &src, None, &mut || false);
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(resumed.toks(i, line), fresh.toks(i, line), "line {i}");
        }
        assert_eq!(
            fresh.toks(64, &lines[64]).first().map(|t| t.1),
            Some(Tok::Comment)
        );
    }

    #[test]
    fn a_stopped_parse_keeps_its_lines_and_the_next_one_finishes_it() {
        let lines: Vec<String> = (0..100).map(|i| format!("let v{i} = \"s\";")).collect();
        let src = Source {
            lang: "rust".into(),
            path: String::new(),
            lines: lines.clone(),
            resets: Vec::new(),
        };
        let set = set();
        let table = Table::new();
        let mut asked = 0;
        let part = highlight(&set, &table, &src, None, &mut || {
            asked += 1;
            asked >= 2
        });
        // Stopped at the second checkpoint after the start: lines 0..64 are done.
        assert_eq!(part.lines.len(), 64);
        assert!(part.line(63, &lines[63]).is_some());
        assert!(part.line(64, &lines[64]).is_none());
        let done = highlight(&set, &table, &src, Some(&part), &mut || false);
        let fresh = highlight(&set, &table, &src, None, &mut || false);
        assert_eq!(done.lines.len(), 100);
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(done.toks(i, line), fresh.toks(i, line), "line {i}");
        }
    }

    #[test]
    fn a_line_past_the_byte_limit_stays_plain() {
        let long = format!("let s = \"{}\";", "x".repeat(MAX_LINE_BYTES));
        let src = Source {
            lang: "rust".into(),
            path: String::new(),
            lines: vec![long.clone(), "let a = 1;".into()],
            resets: Vec::new(),
        };
        let s = run_on(&src, None);
        assert!(s.line(0, &long).is_some_and(<[Span]>::is_empty));
        assert!(s.line(1, "let a = 1;").is_some_and(|l| !l.is_empty()));
    }

    fn plain_source() -> Source {
        Source {
            lang: String::new(),
            path: String::new(),
            lines: vec!["x".into()],
            resets: Vec::new(),
        }
    }

    fn frame(ctx: &Context, f: impl FnOnce(&Context)) {
        let mut f = Some(f);
        ctx.run_ui(egui::RawInput::default(), |ui| {
            if let Some(f) = f.take() {
                f(ui.ctx());
            }
        })
        .textures_delta
        .clear();
    }

    #[test]
    fn a_frame_that_draws_more_blocks_than_slots_keeps_them_all() {
        let ctx = Context::default();
        let cache = Cache::default();
        let n = MAX_SLOTS + 30;
        frame(&ctx, |c| {
            for k in 0..n {
                cache.get(c, &format!("k{k}"), 1, plain_source);
            }
        });
        assert_eq!(cache.len(), n, "nothing the frame asked for is evicted");
        // Later frames draw a few of them and a new one: the idle ones make room.
        frame(&ctx, |c| {
            cache.get(c, "k0", 1, plain_source);
            cache.get(c, "new", 1, plain_source);
        });
        assert!(cache.len() <= MAX_SLOTS, "{}", cache.len());
        let s = lock(&cache.slots);
        assert!(s.map.contains_key("k0") && s.map.contains_key("new"));
    }

    #[test]
    fn a_new_generation_returns_the_old_result_checked_per_line_until_its_own_lands() {
        let ctx = Context::default();
        let cache = Cache::default();
        let src = |text: &str| source_of_text("rust", "", text);
        let wait = |gen_: u64, text: &str| {
            for _ in 0..400 {
                let got = cache.get(&ctx, "k", gen_, || src(text));
                if let Some(h) = got.filter(|h| h.exact) {
                    return h;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            panic!("no result for generation {gen_}");
        };
        let first = wait(1, "let a = 1;");
        assert!(first.line(0, "let a = 1;").is_some());
        // Generation 2 is not parsed yet: the answer is generation 1's, and a line whose text
        // changed gets no colour from it.
        let old = cache.get(&ctx, "k", 2, || src("let a = 2;"));
        if let Some(old) = old.filter(|h| !h.exact) {
            assert!(old.line(0, "let a = 2;").is_none());
            assert!(old.line(0, "let a = 1;").is_some());
        }
        let second = wait(2, "let a = 2;");
        assert!(second.line(0, "let a = 2;").is_some());
    }

    #[test]
    fn operators_spelt_as_words_are_keywords() {
        let rs = toks("rust", "for i in 0..n { let b = a as u8; }");
        assert_eq!(tok_of(&rs, "in"), Tok::Keyword, "{rs:?}");
        assert_eq!(tok_of(&rs, "as"), Tok::Keyword, "{rs:?}");
        let py = toks("py", "if a and not b: pass");
        assert_eq!(tok_of(&py, "and"), Tok::Keyword, "{py:?}");
        let js = toks("js", "x instanceof Y");
        assert_eq!(tok_of(&js, "instanceof"), Tok::Keyword, "{js:?}");
        // Symbol operators stay operators.
        let ops = toks("rust", "a == b && c");
        assert_eq!(tok_of(&ops, "=="), Tok::Operator, "{ops:?}");
    }

    #[test]
    fn typescript_and_toml_have_their_own_grammars() {
        let ts = toks("ts", "export const n: number = 42; // c");
        assert_eq!(tok_of(&ts, "export"), Tok::Keyword, "{ts:?}");
        assert_eq!(tok_of(&ts, "number"), Tok::Type, "{ts:?}");
        assert_eq!(tok_of(&ts, "42"), Tok::Number, "{ts:?}");
        // Only the TypeScript grammar knows type annotations of an interface.
        let iface = toks("tsx", "interface Props { id: string }");
        assert_eq!(tok_of(&iface, "interface"), Tok::Keyword, "{iface:?}");
        let toml = toks("toml", "version = \"1.2\" # note");
        assert_eq!(tok_of(&toml, "version"), Tok::Key, "{toml:?}");
        assert!(
            toml.iter()
                .any(|(t, k)| t.contains("1.2") && *k == Tok::Str),
            "{toml:?}"
        );
        assert!(
            toml.iter()
                .any(|(t, k)| t.contains("note") && *k == Tok::Comment),
            "{toml:?}"
        );
        let head = toks("toml", "[dependencies]");
        assert_eq!(tok_of(&head, "dependencies"), Tok::Func, "{head:?}");
    }

    #[test]
    fn a_diff_fence_colours_added_and_removed_lines() {
        let del = toks("diff", "-let a = 1;");
        let add = toks("diff", "+let a = 2;");
        assert!(
            del.iter()
                .any(|(t, k)| t.contains("let a") && *k == Tok::Tag),
            "{del:?}"
        );
        assert!(
            add.iter()
                .any(|(t, k)| t.contains("let a") && *k == Tok::Str),
            "{add:?}"
        );
    }

    #[test]
    fn a_reset_starts_the_parse_over() {
        let src = Source {
            lang: "rust".into(),
            path: String::new(),
            lines: vec!["/* open".into(), "let x = 1;".into()],
            resets: vec![1],
        };
        let s = run_on(&src, None);
        // Without the reset line 1 would be inside the comment.
        assert!(
            s.toks(1, "let x = 1;")
                .iter()
                .any(|(_, k)| *k == Tok::Keyword)
        );
    }

    fn ink_for(style: deck_core::config::UiStyle, dark: bool) -> Ink {
        let mut ink = crate::chat_view::style::test_ink();
        ink.p = crate::theme::palette(style, dark);
        ink.dark = dark;
        let (a, b) = crate::deco::accent_pair(ink.p);
        ink.a = a;
        ink.b = b;
        ink
    }

    fn code_bg(ink: &Ink) -> Color32 {
        mix(ink.card(), ink.p.bg, 0.3)
    }

    #[test]
    fn every_style_reads_well_in_its_class() {
        use deck_core::config::UiStyle;
        let mut classes = 0;
        for style in UiStyle::ALL {
            for dark in [true, false] {
                // Gallery styles are dark-only.
                if !dark && !matches!(style, UiStyle::Neutral | UiStyle::Neon) {
                    continue;
                }
                classes += 1;
                let ink = ink_for(style, dark);
                let bg = code_bg(&ink);
                let cols = TokColors::new(&ink, bg);
                for tok in Tok::ALL {
                    let want = if tok == Tok::Comment { 3.0 } else { 3.5 };
                    let got = contrast(cols.get(tok), bg);
                    assert!(
                        got >= want,
                        "{style:?} dark={dark} {tok:?}: {got:.2} < {want}"
                    );
                }
            }
        }
        assert!(classes >= 20);
    }

    #[test]
    fn the_main_token_kinds_stay_apart_from_the_text_and_each_other_in_every_style() {
        use deck_core::config::UiStyle;
        let kinds = [Tok::Keyword, Tok::Str, Tok::Number, Tok::Func, Tok::Type];
        for style in UiStyle::ALL {
            for dark in [true, false] {
                if !dark && !matches!(style, UiStyle::Neutral | UiStyle::Neon) {
                    continue;
                }
                let ink = ink_for(style, dark);
                let cols = TokColors::new(&ink, code_bg(&ink));
                for (i, a) in kinds.iter().enumerate() {
                    let d = apart(cols.get(*a), ink.text());
                    // Keywords carry the hue that sets code apart from prose.
                    let want = if *a == Tok::Keyword { 0.14 } else { 0.10 };
                    assert!(
                        d >= want,
                        "{style:?} dark={dark} {a:?} vs text: {d:.3} < {want}"
                    );
                    for b in &kinds[i + 1..] {
                        let d = apart(cols.get(*a), cols.get(*b));
                        assert!(d >= 0.06, "{style:?} dark={dark} {a:?} vs {b:?}: {d:.3}");
                    }
                }
            }
        }
    }

    #[test]
    fn colours_follow_the_style_class() {
        use deck_core::config::UiStyle;
        let dark = ink_for(UiStyle::Neutral, true);
        let light = ink_for(UiStyle::Neutral, false);
        let (cd, cl) = (
            TokColors::new(&dark, code_bg(&dark)),
            TokColors::new(&light, code_bg(&light)),
        );
        // Dark styles take the bright palette half, light ones the normal half: the keyword
        // colours differ and each is the lighter/darker one for its own background.
        assert_ne!(cd.get(Tok::Keyword), cl.get(Tok::Keyword));
        let lum = |c: Color32| u32::from(c.r()) + u32::from(c.g()) + u32::from(c.b());
        assert!(lum(cd.get(Tok::Keyword)) > lum(code_bg(&dark)));
        assert!(lum(cl.get(Tok::Keyword)) < lum(code_bg(&light)));
        // A string is greenish and a comment quieter than the text in both classes.
        for (ink, c) in [(&dark, &cd), (&light, &cl)] {
            let s = c.get(Tok::Str);
            assert!(s.g() > s.r() && s.g() > s.b(), "{s:?}");
            assert!(
                contrast(c.get(Tok::Comment), code_bg(ink)) < contrast(ink.text(), code_bg(ink))
            );
        }
    }

    #[test]
    fn a_line_becomes_coloured_runs() {
        let ink = crate::chat_view::style::test_ink();
        let cols = TokColors::new(&ink, code_bg(&ink));
        let src = source_of_text("rust", "", "let x = 1;");
        let s = run_on(&src, None);
        let mut job = LayoutJob::default();
        let fmt = TextFormat::simple(egui::FontId::monospace(12.0), ink.text());
        push_line(&mut job, "let x = 1;", s.line(0, "let x = 1;"), &cols, &fmt);
        assert_eq!(job.text, "let x = 1;");
        assert!(
            job.sections
                .iter()
                .any(|sec| sec.format.color == cols.get(Tok::Keyword))
        );
        assert!(
            job.sections
                .iter()
                .any(|sec| sec.format.color == cols.get(Tok::Number))
        );
        // Plain stays the caller's colour; a plain line is one section.
        let mut plain = LayoutJob::default();
        push_line(&mut plain, "let x", None, &cols, &fmt);
        assert_eq!(plain.sections.len(), 1);
    }
}
