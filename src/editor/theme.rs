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

    /// The Solarized Dark color theme (companion to [`solarized_light`](Self::solarized_light)).
    pub fn solarized_dark() -> Self {
        Self {
            background: Color::from_rgba8(0x00, 0x2B, 0x36, 0xFF),
            surface: Color::from_rgba8(0x07, 0x36, 0x42, 0xFF),
            foreground: Color::from_rgba8(0x83, 0x94, 0x96, 0xFF),
            selection: Color::from_rgba8(0x0A, 0x4B, 0x5C, 0xFF),
            comment: Color::from_rgba8(0x58, 0x6E, 0x75, 0xFF),
            red: Color::from_rgba8(0xDC, 0x32, 0x2F, 0xFF),
            orange: Color::from_rgba8(0xCB, 0x4B, 0x16, 0xFF),
            yellow: Color::from_rgba8(0xB5, 0x89, 0x00, 0xFF),
            green: Color::from_rgba8(0x85, 0x99, 0x00, 0xFF),
            cyan: Color::from_rgba8(0x2A, 0xA1, 0x98, 0xFF),
            purple: Color::from_rgba8(0x6C, 0x71, 0xC4, 0xFF),
            pink: Color::from_rgba8(0xD3, 0x36, 0x82, 0xFF),
        }
    }

    /// The Gruvbox Dark color theme: keywords red, types aqua, strings green.
    pub fn gruvbox_dark() -> Self {
        Self {
            background: Color::from_rgba8(0x28, 0x28, 0x28, 0xFF),
            surface: Color::from_rgba8(0x1D, 0x20, 0x21, 0xFF),
            foreground: Color::from_rgba8(0xEB, 0xDB, 0xB2, 0xFF),
            selection: Color::from_rgba8(0x50, 0x49, 0x45, 0xFF),
            comment: Color::from_rgba8(0x92, 0x83, 0x74, 0xFF),
            red: Color::from_rgba8(0xFB, 0x49, 0x34, 0xFF),
            orange: Color::from_rgba8(0xFE, 0x80, 0x19, 0xFF),
            yellow: Color::from_rgba8(0xFA, 0xBD, 0x2F, 0xFF),
            green: Color::from_rgba8(0xB8, 0xBB, 0x26, 0xFF),
            cyan: Color::from_rgba8(0x8E, 0xC0, 0x7C, 0xFF),
            purple: Color::from_rgba8(0xD3, 0x86, 0x9B, 0xFF),
            pink: Color::from_rgba8(0xFB, 0x49, 0x34, 0xFF),
        }
    }

    /// The Tokyo Night color theme (dark): keywords magenta, types cyan.
    pub fn tokyo_night() -> Self {
        Self {
            background: Color::from_rgba8(0x1A, 0x1B, 0x26, 0xFF),
            surface: Color::from_rgba8(0x16, 0x16, 0x1E, 0xFF),
            foreground: Color::from_rgba8(0xC0, 0xCA, 0xF5, 0xFF),
            selection: Color::from_rgba8(0x33, 0x46, 0x7C, 0xFF),
            comment: Color::from_rgba8(0x56, 0x5F, 0x89, 0xFF),
            red: Color::from_rgba8(0xF7, 0x76, 0x8E, 0xFF),
            orange: Color::from_rgba8(0xFF, 0x9E, 0x64, 0xFF),
            yellow: Color::from_rgba8(0xE0, 0xAF, 0x68, 0xFF),
            green: Color::from_rgba8(0x9E, 0xCE, 0x6A, 0xFF),
            cyan: Color::from_rgba8(0x7D, 0xCF, 0xFF, 0xFF),
            purple: Color::from_rgba8(0xBB, 0x9A, 0xF7, 0xFF),
            pink: Color::from_rgba8(0xBB, 0x9A, 0xF7, 0xFF),
        }
    }

    /// The Catppuccin Mocha color theme (dark): keywords mauve, types teal.
    pub fn catppuccin_mocha() -> Self {
        Self {
            background: Color::from_rgba8(0x1E, 0x1E, 0x2E, 0xFF),
            surface: Color::from_rgba8(0x18, 0x18, 0x25, 0xFF),
            foreground: Color::from_rgba8(0xCD, 0xD6, 0xF4, 0xFF),
            selection: Color::from_rgba8(0x45, 0x47, 0x5A, 0xFF),
            comment: Color::from_rgba8(0x93, 0x99, 0xB2, 0xFF),
            red: Color::from_rgba8(0xF3, 0x8B, 0xA8, 0xFF),
            orange: Color::from_rgba8(0xFA, 0xB3, 0x87, 0xFF),
            yellow: Color::from_rgba8(0xF9, 0xE2, 0xAF, 0xFF),
            green: Color::from_rgba8(0xA6, 0xE3, 0xA1, 0xFF),
            cyan: Color::from_rgba8(0x94, 0xE2, 0xD5, 0xFF),
            purple: Color::from_rgba8(0xCB, 0xA6, 0xF7, 0xFF),
            pink: Color::from_rgba8(0xCB, 0xA6, 0xF7, 0xFF),
        }
    }

    /// The Catppuccin Latte color theme (light): keywords mauve, types teal.
    pub fn catppuccin_latte() -> Self {
        Self {
            background: Color::from_rgba8(0xEF, 0xF1, 0xF5, 0xFF),
            surface: Color::from_rgba8(0xE6, 0xE9, 0xEF, 0xFF),
            foreground: Color::from_rgba8(0x4C, 0x4F, 0x69, 0xFF),
            selection: Color::from_rgba8(0xBC, 0xC0, 0xCC, 0xFF),
            comment: Color::from_rgba8(0x8C, 0x8F, 0xA1, 0xFF),
            red: Color::from_rgba8(0xD2, 0x0F, 0x39, 0xFF),
            orange: Color::from_rgba8(0xFE, 0x64, 0x0B, 0xFF),
            yellow: Color::from_rgba8(0xDF, 0x8E, 0x1D, 0xFF),
            green: Color::from_rgba8(0x40, 0xA0, 0x2B, 0xFF),
            cyan: Color::from_rgba8(0x17, 0x92, 0x99, 0xFF),
            purple: Color::from_rgba8(0x88, 0x39, 0xEF, 0xFF),
            pink: Color::from_rgba8(0x88, 0x39, 0xEF, 0xFF),
        }
    }

    /// A built-in theme by its config name, or `None` if unknown.
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "dracula" => Some(Self::dracula()),
            "nord" => Some(Self::nord()),
            "solarized-light" => Some(Self::solarized_light()),
            "solarized-dark" => Some(Self::solarized_dark()),
            "gruvbox-dark" => Some(Self::gruvbox_dark()),
            "tokyo-night" => Some(Self::tokyo_night()),
            "catppuccin-mocha" => Some(Self::catppuccin_mocha()),
            "catppuccin-latte" => Some(Self::catppuccin_latte()),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every built-in preset resolves, and no two share a background — a cheap guard
    /// against a copy-paste slip leaving two presets identical.
    #[test]
    fn presets_resolve_and_are_distinct() {
        let names = [
            "dracula",
            "nord",
            "solarized-light",
            "solarized-dark",
            "gruvbox-dark",
            "tokyo-night",
            "catppuccin-mocha",
            "catppuccin-latte",
        ];
        let mut backgrounds = Vec::new();
        for name in names {
            let theme = EditorTheme::by_name(name)
                .unwrap_or_else(|| panic!("preset '{name}' should resolve"));
            let bg = theme.background.to_rgba8();
            let key = (bg.r, bg.g, bg.b);
            assert!(
                !backgrounds.contains(&key),
                "preset '{name}' shares a background with another preset"
            );
            backgrounds.push(key);
        }
        assert!(EditorTheme::by_name("does-not-exist").is_none());
    }
}
