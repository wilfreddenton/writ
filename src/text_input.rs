//! A reusable, headless single-line text-input core. No winit, no drawing — pure
//! text/caret/selection state. Adapters (find bar, filter box, goto-line prompt)
//! layer input handling and rendering on top of this.
//!
//! Movement steps whole **chars** (Unicode scalar values), not grapheme clusters:
//! the grapheme helpers in `cursor.rs` are rope/`Buffer`-bound and don't apply to
//! the plain `String` here. Char boundaries still never split a multibyte codepoint,
//! which is sufficient for v1; grapheme-cluster movement is a possible refinement.

use std::ops::Range;

pub struct TextField {
    text: String,
    caret: usize,          // byte offset into `text`, always on a char boundary
    anchor: Option<usize>, // selection anchor byte offset; None = collapsed
}

impl TextField {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            caret: 0,
            anchor: None,
        }
    }

    pub fn with_text(s: &str) -> Self {
        Self {
            text: s.to_string(),
            caret: s.len(),
            anchor: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    pub fn selected_range(&self) -> Option<Range<usize>> {
        match self.anchor {
            Some(a) if a != self.caret => Some(a.min(self.caret)..a.max(self.caret)),
            _ => None,
        }
    }

    pub fn set_text(&mut self, s: &str) {
        self.text.clear();
        self.text.push_str(s);
        self.caret = self.text.len();
        self.anchor = None;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
        self.anchor = None;
    }

    pub fn insert(&mut self, s: &str) {
        self.delete_selection();
        self.text.insert_str(self.caret, s);
        self.caret += s.len();
        self.anchor = None;
    }

    pub fn delete_selection(&mut self) -> bool {
        match self.selected_range() {
            Some(range) => {
                self.text.replace_range(range.clone(), "");
                self.caret = range.start;
                self.anchor = None;
                true
            }
            None => false,
        }
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.caret == 0 {
            return;
        }
        let prev = self.prev_char_boundary(self.caret);
        self.text.replace_range(prev..self.caret, "");
        self.caret = prev;
        self.anchor = None;
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.caret >= self.text.len() {
            return;
        }
        let next = self.next_char_boundary(self.caret);
        self.text.replace_range(self.caret..next, "");
        self.anchor = None;
    }

    pub fn move_left(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
            self.caret = self.prev_char_boundary(self.caret);
        } else if let Some(range) = self.selected_range() {
            self.caret = range.start;
            self.anchor = None;
        } else {
            self.caret = self.prev_char_boundary(self.caret);
            self.anchor = None;
        }
    }

    pub fn move_right(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
            self.caret = self.next_char_boundary(self.caret);
        } else if let Some(range) = self.selected_range() {
            self.caret = range.end;
            self.anchor = None;
        } else {
            self.caret = self.next_char_boundary(self.caret);
            self.anchor = None;
        }
    }

    pub fn home(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
        } else {
            self.anchor = None;
        }
        self.caret = 0;
    }

    pub fn end(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
        } else {
            self.anchor = None;
        }
        self.caret = self.text.len();
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.text.len();
    }

    fn ensure_anchor(&mut self) {
        if self.anchor.is_none() {
            self.anchor = Some(self.caret);
        }
    }

    fn prev_char_boundary(&self, offset: usize) -> usize {
        if offset == 0 {
            return 0;
        }
        let mut i = offset - 1;
        while !self.text.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    fn next_char_boundary(&self, offset: usize) -> usize {
        let len = self.text.len();
        if offset >= len {
            return len;
        }
        let mut i = offset + 1;
        while i < len && !self.text.is_char_boundary(i) {
            i += 1;
        }
        i
    }
}

impl Default for TextField {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_into_empty_advances_caret() {
        let mut f = TextField::new();
        f.insert("hello");
        assert_eq!(f.text(), "hello");
        assert_eq!(f.caret(), 5);
    }

    #[test]
    fn insert_at_caret_splits() {
        let mut f = TextField::with_text("hd");
        f.move_left(false); // caret before 'd'
        f.insert("ello worl");
        assert_eq!(f.text(), "hello world");
        assert_eq!(f.caret(), "hello worl".len());
    }

    #[test]
    fn backspace_at_zero_is_noop() {
        let mut f = TextField::with_text("abc");
        f.home(false);
        f.backspace();
        assert_eq!(f.text(), "abc");
        assert_eq!(f.caret(), 0);
    }

    #[test]
    fn backspace_removes_previous_char() {
        let mut f = TextField::with_text("abc");
        f.backspace();
        assert_eq!(f.text(), "ab");
        assert_eq!(f.caret(), 2);
    }

