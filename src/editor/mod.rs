mod action;
mod config;
mod theme;

pub use action::{Direction, DispatchEditorAction, EditorAction};
pub use config::EditorConfig;
pub use theme::EditorTheme;

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

/// Counter for generating unique editor instance IDs.
static NEXT_EDITOR_ID: AtomicUsize = AtomicUsize::new(0);

use gpui::{
    Anchor, AnyElement, App, Context, CursorStyle, DragMoveEvent, Empty, FocusHandle, Focusable,
    Hsla, IntoElement, KeyDownEvent, ListAlignment, ListState, ModifiersChangedEvent, MouseButton,
    ReadGlobal, Render, Rgba, TextRun, Window, anchored, div, font, list, point, prelude::*, px,
};

/// Marker type for text selection drag operations.
/// Used with GPUI's on_drag/on_drag_move to receive mouse events outside element bounds.
struct SelectionDrag;

/// Empty view for the drag ghost (we don't need a visible drag indicator).
struct EmptyDragView;

impl Render for EmptyDragView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

use crate::line::CursorScreenPosition;
use crate::marker::{LineMarkers, MarkerKind, OrderedMarker, UnorderedMarker};
use crate::status_bar::StatusBarInfo;
use crate::title_bar::FileInfo;

use crate::buffer::{Buffer, RenderSnapshot};
use crate::cursor::{Cursor, Selection};
use crate::diff::DiffState;
use crate::git::head_blob_text;
use crate::github::{GitHubClient, GitHubValidationCache, IssueOrPr, IssueStatus};
use crate::inline::{
    GitHubContext, GitHubRef, NakedUrl, RawGitHubMatch, StyledRegion,
    detect_github_references_in_line, detect_naked_urls, github_refs_to_styled_regions,
    naked_urls_to_styled_regions,
};
use crate::line::{Line, LineTheme};
use crate::paste::{PasteContext, transform_paste};

/// Context about the line at the cursor, used by smart editing actions.
pub struct LineContext {
    /// Current cursor byte offset.
    pub cursor_offset: usize,
    /// Index of the current line.
    pub line_idx: usize,
    /// The current line's markers.
    pub line: LineMarkers,
    /// Whether content after markers is empty (whitespace only).
    pub is_empty: bool,
    /// Whether this line has any container markers.
    pub has_container: bool,
    /// The previous line, if any.
    pub prev_line: Option<LineMarkers>,
}

/// Cached tab cycle states for a specific line.
#[derive(Clone, Default)]
struct TabCycleCache {
    /// The line index this cache is for.
    line_idx: usize,
    /// The cached cycle states.
    states: Vec<String>,
}

/// The type of autocomplete trigger.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AutocompleteTrigger {
    /// Issue/PR autocomplete triggered by `#`.
    Issue,
    /// User autocomplete triggered by `@`.
    User,
}

/// Create a RenderSnapshot from issue/PR title text for markdown rendering.
fn render_snapshot_for_title(title: &str) -> RenderSnapshot {
    let mut buffer: Buffer = title.parse().unwrap_or_default();
    buffer.render_snapshot()
}

/// A suggestion from GitHub autocomplete.
#[derive(Clone)]
pub enum AutocompleteSuggestion {
    /// An issue or pull request.
    IssueOrPr {
        number: u64,
        /// Unicode symbol (● for issue, ⎇ for PR)
        symbol: String,
        /// Status for coloring
        status: IssueStatus,
        /// Cached render snapshot for the title (markdown-rendered).
        display_snapshot: RenderSnapshot,
    },
    /// A GitHub user.
    User { login: String, name: Option<String> },
}

/// State for the autocomplete popup.
#[derive(Clone)]
pub struct AutocompleteState {
    /// The type of autocomplete (Issue or User).
    pub trigger: AutocompleteTrigger,
    /// Byte offset where the trigger character (`#` or `@`) was typed.
    pub trigger_offset: usize,
    /// The prefix typed after the trigger (e.g., "123" for `#123`).
    pub prefix: String,
    /// Suggestions fetched from GitHub.
    pub suggestions: Vec<AutocompleteSuggestion>,
    /// Currently selected suggestion index.
    pub selected_index: usize,
    /// Whether we're currently fetching suggestions.
    pub loading: bool,
    /// The prefix we last fetched for (to avoid duplicate fetches).
    pub fetched_prefix: Option<String>,
}

/// Core editing state that can be used without GPUI context.
/// This contains the buffer and selection, and all editing logic.
pub struct EditorState {
    pub buffer: Buffer,
    pub selection: Selection,
    /// Cached tab cycle states to avoid recalculating mid-cycle.
    tab_cycle_cache: Option<TabCycleCache>,
}

impl EditorState {
    pub fn new(content: &str) -> Self {
        let buffer: Buffer = content.parse().unwrap_or_default();
        Self {
            buffer,
            selection: Selection::new(0, 0),
            tab_cycle_cache: None,
        }
    }

    pub fn cursor(&self) -> Cursor {
        self.selection.cursor()
    }

    pub fn text(&self) -> String {
        self.buffer.text()
    }

    /// Set cursor position by byte offset.
    pub fn set_cursor(&mut self, offset: usize) {
        let offset = offset.min(self.buffer.len_bytes());
        self.selection = Selection::new(offset, offset);
    }

    /// Move cursor left by one character.
    pub fn move_left(&mut self) {
        let new_cursor = self.cursor().move_left(&self.buffer);
        self.selection = Selection::new(new_cursor.offset, new_cursor.offset);
    }

    /// Move cursor right by one character.
    pub fn move_right(&mut self) {
        let new_cursor = self.cursor().move_right(&self.buffer);
        self.selection = Selection::new(new_cursor.offset, new_cursor.offset);
    }

    /// Move cursor up by one line.
    pub fn move_up(&mut self) {
        let new_cursor = self.cursor().move_up(&self.buffer);
        self.selection = Selection::new(new_cursor.offset, new_cursor.offset);
    }

    /// Move cursor down by one line.
    pub fn move_down(&mut self) {
        let new_cursor = self.cursor().move_down(&self.buffer);
        self.selection = Selection::new(new_cursor.offset, new_cursor.offset);
    }

    /// Move cursor to start of current line.
    pub fn move_to_line_start(&mut self) {
        let new_cursor = self.cursor().move_to_line_start(&self.buffer);
        self.selection = Selection::new(new_cursor.offset, new_cursor.offset);
    }

    /// Move cursor to end of current line.
    pub fn move_to_line_end(&mut self) {
        let new_cursor = self.cursor().move_to_line_end(&self.buffer);
        self.selection = Selection::new(new_cursor.offset, new_cursor.offset);
    }

    /// Insert text at the current cursor position.
    pub fn insert_text(&mut self, text: &str) {
        // Clear tab cycle cache since content is changing
        self.tab_cycle_cache = None;
        let cursor_before = self.cursor().offset;
        let insert_pos = if !self.selection.is_collapsed() {
            let range = self.selection.range();
            self.buffer.delete(range.clone(), cursor_before);
            range.start
        } else {
            cursor_before
        };
        self.buffer.insert(insert_pos, text, insert_pos);
        let new_pos = insert_pos + text.len();
        self.selection = Selection::new(new_pos, new_pos);

        // After inserting, propagate checkbox state if this line has a checkbox.
        // This handles the case where tab cycling created an incomplete checkbox line
        // (e.g., "- [ ] ") and typing content makes it parseable by tree-sitter.
        self.propagate_checkbox_after_edit();
    }

    fn find_line_at(&self, byte_pos: usize) -> Option<(usize, LineMarkers)> {
        let idx = self.buffer.byte_to_line(byte_pos);
        if idx < self.buffer.line_count() {
            Some((idx, self.buffer.line_markers(idx)))
        } else {
            None
        }
    }

    /// Check if the cursor is inside a code block (between opening and closing fences,
    /// or after an opening fence with no closing fence yet).
    fn cursor_in_code_block(&self) -> bool {
        let Some(tree) = self.buffer.tree() else {
            return false;
        };

        let cursor_offset = self.cursor().offset;
        let root = tree.block_tree().root_node();

        // Find the deepest node at the cursor position and walk up looking for fenced_code_block
        let Some(node) = root.descendant_for_byte_range(cursor_offset, cursor_offset) else {
            return false;
        };

        let mut current = Some(node);
        while let Some(n) = current {
            if n.kind() == "fenced_code_block" {
                return true;
            }
            current = n.parent();
        }
        false
    }

    /// Check if a line has content after its markers.
    /// Lines with code fences are always considered to have content.
    fn line_has_content(&self, line: &LineMarkers) -> bool {
        if line.is_fence() {
            return true;
        }
        let content_start = line
            .marker_range()
            .map(|r| r.end)
            .unwrap_or(line.range.start);
        !self
            .buffer
            .slice_cow(content_start..line.range.end)
            .trim()
            .is_empty()
    }

    /// Get context about the line at the cursor.
    /// Returns None if the cursor is not on a valid line.
    fn line_context(&self) -> Option<LineContext> {
        let cursor_offset = self.cursor().offset;
        let line_idx = self.buffer.byte_to_line(cursor_offset);
        if line_idx >= self.buffer.line_count() {
            return None;
        }
        let line = self.buffer.line_markers(line_idx);

        let is_empty = !self.line_has_content(&line);
        let has_container = line.has_container();

        let prev_line = if line_idx > 0 {
            Some(self.buffer.line_markers(line_idx - 1))
        } else {
            None
        };

        Some(LineContext {
            cursor_offset,
            line_idx,
            line,
            is_empty,
            has_container,
            prev_line,
        })
    }

    /// Auto-insert space after `>` if it just became a blockquote marker.
    /// Returns true if a space was inserted.
    pub fn maybe_complete_blockquote_marker(&mut self) -> bool {
        let cursor_pos = self.cursor().offset;
        if cursor_pos == 0 {
            return false;
        }

        if self.buffer.byte_at(cursor_pos - 1) != Some(b'>') {
            return false;
        }

        if self.buffer.byte_at(cursor_pos) == Some(b' ') {
            return false;
        }

        let line_idx = self.buffer.byte_to_line(cursor_pos);
        if line_idx >= self.buffer.line_count() {
            return false;
        }
        let line = self.buffer.line_markers(line_idx);

        let has_blockquote = line
            .markers
            .iter()
            .any(|m| matches!(m.kind, MarkerKind::BlockQuote));

        if !has_blockquote {
            return false;
        }

        self.insert_text(" ");
        true
    }

    /// After typing ` or ~, check if we just completed "```" or "~~~" at line start
    /// and auto-insert the closing fence.
    pub fn maybe_complete_code_fence(&mut self) {
        let cursor_pos = self.cursor().offset;
        if cursor_pos < 3 {
            return;
        }

        // Check we just typed 3 of the same fence character
        let fence_char = self.buffer.byte_at(cursor_pos - 1);
        if fence_char != Some(b'`') && fence_char != Some(b'~') {
            return;
        }
        if self.buffer.byte_at(cursor_pos - 2) != fence_char
            || self.buffer.byte_at(cursor_pos - 3) != fence_char
        {
            return;
        }

        // Check this is at the start of a line (possibly after blockquote markers)
        let line_idx = self.buffer.byte_to_line(cursor_pos);
        let line_start = self.buffer.line_to_byte(line_idx);
        let before_fence = self.buffer.slice_cow(line_start..(cursor_pos - 3));
        let trimmed = before_fence.trim();

        // Allow only whitespace or blockquote markers before the fence
        if !trimmed.is_empty() && !trimmed.chars().all(|c| c == '>') {
            return;
        }

        // Insert newline + closing fence, cursor stays after opening fence
        let closing = if fence_char == Some(b'`') {
            "\n```"
        } else {
            "\n~~~"
        };
        self.buffer.insert(cursor_pos, closing, cursor_pos);
    }

    /// Try to insert a space. Returns false if space should be ignored
    /// (at line start, or at blockquote content start outside code blocks).
    pub fn try_insert_space(&mut self) -> bool {
        if self.cursor_in_code_block() {
            self.insert_text(" ");
            return true;
        }

        let cursor = self.cursor();
        let line_start = cursor.move_to_line_start(&self.buffer).offset;

        if cursor.offset == line_start || self.cursor_at_blockquote_content_start() {
            return false;
        }

        self.insert_text(" ");
        true
    }

    /// Check if cursor is at the content start of a blockquote-only line.
    /// Used to prevent inserting spaces/tabs at the "beginning" of blockquote content.
    fn cursor_at_blockquote_content_start(&self) -> bool {
        let cursor_pos = self.cursor().offset;
        let line_idx = self.buffer.byte_to_line(cursor_pos);
        if line_idx >= self.buffer.line_count() {
            return false;
        }
        let line = self.buffer.line_markers(line_idx);

        if !line.is_blockquote_only() {
            return false;
        }

        if let Some(marker_range) = line.marker_range() {
            cursor_pos == marker_range.end
        } else {
            false
        }
    }

    /// Tab: cycle forward through nesting states based on tree-sitter context.
    pub fn tab(&mut self) {
        let Some((states, current_idx, prefix_end)) = self.get_tab_cycle_state() else {
            return;
        };

        if states.len() <= 1 {
            return;
        }

        let next_idx = (current_idx + 1) % states.len();
        self.set_line_prefix(&states[next_idx], prefix_end);

        // After changing structure, propagate checkbox state if this line has a checkbox
        self.propagate_checkbox_after_edit();
    }

    /// Shift+Tab: cycle backward through nesting states.
    fn shift_tab_cycle(&mut self) {
        let Some((states, current_idx, prefix_end)) = self.get_tab_cycle_state() else {
            return;
        };

        if states.len() <= 1 {
            return;
        }

        let prev_idx = if current_idx == 0 {
            states.len() - 1
        } else {
            current_idx - 1
        };
        self.set_line_prefix(&states[prev_idx], prefix_end);

        // After changing structure, propagate checkbox state if this line has a checkbox
        self.propagate_checkbox_after_edit();
    }

    /// Get tab cycle states, using cache if available for current line.
    /// Returns (states, current_idx, prefix_end) where prefix_end is where the prefix ends.
    fn get_tab_cycle_state(&mut self) -> Option<(Vec<String>, usize, usize)> {
        let cursor_offset = self.cursor().offset;
        let line_idx = self.buffer.byte_to_line(cursor_offset);
        let line_start = self.buffer.line_to_byte(line_idx);

        // Get current line's checkbox state to pass to state builder
        let current_checkbox = self.buffer.line_markers(line_idx).checkbox();

        // Check if we have a valid cache for this line
        let states = if let Some(ref cache) = self.tab_cycle_cache {
            if cache.line_idx == line_idx {
                cache.states.clone()
            } else {
                // Different line, recalculate and cache
                let states = self.build_cycle_states_from_tree(cursor_offset, current_checkbox);
                self.tab_cycle_cache = Some(TabCycleCache {
                    line_idx,
                    states: states.clone(),
                });
                states
            }
        } else {
            // No cache, calculate and cache
            let states = self.build_cycle_states_from_tree(cursor_offset, current_checkbox);
            self.tab_cycle_cache = Some(TabCycleCache {
                line_idx,
                states: states.clone(),
            });
            states
        };

        if states.len() <= 1 {
            return None;
        }

        // Find which state matches the current line's prefix
        // We check if the line starts with each state (longest match wins)
        let line_end = self
            .buffer
            .line_to_byte(line_idx + 1)
            .min(self.buffer.len_bytes());
        let line_text = self.buffer.slice_cow(line_start..line_end);

        let mut best_match: Option<(usize, &str)> = None;
        for (idx, state) in states.iter().enumerate() {
            if line_text.starts_with(state)
                && (best_match.is_none() || state.len() > best_match.unwrap().1.len())
            {
                best_match = Some((idx, state));
            }
        }

        let (current_idx, prefix_end) = match best_match {
            Some((idx, state)) => (idx, line_start + state.len()),
            None => (0, line_start), // Default to empty prefix at index 0
        };

        Some((states, current_idx, prefix_end))
    }

    /// Build tab cycle states by walking up the tree-sitter parse tree.
    /// The cycle is determined by context ABOVE the current line, not by current line content.
    /// If `checkbox_state` is Some, task list markers will use that state instead of the parent's.
    pub fn build_cycle_states_from_tree(
        &self,
        cursor_offset: usize,
        checkbox_state: Option<bool>,
    ) -> Vec<String> {
        let Some(tree) = self.buffer.tree() else {
            return vec![String::new()];
        };

        let root = tree.block_tree().root_node();
        let cursor_line_idx = self.buffer.byte_to_line(cursor_offset);

        let line_start = self.buffer.line_to_byte(cursor_line_idx);
        let lookup_offset = if line_start > 0 { line_start - 1 } else { 0 };
        let node = root.descendant_for_byte_range(lookup_offset, lookup_offset);

        let Some(node) = node else {
            return vec![String::new()];
        };

        let context_node = if self.is_in_error_node(node) {
            self.find_context_from_error(node).unwrap_or(node)
        } else {
            node
        };

        let mut nodes_to_process: Vec<tree_sitter::Node> = Vec::new();
        let mut blockquote_prefix = String::new();
        let mut current = Some(context_node);

        while let Some(n) = current {
            if n.kind() == "block_quote" {
                if let Some(marker_node) = n
                    .children(&mut n.walk())
                    .find(|c| c.kind() == "block_quote_marker")
                {
                    let marker_text = self
                        .buffer
                        .slice_cow(marker_node.start_byte()..marker_node.end_byte());
                    blockquote_prefix = format!("{}{}", marker_text, blockquote_prefix);
                }
            } else if n.kind() == "list_item" {
                nodes_to_process.push(n);
            }
            current = n.parent();
        }

        let mut list_levels: Vec<(usize, String, usize, bool)> = Vec::new();

        for n in nodes_to_process {
            let mut marker_text = String::new();
            let mut list_marker_len = 0;
            let mut marker_start = 0;
            let mut is_ordered = false;

            for child in n.children(&mut n.walk()) {
                match child.kind() {
                    "list_marker_minus" | "list_marker_plus" | "list_marker_star" => {
                        marker_start = child.start_byte();
                        let text = self.buffer.slice_cow(child.start_byte()..child.end_byte());
                        list_marker_len = text.len();
                        marker_text.push_str(&text);
                    }
                    "list_marker_dot" | "list_marker_parenthesis" => {
                        marker_start = child.start_byte();
                        let text = self.buffer.slice_cow(child.start_byte()..child.end_byte());
                        list_marker_len = text.len();
                        marker_text.push_str(&text);
                        is_ordered = true;
                    }
                    "task_list_marker_checked" | "task_list_marker_unchecked" => {
                        // Use the current line's checkbox state if provided.
                        // If None (line has no checkbox yet), default to unchecked.
                        let checkbox_text = match checkbox_state {
                            Some(true) => "[x]",
                            Some(false) | None => "[ ]",
                        };
                        marker_text.push_str(checkbox_text);
                        marker_text.push(' ');
                    }
                    _ => {}
                }
            }

            if !marker_text.is_empty() {
                let line_idx = self.buffer.byte_to_line(marker_start);
                let line_start = self.buffer.line_to_byte(line_idx);
                let absolute_indent = marker_start - line_start;
                let indent = absolute_indent.saturating_sub(blockquote_prefix.len());
                list_levels.push((indent, marker_text, list_marker_len, is_ordered));
            }
        }

        if list_levels.is_empty() && blockquote_prefix.is_empty() {
            return vec![String::new()];
        }

        list_levels.reverse();

        let mut states = Vec::new();

        if !blockquote_prefix.is_empty() {
            states.push(blockquote_prefix.clone());
        }

        for (indent, marker, list_marker_len, is_ordered) in &list_levels {
            let sibling_marker = if *is_ordered {
                Self::increment_ordered_marker(marker)
            } else {
                marker.clone()
            };
            states.push(format!(
                "{}{}{}",
                blockquote_prefix,
                " ".repeat(*indent),
                sibling_marker
            ));

            states.push(format!(
                "{}{}",
                blockquote_prefix,
                " ".repeat(indent + list_marker_len)
            ));
        }

        if let Some((deepest_indent, deepest_marker, list_marker_len, is_ordered)) =
            list_levels.last()
        {
            let deeper_indent = deepest_indent + list_marker_len;
            let nested_marker = if *is_ordered {
                Self::reset_ordered_marker(deepest_marker)
            } else {
                deepest_marker.clone()
            };
            states.push(format!(
                "{}{}{}",
                blockquote_prefix,
                " ".repeat(deeper_indent),
                nested_marker
            ));
        }

        states.push(String::new());
        states
    }

