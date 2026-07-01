use gpui::{Global, Rgba, rgb};

use crate::highlight::Highlighter;

// Platform-specific default fonts
#[cfg(target_os = "windows")]
pub const DEFAULT_TEXT_FONT: &str = "Segoe UI";
#[cfg(target_os = "windows")]
pub const DEFAULT_CODE_FONT: &str = "Consolas";

#[cfg(target_os = "macos")]
pub const DEFAULT_TEXT_FONT: &str = ".AppleSystemUIFont";
#[cfg(target_os = "macos")]
pub const DEFAULT_CODE_FONT: &str = "Menlo";

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub const DEFAULT_TEXT_FONT: &str = "Liberation Sans";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub const DEFAULT_CODE_FONT: &str = "Liberation Mono";

/// Color theme for the editor.
///
/// Provides colors for the background, foreground, selection, and syntax
/// highlighting. Use [`EditorTheme::dracula()`] for the built-in Dracula theme.
#[derive(Clone)]
pub struct EditorTheme {
    pub background: Rgba,
    pub foreground: Rgba,
    pub selection: Rgba,
    pub comment: Rgba,
    pub red: Rgba,
    pub orange: Rgba,
    pub yellow: Rgba,
    pub green: Rgba,
    pub cyan: Rgba,
    pub purple: Rgba,
    pub pink: Rgba,
}

impl EditorTheme {
    /// The Dracula color theme.
    pub fn dracula() -> Self {
        Self {
            background: rgb(0x282A36),
            foreground: rgb(0xF8F8F2),
            selection: rgb(0x44475A),
            comment: rgb(0x6272A4),
            red: rgb(0xFF5555),
            orange: rgb(0xFFB86C),
            yellow: rgb(0xF1FA8C),
            green: rgb(0x50FA7B),
            cyan: rgb(0x8BE9FD),
            purple: rgb(0xBD93F9),
            pink: rgb(0xFF79C6),
        }
    }

    pub fn color_for_capture(&self, capture: &str) -> Rgba {
        // Handle specific sub-captures first
        match capture {
            "variable.special" => return self.purple,
            "variable.parameter" => return self.orange,
            "punctuation.bracket" => return self.foreground,
            "punctuation.special" => return self.pink,
            "string.escape" => return self.pink,
            "lifetime" => return self.pink,
            _ => {}
        }

        let base = capture.split('.').next().unwrap_or(capture);

        match base {
            "keyword" => self.pink,
            "function" => self.green,
            "type" => self.cyan,
            "string" => self.yellow,
            "number" | "boolean" => self.purple,
            "comment" => self.comment,
            "constant" => self.purple,
            "operator" => self.pink,
            "attribute" => self.pink,
            "property" => self.cyan,
            "punctuation" => self.foreground,
            _ => self.foreground,
        }
    }

    pub fn color_for_highlight(&self, highlight_id: usize) -> Rgba {
        self.color_for_capture(Highlighter::capture_name(highlight_id))
    }
}

impl Default for EditorTheme {
    fn default() -> Self {
        Self::dracula()
    }
}

impl Global for EditorTheme {}
