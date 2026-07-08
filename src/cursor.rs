use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::Buffer;

/// Byte offset one grapheme cluster before `offset` (or `offset` itself at position 0).
/// Steps over a preceding newline (its own grapheme) so backspace at a line start joins
/// lines. Graphemes never span `\n`, so a within-line chunk suffices otherwise. Correct
/// for multi-codepoint clusters (emoji ZWJ sequences, combining marks) where a naive
/// `offset - 1` would split a codepoint or a cluster.
pub fn prev_grapheme_boundary(buffer: &Buffer, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    let rope = buffer.rope();
    if rope.get_byte(offset - 1) == Some(b'\n') {
        return offset - 1;
    }
    let line_start = buffer.line_to_byte(buffer.byte_to_line(offset));
    let chunk = buffer.slice_cow(line_start..offset);
    match chunk.graphemes(true).next_back() {
        Some(g) => offset - g.len(),
        None => offset - 1,
    }
}

/// Byte offset one grapheme cluster after `offset` (or `offset` itself at the buffer
/// end). Steps over a trailing newline into the next line.
pub fn next_grapheme_boundary(buffer: &Buffer, offset: usize) -> usize {
    let len = buffer.len_bytes();
    if offset >= len {
        return offset;
    }
    let rope = buffer.rope();
    if rope.get_byte(offset) == Some(b'\n') {
        return offset + 1;
    }
    // `line_byte_range` already excludes the trailing '\n'.
    let range = buffer.line_byte_range(buffer.byte_to_line(offset));
    let chunk = buffer.slice_cow(offset..range.end);
    match chunk.graphemes(true).next() {
        Some(g) => offset + g.len(),
        None => offset + 1,
    }
}

/// Compute the byte offset on `target_line` at the same **grapheme** column as
/// `offset` on `from_line`, clamped to the target line's content length. Grapheme
/// units keep vertical movement consistent with horizontal (which steps whole
/// clusters) and always land on a cluster boundary. `line_byte_range` excludes the
/// trailing newline, so the clamp lands at end-of-content, not on the `\n`.
fn same_column_offset(
    buffer: &Buffer,
    from_line: usize,
    offset: usize,
    target_line: usize,
) -> usize {
    debug_assert_eq!(from_line, buffer.byte_to_line(offset));
    offset_at_column(buffer, target_line, grapheme_column(buffer, offset))
}

/// Grapheme column of `offset` within its own line.
pub(crate) fn grapheme_column(buffer: &Buffer, offset: usize) -> usize {
    let line = buffer.byte_to_line(offset);
    let start = buffer.line_to_byte(line);
    buffer.slice_cow(start..offset).graphemes(true).count()
}

/// Byte offset `column` graphemes into `line`, clamped to the line's content end.
pub(crate) fn offset_at_column(buffer: &Buffer, line: usize, column: usize) -> usize {
    let range = buffer.line_byte_range(line);
    let chunk = buffer.slice_cow(range.start..range.end);
    let mut byte = range.start;
    for (i, g) in chunk.graphemes(true).enumerate() {
        if i == column {
            return byte;
        }
        byte += g.len();
    }
    byte // fewer graphemes than the column → clamp to end of content
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub offset: usize,
}

impl Cursor {
    pub fn new(offset: usize) -> Self {
        Self { offset }
    }

    pub fn start() -> Self {
        Self { offset: 0 }
    }

    pub fn end(buffer: &Buffer) -> Self {
        Self {
            offset: buffer.len_bytes(),
        }
    }

    /// Move cursor left. Markers are atomic - cursor jumps over entire marker.
    /// Blank lines are not skipped.
    pub fn move_left(&self, buffer: &Buffer) -> Self {
        if self.offset == 0 {
            return *self;
        }

        let current_line_idx = buffer.byte_to_line(self.offset);

        let line = buffer.line_markers(current_line_idx);
        for marker in &line.markers {
            if self.offset == marker.range.end {
                return Self {
                    offset: marker.range.start,
                };
            }
        }

        if self.offset == line.range.start {
            if current_line_idx > 0 {
                let prev_line_range = buffer.line_byte_range(current_line_idx - 1);
                return Self {
                    offset: prev_line_range.end,
                };
            }
            return *self;
        }

        Self {
            offset: prev_grapheme_boundary(buffer, self.offset),
        }
    }

    /// Move cursor right. Markers are atomic - cursor jumps over entire marker.
    /// Blank lines are not skipped.
    pub fn move_right(&self, buffer: &Buffer) -> Self {
        let len = buffer.len_bytes();
        if self.offset >= len {
            return *self;
        }

        let current_line_idx = buffer.byte_to_line(self.offset);

        let line = buffer.line_markers(current_line_idx);
        for marker in line.markers.iter().rev() {
            if self.offset == marker.range.start {
                return Self {
                    offset: marker.range.end,
                };
            }
        }

        Self {
            offset: next_grapheme_boundary(buffer, self.offset),
        }
    }