    fn increment_ordered_marker(marker: &str) -> String {
        let num_end = marker
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(marker.len());
        if num_end == 0 {
            return marker.to_string();
        }
        let num: usize = marker[..num_end].parse().unwrap_or(1);
        format!("{}{}", num + 1, &marker[num_end..])
    }

    fn reset_ordered_marker(marker: &str) -> String {
        let num_end = marker
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(marker.len());
        if num_end == 0 {
            return marker.to_string();
        }
        format!("1{}", &marker[num_end..])
    }

    fn is_in_error_node(&self, node: tree_sitter::Node) -> bool {
        let mut current = Some(node);
        while let Some(n) = current {
            if n.kind() == "ERROR" {
                return true;
            }
            current = n.parent();
        }
        false
    }

    fn find_context_from_error<'a>(
        &self,
        node: tree_sitter::Node<'a>,
    ) -> Option<tree_sitter::Node<'a>> {
        let mut current = Some(node);
        while let Some(n) = current {
            if n.kind() == "ERROR" {
                if let Some(prev) = n.prev_sibling() {
                    return self.find_last_list_item(prev);
                }
                return None;
            }
            current = n.parent();
        }
        None
    }

    fn find_last_list_item<'a>(
        &self,
        node: tree_sitter::Node<'a>,
    ) -> Option<tree_sitter::Node<'a>> {
        let mut result: Option<tree_sitter::Node<'a>> = None;
        if node.kind() == "list_item" {
            result = Some(node);
        }
        let child_count = node.child_count();
        for i in (0..child_count).rev() {
            if let Some(child) = node.child(i as u32)
                && let Some(found) = self.find_last_list_item(child)
            {
                return Some(found);
            }
        }
        result
    }

    /// Find the list_item node containing the given byte offset.
    fn find_list_item_node(&self, byte_offset: usize) -> Option<tree_sitter::Node<'_>> {
        let tree = self.buffer.tree()?;
        let root = tree.block_tree().root_node();
        let node = root.descendant_for_byte_range(byte_offset, byte_offset)?;

        let mut current = Some(node);
        while let Some(n) = current {
            if n.kind() == "list_item" {
                return Some(n);
            }
            current = n.parent();
        }
        None
    }

    /// Find all checkboxes nested within a list_item node.
    /// Returns Vec of (checkbox_byte_offset, is_checked).
    fn find_nested_checkboxes(&self, list_item_node: tree_sitter::Node) -> Vec<(usize, bool)> {
        let mut checkboxes = Vec::new();
        let mut cursor = list_item_node.walk();

        loop {
            let node = cursor.node();
            match node.kind() {
                "task_list_marker_checked" => {
                    checkboxes.push((node.start_byte(), true));
                }
                "task_list_marker_unchecked" => {
                    checkboxes.push((node.start_byte(), false));
                }
                _ => {}
            }

            if cursor.goto_first_child() {
                continue;
            }
            if cursor.goto_next_sibling() {
                continue;
            }
            loop {
                if !cursor.goto_parent() {
                    return checkboxes;
                }
                if cursor.node().id() == list_item_node.id() {
                    return checkboxes;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    /// Build full nested context markers by walking up the tree-sitter tree.
    /// Returns markers from outermost to innermost (e.g., `> - [x] - [ ]`).
    pub fn build_nested_context(&self, cursor_offset: usize) -> Vec<MarkerKind> {
        let Some(tree) = self.buffer.tree() else {
            return Vec::new();
        };

        let root = tree.block_tree().root_node();

        // Handle edge case: cursor at end of file
        let lookup_offset = if cursor_offset > 0
            && root
                .descendant_for_byte_range(cursor_offset, cursor_offset)
                .map(|n| n.kind() == "document")
                .unwrap_or(true)
        {
            cursor_offset - 1
        } else {
            cursor_offset
        };

        let Some(node) = root.descendant_for_byte_range(lookup_offset, lookup_offset) else {
            return Vec::new();
        };

        // Walk up from current node, collecting context from each relevant ancestor
        let mut markers_reversed = Vec::new();
        let mut current = Some(node);

        while let Some(n) = current {
            match n.kind() {
                "block_quote" => {
                    markers_reversed.push(MarkerKind::BlockQuote);
                }
                "list_item" => {
                    // Scan direct children for list marker and checkbox
                    // Collect in reverse order (checkbox then list_marker) because
                    // we reverse the whole list at the end, so we want: - [x]
                    let mut list_marker: Option<MarkerKind> = None;
                    let mut checkbox: Option<MarkerKind> = None;

                    let mut cursor = n.walk();
                    if cursor.goto_first_child() {
                        loop {
                            let child = cursor.node();
                            match child.kind() {
                                "task_list_marker_checked" => {
                                    checkbox = Some(MarkerKind::Checkbox { checked: true });
                                }
                                "task_list_marker_unchecked" => {
                                    checkbox = Some(MarkerKind::Checkbox { checked: false });
                                }
                                "list_marker_minus" => {
                                    list_marker = Some(MarkerKind::ListItem {
                                        ordered: false,
                                        unordered_marker: Some(UnorderedMarker::Minus),
                                        ordered_marker: None,
                                        number: None,
                                    });
                                }
                                "list_marker_star" => {
                                    list_marker = Some(MarkerKind::ListItem {
                                        ordered: false,
                                        unordered_marker: Some(UnorderedMarker::Star),
                                        ordered_marker: None,
                                        number: None,
                                    });
                                }
                                "list_marker_plus" => {
                                    list_marker = Some(MarkerKind::ListItem {
                                        ordered: false,
                                        unordered_marker: Some(UnorderedMarker::Plus),
                                        ordered_marker: None,
                                        number: None,
                                    });
                                }
                                "list_marker_dot" | "list_marker_parenthesis" => {
                                    // Extract the number from the marker text
                                    let marker_text =
                                        self.buffer.slice_cow(child.start_byte()..child.end_byte());
                                    let number = marker_text
                                        .trim()
                                        .chars()
                                        .take_while(|c| c.is_ascii_digit())
                                        .collect::<String>()
                                        .parse::<u32>()
                                        .ok();
                                    let ordered_marker =
                                        Some(if child.kind() == "list_marker_dot" {
                                            OrderedMarker::Dot
                                        } else {
                                            OrderedMarker::Parenthesis
                                        });
                                    list_marker = Some(MarkerKind::ListItem {
                                        ordered: true,
                                        unordered_marker: None,
                                        ordered_marker,
                                        number,
                                    });
                                }
                                _ => {}
                            }
                            if !cursor.goto_next_sibling() {
                                break;
                            }
                        }
                    }

                    // Add in reverse order: checkbox first, then list_marker
                    // After final reverse, this becomes: list_marker, checkbox (i.e., "- [x]")
                    if let Some(cb) = checkbox {
                        markers_reversed.push(cb);
                    }
                    if let Some(lm) = list_marker {
                        markers_reversed.push(lm);
                    }
                }
                "fenced_code_block" => {
                    // Find info_string for language
                    let mut cursor = n.walk();
                    let mut language = None;
                    if cursor.goto_first_child() {
                        loop {
                            let child = cursor.node();
                            if child.kind() == "info_string" {
                                language = Some(
                                    self.buffer
                                        .slice_cow(child.start_byte()..child.end_byte())
                                        .to_string(),
                                );
                                break;
                            }
                            if !cursor.goto_next_sibling() {
                                break;
                            }
                        }
                    }
                    markers_reversed.push(MarkerKind::CodeBlockFence {
                        language,
                        is_opening: true,
                    });
                }
                _ => {}
            }
            current = n.parent();
        }

        // Reverse to get outermost-to-innermost order
        markers_reversed.reverse();
        markers_reversed
    }

    /// Find the parent list_item's checkbox, if any.
    /// Returns (checkbox_byte_offset, is_checked).
    fn find_parent_checkbox(&self, list_item_start: usize) -> Option<(usize, bool)> {
        let tree = self.buffer.tree()?;
        let root = tree.block_tree().root_node();
        let node = root.descendant_for_byte_range(list_item_start, list_item_start)?;

        // Find our list_item first
        let mut current = Some(node);
        let mut our_list_item = None;
        while let Some(n) = current {
            if n.kind() == "list_item" {
                our_list_item = Some(n);
                break;
            }
            current = n.parent();
        }

        // Walk up to find parent list_item
        let our_list_item = our_list_item?;
        let mut current = our_list_item.parent();
        while let Some(n) = current {
            if n.kind() == "list_item" {
                // Found parent list_item, find its checkbox among direct children
                let mut cursor = n.walk();
                if cursor.goto_first_child() {
                    loop {
                        let child = cursor.node();
                        match child.kind() {
                            "task_list_marker_checked" => {
                                return Some((child.start_byte(), true));
                            }
                            "task_list_marker_unchecked" => {
                                return Some((child.start_byte(), false));
                            }
                            _ => {}
                        }
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
                return None;
            }
            current = n.parent();
        }
        None
    }

    /// Find all sibling checkboxes (same nesting level).
    /// Returns Vec of (checkbox_byte_offset, is_checked).
    fn find_sibling_checkboxes(&self, list_item_start: usize) -> Vec<(usize, bool)> {
        let tree = match self.buffer.tree() {
            Some(t) => t,
            None => return Vec::new(),
        };
        let root = tree.block_tree().root_node();
        let node = match root.descendant_for_byte_range(list_item_start, list_item_start) {
            Some(n) => n,
            None => return Vec::new(),
        };

        // Find our list_item
        let mut current = Some(node);
        let mut our_list_item = None;
        while let Some(n) = current {
            if n.kind() == "list_item" {
                our_list_item = Some(n);
                break;
            }
            current = n.parent();
        }

        let our_list_item = match our_list_item {
            Some(n) => n,
            None => return Vec::new(),
        };

        // Get parent list node
        let parent_list = match our_list_item.parent() {
            Some(p) if p.kind() == "list" => p,
            _ => return Vec::new(),
        };

        // Iterate all list_item children and collect their checkboxes
        let mut siblings = Vec::new();
        let mut cursor = parent_list.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "list_item" {
                    // Find checkbox in this list_item (direct child only)
                    let mut inner_cursor = child.walk();
                    if inner_cursor.goto_first_child() {
                        loop {
                            let inner_child = inner_cursor.node();
                            match inner_child.kind() {
                                "task_list_marker_checked" => {
                                    siblings.push((inner_child.start_byte(), true));
                                    break;
                                }
                                "task_list_marker_unchecked" => {
                                    siblings.push((inner_child.start_byte(), false));
                                    break;
                                }
                                _ => {}
                            }
                            if !inner_cursor.goto_next_sibling() {
                                break;
                            }
                        }
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        siblings
    }

    /// Set the line prefix, replacing current markers up to prefix_end.
    /// Preserves any content after prefix_end and adjusts cursor position.
    fn set_line_prefix(&mut self, new_prefix: &str, prefix_end: usize) {
        let cursor_offset = self.cursor().offset;
        let line_idx = self.buffer.byte_to_line(cursor_offset);
        let line_start = self.buffer.line_to_byte(line_idx);

        let old_prefix_len = prefix_end - line_start;
        let new_prefix_len = new_prefix.len();
        let len_diff = new_prefix_len as isize - old_prefix_len as isize;

        // Delete old prefix
        if prefix_end > line_start {
            self.buffer.delete(line_start..prefix_end, cursor_offset);
        }

        // Insert new prefix
        if !new_prefix.is_empty() {
            self.buffer.insert(line_start, new_prefix, line_start);
        }

        // Adjust cursor: if cursor was after prefix, shift by the length difference
        // If cursor was in the prefix area, move to end of new prefix
        let new_cursor = if cursor_offset >= prefix_end {
            (cursor_offset as isize + len_diff) as usize
        } else {
            line_start + new_prefix_len
        };
        self.selection = Selection::new(new_cursor, new_cursor);
    }

    /// Smart enter: creates paragraph break or exits container on empty line.
    /// Enter: just insert a raw newline. No magic.
    pub fn enter(&mut self) {
        self.insert_text("\n");
    }

    /// Shift+Enter: continue container (add markers from current line).
    /// In code blocks, copies leading whitespace for indentation.
    pub fn shift_enter(&mut self) {
        // In code blocks, copy leading whitespace from current line
        if self.cursor_in_code_block() {
            let indent = self.current_line_leading_whitespace();
            self.insert_text("\n");
            if !indent.is_empty() {
                self.insert_text(&indent);
            }
            return;
        }

        let Some(ctx) = self.line_context() else {
            self.insert_text("\n");
            return;
        };

        let continuation = ctx.line.continuation_rope(self.buffer.rope());
        self.insert_text("\n");
        if !continuation.is_empty() {
            self.insert_text(&continuation);
        }
    }

    /// Get leading whitespace (spaces/tabs) from the current line.
    fn current_line_leading_whitespace(&self) -> String {
        let cursor = self.cursor();
        let line_start = cursor.move_to_line_start(&self.buffer).offset;
        let line_end = cursor.move_to_line_end(&self.buffer).offset;
        let line_text = self.buffer.slice_cow(line_start..line_end);

        line_text
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect()
    }

    /// Shift+Alt+Enter: create indented continuation (for nested paragraphs).
    /// For lists: newline + indent (no list marker)
    /// For blockquotes alone: newline + indent (exits blockquote)
    /// For nested (e.g. `> - item`): newline + outer markers + indent
    pub fn shift_alt_enter(&mut self) {
        let indent = {
            let Some(ctx) = self.line_context() else {
                self.insert_text("\n");
                return;
            };

            let has_list = ctx
                .line
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::ListItem { .. }));
            let has_blockquote = ctx
                .line
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::BlockQuote));

            if has_blockquote && !has_list {
                "  ".to_string()
            } else {
                ctx.line.nested_paragraph_indent(self.buffer.rope())
            }
        };

        self.insert_text("\n");
        if !indent.is_empty() {
            self.insert_text(&indent);
        }
    }

    /// Shift+Tab: cycle backward through nesting states.
    pub fn shift_tab(&mut self) {
        self.shift_tab_cycle();
    }

    fn backspace_range_with_type(
        &self,
        cursor_pos: usize,
    ) -> Option<(std::ops::Range<usize>, bool)> {
        let (_, line) = self.find_line_at(cursor_pos)?;

        for marker in &line.markers {
            if cursor_pos == marker.range.end {
                let is_indent = matches!(marker.kind, MarkerKind::Indent);
                return Some((marker.range.clone(), is_indent));
            }
        }

        None
    }

    /// If cursor is at end of an opening code fence and the code block contains
    /// only whitespace, return the full block range to delete.
    fn find_empty_code_block_range(&self, cursor_pos: usize) -> Option<std::ops::Range<usize>> {
        let tree = self.buffer.tree()?;
        let root = tree.block_tree().root_node();

        // Find the node at cursor position (look slightly before since cursor is at end of fence)
        let node = root.descendant_for_byte_range(cursor_pos.saturating_sub(1), cursor_pos)?;

        // Walk up to find fenced_code_block
        let mut current = Some(node);
        let code_block = loop {
            match current {
                Some(n) if n.kind() == "fenced_code_block" => break n,
                Some(n) => current = n.parent(),
                None => return None,
            }
        };

        let block_start = code_block.start_byte();
        let block_end = code_block.end_byte();

        // Find where content starts (after first line / opening fence)
        let block_text = self.buffer.slice_cow(block_start..block_end);
        let first_newline = block_text.find('\n')?;
        let content_start = block_start + first_newline + 1;

        // Check if content (between opening fence and end) is only whitespace + closing fence
        let content = self.buffer.slice_cow(content_start..block_end);
        let trimmed = content.trim();

        if trimmed == "```" || trimmed == "~~~" {
            // Don't include trailing newline after closing fence
            let mut end = block_end;
            if self.buffer.byte_at(end.saturating_sub(1)) == Some(b'\n') {
                end -= 1;
            }
            Some(block_start..end)
        } else {
            None
        }
    }

    /// Delete backward (backspace). Simple: delete one unit.
    /// Markers and indents are atomic - deleted as a whole.
    pub fn delete_backward(&mut self) {
        // Clear tab cycle cache since content is changing
        self.tab_cycle_cache = None;
        if !self.selection.is_collapsed() {
            self.delete_selection();
            self.propagate_checkbox_after_edit();
            return;
        }

        if self.cursor().offset == 0 {
            return;
        }

        let cursor_pos = self.cursor().offset;

        if let Some((marker_range, _is_indent)) = self.backspace_range_with_type(cursor_pos) {
            // Check if we're deleting an opening code fence of an empty code block
            if let Some(block_range) = self.find_empty_code_block_range(cursor_pos) {
                // Delete the entire empty code block
                self.buffer.delete(block_range.clone(), cursor_pos);
                self.selection = Selection::new(block_range.start, block_range.start);
                self.propagate_checkbox_after_edit();
                return;
            }

            // Otherwise just delete the marker
            self.buffer.delete(marker_range.clone(), cursor_pos);
            self.selection = Selection::new(marker_range.start, marker_range.start);
            self.propagate_checkbox_after_edit();
            return;
        }

        let new_pos = cursor_pos - 1;
        self.buffer.delete(new_pos..cursor_pos, cursor_pos);
        self.selection = Selection::new(new_pos, new_pos);
        self.propagate_checkbox_after_edit();
    }

    fn delete_selection(&mut self) {
        let range = self.selection.range();
        let cursor_before = self.cursor().offset;
        self.buffer.delete(range.clone(), cursor_before);
        self.selection = Selection::new(range.start, range.start);
    }

    /// Delete the character after the cursor, or the selection if active.
    pub fn delete_forward(&mut self) {
        // Clear tab cycle cache since content is changing
        self.tab_cycle_cache = None;
        if !self.selection.is_collapsed() {
            self.delete_selection();
        } else if self.cursor().offset < self.buffer.len_bytes() {
            let cursor_before = self.cursor().offset;
            let next = self.cursor().move_right(&self.buffer);
            self.buffer
                .delete(cursor_before..next.offset, cursor_before);
        }
        self.propagate_checkbox_after_edit();
    }

    pub fn handle_click(&mut self, buffer_offset: usize, shift_held: bool, click_count: usize) {
        if shift_held {
            self.selection = self.selection.extend_to(buffer_offset);
        } else {
            match click_count {
                2 => {
                    self.selection = Selection::select_word_at(buffer_offset, &self.buffer);
                }
                3 => {
                    self.selection = Selection::select_line_at(buffer_offset, &self.buffer);
                }
                _ => {
                    self.selection = Selection::new(buffer_offset, buffer_offset);
                }
            }
        }
    }

    pub fn handle_drag(&mut self, buffer_offset: usize) {
        self.selection = self.selection.extend_to(buffer_offset);
    }

    /// Toggle a checkbox on the given line, propagating to children and parents.
    /// Byte offset of the checkbox marker (`[ ]` / `[x]` / `[X]`) within `line`,
    /// given its checked state. `None` if the pattern isn't present in the text.
    fn checkbox_byte_offset(&self, line: &LineMarkers, is_checked: bool) -> Option<usize> {
        let line_text = self.buffer.slice_cow(line.range.clone());
        let pattern = if is_checked { "[x]" } else { "[ ]" };
        // Checked boxes may use an uppercase X.
        let relative = line_text
            .find(pattern)
            .or_else(|| is_checked.then(|| line_text.find("[X]")).flatten())?;
        Some(line.range.start + relative)
    }

    pub fn toggle_checkbox_for_test(&mut self, line_number: usize) {
        let (is_checked, checkbox_byte_start) = {
            if line_number >= self.buffer.line_count() {
                return;
            }
            let line = self.buffer.line_markers(line_number);

            let Some(is_checked) = line.checkbox() else {
                return;
            };

            let Some(checkbox_byte_start) = self.checkbox_byte_offset(&line, is_checked) else {
                return;
            };
            (is_checked, checkbox_byte_start)
        };

        let new_checked = !is_checked;
        let mut cursor_pos = self.cursor().offset;

        // Find the list_item node for this checkbox - use checkbox_byte_start for accurate node finding
        let list_item_node = self.find_list_item_node(checkbox_byte_start);

        // Collect all checkboxes to toggle (clicked + nested children)
        let mut checkboxes_to_toggle: Vec<(usize, bool)> = Vec::new();

        if let Some(node) = list_item_node {
            // Get all nested checkboxes within this list_item
            let nested = self.find_nested_checkboxes(node);
            for (offset, currently_checked) in nested {
                // Only toggle if state differs from target
                if currently_checked != new_checked {
                    checkboxes_to_toggle.push((offset, currently_checked));
                }
            }
        } else {
            // No list_item found, just toggle the clicked checkbox
            checkboxes_to_toggle.push((checkbox_byte_start, is_checked));
        }

        // Sort by offset descending so we can modify without invalidating earlier offsets
        checkboxes_to_toggle.sort_by_key(|c| std::cmp::Reverse(c.0));

        // Toggle each checkbox
        for (offset, _currently_checked) in &checkboxes_to_toggle {
            let content_start = offset + 1; // skip '['
            let content_end = content_start + 1;
            let new_content = if new_checked { "x" } else { " " };
            self.buffer
                .replace(content_start..content_end, new_content, cursor_pos);
        }

        // Handle strikethrough for each toggled checkbox's line
        // Process in reverse order (highest offset first) since strikethrough changes byte offsets
        for (offset, _) in &checkboxes_to_toggle {
            let line_idx = self.buffer.byte_to_line(*offset);
            let adjustment = self.toggle_line_strikethrough(line_idx, new_checked, cursor_pos);
            cursor_pos = (cursor_pos as isize + adjustment) as usize;
        }

        // Propagate upward: if checking and all siblings are now checked, check parent
        // If unchecking, uncheck parent if it was checked
        self.propagate_checkbox_up(checkbox_byte_start, new_checked, &mut cursor_pos);

        self.selection = Selection::new(cursor_pos, cursor_pos);
    }

    /// Propagate checkbox state upward through parent list items.
    fn propagate_checkbox_up(
        &mut self,
        list_item_start: usize,
        checked: bool,
        cursor_pos: &mut usize,
    ) {
        // Find parent checkbox
        let parent_info = self.find_parent_checkbox(list_item_start);
        let Some((parent_offset, parent_checked)) = parent_info else {
            return;
        };

        if checked {
            // When checking: only auto-check parent if ALL siblings are now checked
            let siblings = self.find_sibling_checkboxes(list_item_start);
            let all_checked = siblings.iter().all(|(_, is_checked)| *is_checked);

            if all_checked && !parent_checked {
                // Check the parent
                let content_start = parent_offset + 1;
                let content_end = content_start + 1;
                self.buffer
                    .replace(content_start..content_end, "x", *cursor_pos);

                // Toggle strikethrough for parent's direct content line
                let parent_line = self.buffer.byte_to_line(parent_offset);
                let adjustment = self.toggle_line_strikethrough(parent_line, true, *cursor_pos);
                *cursor_pos = (*cursor_pos as isize + adjustment) as usize;

                // Recursively propagate up
                self.propagate_checkbox_up(parent_offset, true, cursor_pos);
            }
        } else {
            // When unchecking: uncheck parent if it was checked
            if parent_checked {
                let content_start = parent_offset + 1;
                let content_end = content_start + 1;
                self.buffer
                    .replace(content_start..content_end, " ", *cursor_pos);

                // Remove strikethrough from parent's direct content line
                let parent_line = self.buffer.byte_to_line(parent_offset);
                let adjustment = self.toggle_line_strikethrough(parent_line, false, *cursor_pos);
                *cursor_pos = (*cursor_pos as isize + adjustment) as usize;

                // Recursively propagate up
                self.propagate_checkbox_up(parent_offset, false, cursor_pos);
            }
        }
    }

    /// Propagate checkbox state after tab cycling changes the structure.
    /// Propagate checkbox state after editing (insert/delete).
    /// If current line has a checkbox, propagate from it.
    /// If not, check if we're inside a parent checkbox and re-evaluate it.
    fn propagate_checkbox_after_edit(&mut self) {
        let cursor_offset = self.cursor().offset;
        let line_idx = self.buffer.byte_to_line(cursor_offset);
        let markers = self.buffer.line_markers(line_idx);

        if let Some(is_checked) = markers.checkbox() {
            // Current line has a checkbox - propagate from it
            if let Some(checkbox_byte_start) = self.checkbox_byte_offset(&markers, is_checked) {
                let mut cursor_pos = cursor_offset;
                self.propagate_checkbox_up(checkbox_byte_start, is_checked, &mut cursor_pos);
                self.selection = Selection::new(cursor_pos, cursor_pos);
            }
        } else {
            // No checkbox on current line - maybe we deleted one.
            // Check if there's a parent checkbox that needs re-evaluation.
            self.propagate_from_parent_checkbox();
        }
    }

    /// When current line has no checkbox, find parent checkbox and re-evaluate it.
    fn propagate_from_parent_checkbox(&mut self) {
        let cursor_offset = self.cursor().offset;

        // Try to find a parent checkbox using tree-sitter.
        // If cursor is at end of file or outside a node, try one position back.
        let parent_info = self.find_parent_checkbox(cursor_offset).or_else(|| {
            if cursor_offset > 0 {
                self.find_parent_checkbox(cursor_offset - 1)
            } else {
                None
            }
        });

        let Some(parent_info) = parent_info else {
            return;
        };

        // Also need to find siblings from a valid position
        let sibling_offset =
            if self.find_sibling_checkboxes(cursor_offset).is_empty() && cursor_offset > 0 {
                cursor_offset - 1
            } else {
                cursor_offset
            };

        let (parent_checkbox_offset, parent_checked) = parent_info;

        // Find siblings using the adjusted offset
        let siblings = self.find_sibling_checkboxes(sibling_offset);

        // If no siblings with checkboxes, nothing to propagate
        if siblings.is_empty() {
            // No sibling checkboxes - if parent was checked, it should stay checked
            // (the deleted item wasn't affecting the parent's state)
            return;
        }

        let all_siblings_checked = siblings.iter().all(|(_, checked)| *checked);
        let mut cursor_pos = cursor_offset;

        if all_siblings_checked && !parent_checked {
            // All remaining siblings are checked, check the parent
            let content_start = parent_checkbox_offset + 1;
            let content_end = content_start + 1;
            self.buffer
                .replace(content_start..content_end, "x", cursor_pos);

            let parent_line = self.buffer.byte_to_line(parent_checkbox_offset);
            let adjustment = self.toggle_line_strikethrough(parent_line, true, cursor_pos);
            cursor_pos = (cursor_pos as isize + adjustment) as usize;

            self.propagate_checkbox_up(parent_checkbox_offset, true, &mut cursor_pos);
            self.selection = Selection::new(cursor_pos, cursor_pos);
        } else if !all_siblings_checked && parent_checked {
            // Some siblings unchecked, uncheck the parent
            let content_start = parent_checkbox_offset + 1;
            let content_end = content_start + 1;
            self.buffer
                .replace(content_start..content_end, " ", cursor_pos);

            let parent_line = self.buffer.byte_to_line(parent_checkbox_offset);
            let adjustment = self.toggle_line_strikethrough(parent_line, false, cursor_pos);
            cursor_pos = (cursor_pos as isize + adjustment) as usize;

            self.propagate_checkbox_up(parent_checkbox_offset, false, &mut cursor_pos);
            self.selection = Selection::new(cursor_pos, cursor_pos);
        }
    }

    /// Add or remove strikethrough (`~~`) from a line's content.
    fn toggle_line_strikethrough(
        &mut self,
        line_idx: usize,
        add_strikethrough: bool,
        cursor_pos: usize,
    ) -> isize {
        // Clear tab cycle cache since content is changing
        self.tab_cycle_cache = None;
        if line_idx >= self.buffer.line_count() {
            return 0;
        }
        let line = self.buffer.line_markers(line_idx);

        let content_start = line.content_start();
        let content_end = line.range.end;

        if content_start >= content_end {
            return 0;
        }

        let content = self.buffer.slice_cow(content_start..content_end);
        let trimmed = content.trim();

        if trimmed.is_empty() {
            return 0;
        }

        if add_strikethrough {
            if trimmed.starts_with("~~") && trimmed.ends_with("~~") {
                return 0;
            }

            let leading_ws = content.len() - content.trim_start().len();
            let trailing_ws = content.len() - content.trim_end().len();

            let text_start = content_start + leading_ws;
            let text_end = content_end - trailing_ws;

            self.buffer.insert(text_end, "~~", cursor_pos);
            self.buffer.insert(text_start, "~~", cursor_pos);

            let mut adjustment: isize = 0;
            if cursor_pos > text_start {
                adjustment += 2;
            }
            if cursor_pos > text_end {
                adjustment += 2;
            }
            adjustment
        } else {
            let leading_ws = content.len() - content.trim_start().len();
            let text_start = content_start + leading_ws;

            if trimmed.starts_with("~~") && trimmed.ends_with("~~") && trimmed.len() >= 4 {
                let trailing_ws = content.len() - content.trim_end().len();
                let text_end = content_end - trailing_ws;

                self.buffer.delete((text_end - 2)..text_end, cursor_pos);
                self.buffer.delete(text_start..(text_start + 2), cursor_pos);

                let mut adjustment: isize = 0;
                if cursor_pos > text_start + 2 {
                    adjustment -= 2;
                }
                if cursor_pos > text_end {
                    adjustment -= 2;
                }
                adjustment
            } else {
                0
            }
        }
    }
}

pub struct Editor {
    state: EditorState,
    focus_handle: FocusHandle,
    list_state: ListState,
    scroll_to_cursor_pending: bool,
    /// Last known cursor line, used to detect cursor movement for auto-scroll.
    last_cursor_line: Option<usize>,
    /// Last known cursor offset, used to detect cursor movement for autocomplete.
    last_cursor_offset: Option<usize>,
    input_blocked: bool,
    config: EditorConfig,
    /// Whether mouse is over a checkbox.
    hovering_checkbox: bool,
    /// Whether mouse is over a link (regardless of Ctrl state).
    hovering_link_region: bool,
    /// Byte range of currently hovered GitHub ref (if any).
    hovered_github_ref_range: Option<Range<usize>>,
    /// Screen position when hovering a GitHub ref (for popup positioning).
    hovered_ref_position: Option<gpui::Point<gpui::Pixels>>,
    /// Whether Ctrl/Cmd is currently held.
    ctrl_held: bool,
    /// Last buffer version we synced to. Used to detect buffer changes.
    last_synced_version: u64,
    /// Last time we moved cursor during drag-scroll, for throttling.
    last_drag_scroll: Option<std::time::Instant>,
    /// True when we're in the drag-scroll zone, to prevent line's on_drag from resetting selection.
    in_drag_scroll_zone: bool,
    /// True while actively dragging a selection. Used to prevent marker oscillation.
    /// Once set, stays true until mouse up to keep markers expanded.
    is_selecting: bool,
    /// Path to the file being edited (if any).
    file_path: Option<PathBuf>,
    /// Receiver for file watcher events.
    file_watcher_rx: Option<mpsc::Receiver<()>>,
    /// File watcher handle (kept alive to maintain the watch).
    #[allow(dead_code)]
    file_watcher: Option<notify::RecommendedWatcher>,
    /// The mtime of the file after our last save (used to detect external vs our own changes).
    last_save_mtime: Option<std::time::SystemTime>,
    /// GitHub repo context (owner/repo) for autolink detection.
    github_context: Option<GitHubContext>,
    /// Cache for GitHub reference validation results.
    github_validation_cache: GitHubValidationCache,
    /// GitHub API client for validating references.
    github_client: Option<GitHubClient>,
    /// Detected naked URLs by line (updated during render).
    /// Used for atomic cursor movement over shortened GitHub URLs.
    naked_urls_by_line: HashMap<usize, Vec<NakedUrl>>,
    /// Detected GitHub refs by line (updated during render).
    /// Used for autocomplete when cursor is inside a ref.
    github_refs_by_line: HashMap<usize, Vec<RawGitHubMatch>>,
    /// Key of the last link detection: (buffer version, first visible, last
    /// visible). Detection re-runs only when this changes, keeping the regex
    /// scan and ref-validation spawns off the per-repaint path.
    last_link_detect_key: Option<(u64, usize, usize)>,
    /// Autocomplete popup state.
    autocomplete: Option<AutocompleteState>,
    /// Pending autocomplete fetch (for debouncing).
    autocomplete_debounce_task: Option<gpui::Task<()>>,
    /// Unique instance ID for element IDs to prevent GPUI element caching conflicts.
    instance_id: usize,
    /// Inline git-diff decorations against HEAD (ghost lines for deletions,
    /// highlights for additions). Recomputed from `head_base` on every edit.
    diff_state: Option<DiffState>,
    /// Cached git HEAD version of the file: (raw text, rendered snapshot).
    /// The snapshot is reused as the diff base so edits don't re-parse HEAD.
    /// `None` when the file isn't tracked or has no HEAD blob.
    head_base: Option<(String, RenderSnapshot)>,
}

impl Editor {
    /// Create a new editor with the given content and default configuration.
    pub fn new(content: &str, cx: &mut Context<Self>) -> Self {
        Self::with_config(content, EditorConfig::default(), cx)
    }

    /// Create a new editor with the given content and configuration.
    pub fn with_config(content: &str, config: EditorConfig, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let state = EditorState::new(content);
        let line_count = state.buffer.line_count();
        let list_state = ListState::new(line_count, ListAlignment::Top, px(200.0));

        Self {
            state,
            focus_handle,
            list_state,
            scroll_to_cursor_pending: false,
            last_cursor_line: None,
            last_cursor_offset: None,
            input_blocked: false,
            config,
            hovering_checkbox: false,
            hovering_link_region: false,
            hovered_github_ref_range: None,
            hovered_ref_position: None,
            ctrl_held: false,
            last_synced_version: 0,
            last_drag_scroll: None,
            in_drag_scroll_zone: false,
            is_selecting: false,
            file_path: None,
            file_watcher_rx: None,
            file_watcher: None,
            last_save_mtime: None,
            github_context: None,
            github_validation_cache: GitHubValidationCache::new(),
            github_client: None,
            naked_urls_by_line: HashMap::new(),
            github_refs_by_line: HashMap::new(),
            last_link_detect_key: None,
            autocomplete: None,
            autocomplete_debounce_task: None,
            instance_id: NEXT_EDITOR_ID.fetch_add(1, Ordering::Relaxed),
            diff_state: None,
            head_base: None,
        }
    }

    /// Get the file path this editor is editing, if any.
    pub fn file_path(&self) -> Option<&PathBuf> {
        self.file_path.as_ref()
    }

    /// Load the git HEAD version of the current file, cache a render snapshot of
    /// it as the diff base, then recompute the inline diff against the buffer.
    /// Call on open and whenever the file is reloaded from disk (HEAD may have moved).
    pub fn refresh_git_base(&mut self, cx: &mut Context<Self>) {
        self.head_base = self
            .file_path
            .as_ref()
            .and_then(|path| head_blob_text(path))
            .map(|text| {
                let mut base: Buffer = text.parse().expect("Buffer parsing is infallible");
                let snapshot = base.render_snapshot();
                (text, snapshot)
            });
        self.recompute_diff();
        cx.notify();
    }

    /// Recompute the inline diff between the cached git HEAD base and the current
    /// buffer. Cheap enough to run on every edit — reuses the cached base snapshot,
    /// so only the diff itself is recomputed, not the markdown parse of HEAD.
    fn recompute_diff(&mut self) {
        self.diff_state = self
            .head_base
            .as_ref()
            .and_then(|(base_text, base_snapshot)| {
                let current = self.state.buffer.text();
                let state = DiffState::compute(base_snapshot.clone(), base_text, &current);
                state.has_hunks().then_some(state)
            });
    }

    /// Set the GitHub context for autolink detection.
    /// Should be called when opening a file from a GitHub URL (e.g., via writd).
    pub fn set_github_context(&mut self, context: GitHubContext) {
        self.github_context = Some(context);
        // Force link detection to re-run now that context is available.
        self.last_link_detect_key = None;
    }

    /// Set the GitHub client for validating references.
    /// Should be called with an authenticated client when a GitHub token is available.
    pub fn set_github_client(&mut self, client: GitHubClient) {
        self.github_client = Some(client);
        // Force link detection to re-run so refs get validated with the new client.
        self.last_link_detect_key = None;
    }

    /// Clear the GitHub validation cache and re-validate all refs.
    /// Called by Ctrl+R keybind.
    pub fn refresh_github_refs(&mut self, cx: &mut Context<Self>) {
        self.github_validation_cache.clear();
        if let Some(client) = &self.github_client {
            client.clear_autocomplete_cache();
            client.clear_user_cache();
        }
        let line_count = self.state.buffer.line_count();
        let (github_matches, _) = self.detect_links(0, line_count);
        self.spawn_github_validation(&github_matches, cx);
        // Re-detect visible lines on the next render to refresh stored maps.
        self.last_link_detect_key = None;
    }

    /// Detect GitHub refs and naked URLs in a range of lines.
    /// Returns both indexed by line number for use in rendering.
    /// Inclusive range of currently-visible line indices, estimated by scanning
    /// laid-out item bounds from the scroll top until one falls below the viewport.
    fn visible_line_range(&self, total_lines: usize) -> (usize, usize) {
        let first = self.list_state.logical_scroll_top().item_ix;
        let viewport = self.list_state.viewport_bounds();
        let viewport_bottom = viewport.origin.y + viewport.size.height;
        let mut last = first;
        for i in first..total_lines {
            match self.list_state.bounds_for_item(i) {
                Some(bounds) if bounds.origin.y <= viewport_bottom => last = i,
                _ => break,
            }
        }
        (first, last)
    }

    fn detect_links(
        &mut self,
        start_line: usize,
        end_line: usize,
    ) -> (
        HashMap<usize, Vec<RawGitHubMatch>>,
        HashMap<usize, Vec<NakedUrl>>,
    ) {
        let snapshot = self.state.buffer.render_snapshot();
        let mut github_matches_by_line = HashMap::new();
        let mut urls_by_line = HashMap::new();

        for line_idx in start_line..end_line.min(snapshot.line_count()) {
            let line = snapshot.line_markers(line_idx);
            let line_range = line.range.clone();
            let line_text = snapshot
                .rope
                .slice(
                    snapshot.rope.byte_to_char(line_range.start)
                        ..snapshot.rope.byte_to_char(line_range.end),
                )
                .to_string();

            let inline_styles = snapshot.inline_styles_for_line(line_idx);

            // Build code_ranges once for both detections
            let code_ranges: Vec<_> = inline_styles
                .iter()
                .filter(|s| s.style.code)
                .map(|s| s.full_range.clone())
                .collect();

            // Detect GitHub shorthand refs (only if we have context)
            if let Some(github_context) = &self.github_context {
                let matches = detect_github_references_in_line(
                    &line_text,
                    line_range.start,
                    Some(github_context),
                    &code_ranges,
                );
                if !matches.is_empty() {
                    github_matches_by_line.insert(line_idx, matches);
                }
            }

            // Detect naked URLs (skip markdown links)
            let link_ranges: Vec<_> = inline_styles
                .iter()
                .filter(|s| s.link_url.is_some())
                .map(|s| s.full_range.clone())
                .collect();

            let urls = detect_naked_urls(&line_text, line_range.start, &code_ranges, &link_ranges);
            if !urls.is_empty() {
                urls_by_line.insert(line_idx, urls);
            }
        }

        (github_matches_by_line, urls_by_line)
    }

    /// Spawn validation tasks for GitHub refs not already in cache.
    fn spawn_github_validation(
        &mut self,
        matches: &HashMap<usize, Vec<RawGitHubMatch>>,
        cx: &mut Context<Self>,
    ) {
        let client = match &self.github_client {
            Some(c) => c.clone(),
            None => return,
        };

        for m in matches.values().flatten() {
            self.spawn_ref_validation(&m.reference, &client, cx);
        }
    }

    /// Spawn validation tasks for GitHub refs found in naked URLs.
    fn spawn_naked_url_validation(
        &mut self,
        urls: &HashMap<usize, Vec<NakedUrl>>,
        cx: &mut Context<Self>,
    ) {
        let client = match &self.github_client {
            Some(c) => c.clone(),
            None => return,
        };

        for url in urls.values().flatten() {
            if let Some(ref github_ref) = url.github_ref {
                self.spawn_ref_validation(github_ref, &client, cx);
            }
        }
    }

    /// Spawn a single validation task for a GitHub ref.
    fn spawn_ref_validation(
        &mut self,
        reference: &crate::inline::GitHubRef,
        client: &GitHubClient,
        cx: &mut Context<Self>,
    ) {
        if self.github_validation_cache.get(reference).is_some() {
            return;
        }

        self.github_validation_cache.mark_pending(reference.clone());

        let client = client.clone();
        let ref_for_task = reference.clone();
        cx.spawn(async move |weak, cx| {
            let result = client.validate_ref(&ref_for_task).await;
            cx.update(|cx| {
                if let Some(editor) = weak.upgrade() {
                    editor.update(cx, |editor, cx| {
                        match result {
                            crate::github::ValidationResult::ValidWithData(data) => {
                                editor
                                    .github_validation_cache
                                    .set_valid(ref_for_task, Some(data));
                            }
                            crate::github::ValidationResult::ValidNoData => {
                                editor.github_validation_cache.set_valid(ref_for_task, None);
                            }
                            crate::github::ValidationResult::Invalid => {
                                editor.github_validation_cache.set_invalid(ref_for_task);
                            }
                        }
                        cx.notify();
                    });
                }
            });
        })
        .detach();
    }

    /// Fetch autocomplete suggestions after a debounce delay.
    /// Cancels any pending fetch and starts a new timer.
    fn fetch_autocomplete_suggestions_debounced(&mut self, cx: &mut Context<Self>) {
        // Cancel any pending debounce task by dropping it
        self.autocomplete_debounce_task = None;

        // Spawn a new debounced fetch
        let task = cx.spawn(async move |weak, cx| {
            // Wait for debounce delay (150ms)
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;

            // Now do the actual fetch
            cx.update(|cx| {
                if let Some(editor) = weak.upgrade() {
                    editor.update(cx, |editor, cx| {
                        editor.fetch_autocomplete_suggestions(cx);
                    });
                }
            });
        });

        self.autocomplete_debounce_task = Some(task);
    }

    /// Fetch autocomplete suggestions for the current prefix.
    fn fetch_autocomplete_suggestions(&mut self, cx: &mut Context<Self>) {
        let (client, context) = match (&self.github_client, &self.github_context) {
            (Some(c), Some(ctx)) => (c.clone(), ctx.clone()),
            _ => return,
        };

        let (trigger, prefix) = match &self.autocomplete {
            Some(ac) => (ac.trigger, ac.prefix.clone()),
            _ => return,
        };

        let owner = context.owner.clone();
        let repo = context.repo.clone();

        match trigger {
            AutocompleteTrigger::Issue => {
                self.fetch_issue_suggestions(client, owner, repo, prefix, cx);
            }
            AutocompleteTrigger::User => {
                self.fetch_user_suggestions(client, owner, repo, prefix, cx);
            }
        }
    }

    /// Fetch issue/PR suggestions (async).
    fn fetch_issue_suggestions(
        &mut self,
        client: GitHubClient,
        owner: String,
        repo: String,
        prefix: String,
        cx: &mut Context<Self>,
    ) {
        // Mark as loading and record that we're fetching for this prefix
        if let Some(ref mut ac) = self.autocomplete {
            ac.loading = true;
            ac.fetched_prefix = Some(prefix.clone());
        }

        cx.spawn(async move |weak, cx| {
            let issues = client
                .issues_matching_prefix(&owner, &repo, &prefix, 5)
                .await;

            // Build suggestions and keep full issue data for caching
            let suggestions_with_data: Vec<(AutocompleteSuggestion, IssueOrPr)> = issues
                .into_iter()
                .map(|issue| {
                    let suggestion = AutocompleteSuggestion::IssueOrPr {
                        number: issue.number,
                        symbol: issue.symbol().to_string(),
                        status: issue.status(),
                        display_snapshot: render_snapshot_for_title(&issue.title),
                    };
                    (suggestion, issue)
                })
                .collect();

            cx.update(|cx| {
                if let Some(editor) = weak.upgrade() {
                    editor.update(cx, |editor, cx| {
                        // Add fetched issues to validation cache with full data
                        for (_, issue) in &suggestions_with_data {
                            let github_ref = GitHubRef::Issue {
                                owner: owner.clone(),
                                repo: repo.clone(),
                                number: issue.number,
                            };
                            editor.github_validation_cache.set_valid(
                                github_ref,
                                Some(crate::github::ValidatedRefData::Issue(issue.clone())),
                            );
                        }

                        let suggestions: Vec<_> = suggestions_with_data
                            .into_iter()
                            .map(|(s, _)| s)
                            .collect();

                        // Only update if autocomplete is still active with the same prefix
                        if let Some(ref mut ac) = editor.autocomplete
                            && ac.trigger == AutocompleteTrigger::Issue
                            && ac.prefix == prefix
                        {
                            // Check if user's current input is a known valid issue
                            let mut final_suggestions = Vec::new();
                            if let Ok(num) = prefix.parse::<u64>() {
                                let user_ref = GitHubRef::Issue {
                                    owner: owner.clone(),
                                    repo: repo.clone(),
                                    number: num,
                                };
                                if let Some(crate::github::ValidationState::Valid(Some(
                                    crate::github::ValidatedRefData::Issue(issue),
                                ))) = editor.github_validation_cache.get(&user_ref)
                                {
                                    // Don't add if it's already in the suggestions
                                    if !suggestions.iter().any(|s| matches!(s, AutocompleteSuggestion::IssueOrPr { number: n, .. } if *n == num)) {
                                        final_suggestions.push(AutocompleteSuggestion::IssueOrPr {
                                            number: num,
                                            symbol: issue.symbol().to_string(),
                                            status: issue.status(),
                                            display_snapshot: render_snapshot_for_title(&issue.title),
                                        });
                                    }
                                }
                            }
                            final_suggestions.extend(suggestions.clone());

                            ac.suggestions = final_suggestions;
                            ac.loading = false;
                            ac.selected_index = 0;
                            cx.notify();
                        }
                    });
                }
            });
        })
        .detach();
    }

    /// Fetch user suggestions (async - uses GraphQL mentionableUsers).
    fn fetch_user_suggestions(
        &mut self,
        client: GitHubClient,
        owner: String,
        repo: String,
        prefix: String,
        cx: &mut Context<Self>,
    ) {
        // Mark as loading
        if let Some(ref mut ac) = self.autocomplete {
            ac.loading = true;
            ac.fetched_prefix = Some(prefix.clone());
        }

        cx.spawn(async move |weak, cx| {
            let users = client
                .users_matching_prefix(&owner, &repo, &prefix, 5)
                .await;

            let suggestions: Vec<AutocompleteSuggestion> = users
                .into_iter()
                .map(|u| AutocompleteSuggestion::User {
                    login: u.login,
                    name: u.name,
                })
                .collect();

            cx.update(|cx| {
                if let Some(editor) = weak.upgrade() {
                    editor.update(cx, |editor, cx| {
                        // Only update if autocomplete is still active with the same prefix
                        if let Some(ref mut ac) = editor.autocomplete
                            && ac.trigger == AutocompleteTrigger::User
                            && ac.prefix == prefix
                        {
                            ac.suggestions = suggestions;
                            ac.loading = false;
                            ac.selected_index = 0;
                            cx.notify();
                        }
                    });
                }
            });
        })
        .detach();
    }

    /// Set up file watching for external changes.
    /// When the file changes externally, the buffer will be reloaded.
    /// If the file doesn't exist yet, watches the parent directory for its creation.
    pub fn watch_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        use notify::{RecursiveMode, Watcher};

        self.file_path = Some(path.clone());

        // Compute the initial inline git diff against HEAD.
        self.refresh_git_base(cx);

        let (tx, rx) = mpsc::channel();
        let watch_path = path.clone();
        let file_exists = path.exists();

        let mut watcher = match notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            if let Ok(event) = res {
                use notify::EventKind;
                match event.kind {
                    EventKind::Modify(_) => {
                        let _ = tx.send(());
                    }
                    EventKind::Create(_) if event.paths.iter().any(|p| p == &watch_path) => {
                        let _ = tx.send(());
                    }
                    _ => {}
                }
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("Failed to create file watcher: {}", e);
                return;
            }
        };

        let target = if file_exists {
            path.clone()
        } else if let Some(parent) = path.parent() {
            parent.to_path_buf()
        } else {
            eprintln!("Cannot watch file with no parent directory: {:?}", path);
            return;
        };

        if let Err(e) = watcher.watch(&target, RecursiveMode::NonRecursive) {
            eprintln!("Failed to watch {:?}: {}", target, e);
            return;
        }

        self.file_watcher_rx = Some(rx);
        self.file_watcher = Some(watcher);

        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;

                let continue_loop = cx.update(|cx| {
                    if let Some(editor) = weak.upgrade() {
                        editor.update(cx, |editor, cx| {
                            if let Some(rx) = &editor.file_watcher_rx {
                                let mut changed = false;
                                while rx.try_recv().is_ok() {
                                    changed = true;
                                }
                                if changed {
                                    editor.reload_file(cx);
                                }
                            }
                        });
                        true
                    } else {
                        false
                    }
                });

                if !continue_loop {
                    break;
                }
            }
        })
        .detach();
    }

    /// Reload the file from disk after an external change (e.g. a background
    /// agent editing the file), then refresh the inline git diff. The cursor's
    /// line is preserved so reading isn't disrupted.
    fn reload_file(&mut self, cx: &mut Context<Self>) {
        let Some(path) = &self.file_path else { return };

        // Skip reloads triggered by our own save.
        if let Some(last_save_mtime) = self.last_save_mtime
            && let Ok(metadata) = std::fs::metadata(path)
            && let Ok(file_mtime) = metadata.modified()
            && file_mtime == last_save_mtime
        {
            return;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to reload file {:?}: {}", path, e);
                return;
            }
        };

        if content != self.state.buffer.text() {
            let cursor_line = self.state.buffer.byte_to_line(self.state.selection.head);
            self.set_text(&content, cx);
            let line = cursor_line.min(self.state.buffer.line_count().saturating_sub(1));
            let offset = self.state.buffer.line_to_byte(line);
            self.state.selection = Selection::new(offset, offset);
        }

        // HEAD may have moved (e.g. the agent committed); refresh the diff base.
        self.refresh_git_base(cx);
    }

    /// Returns the buffer contents as a string.
    pub fn text(&self) -> String {
        self.state.buffer.text()
    }

    /// Returns the length of the buffer in bytes.
    pub fn len(&self) -> usize {
        self.state.buffer.len_bytes()
    }

    /// Returns true if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.state.buffer.len_bytes() == 0
    }

    /// Check if buffer ends with the given string (efficient, doesn't copy whole buffer).
    pub fn ends_with(&self, suffix: &str) -> bool {
        self.state.buffer.ends_with(suffix)
    }

    /// Find the nearest heading above or at the given line.
    /// Returns the heading level (1-6) if found, None otherwise.
    fn find_current_heading(&self, from_line: usize) -> Option<u8> {
        for line_idx in (0..=from_line).rev() {
            let markers = self.state.buffer.line_markers(line_idx);
            for marker in &markers.markers {
                if let MarkerKind::Heading(level) = marker.kind {
                    return Some(level);
                }
            }
        }
        None
    }

    /// Replace the entire buffer contents, resetting cursor to the start.
    pub fn set_text(&mut self, content: &str, cx: &mut Context<Self>) {
        self.state.buffer = content.parse().unwrap_or_default();
        self.state.selection = Selection::new(0, 0);
        cx.notify();
    }

    /// Sync the list state count with the buffer line count.
    /// Also triggers autosave if enabled.
    fn sync_list_state(&mut self, cx: &mut Context<Self>) {
        let line_count = self.state.buffer.line_count();
        let current_count = self.list_state.item_count();

        if line_count != current_count {
            if line_count > current_count {
                self.list_state
                    .splice(current_count..current_count, line_count - current_count);
            } else {
                self.list_state.splice(line_count..current_count, 0);
            }
        }

        let config = crate::config::Config::global(cx);
        if config.autosave {
            // save() records last_save_mtime so the watcher skips this write.
            self.save(cx);
        }
    }

    fn cursor(&self) -> Cursor {
        self.state.selection.cursor()
    }

    fn move_cursor(&mut self, new_cursor: Cursor, extend: bool) {
        if extend {
            self.state.selection = self.state.selection.extend_to(new_cursor.offset);
        } else {
            self.state.selection = Selection::new(new_cursor.offset, new_cursor.offset);
        }
    }

    fn request_scroll_to_cursor(&mut self) {
        self.scroll_to_cursor_pending = true;
    }

    fn tab(&mut self) {
        self.state.tab();
    }

    fn shift_tab(&mut self) {
        self.state.shift_tab();
    }

    fn toggle_checkbox(&mut self, line_number: usize, cx: &mut Context<Self>) {
        self.state.toggle_checkbox_for_test(line_number);
        cx.notify();
    }

    fn insert_text(&mut self, text: &str) {
        self.state.insert_text(text);
    }

    /// Try to detect an autocomplete trigger at the given position in line_text.
    /// Returns Some((trigger_type, trigger_offset, prefix)) if found.
    fn detect_autocomplete_trigger(
        line_text: &str,
        line_start: usize,
    ) -> Option<(AutocompleteTrigger, usize, String)> {
        // Try each trigger character, preferring the rightmost one
        let triggers = [
            ('#', AutocompleteTrigger::Issue),
            ('@', AutocompleteTrigger::User),
        ];

        let mut best_match: Option<(AutocompleteTrigger, usize, String)> = None;

        for (trigger_char, trigger_type) in triggers {
            if let Some(pos) = line_text.rfind(trigger_char) {
                // Check word boundary
                let is_at_word_boundary = pos == 0
                    || line_text
                        .as_bytes()
                        .get(pos - 1)
                        .is_none_or(|&b| b == b' ' || b == b'\t' || b == b'\n');

                if !is_at_word_boundary {
                    continue;
                }

                let prefix = line_text[pos + 1..].to_string();

                // Validate prefix based on trigger type
                let valid = match trigger_type {
                    AutocompleteTrigger::Issue => {
                        // # followed by whitespace is a heading, not an issue ref
                        !prefix.starts_with(' ') && !prefix.starts_with('\t')
                    }
                    AutocompleteTrigger::User => {
                        // @ prefix: alphanumeric and hyphens, not starting with hyphen
                        prefix.is_empty()
                            || (prefix.chars().all(|c| c.is_alphanumeric() || c == '-')
                                && !prefix.starts_with('-'))
                    }
                };

                if !valid {
                    continue;
                }

                let trigger_offset = line_start + pos;

                // Keep the rightmost valid trigger
                if best_match
                    .as_ref()
                    .map(|(_, off, _)| trigger_offset > *off)
                    .unwrap_or(true)
                {
                    best_match = Some((trigger_type, trigger_offset, prefix));
                }
            }
        }

        best_match
    }

    /// Check if cursor is inside a GitHub ref and update autocomplete accordingly.
    /// Returns true if we should fetch suggestions.
    fn update_autocomplete_from_cursor(&mut self) -> bool {
        // Only trigger autocomplete if we have GitHub context and client
        if self.github_context.is_none() || self.github_client.is_none() {
            self.autocomplete = None;
            return false;
        }

        let cursor = self.state.cursor().offset;
        let cursor_line = self.state.buffer.byte_to_line(cursor);

        // Check if cursor is inside or at the end of a stored GitHub issue ref
        if let Some(refs) = self.github_refs_by_line.get(&cursor_line) {
            for github_match in refs {
                if cursor >= github_match.byte_range.start
                    && cursor <= github_match.byte_range.end
                    && let GitHubRef::Issue { number, .. } = &github_match.reference
                {
                    let prefix = number.to_string();
                    let trigger_offset = github_match.byte_range.start;
                    return self.set_autocomplete_state(
                        AutocompleteTrigger::Issue,
                        trigger_offset,
                        prefix,
                    );
                }
            }
        }

        // Detect trigger from raw text
        if cursor > 0 {
            let line_start = self.state.buffer.line_to_byte(cursor_line);
            let line_text = self.state.buffer.slice_cow(line_start..cursor).into_owned();

            if let Some((trigger, trigger_offset, prefix)) =
                Self::detect_autocomplete_trigger(&line_text, line_start)
            {
                return self.set_autocomplete_state(trigger, trigger_offset, prefix);
            }
        }

        // Cursor not inside any ref - close autocomplete
        self.autocomplete = None;
        false
    }

    /// Update autocomplete state for a detected trigger.
    /// Returns true if suggestions should be fetched/filtered.
    fn set_autocomplete_state(
        &mut self,
        trigger: AutocompleteTrigger,
        trigger_offset: usize,
        prefix: String,
    ) -> bool {
        // Check if state actually changed
        let changed = self
            .autocomplete
            .as_ref()
            .map(|ac| ac.trigger != trigger || ac.prefix != prefix)
            .unwrap_or(true);

        if !changed {
            return false;
        }

        // Preserve old state only if same trigger type
        let old_state = self.autocomplete.take();
        let same_trigger = old_state
            .as_ref()
            .map(|ac| ac.trigger == trigger)
            .unwrap_or(false);

        // For Issue trigger, check if we already fetched this prefix
        let should_fetch = match trigger {
            AutocompleteTrigger::Issue => {
                let already_fetched = old_state
                    .as_ref()
                    .filter(|_| same_trigger)
                    .and_then(|ac| ac.fetched_prefix.as_ref())
                    == Some(&prefix);
                !already_fetched
            }
            // User autocomplete uses pre-cached data, always "fetch" (filter)
            AutocompleteTrigger::User => true,
        };

        self.autocomplete = Some(AutocompleteState {
            trigger,
            trigger_offset,
            prefix,
            suggestions: old_state
                .as_ref()
                .filter(|_| same_trigger)
                .map(|ac| ac.suggestions.clone())
                .unwrap_or_default(),
            selected_index: old_state
                .as_ref()
                .filter(|_| same_trigger)
                .map(|ac| ac.selected_index)
                .unwrap_or(0),
            loading: false,
            fetched_prefix: old_state
                .filter(|_| same_trigger)
                .and_then(|ac| ac.fetched_prefix),
        });

        should_fetch
    }

    /// Render the autocomplete popup if active.
    fn render_autocomplete(
        &self,
        line_theme: &LineTheme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let ac = self.autocomplete.as_ref()?;

        // Don't show popup if loading or no suggestions
        if ac.loading || ac.suggestions.is_empty() {
            return None;
        }

        let theme = &self.config.theme;

        // Get absolute cursor position (set during Line paint)
        let cursor_screen_pos = CursorScreenPosition::global(cx);
        let cursor_pos = cursor_screen_pos.position?;

        // Get viewport bounds for fallback
        let viewport = self.list_state.viewport_bounds();

        // Fixed width for issues/PRs (needs truncation), dynamic for users
        let popup_width = match ac.trigger {
            AutocompleteTrigger::Issue => Some(px(500.0)),
            AutocompleteTrigger::User => None, // auto-sized
        };
        let popup_max_height = px(300.0);
        let gap = px(4.0);

        // Clamp x to keep popup within content area (only if fixed width)
        let popup_x = if let Some(width) = popup_width {
            let content_right = cursor_screen_pos
                .content_right_edge
                .unwrap_or(viewport.origin.x + viewport.size.width);
            cursor_pos.x.min(content_right - width)
        } else {
            cursor_pos.x
        };

        // Position Y below the cursor row, or flip above if not enough space below
        let line_height = self.config.line_height.to_pixels(window.rem_size());
        let viewport_bottom = viewport.origin.y + viewport.size.height;
        let space_below = viewport_bottom - (cursor_pos.y + line_height);
        let space_above = cursor_pos.y - viewport.origin.y;

        let (popup_y, anchor_corner) = if space_below >= popup_max_height + gap {
            // Enough space below - position popup below cursor
            (cursor_pos.y + line_height + gap, Anchor::TopLeft)
        } else if space_above >= popup_max_height + gap {
            // Not enough below but enough above - flip to above cursor
            (cursor_pos.y - gap, Anchor::BottomLeft)
        } else {
            // Not enough space either way - default to below
            (cursor_pos.y + line_height + gap, Anchor::TopLeft)
        };

        // Build suggestion items
        let border_color = theme.comment;
        let suggestion_count = ac.suggestions.len();
        let selection_bg = theme.selection;

        let items: Vec<AnyElement> = ac
            .suggestions
            .iter()
            .enumerate()
            .map(|(i, suggestion)| {
                let is_selected = i == ac.selected_index;
                let is_first = i == 0;
                let is_last = i == suggestion_count - 1;

                // Build display element based on suggestion type
                let display_element: AnyElement = match suggestion {
                    AutocompleteSuggestion::IssueOrPr {
                        number,
                        symbol,
                        status,
                        display_snapshot,
                    } => {
                        // Color based on status
                        let status_color = match status {
                            IssueStatus::Open => theme.green,
                            IssueStatus::Draft => theme.comment,
                            IssueStatus::Merged | IssueStatus::Closed => theme.purple,
                            IssueStatus::ClosedNotPlanned => theme.red,
                        };

                        // Build prefix: "● #123 " with colored runs
                        let prefix_text = format!("{} #{} ", symbol, number);
                        let text_font = line_theme.text_font.clone();
                        let make_run = |len: usize, color: gpui::Rgba| TextRun {
                            len,
                            font: text_font.clone(),
                            color: color.into(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };

                        let number_str = format!("#{} ", number);
                        let prefix_runs = vec![
                            make_run(symbol.len(), status_color),   // symbol
                            make_run(1, theme.foreground),          // space
                            make_run(number_str.len(), theme.cyan), // #123 + space
                        ];

                        // Use Line with prefix for markdown-rendered title
                        let display_line = display_snapshot.line_markers(0);
                        let display_inline_styles = display_snapshot.inline_styles_for_line(0);

                        // 500px popup - 16px padding (px_2 * 2) = 484px available
                        Line::new(
                            display_line,
                            display_snapshot.rope.clone(),
                            usize::MAX, // no cursor
                            display_inline_styles,
                            line_theme.clone(),
                            None,       // no selection
                            Vec::new(), // no code highlights
                            None,       // no base path
                            Vec::new(), // no github refs in popup
                            None,       // no hovered ref in popup
                            true,       // input blocked in popup
                            None,       // no max width in popup
                            None,       // no line background
                            Vec::new(), // no inline highlight ranges
                            None,       // no inline highlight color
                        )
                        .with_prefix(prefix_text, prefix_runs)
                        .truncate(px(484.0))
                        .into_any_element()
                    }
                    AutocompleteSuggestion::User { login, name } => {
                        // Styled text for users: cyan "@login" + dimmed "Display Name"
                        let mut row = div().flex().flex_row().gap_1();
                        row = row.child(div().text_color(theme.cyan).child(format!("@{}", login)));
                        if let Some(n) = name {
                            row = row.child(div().text_color(theme.comment).child(n.clone()));
                        }
                        row.into_any_element()
                    }
                };

                div()
                    .id(("autocomplete-item", i))
                    .px_2()
                    .py_1()
                    .when_some(popup_width, |d, w| d.w(w))
                    .cursor_pointer()
                    .when(is_first, |d| d.rounded_t_md())
                    .when(is_last, |d| d.rounded_b_md())
                    .when(!is_last, |d| d.border_b_1().border_color(border_color))
                    .when(is_selected, |d| d.bg(selection_bg))
                    .hover(|d| d.bg(selection_bg))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |editor, _event, window, cx| {
                            cx.stop_propagation();
                            window.prevent_default();
                            if let Some(ref mut ac) = editor.autocomplete {
                                ac.selected_index = i;
                            }
                            if editor.accept_autocomplete_suggestion() {
                                cx.notify();
                            }
                        }),
                    )
                    .on_mouse_move(cx.listener(move |editor, _event, _window, cx| {
                        if let Some(ref mut ac) = editor.autocomplete
                            && ac.selected_index != i
                        {
                            ac.selected_index = i;
                            cx.notify();
                        }
                    }))
                    .child(display_element)
                    .into_any_element()
            })
            .collect();

        Some(
            anchored()
                .position(point(popup_x, popup_y))
                .anchor(anchor_corner)
                .child(
                    div()
                        .id("autocomplete-popup")
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.comment)
                        .rounded_md()
                        .shadow_md()
                        .overflow_hidden()
                        .when_some(popup_width, |d, w| d.w(w))
                        .max_h(px(300.0))
                        .overflow_y_scroll()
                        .text_size(px(14.0))
                        .font(line_theme.text_font.clone())
                        .on_scroll_wheel(cx.listener(|_editor, _event, _window, cx| {
                            cx.stop_propagation();
                        }))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_editor, _event, _window, cx| {
                                cx.stop_propagation();
                            }),
                        )
                        .children(items),
                )
                .into_any_element(),
        )
    }

    /// Render hover popup for GitHub refs.
    /// Uses the same styling as autocomplete items when detailed data is available.
    fn render_github_ref_hover(
        &self,
        line_theme: &LineTheme,
        window: &Window,
        _cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        use crate::github::{ValidatedRefData, ValidationState};

        // Don't show hover if autocomplete popup is visible
        if self
            .autocomplete
            .as_ref()
            .is_some_and(|ac| !ac.suggestions.is_empty())
        {
            return None;
        }

        // Need a hovered ref
        let hovered_range = self.hovered_github_ref_range.as_ref()?;

        // Find the actual GitHubRef from our stored refs (check both regular refs and naked URLs)
        let github_ref = self
            .github_refs_by_line
            .values()
            .flatten()
            .find_map(|m| {
                if &m.byte_range == hovered_range {
                    Some(&m.reference)
                } else {
                    None
                }
            })
            .or_else(|| {
                // Check naked URLs for GitHub refs
                self.naked_urls_by_line.values().flatten().find_map(|u| {
                    if &u.byte_range == hovered_range {
                        u.github_ref.as_ref()
                    } else {
                        None
                    }
                })
            })?;

        // Get position from stored mouse position
        let pos = self.hovered_ref_position?;

        let theme = &self.config.theme;

        // Check validation status and extract data
        let validation_state = self.github_validation_cache.get(github_ref);

        // Get viewport bounds for positioning
        let viewport = self.list_state.viewport_bounds();
        let line_height = self.config.line_height.to_pixels(window.rem_size());
        let popup_max_height = px(100.0);
        let gap = px(4.0);

        // Position below the ref, or above if not enough space
        let space_below = viewport.origin.y + viewport.size.height - (pos.y + line_height);
        let (popup_y, anchor_corner) = if space_below >= popup_max_height + gap {
            (pos.y + line_height + gap, Anchor::TopLeft)
        } else {
            (pos.y - gap, Anchor::BottomLeft)
        };

        // Build popup content based on validation state
        let popup_content: AnyElement = match &validation_state {
            Some(ValidationState::Valid(Some(ValidatedRefData::Issue(issue)))) => {
                // Render like autocomplete issue item
                let status = issue.status();
                let status_color = match status {
                    IssueStatus::Open => theme.green,
                    IssueStatus::Draft => theme.comment,
                    IssueStatus::Merged | IssueStatus::Closed => theme.purple,
                    IssueStatus::ClosedNotPlanned => theme.red,
                };

                // Build prefix: "● #123 " with colored runs
                let prefix_text = format!("{} #{} ", issue.symbol(), issue.number);
                let text_font = line_theme.text_font.clone();
                let make_run = |len: usize, color: gpui::Rgba| TextRun {
                    len,
                    font: text_font.clone(),
                    color: color.into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };

                let number_str = format!("#{} ", issue.number);
                let prefix_runs = vec![
                    make_run(issue.symbol().len(), status_color), // symbol
                    make_run(1, theme.foreground),                // space
                    make_run(number_str.len(), theme.cyan),       // #123 + space
                ];

                // Render title with markdown
                let display_snapshot = render_snapshot_for_title(&issue.title);
                let display_line = display_snapshot.line_markers(0);
                let display_inline_styles = display_snapshot.inline_styles_for_line(0);

                Line::new(
                    display_line,
                    display_snapshot.rope.clone(),
                    usize::MAX, // no cursor
                    display_inline_styles,
                    line_theme.clone(),
                    None,       // no selection
                    Vec::new(), // no code highlights
                    None,       // no base path
                    Vec::new(), // no github refs in popup
                    None,       // no hovered ref in popup
                    true,       // input blocked in popup
                    None,       // no max width in popup
                    None,       // no line background
                    Vec::new(), // no inline highlight ranges
                    None,       // no inline highlight color
                )
                .with_prefix(prefix_text, prefix_runs)
                .truncate(px(484.0))
                .into_any_element()
            }
            Some(ValidationState::Valid(Some(ValidatedRefData::User(user)))) => {
                // Render like autocomplete user item
                let mut row = div().flex().flex_row().gap_1();
                row = row.child(
                    div()
                        .text_color(theme.cyan)
                        .child(format!("@{}", user.login)),
                );
                if let Some(name) = &user.name {
                    row = row.child(div().text_color(theme.comment).child(name.clone()));
                }
                row.into_any_element()
            }
            Some(ValidationState::Valid(None)) => {
                // Valid but no detailed data (commits, etc.) - show simple checkmark
                let display_text = github_ref.short_display(self.github_context.as_ref());
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(div().text_color(theme.green).child("✓ "))
                    .child(div().text_color(theme.cyan).child(display_text))
                    .into_any_element()
            }
            Some(ValidationState::Invalid) => {
                // Invalid ref - show X mark
                let display_text = github_ref.short_display(self.github_context.as_ref());
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(div().text_color(theme.red).child("✗ "))
                    .child(div().text_color(theme.cyan).child(display_text))
                    .into_any_element()
            }
            Some(ValidationState::Pending) | None => {
                // Pending or unknown - show loading indicator
                let display_text = github_ref.short_display(self.github_context.as_ref());
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(div().text_color(theme.comment).child("… "))
                    .child(div().text_color(theme.cyan).child(display_text))
                    .into_any_element()
            }
        };

        // Determine popup width - fixed for issues (truncation), auto for others
        let popup_width = match &validation_state {
            Some(ValidationState::Valid(Some(ValidatedRefData::Issue(_)))) => Some(px(500.0)),
            _ => None,
        };

        Some(
            anchored()
                .position(point(pos.x, popup_y))
                .anchor(anchor_corner)
                .child(
                    div()
                        .id("github-ref-hover")
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.comment)
                        .rounded_md()
                        .shadow_md()
                        .px_2()
                        .py_1()
                        .text_size(px(14.0))
                        .font(line_theme.text_font.clone())
                        .when_some(popup_width, |d, w| d.w(w))
                        .child(popup_content),
                )
                .into_any_element(),
        )
    }

    /// Accept the currently selected autocomplete suggestion.
    /// Returns true if a suggestion was accepted.
    fn accept_autocomplete_suggestion(&mut self) -> bool {
        let ac = match self.autocomplete.take() {
            Some(ac) => ac,
            None => return false,
        };

        if ac.suggestions.is_empty() {
            return false;
        }

        let suggestion = &ac.suggestions[ac.selected_index];
        let replacement = match suggestion {
            AutocompleteSuggestion::IssueOrPr { number, .. } => format!("#{}", number),
            AutocompleteSuggestion::User { login, .. } => format!("@{}", login),
        };

        // Replace text from trigger_offset to current cursor with the replacement
        let cursor = self.state.cursor().offset;
        let range = ac.trigger_offset..cursor;
        self.state.buffer.delete(range.clone(), cursor);
        self.state
            .buffer
            .insert(ac.trigger_offset, &replacement, ac.trigger_offset);
        let new_pos = ac.trigger_offset + replacement.len();
        self.state.selection = Selection::new(new_pos, new_pos);

        true
    }

    fn delete_backward(&mut self) {
        self.state.delete_backward();
    }

    fn delete_forward(&mut self) {
        self.state.delete_forward();
    }

    fn enter(&mut self) {
        self.state.enter();
        self.scroll_to_cursor_pending = true;
    }

    fn shift_enter(&mut self) {
        self.state.shift_enter();
        self.scroll_to_cursor_pending = true;
    }

    fn shift_alt_enter(&mut self) {
        self.state.shift_alt_enter();
        self.scroll_to_cursor_pending = true;
    }

    fn move_in_direction(&mut self, direction: Direction, extend: bool) {
        let new_cursor = match direction {
            Direction::Left => self.cursor().move_left(&self.state.buffer),
            Direction::Right => self.cursor().move_right(&self.state.buffer),
            Direction::Up => self.cursor().move_up(&self.state.buffer),
            Direction::Down => self.cursor().move_down(&self.state.buffer),
        };

        self.move_cursor(new_cursor, extend);
        self.scroll_to_cursor_pending = true;
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.input_blocked {
            return;
        }

        let keystroke = &event.keystroke;

        // Handle autocomplete keyboard navigation
        if self.autocomplete.is_some() {
            match keystroke.key.as_str() {
                "escape" => {
                    self.autocomplete = None;
                    cx.notify();
                    return;
                }
                "up" => {
                    if let Some(ref mut ac) = self.autocomplete
                        && !ac.suggestions.is_empty()
                    {
                        if ac.selected_index == 0 {
                            ac.selected_index = ac.suggestions.len() - 1;
                        } else {
                            ac.selected_index -= 1;
                        }
                        cx.notify();
                        return;
                    }
                }
                "down" => {
                    if let Some(ref mut ac) = self.autocomplete
                        && !ac.suggestions.is_empty()
                    {
                        ac.selected_index = (ac.selected_index + 1) % ac.suggestions.len();
                        cx.notify();
                        return;
                    }
                }
                "enter" | "tab" => {
                    // Only accept if popup is visible (has suggestions)
                    if let Some(ref ac) = self.autocomplete
                        && !ac.suggestions.is_empty()
                        && self.accept_autocomplete_suggestion()
                    {
                        cx.notify();
                        return;
                    }
                }
                _ => {}
            }
        }

        let extend = keystroke.modifiers.shift;

        match keystroke.key.as_str() {
            "backspace" => {
                self.delete_backward();
            }
            "delete" => {
                self.delete_forward();
            }
            "left" => {
                self.move_in_direction(Direction::Left, extend);
            }
            "right" => {
                self.move_in_direction(Direction::Right, extend);
            }
            "up" => {
                self.move_in_direction(Direction::Up, extend);
            }
            "down" => {
                self.move_in_direction(Direction::Down, extend);
            }
            "home" => {
                let new_cursor = if keystroke.modifiers.control || keystroke.modifiers.platform {
                    self.cursor().move_to_start()
                } else {
                    self.cursor().move_to_line_start(&self.state.buffer)
                };
                self.move_cursor(new_cursor, extend);
                self.scroll_to_cursor_pending = true;
            }
            "end" => {
                let new_cursor = if keystroke.modifiers.control || keystroke.modifiers.platform {
                    self.cursor().move_to_end(&self.state.buffer)
                } else {
                    self.cursor().move_to_line_end(&self.state.buffer)
                };
                self.move_cursor(new_cursor, extend);
                self.scroll_to_cursor_pending = true;
            }
            "enter" => {
                if keystroke.modifiers.shift && keystroke.modifiers.alt {
                    self.shift_alt_enter();
                } else if keystroke.modifiers.shift {
                    self.shift_enter();
                } else {
                    self.enter();
                }
            }
            "tab" => {
                if self.state.cursor_in_code_block() {
                    self.insert_text("    ");
                } else if keystroke.modifiers.shift {
                    self.shift_tab();
                } else {
                    self.tab();
                }
            }
            "a" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                self.state.selection = Selection::select_all(&self.state.buffer);
            }
            "c" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                if !self.state.selection.is_collapsed() {
                    let range = self.state.selection.range();
                    let text = self.state.buffer.slice_cow(range).into_owned();
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                }
            }
            "x" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                if !self.state.selection.is_collapsed() {
                    let range = self.state.selection.range();
                    let text = self.state.buffer.slice_cow(range).into_owned();
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                    self.state.delete_selection();
                }
            }
            "v" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                if let Some(clipboard_item) = cx.read_from_clipboard()
                    && let Some(text) = clipboard_item.text()
                {
                    let ctx =
                        PasteContext::from_buffer(&self.state.buffer, self.state.cursor().offset);
                    let transformed = transform_paste(&text, &ctx);
                    self.insert_text(&transformed);
                }
            }
            "z" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                if keystroke.modifiers.shift {
                    if let Some(cursor_pos) = self.state.buffer.redo() {
                        self.state.selection = Selection::new(cursor_pos, cursor_pos);
                    }
                } else if let Some(cursor_pos) = self.state.buffer.undo() {
                    self.state.selection = Selection::new(cursor_pos, cursor_pos);
                }
            }
            "y" if keystroke.modifiers.control => {
                if let Some(cursor_pos) = self.state.buffer.redo() {
                    self.state.selection = Selection::new(cursor_pos, cursor_pos);
                }
            }
            "s" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                self.save(cx);
            }
            "r" if keystroke.modifiers.control || keystroke.modifiers.platform => {
                self.refresh_github_refs(cx);
            }

            _ => {
                if let Some(key_char) = &keystroke.key_char {
                    if key_char == " " {
                        if !self.state.try_insert_space() {
                            return;
                        }
                    } else {
                        self.insert_text(key_char);
                    }

                    if key_char == ">" {
                        self.state.maybe_complete_blockquote_marker();
                    }

                    if key_char == "`" || key_char == "~" {
                        self.state.maybe_complete_code_fence();
                    }

                    self.scroll_to_cursor_pending = true;
                }
            }
        }

        cx.notify();
    }

    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ctrl_held = event.modifiers.control || event.modifiers.platform;
        if self.ctrl_held != ctrl_held {
            self.ctrl_held = ctrl_held;
            cx.notify();
        }
    }

    /// Block or unblock user input. Useful during demos or streaming.
    pub fn set_input_blocked(&mut self, blocked: bool) {
        self.input_blocked = blocked;
    }

    /// Returns the current cursor position as a byte offset.
    pub fn cursor_position(&self) -> usize {
        self.state.selection.head
    }

    /// Returns the current selection range, or None if the cursor is collapsed.
    pub fn selection_range(&self) -> Option<std::ops::Range<usize>> {
        if self.state.selection.is_collapsed() {
            None
        } else {
            Some(self.state.selection.range())
        }
    }

    /// Set the cursor position to the given byte offset.
    pub fn set_cursor(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.state.buffer.len_bytes());
        self.state.selection = Selection::new(offset, offset);
        cx.notify();
    }

    /// Move the cursor to the end of the buffer.
    pub fn move_to_end(&mut self, cx: &mut Context<Self>) {
        let end = self.state.buffer.len_bytes();
        self.state.selection = Selection::new(end, end);
        cx.notify();
    }

    /// Move the cursor to the start of the buffer.
    pub fn move_to_start(&mut self, cx: &mut Context<Self>) {
        self.state.selection = Selection::new(0, 0);
        cx.notify();
    }

    /// Returns true if the buffer has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.state.buffer.is_dirty()
    }

    /// Mark the buffer as clean (no unsaved changes).
    pub fn mark_clean(&mut self) {
        self.state.buffer.mark_clean();
    }

    /// Save the buffer to the file specified in FileInfo.
    pub fn save(&mut self, cx: &mut Context<Self>) {
        let file_info = FileInfo::global(cx);
        let path = file_info.path.clone();
        let content = self.state.buffer.text();

        if let Err(e) = std::fs::write(&path, &content) {
            eprintln!("Failed to save file: {}", e);
            return;
        }

        // Record the mtime of the file we just wrote so the file watcher can
        // tell our own save apart from an external edit and skip reloading it
        // (which would otherwise force a needless HEAD reload + diff recompute).
        if let Ok(metadata) = std::fs::metadata(&path) {
            self.last_save_mtime = metadata.modified().ok();
        }

        self.state.buffer.mark_clean();
        cx.notify();
    }

    /// Returns true if there are actions to undo.
    pub fn can_undo(&self) -> bool {
        self.state.buffer.can_undo()
    }

    /// Returns true if there are actions to redo.
    pub fn can_redo(&self) -> bool {
        self.state.buffer.can_redo()
    }

    /// Undo the last action.
    pub fn undo(&mut self, cx: &mut Context<Self>) {
        if let Some(cursor_pos) = self.state.buffer.undo() {
            self.state.selection = Selection::new(cursor_pos, cursor_pos);
            cx.notify();
        }
    }

    /// Redo the last undone action.
    pub fn redo(&mut self, cx: &mut Context<Self>) {
        if let Some(cursor_pos) = self.state.buffer.redo() {
            self.state.selection = Selection::new(cursor_pos, cursor_pos);
            cx.notify();
        }
    }

    /// Execute an editor action programmatically.
    ///
    /// This is useful for scripted demos or external control of the editor.
    /// Bypasses `input_blocked` check - use `handle_action` for user input.
    pub fn execute(&mut self, action: &EditorAction, _window: &mut Window, cx: &mut Context<Self>) {
        self.execute_action(action, cx);
    }

    /// Handle an action from GPUI dispatch (respects input_blocked).
    pub fn handle_action(&mut self, action: &EditorAction, cx: &mut Context<Self>) {
        if self.input_blocked {
            // Allow hover updates even when input is blocked
            if let EditorAction::UpdateHover {
                over_checkbox,
                over_link,
                ref hovered_github_ref_range,
                ref hovered_ref_position,
            } = *action
                && (self.hovering_checkbox != over_checkbox
                    || self.hovering_link_region != over_link
                    || self.hovered_github_ref_range != *hovered_github_ref_range)
            {
                self.hovering_checkbox = over_checkbox;
                self.hovering_link_region = over_link;
                self.hovered_github_ref_range = hovered_github_ref_range.clone();
                self.hovered_ref_position = *hovered_ref_position;
                cx.notify();
            }
            return;
        }
        self.execute_action(action, cx);
    }

    /// Internal action execution (no input_blocked check).
    fn execute_action(&mut self, action: &EditorAction, cx: &mut Context<Self>) {
        match action {
            EditorAction::Type(c) => {
                self.insert_text(&c.to_string());
            }
            EditorAction::Enter => {
                self.enter();
            }
            EditorAction::ShiftEnter => {
                self.shift_enter();
            }
            EditorAction::ShiftAltEnter => {
                self.shift_alt_enter();
            }
            EditorAction::Tab => {
                self.tab();
            }
            EditorAction::ShiftTab => {
                self.shift_tab();
            }
            EditorAction::Backspace => {
                self.delete_backward();
            }
            EditorAction::Move(direction) => {
                self.move_in_direction(direction.clone(), false);
            }
            EditorAction::Click {
                offset,
                shift,
                click_count,
            } => {
                self.state.handle_click(*offset, *shift, *click_count);
            }
            EditorAction::Drag { offset } => {
                if !self.in_drag_scroll_zone {
                    self.state.handle_drag(*offset);
                    self.is_selecting = true;
                }
            }
            EditorAction::ToggleCheckbox { line_number } => {
                self.toggle_checkbox(*line_number, cx);
                return; // toggle_checkbox calls cx.notify() itself
            }
            EditorAction::UpdateHover {
                over_checkbox,
                over_link,
                hovered_github_ref_range,
                hovered_ref_position,
            } => {
                if self.hovering_checkbox != *over_checkbox
                    || self.hovering_link_region != *over_link
                    || self.hovered_github_ref_range != *hovered_github_ref_range
                {
                    self.hovering_checkbox = *over_checkbox;
                    self.hovering_link_region = *over_link;
                    self.hovered_github_ref_range = hovered_github_ref_range.clone();
                    self.hovered_ref_position = *hovered_ref_position;
                    cx.notify();
                }
                return; // Only notify if hover state actually changed
            }
            EditorAction::OpenLink { url } => {
                let _ = open::that(url);
                return; // Opening a link doesn't change editor state
            }
        }
        cx.notify();
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let buffer_version = self.state.buffer.version();
        if buffer_version != self.last_synced_version {
            self.last_synced_version = buffer_version;
            self.sync_list_state(cx);
            // Refresh the inline git diff against the cached HEAD base. Runs once
            // per edit batch (not per keystroke) and reuses the cached base snapshot.
            self.recompute_diff();
        }

        // Update global file info (dirty state for title bar)
        let file_info = FileInfo::global(cx);
        if file_info.dirty != self.state.buffer.is_dirty() {
            cx.set_global(FileInfo {
                path: file_info.path.clone(),
                dirty: self.state.buffer.is_dirty(),
            });
        }

        // Update status bar info
        let cursor_offset = self.state.cursor().offset;
        let cursor_line = self.state.buffer.byte_to_line(cursor_offset);
        let line_start = self.state.buffer.line_to_byte(cursor_line);
        let cursor_col = cursor_offset - line_start;
        // Build full nested context by walking up the tree
        let context_markers = self.state.build_nested_context(cursor_offset);
        let heading_level = self.find_current_heading(cursor_line);
        let total_lines = self.state.buffer.line_count();

        let (first_visible_line, last_visible_line) = self.visible_line_range(total_lines);

        // Detect GitHub refs and naked URLs in visible lines. Detection walks
        // every visible line with regexes and takes a fresh snapshot, so only
        // redo it when the content or the visible range actually changes —
        // render also runs on cursor blink, hover, scroll and selection. This
        // is what makes ref validation edit/scroll-driven rather than
        // per-repaint; the network spawns are additionally deduped by the
        // validation cache, so they never fire for an already-seen ref.
        let detect_key = (buffer_version, first_visible_line, last_visible_line);
        if self.last_link_detect_key != Some(detect_key) {
            self.last_link_detect_key = Some(detect_key);
            let (github_matches_by_line, naked_urls_by_line) =
                self.detect_links(first_visible_line, last_visible_line + 1);
            self.spawn_github_validation(&github_matches_by_line, cx);
            self.spawn_naked_url_validation(&naked_urls_by_line, cx);
            // Store refs for autocomplete and atomic cursor movement.
            self.github_refs_by_line = github_matches_by_line;
            self.naked_urls_by_line = naked_urls_by_line;
        }

        // Owned copies for the 'static per-line render closure below. These maps
        // only hold entries for visible lines that contain refs/URLs, so they're
        // usually empty or tiny — cheap next to the gated detection above.
        let github_matches_by_line = self.github_refs_by_line.clone();
        let naked_urls_by_line = self.naked_urls_by_line.clone();

        // Update autocomplete only when cursor position changed (not on every render)
        let cursor_offset_changed = self.last_cursor_offset != Some(cursor_offset);
        self.last_cursor_offset = Some(cursor_offset);
        if cursor_offset_changed && self.update_autocomplete_from_cursor() {
            self.fetch_autocomplete_suggestions_debounced(cx);
        }

        // Update global status bar info
        let new_status_bar_info = StatusBarInfo {
            context_markers,
            heading_level,
            cursor_line: cursor_line + 1, // 1-indexed
            cursor_col: cursor_col + 1,   // 1-indexed
            total_lines,
            first_visible_line,
            last_visible_line,
        };
        if new_status_bar_info != *StatusBarInfo::global(cx) {
            cx.set_global(new_status_bar_info);
        }

        let theme = self.config.theme.clone();
        let code_font = font(&self.config.code_font);

        let text_style = window.text_style();
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let measure_run = gpui::TextRun {
            len: 1,
            font: code_font.clone(),
            color: gpui::transparent_black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window
            .text_system()
            .shape_line(" ".into(), font_size, &[measure_run], None);
        let monospace_char_width = shaped.width;

        let line_theme = LineTheme {
            text_color: theme.foreground,
            cursor_color: theme.purple,
            link_color: theme.cyan,
            selection_color: theme.selection,
            border_color: theme.comment,
            code_color: theme.pink,
            fence_color: theme.comment,
            fence_lang_color: theme.green,
            checkbox_unchecked_color: theme.orange,
            checkbox_checked_color: theme.green,
            text_font: font(&self.config.text_font),
            code_font,
            monospace_char_width,
            line_height: self.config.line_height,
        };

        // Diff colors for line backgrounds (faint) and inline word highlights (stronger).
        let with_alpha = |color: Hsla, a: f32| -> Rgba {
            let mut c = color;
            c.a = a;
            c.into()
        };
        let diff_deleted_bg = with_alpha(theme.red.into(), 0.05);
        let diff_added_bg = with_alpha(theme.green.into(), 0.05);
        let diff_deleted_inline = with_alpha(theme.red.into(), 0.25);
        let diff_added_inline = with_alpha(theme.green.into(), 0.25);

        // Only show cursor and selection when this editor is focused and input is not blocked
        let is_focused = self.focus_handle.is_focused(window);
        let show_cursor = is_focused && !self.input_blocked;
        let cursor_offset = self.state.selection.head;
        let selection_range = if show_cursor && !self.state.selection.is_collapsed() {
            Some(self.state.selection.range())
        } else {
            None
        };
        // Pass usize::MAX to hide cursor visually when not focused or input blocked
        let visual_cursor_offset = if show_cursor {
            cursor_offset
        } else {
            usize::MAX
        };

        let base_path = self.config.base_path.clone();

        let cursor_line = self.state.buffer.byte_to_line(cursor_offset);
        let cursor_line_changed = self.last_cursor_line != Some(cursor_line);
        self.last_cursor_line = Some(cursor_line);

        // When scroll is requested (e.g., typing) or in streaming mode,
        // check if cursor line is near the bottom edge and scroll.
        if self.scroll_to_cursor_pending {
            self.scroll_to_cursor_pending = false;
            let scroll_buffer = self.config.line_height.to_pixels(window.rem_size());
            if let Some(cursor_bounds) = self.list_state.bounds_for_item(cursor_line) {
                let viewport = self.list_state.viewport_bounds();
                let cursor_bottom = cursor_bounds.origin.y + cursor_bounds.size.height;
                let viewport_bottom = viewport.origin.y + viewport.size.height;
                // Only scroll if cursor is near bottom edge (within buffer zone)
                if cursor_bottom > viewport_bottom - scroll_buffer {
                    self.list_state.scroll_to_reveal_item(cursor_line);
                    self.list_state.scroll_by(scroll_buffer);
                }
            } else {
                self.list_state.scroll_to_reveal_item(cursor_line);
            }
        } else if cursor_line_changed {
            if let Some(cursor_bounds) = self.list_state.bounds_for_item(cursor_line) {
                let viewport = self.list_state.viewport_bounds();
                let cursor_top = cursor_bounds.origin.y;
                let cursor_bottom = cursor_top + cursor_bounds.size.height;
                let viewport_top = viewport.origin.y;
                let viewport_bottom = viewport_top + viewport.size.height;

                if cursor_top < viewport_top || cursor_bottom > viewport_bottom {
                    self.list_state.scroll_to_reveal_item(cursor_line);
                }
            } else {
                self.list_state.scroll_to_reveal_item(cursor_line);
            }
        }

        let line_theme_for_list = line_theme.clone();
        let theme_for_highlights = self.config.theme.clone();
        let padding_top = self.config.padding_top;
        let padding_bottom = self.config.padding_bottom;
        let max_line_width = self.config.max_line_width;
        let snapshot = self.state.buffer.render_snapshot();
        let diff_state = self.diff_state.clone();

        let github_cache = self.github_validation_cache.clone();
        let github_context = self.github_context.clone();
        let hovered_github_ref_range = self.hovered_github_ref_range.clone();
        let input_blocked = self.input_blocked;

        let editor_id = self.instance_id;
        let line_list = div().id(("line-list", editor_id)).size_full().child(
            list(self.list_state.clone(), move |ix, _window, _cx| {
                // Bounds check: the list state can transiently be out of sync
                // with a fresh snapshot during edits.
                if ix >= snapshot.line_count() {
                    return div().into_any_element();
                }

                // Helper to build a Line element from a snapshot
                let build_line = |snap: &RenderSnapshot,
                                  line_idx: usize,
                                  extra_styles: Vec<StyledRegion>,
                                  github_ref_ranges: Vec<Range<usize>>,
                                  hovered_ref_range: Option<Range<usize>>,
                                  line_background: Option<Rgba>,
                                  inline_highlight_ranges: Vec<Range<usize>>,
                                  inline_highlight_color: Option<Rgba>,
                                  block_input: bool|
                 -> Line {
                    let line_markers = snap.line_markers(line_idx);
                    let mut inline_styles = snap.inline_styles_for_line(line_idx);
                    inline_styles.extend(extra_styles);
                    inline_styles.sort_by_key(|s| s.full_range.start);

                    let code_highlights: Vec<_> = snap
                        .code_highlights_for_line(line_idx)
                        .iter()
                        .map(|span| {
                            (
                                span.clone(),
                                theme_for_highlights.color_for_highlight(span.highlight_id),
                            )
                        })
                        .collect();

                    Line::new(
                        line_markers,
                        snap.rope.clone(),
                        if block_input {
                            usize::MAX
                        } else {
                            visual_cursor_offset
                        },
                        inline_styles,
                        line_theme_for_list.clone(),
                        if block_input {
                            None
                        } else {
                            selection_range.clone()
                        },
                        code_highlights,
                        base_path.clone(),
                        github_ref_ranges,
                        hovered_ref_range,
                        block_input || input_blocked,
                        max_line_width,
                        line_background,
                        inline_highlight_ranges,
                        inline_highlight_color,
                    )
                };

                // Collect GitHub reference links and URLs for this line
                let mut extra_styles = Vec::new();
                let mut github_ref_ranges: Vec<Range<usize>> =
                    if let Some(github_matches) = github_matches_by_line.get(&ix) {
                        let github_styles =
                            github_refs_to_styled_regions(github_matches, &github_cache);
                        extra_styles.extend(github_styles);
                        github_matches
                            .iter()
                            .map(|m| m.byte_range.clone())
                            .collect()
                    } else {
                        Vec::new()
                    };

                if let Some(naked_urls) = naked_urls_by_line.get(&ix) {
                    let url_styles = naked_urls_to_styled_regions(
                        naked_urls,
                        &github_cache,
                        github_context.as_ref(),
                    );
                    extra_styles.extend(url_styles);

                    for url in naked_urls {
                        if url.github_ref.is_some() {
                            github_ref_ranges.push(url.byte_range.clone());
                        }
                    }
                }

                let hovered_ref_on_this_line = hovered_github_ref_range.as_ref().and_then(|hr| {
                    if github_ref_ranges.iter().any(|r| r == hr) {
                        Some(hr.clone())
                    } else {
                        None
                    }
                });

                // Determine line background and inline highlight colors
                let is_addition = diff_state
                    .as_ref()
                    .map(|ds| ds.is_addition(ix))
                    .unwrap_or(false);

                let line_bg = if is_addition {
                    Some(diff_added_bg)
                } else {
                    None
                };

                // Get inline diff ranges for this line (word-level changes)
                let inline_highlight_ranges: Vec<Range<usize>> = diff_state
                    .as_ref()
                    .and_then(|ds| ds.new_inline_changes(ix))
                    .map(|changes| changes.iter().map(|c| c.range.clone()).collect())
                    .unwrap_or_default();
                let inline_highlight_color = if !inline_highlight_ranges.is_empty() {
                    Some(diff_added_inline)
                } else {
                    None
                };

                // Build the main line element
                let line_element = build_line(
                    &snapshot,
                    ix,
                    extra_styles,
                    github_ref_ranges,
                    hovered_ref_on_this_line,
                    line_bg,
                    inline_highlight_ranges,
                    inline_highlight_color,
                    false, // don't block input for main lines
                );

                // Check for ghost lines (deleted lines from old snapshot) before this line
                let ghost_line_elements: Vec<_> = if let Some(ds) = diff_state.as_ref() {
                    if let Some(old_line_range) = ds.ghost_lines_before(ix) {
                        old_line_range
                            .filter_map(|old_ix| {
                                if old_ix >= ds.old_snapshot.line_count() {
                                    return None;
                                }
                                // Get inline diff ranges for this ghost line
                                let ghost_inline_ranges: Vec<Range<usize>> = ds
                                    .old_inline_changes(old_ix)
                                    .map(|changes| {
                                        changes.iter().map(|c| c.range.clone()).collect()
                                    })
                                    .unwrap_or_default();
                                let ghost_inline_color = if !ghost_inline_ranges.is_empty() {
                                    Some(diff_deleted_inline)
                                } else {
                                    None
                                };
                                Some(build_line(
                                    &ds.old_snapshot,
                                    old_ix,
                                    Vec::new(),
                                    Vec::new(),
                                    None,
                                    Some(diff_deleted_bg), // ghost line background
                                    ghost_inline_ranges,
                                    ghost_inline_color,
                                    true, // block input for ghost lines
                                ))
                            })
                            .collect()
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };

                // Add top padding to first line, bottom padding to last line
                let is_first = ix == 0;
                let is_last = ix == snapshot.line_count().saturating_sub(1);
                div()
                    .when(is_first, |d| d.pt(padding_top))
                    .when(is_last, |d| d.pb(padding_bottom))
                    .children(ghost_line_elements)
                    .child(line_element)
                    .into_any_element()
            })
            .size_full(),
        );

        div()
            .id(("editor", editor_id))
            .track_focus(&self.focus_handle)
            .key_context("Editor")
            .on_key_down(cx.listener(Self::on_key_down))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .on_action(cx.listener(
                |editor: &mut Editor, action: &DispatchEditorAction, _window, cx| {
                    editor.handle_action(&action.0, cx);
                },
            ))
            // IMPORTANT: Use capture phase to focus this editor BEFORE child elements
            // (Line components) handle mouse events. This ensures DispatchEditorAction
            // from Line click handlers will be routed to THIS editor.
            // Don't focus if input is blocked (read-only mode).
            .capture_any_mouse_down(cx.listener(
                |editor, _event: &gpui::MouseDownEvent, window, cx| {
                    if !editor.input_blocked {
                        editor.focus_handle.focus(window, cx);
                    }
                },
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|editor, event: &gpui::MouseDownEvent, window, cx| {
                    // Focus already handled in capture phase above

                    if editor.input_blocked {
                        return;
                    }
                    // Only handle if not already handled by a line element
                    // (lines call prevent_default but don't stop propagation to allow on_drag)
                    if window.default_prevented() {
                        return;
                    }
                    // Check if click is below the last line (empty space at bottom)
                    // Only then do we jump cursor to end of buffer
                    let line_count = editor.state.buffer.line_count();
                    if line_count > 0 {
                        if let Some(last_line_bounds) =
                            editor.list_state.bounds_for_item(line_count - 1)
                        {
                            let last_line_bottom =
                                last_line_bounds.origin.y + last_line_bounds.size.height;
                            if event.position.y <= last_line_bottom {
                                // Click is in side margins at height of existing content - ignore
                                return;
                            }
                        } else {
                            // Last line not visible/measured - ignore click
                            return;
                        }
                    }
                    // Click is in empty space below content
                    let end = editor.state.buffer.len_bytes();
                    editor.state.selection = Selection::new(end, end);
                    editor.request_scroll_to_cursor();
                    window.refresh();
                    cx.notify();
                }),
            )
            .on_drag(SelectionDrag, |_drag, _point, _window, cx| {
                // Return an empty view - we don't need a visible drag indicator
                cx.new(|_| EmptyDragView)
            })
            .on_drag_move(cx.listener(
                |editor, event: &DragMoveEvent<SelectionDrag>, window, cx| {
                    use std::time::{Duration, Instant};

                    // When dragging near viewport edges, move cursor to trigger auto-scroll
                    let viewport = editor.list_state.viewport_bounds();
                    let mouse_y = event.event.position.y;

                    // Get window bounds to handle maximized windows
                    let window_bounds = window.bounds();

                    // Create "hot zones" at the edges that trigger scrolling
                    // Zone size is one line height - scrolling triggers when mouse enters
                    // this margin or goes past the viewport entirely
                    let zone_size = editor.config.line_height.to_pixels(window.rem_size());

                    // For top: use viewport top (content starts there)
                    let top_threshold = viewport.origin.y + zone_size;

                    // For bottom: use the smaller of viewport bottom or window bottom
                    // This handles maximized windows where viewport == window
                    let viewport_bottom = viewport.origin.y + viewport.size.height;
                    let window_bottom = window_bounds.origin.y + window_bounds.size.height;
                    let effective_bottom = viewport_bottom.min(window_bottom);
                    let bottom_threshold = effective_bottom - zone_size;

                    // Calculate distance outside the inset bounds and direction
                    let (delta, direction): (f32, i32) = if mouse_y < top_threshold {
                        ((top_threshold - mouse_y).into(), -1) // up
                    } else if mouse_y > bottom_threshold {
                        ((mouse_y - bottom_threshold).into(), 1) // down
                    } else {
                        // Mouse is inside safe zone - reset throttle and allow line's on_drag
                        editor.last_drag_scroll = None;
                        editor.in_drag_scroll_zone = false;
                        return;
                    };

                    // We're in the scroll zone - prevent line's on_drag from resetting selection
                    editor.in_drag_scroll_zone = true;

                    // Throttle inversely proportional to distance
                    // Close to edge: ~30ms, far from edge: ~8ms
                    let speed_factor = (delta.powf(1.2) / 50.0).clamp(0.5, 6.0);
                    let throttle_ms = (30.0 / speed_factor) as u64;
                    let throttle = Duration::from_millis(throttle_ms.clamp(8, 50));

                    let now = Instant::now();
                    if let Some(last) = editor.last_drag_scroll
                        && now.duration_since(last) < throttle
                    {
                        return;
                    }
                    editor.last_drag_scroll = Some(now);

                    // Scroll by one line height in the appropriate direction
                    // Using scroll_by instead of scroll_to_reveal_item gives smoother
                    // scrolling through wrapped lines (doesn't jump entire item)
                    let scroll_amount = if direction < 0 { -zone_size } else { zone_size };
                    editor.list_state.scroll_by(scroll_amount);

                    // Move cursor one line in the appropriate direction
                    let cursor = editor.state.selection.cursor();
                    let new_cursor = if direction < 0 {
                        cursor.move_up(&editor.state.buffer)
                    } else {
                        cursor.move_down(&editor.state.buffer)
                    };
                    editor.state.selection = editor.state.selection.extend_to(new_cursor.offset);
                    cx.notify();
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|editor, _event: &gpui::MouseUpEvent, _window, cx| {
                    // Reset is_selecting when mouse is released
                    if editor.is_selecting {
                        editor.is_selecting = false;
                        cx.notify();
                    }
                }),
            )
            .size_full()
            .px(self.config.padding_x)
            .font(line_theme.text_font.clone())
            .text_color(line_theme.text_color)
            .cursor(
                if self.hovering_checkbox || (self.hovering_link_region && self.ctrl_held) {
                    CursorStyle::PointingHand
                } else {
                    CursorStyle::IBeam
                },
            )
            .child(line_list)
            .children(self.render_autocomplete(&line_theme, window, cx))
            .children(self.render_github_ref_hover(&line_theme, window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trim leading newline from raw string literals for readability.
    /// Allows writing:
    /// ```
    /// r#"
    /// - item one
    /// - item two
    /// "#
    /// ```
    fn trim_raw(s: &str) -> &str {
        s.strip_prefix('\n').unwrap_or(s)
    }

    /// Helper to create an EditorState with cursor at a specific position.
    /// The cursor position is indicated by | in the input string.
    fn editor_with_cursor(input: &str) -> EditorState {
        let input = trim_raw(input);
        let cursor_pos = input
            .find('|')
            .expect("Input must contain | for cursor position");
        let content = input.replace('|', "");
        let mut state = EditorState::new(&content);
        state.set_cursor(cursor_pos);
        state
    }

    /// Helper to check editor state matches expected content with cursor.
    fn assert_editor_eq(state: &EditorState, expected: &str) {
        let expected = trim_raw(expected);
        let text = state.text();
        let cursor = state.cursor().offset;
        let mut actual = String::new();
        actual.push_str(&text[..cursor]);
        actual.push('|');
        actual.push_str(&text[cursor..]);
        assert_eq!(actual, expected);
    }

    /// Helper to check editor state with selection.
    /// Format: `<` marks start of selection, `|` marks head (cursor), `>` marks end.
    /// Examples:
    ///   - `|hello` - cursor at start, no selection
    ///   - `<hello|>` - "hello" selected, cursor at end
    ///   - `<|hello>` - "hello" selected, cursor at start
    fn assert_selection_eq(state: &EditorState, expected: &str) {
        let expected = trim_raw(expected);
        let text = state.text();
        let selection = &state.selection;

        let anchor = selection.anchor;
        let head = selection.head;
        let start = anchor.min(head);
        let end = anchor.max(head);
        let is_collapsed = anchor == head;

        let mut actual = String::new();
        let mut byte_pos = 0;

        for c in text.chars() {
            if !is_collapsed && byte_pos == start {
                actual.push('<');
            }
            if byte_pos == head {
                actual.push('|');
            }
            if !is_collapsed && byte_pos == end {
                actual.push('>');
            }
            actual.push(c);
            byte_pos += c.len_utf8();
        }

        // Handle markers at end of text
        if !is_collapsed && byte_pos == start {
            actual.push('<');
        }
        if byte_pos == head {
            actual.push('|');
        }
        if !is_collapsed && byte_pos == end {
            actual.push('>');
        }

        assert_eq!(actual, expected, "Selection mismatch");
    }

    mod click_tests {
        use super::*;

        #[test]
        fn click_sets_cursor() {
            let mut state = editor_with_cursor("hello| world");
            state.handle_click(0, false, 1);
            assert_editor_eq(&state, "|hello world");
        }

        #[test]
        fn click_middle() {
            let mut state = editor_with_cursor("|hello world");
            state.handle_click(6, false, 1);
            assert_editor_eq(&state, "hello |world");
        }

        #[test]
        fn shift_click_extends_selection() {
            let mut state = editor_with_cursor("hello| world");
            state.handle_click(11, true, 1);
            assert_selection_eq(&state, "hello< world|>");
        }

        #[test]
        fn shift_click_backward() {
            let mut state = editor_with_cursor("hello| world");
            state.handle_click(0, true, 1);
            assert_selection_eq(&state, "<|hello> world");
        }

        #[test]
        fn double_click_selects_word() {
            let mut state = editor_with_cursor("|hello world");
            state.handle_click(2, false, 2);
            assert_selection_eq(&state, "<hello|> world");
        }

        #[test]
        fn double_click_second_word() {
            let mut state = editor_with_cursor("|hello world");
            state.handle_click(8, false, 2);
            assert_selection_eq(&state, "hello <world|>");
        }

        #[test]
        fn triple_click_selects_line() {
            let mut state = editor_with_cursor("|hello world");
            state.handle_click(2, false, 3);
            assert_selection_eq(&state, "<hello world|>");
        }

        #[test]
        fn drag_extends_selection() {
            let mut state = editor_with_cursor("|hello world");
            state.handle_click(0, false, 1);
            state.handle_drag(5);
            assert_selection_eq(&state, "<hello|> world");
        }

        #[test]
        fn drag_backward() {
            let mut state = editor_with_cursor("hello world|");
            state.handle_click(11, false, 1);
            state.handle_drag(6);
            assert_selection_eq(&state, "hello <|world>");
        }
    }

    mod cursor_movement_tests {
        use super::*;

        #[test]
        fn move_left() {
            let mut state = editor_with_cursor("hel|lo");
            state.move_left();
            assert_editor_eq(&state, "he|llo");
        }

        #[test]
        fn move_left_at_start() {
            let mut state = editor_with_cursor("|hello");
            state.move_left();
            assert_editor_eq(&state, "|hello");
        }

        #[test]
        fn move_right() {
            let mut state = editor_with_cursor("he|llo");
            state.move_right();
            assert_editor_eq(&state, "hel|lo");
        }

        #[test]
        fn move_right_at_end() {
            let mut state = editor_with_cursor("hello|");
            state.move_right();
            assert_editor_eq(&state, "hello|");
        }

        #[test]
        fn move_up() {
            let mut state = editor_with_cursor("line one\nline |two\nline three");
            state.move_up();
            assert_editor_eq(&state, "line |one\nline two\nline three");
        }

        #[test]
        fn move_up_from_first_line() {
            let mut state = editor_with_cursor("hel|lo\nworld");
            state.move_up();
            assert_editor_eq(&state, "|hello\nworld");
        }

        #[test]
        fn move_down() {
            let mut state = editor_with_cursor("line |one\nline two\nline three");
            state.move_down();
            assert_editor_eq(&state, "line one\nline |two\nline three");
        }

        #[test]
        fn move_down_from_last_line() {
            let mut state = editor_with_cursor("hello\nwor|ld");
            state.move_down();
            assert_editor_eq(&state, "hello\nworld|");
        }

        #[test]
        fn move_up_preserves_column() {
            let mut state = editor_with_cursor("short\nlonger line|");
            state.move_up();
            assert_editor_eq(&state, "short|\nlonger line");
        }

        #[test]
        fn move_to_line_start() {
            let mut state = editor_with_cursor("hello\nwor|ld");
            state.move_to_line_start();
            assert_editor_eq(&state, "hello\n|world");
        }

        #[test]
        fn move_to_line_end() {
            let mut state = editor_with_cursor("hello\nwor|ld");
            state.move_to_line_end();
            assert_editor_eq(&state, "hello\nworld|");
        }
    }

    // ========================================================================
    // New "raw markdown" behavior tests
    // These test the simplified, non-controlling editing paradigm.
    // ========================================================================

    mod raw_enter_tests {
        use super::*;

        // --- Enter: always raw \n ---

        #[test]
        fn enter_on_paragraph_inserts_newline() {
            let mut state = editor_with_cursor("Hello world|");
            state.enter();
            assert_editor_eq(&state, "Hello world\n|");
        }

        #[test]
        fn enter_on_heading_inserts_newline() {
            let mut state = editor_with_cursor("# Hello|");
            state.enter();
            assert_editor_eq(&state, "# Hello\n|");
        }

        #[test]
        fn enter_on_list_item_inserts_newline_no_marker() {
            let mut state = editor_with_cursor("- item one|");
            state.enter();
            assert_editor_eq(&state, "- item one\n|");
        }

        #[test]
        fn enter_on_blockquote_inserts_newline_no_marker() {
            let mut state = editor_with_cursor("> quote|");
            state.enter();
            assert_editor_eq(&state, "> quote\n|");
        }

        #[test]
        fn enter_on_nested_container_inserts_newline_no_markers() {
            let mut state = editor_with_cursor("> - item|");
            state.enter();
            assert_editor_eq(&state, "> - item\n|");
        }

        #[test]
        fn enter_on_empty_list_item_inserts_newline_keeps_marker() {
            let mut state = editor_with_cursor("- item one\n- |");
            state.enter();
            assert_editor_eq(&state, "- item one\n- \n|");
        }

        #[test]
        fn enter_on_empty_blockquote_inserts_newline_keeps_marker() {
            let mut state = editor_with_cursor("> quote one\n> |");
            state.enter();
            assert_editor_eq(&state, "> quote one\n> \n|");
        }

        #[test]
        fn enter_in_code_block_inserts_newline() {
            let mut state = editor_with_cursor("```rust\nlet x = 1;|");
            state.enter();
            assert_editor_eq(&state, "```rust\nlet x = 1;\n|");
        }

        #[test]
        fn enter_on_code_fence_inserts_newline() {
            let mut state = editor_with_cursor("```rust|");
            state.enter();
            assert_editor_eq(&state, "```rust\n|");
        }

        #[test]
        fn enter_preserves_soft_wrap_style() {
            // Adjacent lines without blank line between them
            let mut state = editor_with_cursor("First sentence.\nSecond sentence.|");
            state.enter();
            assert_editor_eq(&state, "First sentence.\nSecond sentence.\n|");
        }

        // --- Shift+Enter: continue container ---

        #[test]
        fn shift_enter_on_list_item_continues_list() {
            let mut state = editor_with_cursor("- item one|");
            state.shift_enter();
            assert_editor_eq(&state, "- item one\n- |");
        }

        #[test]
        fn shift_enter_on_blockquote_continues_blockquote() {
            let mut state = editor_with_cursor("> quote|");
            state.shift_enter();
            assert_editor_eq(&state, "> quote\n> |");
        }

        #[test]
        fn shift_enter_on_nested_container_continues_all() {
            let mut state = editor_with_cursor("> - item|");
            state.shift_enter();
            assert_editor_eq(&state, "> - item\n> - |");
        }

        #[test]
        fn shift_enter_on_paragraph_just_inserts_newline() {
            let mut state = editor_with_cursor("Hello world|");
            state.shift_enter();
            assert_editor_eq(&state, "Hello world\n|");
        }

        #[test]
        fn shift_enter_on_heading_just_inserts_newline() {
            let mut state = editor_with_cursor("# Hello|");
            state.shift_enter();
            assert_editor_eq(&state, "# Hello\n|");
        }

        // --- Shift+Alt+Enter: indented continuation ---

        #[test]
        fn shift_alt_enter_on_list_item_creates_indent() {
            let mut state = editor_with_cursor("- item one|");
            state.shift_alt_enter();
            assert_editor_eq(&state, "- item one\n  |");
        }

        #[test]
        fn shift_alt_enter_on_blockquote_creates_indent_outside() {
            let mut state = editor_with_cursor("> quote|");
            state.shift_alt_enter();
            assert_editor_eq(&state, "> quote\n  |");
        }

        #[test]
        fn shift_alt_enter_on_nested_container_creates_indent_inside() {
            let mut state = editor_with_cursor("> - item|");
            state.shift_alt_enter();
            assert_editor_eq(&state, "> - item\n>   |");
        }

        #[test]
        fn shift_alt_enter_on_paragraph_just_inserts_newline() {
            let mut state = editor_with_cursor("Hello world|");
            state.shift_alt_enter();
            assert_editor_eq(&state, "Hello world\n|");
        }
    }

    mod raw_backspace_tests {
        use super::*;

        #[test]
        fn backspace_deletes_char() {
            let mut state = editor_with_cursor("hello|");
            state.delete_backward();
            assert_editor_eq(&state, "hell|");
        }

        #[test]
        fn backspace_at_line_start_joins_lines() {
            let mut state = editor_with_cursor("line one\n|line two");
            state.delete_backward();
            assert_editor_eq(&state, "line one|line two");
        }

        #[test]
        fn backspace_deletes_entire_list_marker() {
            let mut state = editor_with_cursor("- |");
            state.delete_backward();
            assert_editor_eq(&state, "|");
        }

        #[test]
        fn backspace_deletes_innermost_marker_first() {
            let mut state = editor_with_cursor("> - |");
            state.delete_backward();
            assert_editor_eq(&state, "> |");
        }

        #[test]
        fn backspace_then_deletes_outer_marker() {
            let mut state = editor_with_cursor("> |");
            state.delete_backward();
            assert_editor_eq(&state, "|");
        }

        #[test]
        fn backspace_deletes_entire_indent() {
            // Indent after list item is atomic - need context for tree-sitter to recognize it
            let mut state = editor_with_cursor("- item\n  |text");
            state.delete_backward();
            assert_editor_eq(&state, "- item\n|text");
        }

        #[test]
        fn backspace_in_middle_of_text_deletes_char() {
            let mut state = editor_with_cursor("- item o|ne");
            state.delete_backward();
            assert_editor_eq(&state, "- item |ne");
        }

        #[test]
        fn backspace_on_empty_line_after_list_joins() {
            let mut state = editor_with_cursor("- item one\n|");
            state.delete_backward();
            assert_editor_eq(&state, "- item one|");
        }

        #[test]
        fn backspace_sequence_through_markers_and_join() {
            // Start: "- item one\n- |"
            // Backspace 1: delete "- " marker -> "- item one\n|"
            // Backspace 2: join lines -> "- item one|"
            let mut state = editor_with_cursor("- item one\n- |");
            state.delete_backward();
            assert_editor_eq(&state, "- item one\n|");
            state.delete_backward();
            assert_editor_eq(&state, "- item one|");
        }

        #[test]
        fn backspace_with_content_after_cursor_deletes_marker() {
            let mut state = editor_with_cursor("- |two");
            state.delete_backward();
            assert_editor_eq(&state, "|two");
        }

        #[test]
        fn backspace_deletes_entire_task_list_marker() {
            // Task list now has separate Checkbox and ListItem markers
            // First backspace deletes the checkbox, second deletes the list marker
            let mut state = editor_with_cursor("- [ ] |");
            state.delete_backward();
            assert_editor_eq(&state, "- |");
            state.delete_backward();
            assert_editor_eq(&state, "|");
        }

        #[test]
        fn backspace_deletes_checked_task_list_marker() {
            let mut state = editor_with_cursor("- [x] |");
            state.delete_backward();
            assert_editor_eq(&state, "- |");
            state.delete_backward();
            assert_editor_eq(&state, "|");
        }
    }

    mod raw_tab_tests {
        use super::*;

        // --- Tab cycling through states ---
        // Tree-based: cycle is marker → (para indent if blank) → nested marker → empty

        #[test]
        fn tab_on_empty_line_after_list_adds_marker() {
            // Blank line cycle: ["- ", "  ", "  - ", ""]
            let mut state = editor_with_cursor("- item\n|");
            state.tab();
            assert_editor_eq(&state, "- item\n- |");
        }

        #[test]
        fn tab_twice_after_list_adds_nested_marker() {
            // Cycle is: "" -> "- " -> "  " -> "  - " -> ""
            let mut state = editor_with_cursor("- item\n|");
            state.tab();
            state.tab();
            assert_editor_eq(&state, "- item\n  |"); // para indent
            state.tab();
            assert_editor_eq(&state, "- item\n  - |"); // nested marker
        }

        #[test]
        fn tab_three_times_cycles_back() {
            // Cycle is: "" -> "- " -> "  " -> "  - " -> "" (4 states)
            let mut state = editor_with_cursor("- item\n|");
            state.tab();
            state.tab();
            state.tab();
            state.tab();
            assert_editor_eq(&state, "- item\n|");
        }

        #[test]
        fn tab_cycles_ordered_list_after_checkbox() {
            // Bug case: ordered list preceded by checkbox content
            // Cycle should be: "" -> "2. " -> "   " -> "   1. " -> "" (4 states)
            let mut state = editor_with_cursor("## Writ\n- [ ] item\n\n1. hey\n|");

            state.tab();
            assert_editor_eq(&state, "## Writ\n- [ ] item\n\n1. hey\n2. |");

            state.tab();
            assert_editor_eq(&state, "## Writ\n- [ ] item\n\n1. hey\n   |"); // para indent

            state.tab();
            assert_editor_eq(&state, "## Writ\n- [ ] item\n\n1. hey\n   1. |");

            state.tab();
            assert_editor_eq(&state, "## Writ\n- [ ] item\n\n1. hey\n|");
        }

        #[test]
        fn tab_indents_line_with_content() {
            // Tab should cycle the prefix even when there's content after it
            // Content is preserved and cursor stays in place relative to content
            let mut state = editor_with_cursor("1. hey\n2. asdf|");
            state.tab();
            assert_editor_eq(&state, "1. hey\n   asdf|"); // para indent, content preserved
            state.tab();
            assert_editor_eq(&state, "1. hey\n   1. asdf|"); // nested, content preserved
        }

        #[test]
        fn tab_preserves_unchecked_checkbox_state() {
            // Tab cycling preserves the current line's checkbox state
            // Propagation doesn't happen because tree-sitter can't parse incomplete lines
            // Cycle: "" -> "- [ ] " -> "  " -> "  - [ ] " -> ""
            let mut state = editor_with_cursor("- [x] hey\n- [ ] |");
            state.tab();
            // Checkbox stays unchecked (from current line), no propagation
            assert_editor_eq(&state, "- [x] hey\n  |"); // para indent
            state.tab();
            assert_editor_eq(&state, "- [x] hey\n  - [ ] |"); // nested
            state.tab();
            assert_editor_eq(&state, "- [x] hey\n|");
            state.tab();
            assert_editor_eq(&state, "- [x] hey\n- [ ] |");
        }

        #[test]
        fn tab_preserves_checked_checkbox_state() {
            // Tab cycling preserves the current line's checkbox state
            // Cycle: "" -> "- [x] " -> "  " -> "  - [x] " -> ""
            let mut state = editor_with_cursor("- [ ] hey\n- [x] |");
            state.tab();
            // Checkbox stays checked (from current line), no propagation
            assert_editor_eq(&state, "- [ ] hey\n  |"); // para indent
            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n  - [x] |"); // nested
            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n|");
            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n- [x] |");
        }

        #[test]
        fn tab_new_checkbox_defaults_unchecked() {
            // Starting from empty line, new checkboxes default to unchecked
            // Cycle: "" -> "- [ ] " -> "  " -> "  - [ ] " -> ""
            let mut state = editor_with_cursor("- [x] ~~hey~~\n|");
            state.tab(); // sibling: - [ ] |
            assert_editor_eq(&state, "- [x] ~~hey~~\n- [ ] |");
            state.tab(); // para indent
            assert_editor_eq(&state, "- [x] ~~hey~~\n  |");
            state.tab(); // nested: - [ ] |
            assert_editor_eq(&state, "- [x] ~~hey~~\n  - [ ] |");
        }

        #[test]
        fn typing_after_tab_propagates_checkbox() {
            // Tab creates incomplete line "- [ ] |" which tree-sitter can't parse.
            // Once we type content, tree-sitter recognizes it and propagation happens.
            // Cycle: "" -> "- [ ] " -> "  " -> "  - [ ] " -> ""
            let mut state = editor_with_cursor("- [x] hey\n|");
            state.tab(); // "- [ ] |" - incomplete, no propagation yet
            assert_editor_eq(&state, "- [x] hey\n- [ ] |");
            state.tab(); // para indent
            assert_editor_eq(&state, "- [x] hey\n  |");
            state.tab(); // nest it: "  - [ ] |"
            assert_editor_eq(&state, "- [x] hey\n  - [ ] |");
            // Type a character - now tree-sitter can parse, propagation unchecks parent
            state.insert_text("a");
            assert_editor_eq(&state, "- [ ] hey\n  - [ ] a|");
        }

        #[test]
        fn delete_backward_propagates_checkbox() {
            // Deleting content can affect checkbox propagation
            let mut state = editor_with_cursor("- [x] hey\n  - [ ] ab|");
            // Delete 'b' - still has content, propagation runs (parent stays unchecked)
            state.delete_backward();
            assert_editor_eq(&state, "- [ ] hey\n  - [ ] a|");
        }

        #[test]
        fn delete_forward_propagates_checkbox() {
            // Deleting content forward can affect checkbox propagation
            let mut state = editor_with_cursor("- [x] hey\n  - [ ] |ab");
            // Delete 'a' - still has content, propagation runs
            state.delete_forward();
            assert_editor_eq(&state, "- [ ] hey\n  - [ ] |b");
        }

        #[test]
        fn delete_checkbox_marker_rechecks_parent() {
            // Start with checked parent and one checked nested child
            // Cycle: "" -> "- [ ] " -> "  " -> "  - [ ] " -> ""
            let mut state = editor_with_cursor("- [x] ~~parent~~\n  - [x] ~~nested~~\n|");
            // Tab three times to create a new nested unchecked checkbox (with para indent now in cycle)
            state.tab();
            state.tab();
            state.tab();
            assert_editor_eq(&state, "- [x] ~~parent~~\n  - [x] ~~nested~~\n  - [ ] |");
            // Type to make it parseable - this should uncheck the parent
            state.insert_text("new");
            assert_editor_eq(&state, "- [ ] parent\n  - [x] ~~nested~~\n  - [ ] new|");
            // Now delete backwards to remove the unchecked child entirely
            // First delete the content
            state.delete_backward();
            state.delete_backward();
            state.delete_backward();
            assert_editor_eq(&state, "- [ ] parent\n  - [x] ~~nested~~\n  - [ ] |");
            // Delete the checkbox marker
            state.delete_backward();
            assert_editor_eq(&state, "- [ ] parent\n  - [x] ~~nested~~\n  - |");
            // Delete the list marker
            state.delete_backward();
            assert_editor_eq(&state, "- [x] ~~parent~~\n  - [x] ~~nested~~\n  |");
        }

        #[test]
        fn tab_with_blank_line_between_still_works() {
            // Tree-sitter includes blank lines in list_item
            let mut state = editor_with_cursor("- item\n\n|");
            state.tab();
            assert_editor_eq(&state, "- item\n\n- |");
        }

        #[test]
        fn tab_with_two_blank_lines_still_works() {
            // Tree-sitter includes multiple blank lines in list_item
            let mut state = editor_with_cursor("- item\n\n\n|");
            state.tab();
            assert_editor_eq(&state, "- item\n\n\n- |");
        }

        #[test]
        fn tab_on_blockquote_context_adds_marker() {
            let mut state = editor_with_cursor("> quote\n|");
            state.tab();
            assert_editor_eq(&state, "> quote\n> |");
        }

        #[test]
        fn tab_twice_on_blockquote_context_cycles_back() {
            let mut state = editor_with_cursor("> quote\n|");
            state.tab();
            state.tab();
            assert_editor_eq(&state, "> quote\n|");
        }

        #[test]
        fn tab_on_nested_context_cycles() {
            // Cycle: ["> ", "> - ", ">   ", ">   - ", ""]
            let mut state = editor_with_cursor("> - item\n|");

            state.tab();
            assert_editor_eq(&state, "> - item\n> |");

            state.tab();
            assert_editor_eq(&state, "> - item\n> - |");

            state.tab();
            assert_editor_eq(&state, "> - item\n>   |"); // para indent

            state.tab();
            assert_editor_eq(&state, "> - item\n>   - |");

            state.tab();
            assert_editor_eq(&state, "> - item\n|");
        }

        // --- Shift+Tab cycling backwards ---

        #[test]
        fn shift_tab_cycles_backwards() {
            // Cycle: ["- ", "  ", "  - ", ""]
            // Backwards from "" goes to "  - "
            let mut state = editor_with_cursor("- item\n|");
            state.shift_tab();
            assert_editor_eq(&state, "- item\n  - |");
        }

        #[test]
        fn shift_tab_from_marker_goes_to_empty() {
            let mut state = editor_with_cursor("- item\n- |");
            state.shift_tab();
            assert_editor_eq(&state, "- item\n|");
        }

        #[test]
        fn shift_tab_from_nested_marker_goes_to_marker() {
            // "  - " is nested list, cycle found via ERROR handling
            // Cycle backwards: "  - " -> "  " -> "- " -> ""
            let mut state = editor_with_cursor("- item\n  - |");
            state.shift_tab();
            assert_editor_eq(&state, "- item\n  |"); // para indent
            state.shift_tab();
            assert_editor_eq(&state, "- item\n- |");
        }

        #[test]
        fn tab_after_blank_line_includes_para_indent() {
            // With blank line, para indent should be in cycle
            // Cycle: ["- ", "  ", "  - ", "    ", "    - ", ""]
            let mut state = editor_with_cursor("- parent\n  - nested\n\n|");

            state.tab();
            assert_editor_eq(&state, "- parent\n  - nested\n\n- |");

            state.tab();
            assert_editor_eq(&state, "- parent\n  - nested\n\n  |"); // para indent

            state.tab();
            assert_editor_eq(&state, "- parent\n  - nested\n\n  - |");

            state.tab();
            assert_editor_eq(&state, "- parent\n  - nested\n\n    |"); // nested para indent

            state.tab();
            assert_editor_eq(&state, "- parent\n  - nested\n\n    - |");

            state.tab();
            assert_editor_eq(&state, "- parent\n  - nested\n\n|"); // back to empty
        }

        #[test]
        fn tab_no_blank_line_includes_para_indent() {
            // Para indent is now always in cycle, even without blank line
            // Cycle: ["- ", "  ", "  - ", "    ", "    - ", ""]
            let mut state = editor_with_cursor("- parent item\n  - nested with tab\n|");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n- |");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n  |"); // para indent

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n  - |");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n    |"); // nested para indent

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n    - |");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n|");
        }

        #[test]
        fn tab_with_trailing_newline() {
            // Cursor on line with newline after it - should still cycle correctly
            // Cycle: ["- ", "  ", "  - ", "    ", "    - ", ""]
            let mut state = editor_with_cursor("- parent item\n  - nested with tab\n|\n");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n- |\n");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n  |\n"); // para indent

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n  - |\n");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n    |\n"); // nested para indent

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n    - |\n");

            state.tab();
            assert_editor_eq(&state, "- parent item\n  - nested with tab\n|\n");
        }

        #[test]
        fn tab_task_list_uses_list_marker_width_not_full_marker() {
            // Task list "- [ ] " is 6 chars, but para indent should use list marker width (2)
            // Cycle: ["- [ ] ", "  ", "  - [ ] ", ""]
            let mut state = editor_with_cursor("- [ ] hey\n\n|");

            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n\n- [ ] |");

            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n\n  |"); // 2 spaces, not 6

            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n\n  - [ ] |"); // nested at 2 spaces

            state.tab();
            assert_editor_eq(&state, "- [ ] hey\n\n|");
        }
    }

    mod raw_cursor_movement_tests {
        use super::*;

        #[test]
        fn move_left_through_marker_is_atomic() {
            let mut state = editor_with_cursor("- |item");
            state.move_left();
            assert_editor_eq(&state, "|- item");
        }

        #[test]
        fn move_right_through_marker_is_atomic() {
            let mut state = editor_with_cursor("|- item");
            state.move_right();
            assert_editor_eq(&state, "- |item");
        }

        #[test]
        fn move_left_through_nested_markers_one_at_a_time() {
            let mut state = editor_with_cursor("> - |item");
            state.move_left();
            assert_editor_eq(&state, "> |- item");
            state.move_left();
            assert_editor_eq(&state, "|> - item");
        }

        #[test]
        fn move_left_does_not_skip_blank_lines() {
            let mut state = editor_with_cursor("line one\n\n|line three");
            state.move_left();
            assert_editor_eq(&state, "line one\n|\nline three");
        }

        #[test]
        fn move_left_from_blank_line_goes_to_previous() {
            let mut state = editor_with_cursor("line one\n|\nline three");
            state.move_left();
            assert_editor_eq(&state, "line one|\n\nline three");
        }

        #[test]
        fn move_up_maintains_column_in_content_area() {
            let mut state = editor_with_cursor("- item one\n- item |two");
            state.move_up();
            assert_editor_eq(&state, "- item |one\n- item two");
        }

        #[test]
        fn move_left_through_blockquote_ordered_list() {
            let mut state = editor_with_cursor("> 1. |");
            state.move_left();
            assert_editor_eq(&state, "> |1. ");
            state.move_left();
            assert_editor_eq(&state, "|> 1. ");
        }
    }

    mod checkbox_propagation_tests {
        use super::*;

        #[test]
        fn check_parent_checks_all_children() {
            let mut state = editor_with_cursor("- [ ] |parent\n  - [ ] child1\n  - [ ] child2\n");
            state.toggle_checkbox_for_test(0);
            let text = state.text();
            assert!(text.contains("[x] ~~parent~~"), "parent should be checked");
            assert!(text.contains("[x] ~~child1~~"), "child1 should be checked");
            assert!(text.contains("[x] ~~child2~~"), "child2 should be checked");
        }

        #[test]
        fn uncheck_parent_unchecks_all_children() {
            let mut state =
                editor_with_cursor("- [x] ~~|parent~~\n  - [x] ~~child1~~\n  - [x] ~~child2~~\n");
            state.toggle_checkbox_for_test(0);
            let text = state.text();
            assert!(text.contains("[ ] parent"), "parent should be unchecked");
            assert!(text.contains("[ ] child1"), "child1 should be unchecked");
            assert!(text.contains("[ ] child2"), "child2 should be unchecked");
            assert!(!text.contains("~~"), "no strikethrough should remain");
        }

        #[test]
        fn check_all_siblings_checks_parent() {
            let mut state =
                editor_with_cursor("- [ ] parent\n  - [x] ~~child1~~\n  - [ ] |child2\n");
            state.toggle_checkbox_for_test(2);
            let text = state.text();
            assert!(
                text.contains("[x] ~~parent~~"),
                "parent should be auto-checked"
            );
            assert!(
                text.contains("[x] ~~child1~~"),
                "child1 should remain checked"
            );
            assert!(text.contains("[x] ~~child2~~"), "child2 should be checked");
        }

        #[test]
        fn uncheck_child_unchecks_parent() {
            let mut state =
                editor_with_cursor("- [x] ~~parent~~\n  - [x] ~~|child1~~\n  - [x] ~~child2~~\n");
            state.toggle_checkbox_for_test(1);
            let text = state.text();
            assert!(text.contains("[ ] parent"), "parent should be unchecked");
            assert!(text.contains("[ ] child1"), "child1 should be unchecked");
            assert!(
                text.contains("[x] ~~child2~~"),
                "child2 should remain checked"
            );
        }

        #[test]
        fn deeply_nested_propagation_down() {
            let mut state = editor_with_cursor("- [ ] |level1\n  - [ ] level2\n    - [ ] level3\n");
            state.toggle_checkbox_for_test(0);
            let text = state.text();
            assert!(text.contains("[x] ~~level1~~"), "level1 should be checked");
            assert!(text.contains("[x] ~~level2~~"), "level2 should be checked");
            assert!(text.contains("[x] ~~level3~~"), "level3 should be checked");
        }

        #[test]
        fn deeply_nested_propagation_up() {
            let mut state = editor_with_cursor("- [ ] level1\n  - [ ] level2\n    - [ ] |level3\n");
            state.toggle_checkbox_for_test(2);
            let text = state.text();
            assert!(
                text.contains("[x] ~~level1~~"),
                "level1 should be auto-checked"
            );
            assert!(
                text.contains("[x] ~~level2~~"),
                "level2 should be auto-checked"
            );
            assert!(text.contains("[x] ~~level3~~"), "level3 should be checked");
        }

        #[test]
        fn mixed_siblings_parent_stays_unchecked() {
            let mut state = editor_with_cursor("- [ ] parent\n  - [ ] |child1\n  - [ ] child2\n");
            state.toggle_checkbox_for_test(1);
            let text = state.text();
            assert!(text.contains("[ ] parent"), "parent should stay unchecked");
            assert!(text.contains("[x] ~~child1~~"), "child1 should be checked");
            assert!(text.contains("[ ] child2"), "child2 should stay unchecked");
        }
    }
}

