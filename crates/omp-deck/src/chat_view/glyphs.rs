//! Tern icon names → Lucide glyphs (the bundled icon font). Names without a match get a neutral
//! dot; omp's private-use (Nerd Font) codepoints are never drawn.

use egui::{Color32, FontFamily, FontId, Painter, Pos2, Rect, Ui, vec2};

/// Family name of the bundled Lucide face.
const LUCIDE: &str = "lucide";

/// The Lucide glyph for a Tern icon `name`.
pub fn lucide(name: &str) -> &'static str {
    let n = name.trim().to_ascii_lowercase();
    let n = n.as_str();
    if n.starts_with("provider:") || n.starts_with("provider-") || n.starts_with("app-") {
        return "\u{e1bb}"; // bot
    }
    if n.starts_with("mi-") {
        return "\u{e0c0}"; // file
    }
    match n {
        "copy" => "\u{e09e}",
        "rewind" | "undo" => "\u{e147}",
        "history" | "recap" => "\u{e1f5}",
        "lightbulb" | "bulb" | "tip" => "\u{e1c2}",
        "corner-down-right" | "queue" => "\u{e0a2}",
        "corner-down-left" | "enter" | "return" => "\u{e0a1}",
        "eye" => "\u{e0ba}",
        "eye-off" => "\u{e0bb}",
        "chev" | "chevron" | "chevron-down" | "chev-down" => "\u{e06d}",
        "chev-right" | "chevron-right" => "\u{e06f}",
        "chev-up" | "chevron-up" => "\u{e070}",
        "check" | "done" | "ok" => "\u{e06c}",
        "x" | "close" | "cancel" => "\u{e1b2}",
        "warn" | "warning" | "alert" => "\u{e193}",
        "error" | "alert-circle" => "\u{e077}",
        "info" => "\u{e0f9}",
        "bolt" | "zap" | "fast" | "fast-mode" => "\u{e1b4}",
        "flame" | "fire" => "\u{e0d2}",
        "brain" | "thinking" => "\u{e3c6}",
        "sparkle" | "sparkles" | "new" => "\u{e412}",
        "branch" | "git-branch" => "\u{e0e2}",
        "merge" => "\u{e0e4}",
        "folder" | "dir" => "\u{e0d7}",
        "folder-open" => "\u{e247}",
        "file" | "document" => "\u{e0c0}",
        "file-text" | "text" | "notes" => "\u{e0cc}",
        "file-code" | "code" => "\u{e0c3}",
        "file-plus" | "create" => "\u{e0c9}",
        "edit" | "pencil" | "write" => "\u{e1f9}",
        "diff" => "\u{e30c}",
        "terminal" | "bash" | "shell" => "\u{e181}",
        "gear" | "settings" | "cog" => "\u{e154}",
        "model" | "cpu" => "\u{e0a9}",
        "jobs" | "tasks" | "list-checks" | "checklist" | "todo" => "\u{e1d0}",
        "refresh" | "retry" | "reload" => "\u{e145}",
        "context.compaction" | "compaction" | "compact" => "\u{e43e}",
        "search" | "grep" | "find" => "\u{e151}",
        "link" | "fetch" => "\u{e102}",
        "external-link" | "open" => "\u{e0b9}",
        "globe" | "web" | "web_search" => "\u{e0e8}",
        "trash" | "delete" | "trash-2" => "\u{e18e}",
        "bot" | "agent" | "subagent" | "task" => "\u{e1bb}",
        "users" | "collab" => "\u{e1a4}",
        "play" | "run" => "\u{e13c}",
        "pause" => "\u{e12e}",
        "stop" | "square" => "\u{e167}",
        "send" => "\u{e152}",
        "arrow-up" | "up" => "\u{e04a}",
        "arrow-down" | "down" => "\u{e042}",
        "arrow-right" | "right" => "\u{e049}",
        "arrow-left" | "left" => "\u{e048}",
        "bell" => "\u{e059}",
        "lock" => "\u{e10b}",
        "key" => "\u{e0fd}",
        "image" | "media" => "\u{e0f6}",
        "list" => "\u{e106}",
        "hash" => "\u{e0ef}",
        "cloud" => "\u{e088}",
        "database" | "db" => "\u{e0ad}",
        "wrench" | "tool" | "tools" => "\u{e1b1}",
        "hammer" | "build" => "\u{e0ec}",
        "package" => "\u{e129}",
        "book" | "book-open" | "docs" => "\u{e05f}",
        "message" | "message-square" | "chat" => "\u{e117}",
        "plus" | "add" => "\u{e13d}",
        "minus" => "\u{e11c}",
        "scissors" | "cut" => "\u{e14e}",
        "download" => "\u{e0b2}",
        "upload" => "\u{e19e}",
        "save" => "\u{e14d}",
        "shield" | "shield-check" | "approve" => "\u{e1ff}",
        "gauge" | "context" | "meter" => "\u{e1bf}",
        "coins" | "cost" | "usage" => "\u{e097}",
        "timer" | "clock" | "time" | "elapsed" => "\u{e1e0}",
        "activity" | "pulse" => "\u{e038}",
        "star" => "\u{e176}",
        "bookmark" => "\u{e060}",
        "tag" => "\u{e17f}",
        "flag" => "\u{e0d1}",
        "target" => "\u{e180}",
        "ellipsis" | "more" => "\u{e0b6}",
        "ban" | "deny" => "\u{e051}",
        "bug" => "\u{e20c}",
        "rocket" => "\u{e286}",
        "paperclip" | "attach" => "\u{e12d}",
        "mic" | "dictation" => "\u{e118}",
        "circle" | "dot" => "\u{e076}",
        "circle-x" => "\u{e084}",
        "circle-stop" => "\u{e083}",
        "loader" | "spinner" => "\u{e109}",
        _ => "\u{e076}",
    }
}

