//! Styled text: TSP `spans` (`{t, s}` with space-separated style tokens) and plain `text` as egui
//! layout jobs, with the private-use glyphs omp's Nerd-Font icons arrive as removed.

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Align, Color32, FontId, Stroke};
use serde_json::Value;

use super::snap::SNode;
use super::style::Ink;

/// Private-use codepoints (Nerd Font icons in omp's spans): never drawn.
pub fn is_pua(c: char) -> bool {
    matches!(c, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}' | '\u{100000}'..='\u{10FFFD}')
}

/// `s` without private-use characters (borrowed when it has none).
pub fn clean(s: &str) -> std::borrow::Cow<'_, str> {
    if s.chars().any(is_pua) {
        s.chars().filter(|c| !is_pua(*c)).collect::<String>().into()
    } else {
        s.into()
    }
}

/// What a span's style tokens ask for.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SpanStyle {
    pub color: Option<Color32>,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub mono: bool,
    pub background: Option<Color32>,
    pub hide: bool,
    pub link: bool,
}

/// Maps the tokens of one span onto the theme (omp theme token names included).
pub fn span_style(ink: &Ink, tokens: &str) -> SpanStyle {
    let mut st = SpanStyle::default();
    for t in tokens.split_whitespace() {
        match t {
            "muted" | "dim" | "typo" => st.color = Some(ink.dim()),
            "strong" | "toolTitle" => st.color = Some(ink.strong()),
            "em" => st.italic = true,
            "accent" | "info" | "key" | "statusLineModel" | "mdHeading" => {
                st.color = Some(ink.tone("accent"));
            }
            "success" | "ins" | "toolDiffAdded" | "statusLineGitClean" => {
                st.color = Some(ink.tone("success"));
            }
            "warning" | "statusLineCost" | "statusLineSpend" => {
                st.color = Some(ink.tone("warning"));
            }
            "error" | "del" | "toolDiffRemoved" | "statusLineGitDirty" => {
                st.color = Some(ink.tone("error"));
                if t == "del" {
                    st.strike = true;
                }
            }
            "code" | "mdCode" => {
                st.mono = true;
                st.background = Some(ink.chip());
            }
            "mono" | "path" | "num" => st.mono = true,
            "link" => {
                st.link = true;
                st.underline = true;
                st.color = Some(ink.tone("accent"));
            }
            "mark" => st.background = Some(ink.wash(ink.p.warn)),
            "icon" | "hide" => st.hide = true,
            "thinkingText" => {
                st.color = Some(ink.dim());
                st.italic = true;
            }
            "customMessageLabel" => st.color = Some(ink.tone("user")),
            _ => {}
        }
    }
    st
}

/// Look of the plain text a job starts from.
#[derive(Clone)]
pub struct Base {
    pub font: FontId,
    pub color: Color32,
    pub mono: FontId,
    pub line_height: Option<f32>,
}

impl Base {
    pub fn new(font: FontId, color: Color32, ink: &Ink) -> Base {
        Base {
            mono: ink.mono_px(font.size * 0.92),
            font,
            color,
            line_height: None,
        }
    }

    pub fn lined(mut self, line_height: f32) -> Base {
        self.line_height = Some(line_height);
        self
    }

    /// The plain (unstyled) format.
    pub fn format_plain(&self) -> TextFormat {
        self.format(&SpanStyle::default())
    }

    fn format(&self, st: &SpanStyle) -> TextFormat {
        TextFormat {
            font_id: if st.mono {
                self.mono.clone()
            } else {
                self.font.clone()
            },
            color: st.color.unwrap_or(self.color),
            background: st.background.unwrap_or(Color32::TRANSPARENT),
            italics: st.italic,
            strikethrough: if st.strike {
                Stroke::new(1.0, st.color.unwrap_or(self.color))
            } else {
                Stroke::NONE
            },
            underline: if st.underline {
                Stroke::new(1.0, st.color.unwrap_or(self.color))
            } else {
                Stroke::NONE
            },
            line_height: self.line_height,
            valign: Align::Center,
            ..TextFormat::default()
        }
    }
}