#[cfg(test)]
mod nested_context_tests {
    use super::*;

    #[test]
    fn nested_context_simple_list() {
        let state = EditorState::new("- item\n");
        let cursor_offset = 2; // on "item"
        let markers = state.build_nested_context(cursor_offset);
        assert_eq!(markers.len(), 1);
        assert!(matches!(
            markers[0],
            MarkerKind::ListItem { ordered: false, .. }
        ));
    }

    #[test]
    fn nested_context_nested_list() {
        let state = EditorState::new("- parent\n  - child\n");
        let cursor_offset = 14; // on "child"
        let markers = state.build_nested_context(cursor_offset);
        // Should show: - -
        assert_eq!(markers.len(), 2);
        assert!(matches!(
            markers[0],
            MarkerKind::ListItem { ordered: false, .. }
        ));
        assert!(matches!(
            markers[1],
            MarkerKind::ListItem { ordered: false, .. }
        ));
    }

    #[test]
    fn nested_context_checkbox_nested() {
        let state = EditorState::new("- [x] parent\n  - [ ] child\n");
        let cursor_offset = 20; // on "child"
        let markers = state.build_nested_context(cursor_offset);
        // Should show: - [x] - [ ]
        assert_eq!(markers.len(), 4);
        assert!(matches!(
            markers[0],
            MarkerKind::ListItem { ordered: false, .. }
        ));
        assert!(matches!(markers[1], MarkerKind::Checkbox { checked: true }));
        assert!(matches!(
            markers[2],
            MarkerKind::ListItem { ordered: false, .. }
        ));
        assert!(matches!(
            markers[3],
            MarkerKind::Checkbox { checked: false }
        ));
    }

