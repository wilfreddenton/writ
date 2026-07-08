//! Obsidian-style callouts / admonitions.
//!
//! A callout is a blockquote whose first line is a type marker — `> [!note]`,
//! `> [!warning] Custom title`, `> [!tip]- Collapsed by default`. Since a blockquote is
//! a real tree-sitter `block_quote` node, callouts are detected at parse time (in
//! `collect_node_infos`) rather than by a viewport scan: each top-level `block_quote`
//! whose first line parses as a marker becomes a [`CalloutInfo`] in `ParsedNodes`.
//!
//! Rendering (see `render::build_line_render`) hides the `[!type]` marker on the header
//! line and substitutes an icon + title in the type's accent color; the raw syntax is
//! revealed while the caret is on the line. Folding (`+`/`-`) reuses the generic fold
//! system (see `fold`), keyed on the header line's byte offset like headings/list items.

use std::ops::Range;

use vello::peniko::Color;

use crate::editor::EditorTheme;

/// A callout type. Aliases collapse to one kind (Obsidian's grouping), which drives the
/// icon and accent color. Unknown types are not callouts (the blockquote renders plainly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalloutKind {
    Note,
    Abstract,
    Info,
    Todo,
    Tip,
    Success,
    Question,
    Warning,
    Failure,
    Danger,
    Bug,
    Example,
    Quote,
}

impl CalloutKind {
    /// Resolve a type identifier (case-insensitive), including Obsidian's aliases.
    pub fn from_label(label: &str) -> Option<Self> {
        let l = label.to_ascii_lowercase();
        Some(match l.as_str() {
            "note" => Self::Note,
            "abstract" | "summary" | "tldr" => Self::Abstract,
            "info" => Self::Info,
            "todo" => Self::Todo,
            "tip" | "hint" | "important" => Self::Tip,
            "success" | "check" | "done" => Self::Success,
            "question" | "help" | "faq" => Self::Question,
            "warning" | "caution" | "attention" => Self::Warning,
            "failure" | "fail" | "missing" => Self::Failure,
            "danger" | "error" => Self::Danger,
            "bug" => Self::Bug,
            "example" => Self::Example,
            "quote" | "cite" => Self::Quote,
            _ => return None,
        })
    }

    /// A mono-font glyph for the type (verified to render in the default + Iosevka stacks).
    pub fn icon(self) -> &'static str {
        match self {
            Self::Note => "✎",
            Self::Abstract => "≡",
            Self::Info => "ⓘ",
            Self::Todo => "○",
            Self::Tip => "★",
            Self::Success => "✓",
            Self::Question => "?",
            Self::Warning => "⚠",
            Self::Failure => "✗",
            Self::Danger => "⚡",
            Self::Bug => "⊗",
            Self::Example => "❯",
            Self::Quote => "❝",
        }
    }

    /// The default header title when the callout has no custom title.
    pub fn default_title(self) -> &'static str {
        match self {
            Self::Note => "Note",
            Self::Abstract => "Abstract",
            Self::Info => "Info",
            Self::Todo => "Todo",
            Self::Tip => "Tip",
            Self::Success => "Success",
            Self::Question => "Question",
            Self::Warning => "Warning",
            Self::Failure => "Failure",
            Self::Danger => "Danger",
            Self::Bug => "Bug",
            Self::Example => "Example",
            Self::Quote => "Quote",
        }
    }

    /// The accent color, mapped onto the theme's palette (Obsidian's color groups: the
    /// blue family → cyan, positive → green, caution → yellow/orange, severe → red).
    pub fn color(self, theme: &EditorTheme) -> Color {
        match self {
            Self::Note | Self::Info | Self::Todo => theme.cyan,
            Self::Abstract | Self::Tip | Self::Success => theme.green,
            Self::Question => theme.yellow,
            Self::Warning => theme.orange,
            Self::Failure | Self::Danger | Self::Bug => theme.red,
            Self::Example => theme.purple,
            Self::Quote => theme.comment,
        }
    }
}

/// A detected callout: the header line plus its foldable body extent. Byte-offset anchored
/// (like [`crate::marker::HeadingInfo`]) so edit-remap and folding treat it uniformly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalloutInfo {
    pub kind: CalloutKind,
    /// The display title: the custom title if present, else the type's default name.
    pub title: String,
    /// Whether the callout opted into folding (`+`/`-` after the type). Obsidian only
    /// shows a fold arrow for foldable callouts; a plain `[!note]` is not collapsible.
    pub foldable: bool,
    /// `[!note]-` starts collapsed; `[!note]+` (or plain) starts expanded.
    pub default_collapsed: bool,
    /// 0-based line of the `[!type]` header.
    pub header_line: usize,
    /// Byte offset of the header line's start (the fold anchor).
    pub byte_offset: usize,
    /// First line NOT part of the callout. Body = `(header_line+1)..end_line`; the
    /// callout is foldable-in-practice iff it opted in AND `end_line > header_line + 1`.
    pub end_line: usize,
    /// Blockquote-nesting depth (top-level = 1, a callout nested one `>` deeper = 2, …).
    /// Groups callouts for the "fold all at this level" (Ctrl+click) gesture.
    pub depth: usize,
}