    pub fn move_up(&self, buffer: &Buffer) -> Self {
        let current_line = buffer.byte_to_line(self.offset);
        if current_line == 0 {
            return Self::start();
        }
        Self {
            offset: same_column_offset(buffer, current_line, self.offset, current_line - 1),
        }
    }

    pub fn move_down(&self, buffer: &Buffer) -> Self {
        let current_line = buffer.byte_to_line(self.offset);
        let line_count = buffer.line_count();

        if current_line >= line_count - 1 {
            return Self::end(buffer);
        }
        Self {
            offset: same_column_offset(buffer, current_line, self.offset, current_line + 1),
        }
    }

    pub fn move_to_line_start(&self, buffer: &Buffer) -> Self {
        let current_line = buffer.byte_to_line(self.offset);
        Self {
            offset: buffer.line_to_byte(current_line),
        }
    }

    pub fn move_to_line_end(&self, buffer: &Buffer) -> Self {
        let current_line = buffer.byte_to_line(self.offset);
        let line_range = buffer.line_byte_range(current_line);
        Self {
            offset: line_range.end,
        }
    }

    pub fn move_to_start(&self) -> Self {
        Self::start()
    }

    pub fn move_to_end(&self, buffer: &Buffer) -> Self {
        Self::end(buffer)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    pub fn is_collapsed(&self) -> bool {
        self.anchor == self.head
    }

    pub fn cursor(&self) -> Cursor {
        Cursor::new(self.head)
    }

    pub fn range(&self) -> Range<usize> {
        if self.anchor <= self.head {
            self.anchor..self.head
        } else {
            self.head..self.anchor
        }
    }

    pub fn extend_to(&self, new_head: usize) -> Self {
        Self {
            anchor: self.anchor,
            head: new_head,
        }
    }

    pub fn select_all(buffer: &Buffer) -> Self {
        Self {
            anchor: 0,
            head: buffer.len_bytes(),
        }
    }

    pub fn select_word_at(offset: usize, buffer: &Buffer) -> Self {
        let rope = buffer.rope();
        let len_bytes = buffer.len_bytes();

        if len_bytes == 0 || offset >= len_bytes {
            return Self::new(offset.min(len_bytes), offset.min(len_bytes));
        }

        let is_word_char = |c: char| c.is_alphanumeric() || c == '_';
        let char_idx = rope.byte_to_char(offset);
        let char_count = rope.len_chars();

        if char_idx >= char_count {
            return Self::new(offset, offset);
        }

        let c = rope.char(char_idx);

        if !is_word_char(c) {
            let char_end = rope.char_to_byte(char_idx + 1);
            return Self::new(offset, char_end.min(len_bytes));
        }

        let mut start_char_idx = char_idx;
        for i in (0..char_idx).rev() {
            if is_word_char(rope.char(i)) {
                start_char_idx = i;
            } else {
                break;
            }
        }

        let mut end_char_idx = char_idx + 1;
        for i in (char_idx + 1)..char_count {
            if is_word_char(rope.char(i)) {
                end_char_idx = i + 1;
            } else {
                break;
            }
        }

        let start_byte = rope.char_to_byte(start_char_idx);
        let end_byte = rope.char_to_byte(end_char_idx);
        Self::new(start_byte, end_byte)
    }

    pub fn select_line_at(offset: usize, buffer: &Buffer) -> Self {
        let line = buffer.byte_to_line(offset);
        let line_range = buffer.line_byte_range(line);
        Self::new(line_range.start, line_range.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cursor movement tests are in editor/mod.rs using the | cursor style.
    // These tests cover Selection data structure behavior.

    #[test]
    fn test_selection_range() {
        let sel = Selection::new(5, 10);
        assert_eq!(sel.range(), 5..10);

        // Reversed selection
        let sel_rev = Selection::new(10, 5);
        assert_eq!(sel_rev.range(), 5..10);
    }

    #[test]
    fn test_selection_is_collapsed() {
        let sel = Selection::new(5, 5);
        assert!(sel.is_collapsed());

        let sel2 = Selection::new(5, 10);
        assert!(!sel2.is_collapsed());
    }

    #[test]
    fn test_selection_extend() {
        let sel = Selection::new(5, 10);
        let extended = sel.extend_to(15);
        assert_eq!(extended.anchor, 5);
        assert_eq!(extended.head, 15);
    }

    #[test]
    fn move_horizontal_steps_whole_grapheme_clusters() {
        let family = "👨‍👩‍👧‍👦"; // 7 codepoints, one grapheme
        let combining = "e\u{301}"; // 'e' + combining acute, one grapheme
        let text = format!("{family}{combining}x\n"); // graphemes: family, é, x
        let buf: Buffer = text.parse().unwrap();
        let fam = family.len();
        let comb = combining.len();

        // Right crosses each grapheme whole.
        let a = Cursor::new(0).move_right(&buf);
        assert_eq!(a.offset, fam, "family emoji crossed in one step");
        let b = a.move_right(&buf);
        assert_eq!(b.offset, fam + comb, "combining accent crossed whole");
        let c = b.move_right(&buf);
        assert_eq!(c.offset, fam + comb + 1, "then the ascii char");

        // Left mirrors it exactly.
        assert_eq!(c.move_left(&buf).offset, fam + comb);
        assert_eq!(b.move_left(&buf).offset, fam);
        assert_eq!(a.move_left(&buf).offset, 0);
    }

    #[test]
    fn vertical_movement_preserves_grapheme_column() {
        // Line 0: a wide ZWJ family emoji then 'a' → the cursor after 'a' is at grapheme
        // column 2. Char-column math would say column 8 (7 codepoints + 'a') and clamp to
        // the end of "xyz"; grapheme columns correctly land after "xy".
        let family = "👨‍👩‍👧‍👦";
        let text = format!("{family}a\nxyz\n");
        let buf: Buffer = text.parse().unwrap();
        let after_a = family.len() + 1;
        let line1 = text.find("xyz").unwrap();

        let down = Cursor::new(after_a).move_down(&buf);
        assert_eq!(down.offset, line1 + 2, "down lands at grapheme column 2");
        // And back up returns to just after the emoji cluster (column 2 on line 0).
        assert_eq!(down.move_up(&buf).offset, after_a);
    }

    #[test]
    fn grapheme_boundaries_cross_newlines_by_one() {
        // `\n` is its own grapheme: stepping over a line boundary moves exactly one byte.
        let buf: Buffer = "aé\nb".parse().unwrap();
        let nl = "aé".len(); // byte offset of '\n'
        assert_eq!(
            next_grapheme_boundary(&buf, nl),
            nl + 1,
            "step over newline"
        );
        assert_eq!(
            prev_grapheme_boundary(&buf, nl + 1),
            nl,
            "step back over newline"
        );
        // 'é' here is a single 2-byte codepoint; stepping still lands on its boundary.
        assert_eq!(
            next_grapheme_boundary(&buf, 1),
            nl,
            "cross é to end of line"
        );
        assert_eq!(prev_grapheme_boundary(&buf, nl), 1, "back over é");
    }

    #[test]
    fn test_move_vertical_multibyte_lands_on_boundary() {
        // Second line starts with a 2-byte char; a byte-column carry would land
        // mid-codepoint (byte offset 6). All offsets must sit on char boundaries.
        let buf: Buffer = "abcd\néfgh\nijkl".parse().unwrap();
        let rope = buf.rope();
        let on_boundary = |o: usize| rope.char_to_byte(rope.byte_to_char(o)) == o;

        // From line 0 column 1 (byte 1, after 'a') moving down onto the "é" line.
        // Column 1 on "éfgh" is just after "é" (2 bytes): byte offset 5 + 2 = 7.
        let down = Cursor::new(1).move_down(&buf);
        assert!(on_boundary(down.offset));
        assert_eq!(down.offset, 7);

        // Moving back up preserves the char column (col 1 on "abcd").
        let up = down.move_up(&buf);
        assert!(on_boundary(up.offset));
        assert_eq!(up.offset, 1);

        // `down` is at column 1 ("f"); moving down again lands on column 1 of
        // "ijkl" = "j" at byte offset 12.
        let down2 = down.move_down(&buf);
        assert!(on_boundary(down2.offset));
        assert_eq!(down2.offset, 12);
    }

    #[test]
    fn test_move_vertical_clamps_to_shorter_line() {
        // Moving onto a shorter line clamps to that line's end.
        let buf: Buffer = "abcdef\nxy\nghijkl".parse().unwrap();
        // Column 5 on line 0 → clamp to end of "xy" (line start 7 + 2 chars = 9).
        let down = Cursor::new(5).move_down(&buf);
        assert_eq!(down.offset, 9);
    }

    #[test]
    fn test_selection_select_all() {
        let buf: Buffer = "hello world".parse().unwrap();
        let sel = Selection::select_all(&buf);
        assert_eq!(sel.anchor, 0);
        assert_eq!(sel.head, 11);
    }

    #[test]
    fn test_selection_select_word_at() {
        let buf: Buffer = "hello world test".parse().unwrap();

        // Click in middle of "hello"
        let sel = Selection::select_word_at(2, &buf);
        assert_eq!(sel.range(), 0..5); // "hello"

        // Click in middle of "world"
        let sel = Selection::select_word_at(8, &buf);
        assert_eq!(sel.range(), 6..11); // "world"

        // Click on space (non-word char)
        let sel = Selection::select_word_at(5, &buf);
        assert_eq!(sel.range(), 5..6); // just the space
    }

    #[test]
    fn test_selection_select_line_at() {
        let buf: Buffer = "line one\nline two\nline three".parse().unwrap();

        // Click on first line (excludes newline)
        let sel = Selection::select_line_at(3, &buf);
        assert_eq!(sel.range(), 0..8); // "line one"

        // Click on second line (excludes newline)
        let sel = Selection::select_line_at(12, &buf);
        assert_eq!(sel.range(), 9..17); // "line two"

        // Click on last line (no trailing newline)
        let sel = Selection::select_line_at(22, &buf);
        assert_eq!(sel.range(), 18..28); // "line three"
    }
}