    #[test]
    fn nested_context_blockquote_list() {
        let state = EditorState::new("> - item\n");
        let cursor_offset = 4; // on "item"
        let markers = state.build_nested_context(cursor_offset);
        // Should show: > -
        assert_eq!(markers.len(), 2);
        assert!(matches!(markers[0], MarkerKind::BlockQuote));
        assert!(matches!(
            markers[1],
            MarkerKind::ListItem { ordered: false, .. }
        ));
    }

    #[test]
    fn nested_context_ordered_list() {
        let state = EditorState::new("1. first\n2. second\n");
        let cursor_offset = 12; // on "second"
        let markers = state.build_nested_context(cursor_offset);
        assert_eq!(markers.len(), 1);
        assert!(matches!(
            markers[0],
            MarkerKind::ListItem { ordered: true, .. }
        ));
    }

    #[test]
    fn nested_context_empty_line() {
        let state = EditorState::new("hello\n");
        let cursor_offset = 2; // on "llo"
        let markers = state.build_nested_context(cursor_offset);
        assert_eq!(markers.len(), 0);
    }
}

#[cfg(test)]
mod debug_tree_structure {
    use super::*;

    #[test]
    fn check_blockquote_list_paragraph() {
        let state = EditorState::new("> - hey\n>   paragraph\n");

        if let Some(tree) = state.buffer.tree() {
            let root = tree.block_tree().root_node();
            eprintln!("Tree: {}", root.to_sexp());
        }
    }