    #[test]
    fn delete_forward_at_end_is_noop() {
        let mut f = TextField::with_text("abc");
        f.delete_forward();
        assert_eq!(f.text(), "abc");
        assert_eq!(f.caret(), 3);
    }

    #[test]
    fn delete_forward_removes_next_char() {
        let mut f = TextField::with_text("abc");
        f.home(false);
        f.delete_forward();
        assert_eq!(f.text(), "bc");
        assert_eq!(f.caret(), 0);
    }

    #[test]
    fn move_clamps_at_bounds_and_boundaries() {
        let mut f = TextField::with_text("ab");
        f.home(false);
        f.move_left(false); // clamp at 0
        assert_eq!(f.caret(), 0);
        f.end(false);
        f.move_right(false); // clamp at len
        assert_eq!(f.caret(), 2);
        assert!(f.text().is_char_boundary(f.caret()));
    }

    #[test]
    fn home_end_with_extend_build_selection() {
        let mut f = TextField::with_text("hello");
        f.home(true);
        assert_eq!(f.selected_range(), Some(0..5));
        f.end(true);
        assert_eq!(f.selected_range(), None); // collapsed back onto caret==anchor
    }

    #[test]
    fn home_end_plain_move() {
        let mut f = TextField::with_text("hello");
        f.home(false);
        assert_eq!(f.caret(), 0);
        assert_eq!(f.selected_range(), None);
        f.end(false);
        assert_eq!(f.caret(), 5);
    }

    #[test]
    fn select_all_then_insert_replaces() {
        let mut f = TextField::with_text("hello world");
        f.select_all();
        assert_eq!(f.selected_range(), Some(0..11));
        f.insert("x");
        assert_eq!(f.text(), "x");
        assert_eq!(f.caret(), 1);
        assert_eq!(f.selected_range(), None);
    }

    #[test]
    fn delete_selection_returns_whether_deleted() {
        let mut f = TextField::with_text("abcdef");
        f.home(false);
        f.move_right(true);
        f.move_right(true);
        f.move_right(true);
        assert_eq!(f.selected_range(), Some(0..3));
        assert!(f.delete_selection());
        assert_eq!(f.text(), "def");
        assert_eq!(f.caret(), 0);
        // Collapsed now: no-op, returns false.
        assert!(!f.delete_selection());
    }

    #[test]
    fn multibyte_movement_steps_whole_chars() {
        // "áé😀": á and é are 2 bytes each, 😀 is 4 bytes.
        let mut f = TextField::with_text("áé😀");
        f.home(false);
        assert!(f.text().is_char_boundary(f.caret()));

        f.move_right(false);
        assert!(f.text().is_char_boundary(f.caret()));
        assert_eq!(f.caret(), 2); // past á

        f.move_right(false);
        assert!(f.text().is_char_boundary(f.caret()));
        assert_eq!(f.caret(), 4); // past é

        f.move_right(false);
        assert!(f.text().is_char_boundary(f.caret()));
        assert_eq!(f.caret(), 8); // past 😀

        f.move_left(false);
        assert!(f.text().is_char_boundary(f.caret()));
        assert_eq!(f.caret(), 4);
    }

    #[test]
    fn backspace_removes_whole_multibyte_char() {
        let mut f = TextField::with_text("áé😀");
        f.backspace(); // remove 😀 (4 bytes)
        assert_eq!(f.text(), "áé");
        assert_eq!(f.caret(), 4);
        assert!(f.text().is_char_boundary(f.caret()));
        f.backspace(); // remove é (2 bytes)
        assert_eq!(f.text(), "á");
        assert_eq!(f.caret(), 2);
        assert!(f.text().is_char_boundary(f.caret()));
    }

    #[test]
    fn shift_extend_then_collapse() {
        let mut f = TextField::with_text("hello");
        f.home(false);
        f.move_right(true);
        f.move_right(true);
        assert_eq!(f.selected_range(), Some(0..2)); // "he"
        // Non-extend move collapses to the selection edge (right → end).
        f.move_right(false);
        assert_eq!(f.selected_range(), None);
        assert_eq!(f.caret(), 2);
    }

    #[test]
    fn non_extend_left_collapses_to_start() {
        let mut f = TextField::with_text("hello");
        f.home(false);
        f.move_right(true);
        f.move_right(true);
        f.move_right(true);
        assert_eq!(f.selected_range(), Some(0..3));
        f.move_left(false); // collapse to start, not one-left of caret
        assert_eq!(f.selected_range(), None);
        assert_eq!(f.caret(), 0);
    }
}
