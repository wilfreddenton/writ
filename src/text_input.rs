//! A reusable, headless single-line text-input core. No winit, no drawing — pure
//! text/caret/selection state. Adapters (find bar, filter box, goto-line prompt)
//! layer input handling and rendering on top of this.
//!
//! Movement steps whole **chars** (Unicode scalar values), not grapheme clusters:
//! the grapheme helpers in `cursor.rs` are rope/`Buffer`-bound and don't apply to
//! the plain `String` here. Char boundaries still never split a multibyte codepoint,
//! which is sufficient for v1; grapheme-cluster movement is a possible refinement.

use std::ops::Range;

use parley::{Affinity, Cursor};
use vello::Scene;
use vello::kurbo::{Affine, Rect};
use vello::peniko::Fill;
use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::consts::{CARET_WIDTH, UI_LINE_HEIGHT};
use crate::editor::EditorTheme;
use crate::text_engine::{StyleRun, TextEngine, display_range_selection, peniko_color};

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

/// Font size (logical px) for text-field content. Matches the chrome bars' tone.
const FIELD_FONT_SIZE: f32 = 15.0;

/// Render a `TextField` inside `rect` (device px): its text, selection highlight,
/// and — when `focused` — a caret. Left-aligned within a small inset; horizontal
/// scroll-within-field for long text is a v2 concern.
pub fn draw_text_field(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    field: &TextField,
    rect: &Rect,
    scale: f32,
    focused: bool,
) {
    // A small inset inside the field — NOT the document's large PADDING, which pushed
    // the text/caret far from the field's left edge.
    let pad = (4.0 * scale) as f64;
    let layout = engine.build_line(
        field.text(),
        scale,
        FIELD_FONT_SIZE,
        UI_LINE_HEIGHT,
        peniko_color(theme.foreground),
        None,
        &[] as &[StyleRun],
    );
    let origin_x = rect.x0 + pad;
    let origin_y = rect.y0 + (rect.height() - layout.height() as f64) / 2.0;

    if let Some(range) = field.selected_range() {
        let sel = display_range_selection(&layout, range);
        for (bb, _) in sel.geometry(&layout) {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                peniko_color(theme.selection),
                None,
                &Rect::new(
                    bb.x0 + origin_x,
                    bb.y0 + origin_y,
                    bb.x1 + origin_x,
                    bb.y1 + origin_y,
                ),
            );
        }
    }

    engine.draw_line(scene, &layout, (origin_x as f32, origin_y as f32));

    if focused {
        let cursor = Cursor::from_byte_index(&layout, field.caret(), Affinity::Downstream);
        let bb = cursor.geometry(&layout, CARET_WIDTH * scale);
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            peniko_color(theme.foreground),
            None,
            &Rect::new(
                bb.x0 + origin_x,
                bb.y0 + origin_y,
                bb.x1 + origin_x,
                bb.y1 + origin_y,
            ),
        );
    }
}

/// Map a winit key event to a `TextField` mutation, returning whether it was
/// consumed. This is the winit boundary, kept out of the tested core. Enter, Tab,
/// and Escape are intentionally *not* handled here — the shell owns those (submit /
/// dismiss). Ctrl+V (paste) is also the shell's job, since it holds the clipboard.
pub fn apply_key(field: &mut TextField, event: &KeyEvent, mods: ModifiersState) -> bool {
    let shift = mods.shift_key();
    let ctrl = mods.control_key() || mods.super_key();
    match &event.logical_key {
        Key::Named(NamedKey::Backspace) => {
            field.backspace();
            true
        }
        Key::Named(NamedKey::Delete) => {
            field.delete_forward();
            true
        }
        Key::Named(NamedKey::ArrowLeft) => {
            field.move_left(shift);
            true
        }
        Key::Named(NamedKey::ArrowRight) => {
            field.move_right(shift);
            true
        }
        Key::Named(NamedKey::Home) => {
            field.home(shift);
            true
        }
        Key::Named(NamedKey::End) => {
            field.end(shift);
            true
        }
        Key::Character(c) if ctrl && c.as_str().eq_ignore_ascii_case("a") => {
            field.select_all();
            true
        }
        _ => {
            // Insert typed text, but never control chars (Enter/Tab arrive as "\r"/"\t"
            // in `event.text`) and never while a modifier that isn't Shift is held (so
            // Ctrl+<key> chords don't leak literal characters into the field).
            if ctrl {
                return false;
            }
            match event.text.as_ref() {
                Some(t) if !t.is_empty() && !t.chars().any(|ch| ch.is_control()) => {
                    field.insert(t);
                    true
                }
                _ => false,
            }
        }
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