    #[test]
    fn check_simple_list_paragraph() {
        let state = EditorState::new("- hey\n  paragraph\n");

        if let Some(tree) = state.buffer.tree() {
            let root = tree.block_tree().root_node();
            eprintln!("Tree: {}", root.to_sexp());
        }
    }
}

#[cfg(test)]
mod debug_tree_detail {
    use super::*;

    #[test]
    fn show_tree_detail() {
        let content = "> - hey\n>   paragraph\n";
        eprintln!("Content: {:?}", content);
        eprintln!("Bytes:");
        for (i, b) in content.bytes().enumerate() {
            eprintln!("  {}: {:?} ({})", i, b as char, b);
        }

        let state = EditorState::new(content);

        if let Some(tree) = state.buffer.tree() {
            let root = tree.block_tree().root_node();
            eprintln!("\nTree: {}", root.to_sexp());

            // Show each node with byte ranges
            fn print_node(node: tree_sitter::Node, indent: usize) {
                eprintln!(
                    "{}{} [{}-{}]",
                    "  ".repeat(indent),
                    node.kind(),
                    node.start_byte(),
                    node.end_byte()
                );
                for child in node.children(&mut node.walk()) {
                    print_node(child, indent + 1);
                }
            }
            print_node(root, 0);
        }
    }
}
