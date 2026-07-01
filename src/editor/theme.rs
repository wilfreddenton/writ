use vello::peniko::Color;

use crate::highlight::Highlighter;

/// Color theme for the editor.
///
/// Provides colors for the background, foreground, selection, and syntax
/// highlighting. Use [`EditorTheme::dracula()`] for the built-in Dracula theme.
#[derive(Clone)]
pub struct EditorTheme {
    pub background: Color,
    pub foreground: Color,
    pub selection: Color,
    pub comment: Color,
    pub red: Color,
    pub orange: Color,
    pub yellow: Color,
    pub green: Color,
    pub cyan: Color,
    pub purple: Color,
    pub pink: Color,
}

impl EditorTheme {
    /// The Dracula color theme.
    pub fn dracula() -> Self {
        Self {
            background: Color::from_rgba8(0x28, 0x2A, 0x36, 0xFF),
            foreground: Color::from_rgba8(0xF8, 0xF8, 0xF2, 0xFF),
            selection: Color::from_rgba8(0x44, 0x47, 0x5A, 0xFF),
            comment: Color::from_rgba8(0x62, 0x72, 0xA4, 0xFF),
            red: Color::from_rgba8(0xFF, 0x55, 0x55, 0xFF),
            orange: Color::from_rgba8(0xFF, 0xB8, 0x6C, 0xFF),
            yellow: Color::from_rgba8(0xF1, 0xFA, 0x8C, 0xFF),
            green: Color::from_rgba8(0x50, 0xFA, 0x7B, 0xFF),
            cyan: Color::from_rgba8(0x8B, 0xE9, 0xFD, 0xFF),
            purple: Color::from_rgba8(0xBD, 0x93, 0xF9, 0xFF),
            pink: Color::from_rgba8(0xFF, 0x79, 0xC6, 0xFF),
        }
    }

    pub fn color_for_capture(&self, capture: &str) -> Color {
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

    pub fn color_for_highlight(&self, highlight_id: usize) -> Color {
        self.color_for_capture(Highlighter::capture_name(highlight_id))
    }
}

impl Default for EditorTheme {
    fn default() -> Self {
        Self::dracula()
    }
}
