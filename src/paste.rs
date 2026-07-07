//! Context-aware paste handling for markdown.
//!
//! This module handles pasted content with minimal transformation:
//! - Normalizes line endings (CRLF → LF)
//! - Normalizes curly quotes to straight quotes
//! - Preserves content literally in code blocks
//! - Adds blockquote prefixes to continuation lines when in a blockquote

use crate::buffer::Buffer;
use crate::marker::MarkerKind;

/// Context about the cursor position for paste operations.
#[derive(Debug, Clone)]
pub struct PasteContext {
    /// Whether the cursor is inside a code block.
    pub in_code_block: bool,
    /// The blockquote prefix to apply to continuation lines (e.g., "> " or "> > ").
    pub blockquote_prefix: String,
}

impl PasteContext {
    /// Analyze the buffer at cursor position to determine paste context.
    pub fn from_buffer(buffer: &Buffer, cursor_offset: usize) -> Self {
        let line_idx = buffer.byte_to_line(cursor_offset);
        let current_line = buffer.line_markers(line_idx);

        let mut in_code_block = false;
        for code_block in &buffer.parsed().code_blocks {
            if cursor_offset >= code_block.block_range.start
                && cursor_offset < code_block.block_range.end
            {
                in_code_block = cursor_offset >= code_block.content_range.start
                    || code_block.content_range.is_empty();
                break;
            }
        }

        let bq_depth = current_line
            .markers
            .iter()
            .filter(|m| matches!(m.kind, MarkerKind::BlockQuote))
            .count();
        let blockquote_prefix = "> ".repeat(bq_depth);

        Self {
            in_code_block,
            blockquote_prefix,
        }
    }
}

/// Normalize line endings and curly quotes.
fn normalize(text: &str) -> String {
    // Single pass: CRLF/CR → LF, and curly quotes → ASCII, in one allocation.
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\u{201C}' | '\u{201D}' => out.push('"'),
            '\u{2018}' | '\u{2019}' => out.push('\''),
            _ => out.push(c),
        }
    }
    out
}

/// Transform pasted content based on context.
pub fn transform_paste(text: &str, ctx: &PasteContext) -> String {
    let normalized = normalize(text);

    if ctx.in_code_block {
        return normalized;
    }

    if ctx.blockquote_prefix.is_empty() {
        return normalized;
    }

    normalized
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                line.to_string()
            } else if line.is_empty() {
                ctx.blockquote_prefix.trim_end().to_string()
            } else {
                format!("{}{}", ctx.blockquote_prefix, line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(in_code_block: bool, blockquote_prefix: &str) -> PasteContext {
        PasteContext {
            in_code_block,
            blockquote_prefix: blockquote_prefix.to_string(),
        }
    }

    #[test]
    fn test_paste_simple_text_normal_line() {
        let ctx = ctx(false, "");
        let result = transform_paste("hello world", &ctx);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_paste_multiline_normal_line() {
        let ctx = ctx(false, "");
        let result = transform_paste("line 1\nline 2", &ctx);
        assert_eq!(result, "line 1\nline 2");
    }

    #[test]
    fn test_paste_blockquote_in_code_block() {
        let ctx = ctx(true, "");
        let result = transform_paste("> quoted", &ctx);
        assert_eq!(result, "> quoted");
    }

    #[test]
    fn test_paste_heading_in_code_block() {
        let ctx = ctx(true, "");
        let result = transform_paste("# heading", &ctx);
        assert_eq!(result, "# heading");
    }

    #[test]
    fn test_paste_simple_text_in_blockquote() {
        let ctx = ctx(false, "> ");
        let result = transform_paste("hello", &ctx);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_paste_multiline_in_blockquote() {
        let ctx = ctx(false, "> ");
        let result = transform_paste("line 1\nline 2", &ctx);
        assert_eq!(result, "line 1\n> line 2");
    }

    #[test]
    fn test_paste_paragraphs_in_blockquote() {
        let ctx = ctx(false, "> ");
        let result = transform_paste("para 1\n\npara 2", &ctx);
        assert_eq!(result, "para 1\n>\n> para 2");
    }

    #[test]
    fn test_paste_multiline_in_nested_blockquote() {
        let ctx = ctx(false, "> > ");
        let result = transform_paste("line 1\nline 2", &ctx);
        assert_eq!(result, "line 1\n> > line 2");
    }

    #[test]
    fn test_paste_multiline_on_list_item() {
        let ctx = ctx(false, "");
        let result = transform_paste("line 1\nline 2", &ctx);
        assert_eq!(result, "line 1\nline 2");
    }

    #[test]
    fn test_paste_curly_quotes_normalized() {
        let ctx = ctx(false, "");
        let result = transform_paste("\u{201C}hello\u{201D}", &ctx);
        assert_eq!(result, "\"hello\"");
    }

    #[test]
    fn test_paste_single_curly_quotes_normalized() {
        let ctx = ctx(false, "");
        let result = transform_paste("\u{2018}hello\u{2019}", &ctx);
        assert_eq!(result, "'hello'");
    }

    #[test]
    fn test_paste_crlf_normalized() {
        let ctx = ctx(false, "");
        let result = transform_paste("line 1\r\nline 2", &ctx);
        assert_eq!(result, "line 1\nline 2");
    }

    #[test]
    fn test_paste_cr_normalized() {
        let ctx = ctx(false, "");
        let result = transform_paste("line 1\rline 2", &ctx);
        assert_eq!(result, "line 1\nline 2");
    }
}
