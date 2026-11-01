//! Lucide icon font (ISC, `assets/fonts/lucide.ttf`, lucide-static 1.61.0): the `Icon` glyph type and the
//! named constants the UI uses. Glyphs live in the private-use area; the font is registered both
//! as the dedicated `lucide` family and as a Proportional fallback so a glyph embedded in a label
//! string still renders.

use std::sync::LazyLock;

use egui::{FontFamily, RichText};

/// Family name of the dedicated icon face.
pub const FONT_FAMILY: &str = "lucide";

/// One Lucide glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Icon {
    pub codepoint: &'static str,
}

impl Icon {
    pub const fn new(codepoint: &'static str) -> Self {
        Self { codepoint }
    }

    pub fn font_family(&self) -> FontFamily {
        static FAMILY: LazyLock<FontFamily> =
            LazyLock::new(|| FontFamily::Name(FONT_FAMILY.into()));
        FAMILY.clone()
    }

    /// The glyph as [`RichText`] in the icon family.
    pub fn rich_text(self) -> RichText {
        RichText::new(self.codepoint).family(self.font_family())
    }
}

pub const ICON_A_LARGE_SMALL: Icon = Icon::new("\u{e587}");
pub const ICON_ACTIVITY: Icon = Icon::new("\u{e038}");
pub const ICON_ALIGN_LEFT: Icon = Icon::new("\u{e185}");
pub const ICON_APP_WINDOW: Icon = Icon::new("\u{e426}");
pub const ICON_ARCHIVE_RESTORE: Icon = Icon::new("\u{e2cd}");
pub const ICON_ARROW_DOWN_TO_LINE: Icon = Icon::new("\u{e455}");
pub const ICON_ARROW_DOWN: Icon = Icon::new("\u{e042}");
pub const ICON_ARROW_LEFT_RIGHT: Icon = Icon::new("\u{e24a}");
pub const ICON_ARROW_RIGHT: Icon = Icon::new("\u{e049}");
pub const ICON_ARROW_UP: Icon = Icon::new("\u{e04a}");
pub const ICON_BELL: Icon = Icon::new("\u{e059}");
pub const ICON_BOOKMARK: Icon = Icon::new("\u{e060}");
pub const ICON_BOT: Icon = Icon::new("\u{e1bb}");
pub const ICON_BRAIN: Icon = Icon::new("\u{e3c6}");
pub const ICON_CHART_PIE: Icon = Icon::new("\u{e06b}");
pub const ICON_CHECK: Icon = Icon::new("\u{e06c}");
pub const ICON_CHEVRON_DOWN: Icon = Icon::new("\u{e06d}");
pub const ICON_CHEVRON_LEFT: Icon = Icon::new("\u{e06e}");
pub const ICON_CHEVRON_RIGHT: Icon = Icon::new("\u{e06f}");
pub const ICON_CHEVRON_UP: Icon = Icon::new("\u{e070}");
pub const ICON_CHEVRONS_DOWN: Icon = Icon::new("\u{e071}");
pub const ICON_CHEVRONS_DOWN_UP: Icon = Icon::new("\u{e228}");
pub const ICON_CHEVRONS_UP_DOWN: Icon = Icon::new("\u{e211}");
pub const ICON_CIRCLE_ALERT: Icon = Icon::new("\u{e077}");
pub const ICON_CIRCLE_ARROW_UP: Icon = Icon::new("\u{e07b}");
pub const ICON_CIRCLE_CHECK: Icon = Icon::new("\u{e226}");
pub const ICON_CIRCLE_PAUSE: Icon = Icon::new("\u{e07f}");
pub const ICON_CIRCLE_STOP: Icon = Icon::new("\u{e083}");
pub const ICON_CIRCLE_X: Icon = Icon::new("\u{e084}");
pub const ICON_CLIPBOARD_LIST: Icon = Icon::new("\u{e086}");
pub const ICON_COLUMNS_2: Icon = Icon::new("\u{e098}");
pub const ICON_CLOUD: Icon = Icon::new("\u{e088}");
pub const ICON_CLOUD_UPLOAD: Icon = Icon::new("\u{e091}");
pub const ICON_COPY: Icon = Icon::new("\u{e09e}");
pub const ICON_COINS: Icon = Icon::new("\u{e097}");
pub const ICON_CORNER_DOWN_RIGHT: Icon = Icon::new("\u{e0a2}");
pub const ICON_CPU: Icon = Icon::new("\u{e0a9}");
pub const ICON_CROSSHAIR: Icon = Icon::new("\u{e0ac}");
pub const ICON_DOWNLOAD: Icon = Icon::new("\u{e0b2}");
pub const ICON_ELLIPSIS: Icon = Icon::new("\u{e0b6}");
pub const ICON_EXTERNAL_LINK: Icon = Icon::new("\u{e0b9}");
pub const ICON_FILE_PEN_LINE: Icon = Icon::new("\u{e320}");
pub const ICON_FILE_PLUS: Icon = Icon::new("\u{e0c9}");
pub const ICON_FILE_TEXT: Icon = Icon::new("\u{e0cc}");
pub const ICON_FILES: Icon = Icon::new("\u{e0cf}");
pub const ICON_FOCUS: Icon = Icon::new("\u{e29e}");
pub const ICON_FOLD_VERTICAL: Icon = Icon::new("\u{e43c}");
pub const ICON_FOLDER: Icon = Icon::new("\u{e0d7}");
pub const ICON_FOLDER_OPEN: Icon = Icon::new("\u{e247}");
pub const ICON_FOLDER_OUTPUT: Icon = Icon::new("\u{e337}");
pub const ICON_FOLDER_INPUT: Icon = Icon::new("\u{e334}");
pub const ICON_FOLDER_PLUS: Icon = Icon::new("\u{e0d9}");
pub const ICON_FOLDER_GIT_2: Icon = Icon::new("\u{e40a}");
pub const ICON_FOLDER_X: Icon = Icon::new("\u{e33e}");
pub const ICON_GAUGE: Icon = Icon::new("\u{e1bf}");
pub const ICON_GIT_BRANCH: Icon = Icon::new("\u{e0e2}");
pub const ICON_GRID_2X2: Icon = Icon::new("\u{e4ff}");
pub const ICON_GIT_BRANCH_PLUS: Icon = Icon::new("\u{e1f4}");
pub const ICON_GIT_MERGE: Icon = Icon::new("\u{e0e4}");
pub const ICON_GIT_FORK: Icon = Icon::new("\u{e28d}");
pub const ICON_GLOBE: Icon = Icon::new("\u{e0e8}");
pub const ICON_HANDSHAKE: Icon = Icon::new("\u{e5c0}");
pub const ICON_HARD_DRIVE: Icon = Icon::new("\u{e0ed}");
pub const ICON_HISTORY: Icon = Icon::new("\u{e1f5}");
pub const ICON_INFO: Icon = Icon::new("\u{e0f9}");
pub const ICON_KEY_ROUND: Icon = Icon::new("\u{e4a3}");
pub const ICON_KEYBOARD: Icon = Icon::new("\u{e284}");
pub const ICON_LIST: Icon = Icon::new("\u{e106}");
pub const ICON_LIST_CHECKS: Icon = Icon::new("\u{e1d0}");
pub const ICON_LIST_TREE: Icon = Icon::new("\u{e408}");
pub const ICON_LOCK: Icon = Icon::new("\u{e10b}");
pub const ICON_LOG_IN: Icon = Icon::new("\u{e10d}");
pub const ICON_LOG_OUT: Icon = Icon::new("\u{e10e}");
pub const ICON_MAP: Icon = Icon::new("\u{e110}");
pub const ICON_MENU: Icon = Icon::new("\u{e115}");
pub const ICON_MESSAGES_SQUARE: Icon = Icon::new("\u{e40d}");
pub const ICON_MINUS: Icon = Icon::new("\u{e11c}");
pub const ICON_MOON: Icon = Icon::new("\u{e11e}");
pub const ICON_MOON_STAR: Icon = Icon::new("\u{e410}");
pub const ICON_NETWORK: Icon = Icon::new("\u{e125}");
pub const ICON_NEWSPAPER: Icon = Icon::new("\u{e348}");
pub const ICON_PACKAGE: Icon = Icon::new("\u{e129}");
pub const ICON_PALETTE: Icon = Icon::new("\u{e1dd}");
pub const ICON_PENCIL: Icon = Icon::new("\u{e1f9}");
pub const ICON_PLAY: Icon = Icon::new("\u{e13c}");
pub const ICON_PLUS: Icon = Icon::new("\u{e13d}");
pub const ICON_POWER: Icon = Icon::new("\u{e140}");
pub const ICON_PUZZLE: Icon = Icon::new("\u{e29c}");
pub const ICON_REFRESH_CW: Icon = Icon::new("\u{e145}");
pub const ICON_ROCKET: Icon = Icon::new("\u{e286}");
pub const ICON_ROTATE_CCW: Icon = Icon::new("\u{e148}");
pub const ICON_ROWS_2: Icon = Icon::new("\u{e439}");
pub const ICON_SAVE: Icon = Icon::new("\u{e14d}");
pub const ICON_SCROLL_TEXT: Icon = Icon::new("\u{e45f}");
pub const ICON_SEARCH: Icon = Icon::new("\u{e151}");
pub const ICON_SEARCH_X: Icon = Icon::new("\u{e4ad}");
pub const ICON_SEND: Icon = Icon::new("\u{e152}");
pub const ICON_SETTINGS: Icon = Icon::new("\u{e154}");
pub const ICON_SETTINGS_2: Icon = Icon::new("\u{e245}");
pub const ICON_SLIDERS_HORIZONTAL: Icon = Icon::new("\u{e29a}");
pub const ICON_SPARKLES: Icon = Icon::new("\u{e412}");
pub const ICON_SQUARE: Icon = Icon::new("\u{e167}");
pub const ICON_STAR: Icon = Icon::new("\u{e176}");
pub const ICON_SUN: Icon = Icon::new("\u{e178}");
pub const ICON_TERMINAL: Icon = Icon::new("\u{e181}");
pub const ICON_TRASH_2: Icon = Icon::new("\u{e18e}");
pub const ICON_TRIANGLE_ALERT: Icon = Icon::new("\u{e193}");
pub const ICON_UNDO_2: Icon = Icon::new("\u{e2a1}");
pub const ICON_USER: Icon = Icon::new("\u{e19f}");
pub const ICON_VIBRATE: Icon = Icon::new("\u{e223}");
pub const ICON_WRENCH: Icon = Icon::new("\u{e1b1}");
pub const ICON_X: Icon = Icon::new("\u{e1b2}");
