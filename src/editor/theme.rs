use vello::peniko::Color;

use crate::highlight::Highlighter;

/// Color theme for the editor.
///
/// Provides colors for the background, foreground, selection, and syntax
/// highlighting. Use [`EditorTheme::dracula()`] for the built-in Dracula theme.
#[derive(Clone, Debug)]
pub struct EditorTheme {
    pub background: Color,
    /// Slightly-darker fill for chrome (the status bar) so it reads as a distinct
    /// surface rather than blending into the document background.
    pub surface: Color,
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
            surface: Color::from_rgba8(0x21, 0x22, 0x2C, 0xFF),
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

    /// The Nord color theme (arctic dark): keywords blue, types teal, strings amber.
    pub fn nord() -> Self {
        Self {
            background: Color::from_rgba8(0x2E, 0x34, 0x40, 0xFF),
            surface: Color::from_rgba8(0x2B, 0x30, 0x3B, 0xFF),
            foreground: Color::from_rgba8(0xD8, 0xDE, 0xE9, 0xFF),
            selection: Color::from_rgba8(0x43, 0x4C, 0x5E, 0xFF),
            comment: Color::from_rgba8(0x61, 0x6E, 0x88, 0xFF),
            red: Color::from_rgba8(0xBF, 0x61, 0x6A, 0xFF),
            orange: Color::from_rgba8(0xD0, 0x87, 0x70, 0xFF),
            yellow: Color::from_rgba8(0xEB, 0xCB, 0x8B, 0xFF),
            green: Color::from_rgba8(0xA3, 0xBE, 0x8C, 0xFF),
            cyan: Color::from_rgba8(0x8F, 0xBC, 0xBB, 0xFF),
            purple: Color::from_rgba8(0xB4, 0x8E, 0xAD, 0xFF),
            pink: Color::from_rgba8(0x81, 0xA1, 0xC1, 0xFF),
        }
    }

    /// The Solarized Light color theme: keywords magenta, types cyan, strings amber.
    pub fn solarized_light() -> Self {
        Self {
            background: Color::from_rgba8(0xFD, 0xF6, 0xE3, 0xFF),
            surface: Color::from_rgba8(0xEE, 0xE8, 0xD5, 0xFF),
            foreground: Color::from_rgba8(0x65, 0x7B, 0x83, 0xFF),
            selection: Color::from_rgba8(0xD9, 0xD2, 0xBE, 0xFF),
            comment: Color::from_rgba8(0x93, 0xA1, 0xA1, 0xFF),
            red: Color::from_rgba8(0xDC, 0x32, 0x2F, 0xFF),
            orange: Color::from_rgba8(0xCB, 0x4B, 0x16, 0xFF),
            yellow: Color::from_rgba8(0xB5, 0x89, 0x00, 0xFF),
            green: Color::from_rgba8(0x85, 0x99, 0x00, 0xFF),
            cyan: Color::from_rgba8(0x2A, 0xA1, 0x98, 0xFF),
            purple: Color::from_rgba8(0x6C, 0x71, 0xC4, 0xFF),
            pink: Color::from_rgba8(0xD3, 0x36, 0x82, 0xFF),
        }
    }

    /// A built-in theme by its config name, or `None` if unknown.
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "dracula" => Some(Self::dracula()),
            "nord" => Some(Self::nord()),
            "solarized-light" => Some(Self::solarized_light()),
            _ => None,
        }
    }

    pub fn color_for_capture(&self, capture: &str) -> Color {
        // Handle specific sub-captures first
        match capture {
            "variable.special" => return self.purple,
            "variable.parameter" => return self.orange,
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