/// A Lucide glyph of `size` px.
pub fn font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(LUCIDE.into()))
}

/// Paints the icon `name` centred on `at`.
pub fn paint(painter: &Painter, name: &str, at: Pos2, size: f32, color: Color32) {
    painter.text(
        at,
        egui::Align2::CENTER_CENTER,
        lucide(name),
        font(size),
        color,
    );
}

/// Allocates a square of `size` and paints the icon in it; the response senses `sense`.
pub fn icon(
    ui: &mut Ui,
    name: &str,
    size: f32,
    color: Color32,
    sense: egui::Sense,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size + 4.0, size + 4.0), sense);
    if ui.is_rect_visible(rect) {
        paint(ui.painter(), name, rect.center(), size, color);
    }
    resp
}

/// Whether `name` is a glyph [`lucide`] knows (everything else shows the neutral dot).
#[cfg(test)]
pub fn known(name: &str) -> bool {
    lucide(name) != lucide("")
}

/// Rect of a `size` square centred at `c`.
pub fn square(c: Pos2, size: f32) -> Rect {
    Rect::from_center_size(c, vec2(size, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omp_icon_names_resolve_and_unknown_ones_fall_back_to_a_dot() {
        for n in [
            "copy",
            "rewind",
            "history",
            "lightbulb",
            "corner-down-right",
            "eye",
            "chev",
            "check",
            "x",
            "warn",
            "bolt",
            "flame",
            "brain",
            "sparkle",
            "branch",
            "folder",
            "file",
            "diff",
            "terminal",
            "gear",
            "model",
            "jobs",
            "refresh",
            "context.compaction",
        ] {
            assert!(known(n), "{n}");
        }
        assert_eq!(lucide("provider:anthropic"), lucide("bot"));
        assert_eq!(lucide("mi-rust"), lucide("file"));
        assert_eq!(lucide("totally-unknown"), "\u{e076}");
        assert!(!known("totally-unknown"));
    }
}