/// Wrapping rules of a node's `wrap`/`truncate`/`lines` props at `width` (`None` = unbounded).
pub fn wrapping(n: &SNode, width: Option<f32>) -> TextWrapping {
    let wrap = n.str("wrap").unwrap_or("word");
    let lines = n.num("lines").map_or(usize::MAX, |l| l.max(1.0) as usize);
    let mut w = TextWrapping {
        max_width: width.unwrap_or(f32::INFINITY),
        max_rows: lines,
        break_anywhere: wrap == "char",
        overflow_character: Some('…'),
    };
    if wrap == "none" {
        // One row; what does not fit is cut with an ellipsis.
        w.max_rows = 1;
    }
    w
}

/// Appends `spans` (a TSP span array) to `job`.
pub fn push_spans(job: &mut LayoutJob, ink: &Ink, base: &Base, spans: &[Value]) {
    for span in spans {
        let (text, tokens) = match span {
            Value::String(s) => (s.as_str(), ""),
            Value::Object(o) => (
                o.get("t").and_then(Value::as_str).unwrap_or_default(),
                o.get("s").and_then(Value::as_str).unwrap_or_default(),
            ),
            _ => continue,
        };
        let st = span_style(ink, tokens);
        if st.hide || text.is_empty() {
            continue;
        }
        job.append(&clean(text), 0.0, base.format(&st));
    }
}

/// The text of `spans` without styling (for tooltips, copy, measuring).
pub fn spans_plain(spans: &[Value]) -> String {
    let mut s = String::new();
    for span in spans {
        let (t, tokens) = match span {
            Value::String(t) => (t.as_str(), ""),
            Value::Object(o) => (
                o.get("t").and_then(Value::as_str).unwrap_or_default(),
                o.get("s").and_then(Value::as_str).unwrap_or_default(),
            ),
            _ => continue,
        };
        if !tokens
            .split_whitespace()
            .any(|x| x == "icon" || x == "hide")
        {
            s.push_str(&clean(t));
        }
    }
    s
}

/// A prop that is either plain text or a span array, as plain text (`None` when absent or empty).
pub fn prop(n: &SNode, key: &str) -> Option<String> {
    let t = prop_value(n.get(key)?);
    (!t.is_empty()).then_some(t)
}

/// `v` (a string or a span array) as plain text.
pub fn prop_value(v: &Value) -> String {
    match v {
        Value::String(s) => clean(s).into_owned(),
        Value::Array(a) => spans_plain(a),
        _ => String::new(),
    }
}

/// The job of a node's `spans`, else its `text`, laid out at `width` under its wrap props.
pub fn node_job(ink: &Ink, n: &SNode, base: &Base, width: Option<f32>) -> LayoutJob {
    let mut job = LayoutJob::default();
    let spans = n.arr("spans");
    if spans.is_empty() {
        let st = SpanStyle::default();
        let t = clean(n.text());
        if !t.is_empty() {
            job.append(&t, 0.0, base.format(&st));
        }
    } else {
        push_spans(&mut job, ink, base, spans);
    }
    job.wrap = wrapping(n, width);
    job
}

/// Plain text of a node: `spans` flattened, else `text`.
pub fn node_plain(n: &SNode) -> String {
    let spans = n.arr("spans");
    if spans.is_empty() {
        clean(n.text()).into_owned()
    } else {
        spans_plain(spans)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn private_use_glyphs_are_dropped_everywhere() {
        assert_eq!(clean("a\u{f121}b\u{e0b0}c"), "abc");
        assert_eq!(clean("plain"), "plain");
        assert!(is_pua('\u{F0001}'));
        assert!(!is_pua('é'));
        let spans = [
            json!({"t": "\u{f015}", "s": "icon"}),
            json!({"t": " home \u{e0a0}", "s": "strong"}),
        ];
        assert_eq!(spans_plain(&spans), " home ");
    }
}