impl CalloutInfo {
    /// Hidden-line extent when folded: the body lines under the header.
    pub fn body_extent(&self) -> Range<usize> {
        (self.header_line + 1)..self.end_line.max(self.header_line + 1)
    }

    /// Whether folding this callout would hide at least one line (it opted in and has a body).
    pub fn is_foldable(&self) -> bool {
        self.foldable && self.end_line > self.header_line + 1
    }
}

/// Parse a blockquote's first line into a callout header, if it is one. `line` is the raw
/// first line (leading indentation and the `>` marker included). Returns the kind, the
/// resolved display title, and the fold flags. Unknown types return `None`.
pub fn parse_callout_header(line: &str) -> Option<(CalloutKind, String, bool, bool)> {
    let s = line.trim_start();
    let s = s.strip_prefix('>')?;
    // One optional space after the `>` marker (`>[!note]` and `> [!note]` both valid).
    let s = s.strip_prefix(' ').unwrap_or(s);
    let s = s.strip_prefix("[!")?;
    let close = s.find(']')?;
    let label = &s[..close];
    if label.is_empty() {
        return None;
    }
    let kind = CalloutKind::from_label(label)?;

    let after = &s[close + 1..];
    let (foldable, default_collapsed, rest) = match after.as_bytes().first() {
        Some(b'-') => (true, true, &after[1..]),
        Some(b'+') => (true, false, &after[1..]),
        _ => (false, false, after),
    };
    let custom = rest.trim();
    let title = if custom.is_empty() {
        kind.default_title().to_string()
    } else {
        custom.to_string()
    };
    Some((kind, title, foldable, default_collapsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve() {
        assert_eq!(CalloutKind::from_label("note"), Some(CalloutKind::Note));
        assert_eq!(
            CalloutKind::from_label("WARNING"),
            Some(CalloutKind::Warning)
        );
        assert_eq!(CalloutKind::from_label("tldr"), Some(CalloutKind::Abstract));
        assert_eq!(
            CalloutKind::from_label("caution"),
            Some(CalloutKind::Warning)
        );
        assert_eq!(CalloutKind::from_label("error"), Some(CalloutKind::Danger));
        assert_eq!(CalloutKind::from_label("nonsense"), None);
    }

    #[test]
    fn parses_plain() {
        let (kind, title, foldable, collapsed) = parse_callout_header("> [!note]").unwrap();
        assert_eq!(kind, CalloutKind::Note);
        assert_eq!(title, "Note");
        assert!(!foldable);
        assert!(!collapsed);
    }

    #[test]
    fn parses_custom_title() {
        let (kind, title, _, _) = parse_callout_header("> [!warning] Be careful now").unwrap();
        assert_eq!(kind, CalloutKind::Warning);
        assert_eq!(title, "Be careful now");
    }

    #[test]
    fn parses_fold_signs() {
        let (_, _, foldable, collapsed) = parse_callout_header("> [!tip]- Collapsed").unwrap();
        assert!(foldable);
        assert!(collapsed);
        let (_, _, foldable, collapsed) = parse_callout_header("> [!tip]+ Expanded").unwrap();
        assert!(foldable);
        assert!(!collapsed);
    }

    #[test]
    fn no_space_after_marker() {
        assert!(parse_callout_header(">[!info]").is_some());
    }

    #[test]
    fn indented_blockquote() {
        // A blockquote nested in a list still carries a leading indent before `>`.
        assert_eq!(
            parse_callout_header("  > [!success] Done").map(|t| t.0),
            Some(CalloutKind::Success)
        );
    }

    #[test]
    fn not_a_callout() {
        assert!(parse_callout_header("> just a normal quote").is_none());
        assert!(parse_callout_header("> [!] empty type").is_none());
        assert!(parse_callout_header("no blockquote").is_none());
        assert!(parse_callout_header("> [!mystery] unknown type").is_none());
    }

    #[test]
    fn extent_and_foldability() {
        let c = CalloutInfo {
            kind: CalloutKind::Note,
            title: "Note".into(),
            foldable: true,
            default_collapsed: false,
            header_line: 3,
            byte_offset: 300,
            end_line: 6,
            depth: 1,
        };
        assert_eq!(c.body_extent(), 4..6);
        assert!(c.is_foldable());

        // Opted-in but no body → not foldable in practice.
        let empty = CalloutInfo {
            end_line: 4,
            ..c.clone()
        };
        assert_eq!(empty.body_extent(), 4..4);
        assert!(!empty.is_foldable());

        // Has a body but didn't opt into folding.
        let plain = CalloutInfo {
            foldable: false,
            ..c
        };
        assert!(!plain.is_foldable());
    }
}
