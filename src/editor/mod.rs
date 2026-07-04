mod action;
mod theme;

pub use action::Direction;
pub use theme::EditorTheme;

use crate::buffer::Buffer;
use crate::cursor::{
    Cursor, Selection, grapheme_column, offset_at_column, prev_grapheme_boundary,
};
use crate::marker::{LineMarkers, MarkerKind, OrderedMarker, UnorderedMarker};

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

/// Ascend from `node` (inclusive) through its parents, returning the first node
/// whose kind is `kind`.
fn ancestor_of_kind<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == kind {
            return Some(n);
        }
        current = n.parent();
    }
    None
}

/// Core editing state that can be used without GPUI context.
/// This contains the buffer and selection, and all editing logic.
pub struct EditorState {
    pub buffer: Buffer,
    pub selection: Selection,
    /// Cached tab cycle states to avoid recalculating mid-cycle.
    tab_cycle_cache: Option<TabCycleCache>,
    /// Sticky vertical-movement goal: `(grapheme_column, offset_it_landed_at)`. Reused by
    /// the next Up/Down only if the cursor is still at that offset, so passing through a
    /// short line doesn't lose the original column; any other move invalidates it for free.
    goal_column: Option<(usize, usize)>,
}

impl EditorState {
    pub fn new(content: &str) -> Self {
        let buffer: Buffer = content.parse().unwrap_or_default();
        Self {
            buffer,
            selection: Selection::new(0, 0),
            tab_cycle_cache: None,
            goal_column: None,
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

    /// Collapse the selection onto a single cursor.
    fn set_cursor_to(&mut self, c: Cursor) {
        self.selection = Selection::new(c.offset, c.offset);
    }

    /// Compute the cursor after moving one step in `direction`. Vertical moves keep a
    /// sticky goal column (`&mut self` to read/update it); other moves leave it stale,
    /// and it self-invalidates because the cursor no longer sits at its landing offset.
    pub fn cursor_in_direction(&mut self, direction: Direction) -> Cursor {
        let c = self.cursor();
        if matches!(direction, Direction::Up | Direction::Down) {
            let line = self.buffer.byte_to_line(c.offset);
            let column = match self.goal_column {
                Some((col, at)) if at == c.offset => col,
                _ => grapheme_column(&self.buffer, c.offset),
            };
            let offset = if direction == Direction::Up {
                if line == 0 {
                    Cursor::start().offset
                } else {
                    offset_at_column(&self.buffer, line - 1, column)
                }
            } else if line >= self.buffer.line_count().saturating_sub(1) {
                Cursor::end(&self.buffer).offset
            } else {
                offset_at_column(&self.buffer, line + 1, column)
            };
            self.goal_column = Some((column, offset));
            return Cursor { offset };
        }
        match direction {
            Direction::Left => c.move_left(&self.buffer),
            Direction::Right => c.move_right(&self.buffer),
            Direction::LineStart => c.move_to_line_start(&self.buffer),
            Direction::LineEnd => c.move_to_line_end(&self.buffer),
            Direction::DocStart => Cursor::start(),
            Direction::DocEnd => Cursor::end(&self.buffer),
            Direction::Up | Direction::Down => unreachable!("handled above"),
        }
    }

    /// Move cursor left by one character.
    pub fn move_left(&mut self) {
        let c = self.cursor_in_direction(Direction::Left);
        self.set_cursor_to(c);
    }

    /// Move cursor right by one character.
    pub fn move_right(&mut self) {
        let c = self.cursor_in_direction(Direction::Right);
        self.set_cursor_to(c);
    }

    /// Move cursor up by one line.
    pub fn move_up(&mut self) {
        let c = self.cursor_in_direction(Direction::Up);
        self.set_cursor_to(c);
    }

    /// Move cursor down by one line.
    pub fn move_down(&mut self) {
        let c = self.cursor_in_direction(Direction::Down);
        self.set_cursor_to(c);
    }

    /// Move cursor to start of current line.
    pub fn move_to_line_start(&mut self) {
        self.set_cursor_to(self.cursor().move_to_line_start(&self.buffer));
    }

    /// Move cursor to end of current line.
    pub fn move_to_line_end(&mut self) {
        self.set_cursor_to(self.cursor().move_to_line_end(&self.buffer));
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
    pub fn cursor_in_code_block(&self) -> bool {
        let Some(tree) = self.buffer.tree() else {
            return false;
        };

        let cursor_offset = self.cursor().offset;
        let root = tree.block_tree().root_node();

        // Find the deepest node at the cursor position and walk up looking for fenced_code_block
        let Some(node) = root.descendant_for_byte_range(cursor_offset, cursor_offset) else {
            return false;
        };

        ancestor_of_kind(node, "fenced_code_block").is_some()
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

        // Reuse the cache only when it's for this line; otherwise (wrong line or no
        // cache) rebuild and store.
        let states = if self.tab_cycle_cache.as_ref().is_some_and(|c| c.line_idx == line_idx) {
            self.tab_cycle_cache.as_ref().unwrap().states.clone()
        } else {
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
        ancestor_of_kind(node, "ERROR").is_some()
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
    fn list_item_at(&self, byte_offset: usize) -> Option<tree_sitter::Node<'_>> {
        let tree = self.buffer.tree()?;
        let root = tree.block_tree().root_node();
        let node = root.descendant_for_byte_range(byte_offset, byte_offset)?;

        ancestor_of_kind(node, "list_item")
    }

    /// Find the checkbox marker among a list_item's direct children, if any.
    /// Returns (checkbox_byte_offset, is_checked).
    fn direct_checkbox(&self, list_item: tree_sitter::Node) -> Option<(usize, bool)> {
        let mut cursor = list_item.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                match child.kind() {
                    "task_list_marker_checked" => return Some((child.start_byte(), true)),
                    "task_list_marker_unchecked" => return Some((child.start_byte(), false)),
                    _ => {}
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
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
        let our_list_item = self.list_item_at(list_item_start)?;

        // Walk up to find parent list_item, then read its direct checkbox
        let parent = ancestor_of_kind(our_list_item.parent()?, "list_item")?;
        self.direct_checkbox(parent)
    }

    /// Find all sibling checkboxes (same nesting level).
    /// Returns Vec of (checkbox_byte_offset, is_checked).
    fn find_sibling_checkboxes(&self, list_item_start: usize) -> Vec<(usize, bool)> {
        let our_list_item = match self.list_item_at(list_item_start) {
            Some(n) => n,
            None => return Vec::new(),
        };

        // Get parent list node
        let parent_list = match our_list_item.parent() {
            Some(p) if p.kind() == "list" => p,
            _ => return Vec::new(),
        };

        // Iterate all list_item children and collect their (direct) checkboxes
        let mut siblings = Vec::new();
        let mut cursor = parent_list.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if child.kind() == "list_item"
                    && let Some(cb) = self.direct_checkbox(child)
                {
                    siblings.push(cb);
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
        let code_block = ancestor_of_kind(node, "fenced_code_block")?;

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

        // One grapheme cluster back, not one byte — so a codepoint or an emoji/combining
        // cluster is deleted whole rather than split.
        let new_pos = prev_grapheme_boundary(&self.buffer, cursor_pos);
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

    pub fn toggle_checkbox(&mut self, line_number: usize) {
        // Capture pre-toggle state so the whole cascade (child + parent + all
        // strikethrough edits) collapses to a single undo entry at the end.
        let head_before = self.buffer.undo_head();
        let cursor_before = self.cursor().offset;
        let text_before = self.buffer.text();

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
        let list_item_node = self.list_item_at(checkbox_byte_start);

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

        // Toggle each checkbox + its line strikethrough. Descending offset order keeps a
        // strikethrough's byte shift from invalidating the lower offsets processed next;
        // the state replace is length-preserving.
        for (offset, _currently_checked) in &checkboxes_to_toggle {
            self.set_checkbox(*offset, new_checked, &mut cursor_pos);
        }

        // Propagate upward: if checking and all siblings are now checked, check parent
        // If unchecking, uncheck parent if it was checked
        self.propagate_checkbox_up(checkbox_byte_start, new_checked, &mut cursor_pos);

        let text_after = self.buffer.text();
        self.buffer
            .coalesce_since(head_before, &text_before, &text_after, cursor_before, cursor_pos);

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
                self.set_checkbox(parent_offset, true, cursor_pos);
                // Recursively propagate up
                self.propagate_checkbox_up(parent_offset, true, cursor_pos);
            }
        } else {
            // When unchecking: uncheck parent if it was checked
            if parent_checked {
                self.set_checkbox(parent_offset, false, cursor_pos);
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
            self.set_checkbox(parent_checkbox_offset, true, &mut cursor_pos);
            self.propagate_checkbox_up(parent_checkbox_offset, true, &mut cursor_pos);
            self.selection = Selection::new(cursor_pos, cursor_pos);
        } else if !all_siblings_checked && parent_checked {
            // Some siblings unchecked, uncheck the parent
            self.set_checkbox(parent_checkbox_offset, false, &mut cursor_pos);
            self.propagate_checkbox_up(parent_checkbox_offset, false, &mut cursor_pos);
            self.selection = Selection::new(cursor_pos, cursor_pos);
        }
    }

    /// Flip a single checkbox's state byte (`[ ]`↔`[x]`), toggle its line's
    /// strikethrough to match, and advance `cursor_pos` by the strikethrough's byte
    /// adjustment. The state replace is length-preserving; only strikethrough shifts bytes.
    fn set_checkbox(&mut self, checkbox_offset: usize, checked: bool, cursor_pos: &mut usize) {
        let content_start = checkbox_offset + 1; // skip '['
        let content_end = content_start + 1;
        let new_content = if checked { "x" } else { " " };
        self.buffer
            .replace(content_start..content_end, new_content, *cursor_pos);
        let line = self.buffer.byte_to_line(checkbox_offset);
        let adjustment = self.toggle_line_strikethrough(line, checked, *cursor_pos);
        *cursor_pos = (*cursor_pos as isize + adjustment) as usize;
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

            // Single replace wrapping the text in `~~` — one undo entry, byte-identical
            // to inserting `~~` at both ends.
            let wrapped = format!("~~{trimmed}~~");
            self.buffer.replace(text_start..text_end, &wrapped, cursor_pos);

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

                // Single replace stripping the wrapping `~~` — one undo entry, byte-identical
                // to deleting the trailing and leading `~~` separately.
                let inner = trimmed[2..trimmed.len() - 2].to_string();
                self.buffer.replace(text_start..text_end, &inner, cursor_pos);

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
        fn sticky_goal_column_survives_short_line() {
            // Down through a 2-char line then Down again restores the original column 7,
            // instead of clamping permanently to the short line's end.
            let mut state = editor_with_cursor("0123456|789\nab\nXYZ0123456");
            state.move_down(); // clamps to end of "ab"
            state.move_down(); // restores column 7 on the long last line
            assert_editor_eq(&state, "0123456789\nab\nXYZ0123|456");
        }

        #[test]
        fn goal_column_reset_by_horizontal_move() {
            // A horizontal move between vertical moves invalidates the goal column.
            let mut state = editor_with_cursor("0123456|789\nab\nXYZ0123456");
            state.move_down(); // end of "ab" (column 2)
            state.move_left(); // column 1 — invalidates the goal
            state.move_down(); // uses column 1, not 7
            assert_editor_eq(&state, "0123456789\nab\nX|YZ0123456");
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
        fn backspace_deletes_whole_zwj_emoji() {
            // A family emoji is 7 codepoints but one grapheme — deleted whole, not split.
            let mut state = editor_with_cursor("a👨‍👩‍👧‍👦|b");
            state.delete_backward();
            assert_editor_eq(&state, "a|b");
        }

        #[test]
        fn backspace_deletes_whole_combining_accent() {
            // "é" as base 'e' + combining acute (2 codepoints, 1 grapheme).
            let mut state = editor_with_cursor("e\u{301}|");
            state.delete_backward();
            assert_editor_eq(&state, "|");
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
            state.toggle_checkbox(0);
            let text = state.text();
            assert!(text.contains("[x] ~~parent~~"), "parent should be checked");
            assert!(text.contains("[x] ~~child1~~"), "child1 should be checked");
            assert!(text.contains("[x] ~~child2~~"), "child2 should be checked");
        }

        #[test]
        fn uncheck_parent_unchecks_all_children() {
            let mut state =
                editor_with_cursor("- [x] ~~|parent~~\n  - [x] ~~child1~~\n  - [x] ~~child2~~\n");
            state.toggle_checkbox(0);
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
            state.toggle_checkbox(2);
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
            state.toggle_checkbox(1);
            let text = state.text();
            assert!(text.contains("[ ] parent"), "parent should be unchecked");
            assert!(text.contains("[ ] child1"), "child1 should be unchecked");
            assert!(
                text.contains("[x] ~~child2~~"),
                "child2 should remain checked"
            );
        }

        #[test]
        fn checkbox_cascade_caches_match_fresh_parse() {
            // The suspend_caches batching must leave the derived inline styles identical
            // to a from-scratch parse of the resulting text (rebuilt once at the end).
            let mut state =
                editor_with_cursor("- [ ] |parent\n  - [ ] child1\n  - [ ] child2\n");
            state.toggle_checkbox(0); // cascades to children
            let mut fresh: Buffer = state.text().parse().unwrap();
            assert_eq!(
                *state.buffer.render_snapshot().inline_styles,
                *fresh.render_snapshot().inline_styles,
                "styles after the cascade equal a fresh parse"
            );
        }

        #[test]
        fn caches_not_frozen_after_cascade() {
            // suspend_caches must reset on every exit, so a later edit re-derives caches.
            let mut state = editor_with_cursor("- [ ] |task\nplain line\n");
            state.toggle_checkbox(0);
            let at = state.text().find("plain").unwrap();
            state.set_cursor(at);
            state.insert_text("**bold** ");
            let mut fresh: Buffer = state.text().parse().unwrap();
            assert_eq!(
                *state.buffer.render_snapshot().inline_styles,
                *fresh.render_snapshot().inline_styles,
                "caches re-derived after a post-cascade edit (not frozen)"
            );
        }

        #[test]
        fn deeply_nested_propagation_down() {
            let mut state = editor_with_cursor("- [ ] |level1\n  - [ ] level2\n    - [ ] level3\n");
            state.toggle_checkbox(0);
            let text = state.text();
            assert!(text.contains("[x] ~~level1~~"), "level1 should be checked");
            assert!(text.contains("[x] ~~level2~~"), "level2 should be checked");
            assert!(text.contains("[x] ~~level3~~"), "level3 should be checked");
        }

        #[test]
        fn deeply_nested_propagation_up() {
            let mut state = editor_with_cursor("- [ ] level1\n  - [ ] level2\n    - [ ] |level3\n");
            state.toggle_checkbox(2);
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
            state.toggle_checkbox(1);
            let text = state.text();
            assert!(text.contains("[ ] parent"), "parent should stay unchecked");
            assert!(text.contains("[x] ~~child1~~"), "child1 should be checked");
            assert!(text.contains("[ ] child2"), "child2 should stay unchecked");
        }
    }

    mod checkbox_undo_tests {
        use super::*;

        #[test]
        fn single_undo_reverts_cascade_to_children_and_parent() {
            // Checking child2 checks child2 AND auto-checks the parent — a cascade
            // spanning multiple lines. One undo must revert the entire toggle.
            let before = trim_raw("- [ ] parent\n  - [x] ~~child1~~\n  - [ ] child2\n");
            let mut state = editor_with_cursor("- [ ] parent\n  - [x] ~~child1~~\n  - [ ] |child2\n");
            state.toggle_checkbox(2);
            assert!(state.text().contains("[x] ~~parent~~"), "parent auto-checked");
            assert!(state.text().contains("[x] ~~child2~~"), "child2 checked");

            state.buffer.undo();
            assert_eq!(state.text(), before, "one undo reverts the whole cascade");
            assert!(!state.buffer.can_undo(), "toggle was a single undo entry");
        }

        #[test]
        fn redo_reapplies_full_cascade() {
            let mut state = editor_with_cursor("- [ ] parent\n  - [x] ~~child1~~\n  - [ ] |child2\n");
            state.toggle_checkbox(2);
            let after = state.text();

            state.buffer.undo();
            state.buffer.redo();
            assert_eq!(state.text(), after, "one redo re-applies the whole cascade");
        }

        #[test]
        fn toggle_leaf_box_text_and_single_entry() {
            let mut state = editor_with_cursor("- [ ] |task\n");
            state.toggle_checkbox(0);
            assert_eq!(state.text(), "- [x] ~~task~~\n");
            state.buffer.undo();
            assert_eq!(state.text(), "- [ ] task\n");
            assert!(!state.buffer.can_undo(), "leaf toggle is one undo entry");
        }

        #[test]
        fn toggle_parent_all_children_text() {
            let mut state = editor_with_cursor("- [ ] |parent\n  - [ ] child1\n  - [ ] child2\n");
            state.toggle_checkbox(0);
            assert_eq!(
                state.text(),
                "- [x] ~~parent~~\n  - [x] ~~child1~~\n  - [x] ~~child2~~\n"
            );
        }

        #[test]
        fn uncheck_cascades_to_parent_text() {
            let mut state = editor_with_cursor(
                "- [x] ~~parent~~\n  - [x] ~~|child1~~\n  - [x] ~~child2~~\n",
            );
            state.toggle_checkbox(1);
            assert_eq!(
                state.text(),
                "- [ ] parent\n  - [ ] child1\n  - [x] ~~child2~~\n"
            );
        }
    }

    mod strikethrough_tests {
        use super::*;

        #[test]
        fn strikethrough_add_remove_round_trips() {
            let mut state = EditorState::new("hello world\n");
            state.toggle_line_strikethrough(0, true, 0);
            assert_eq!(state.text(), "~~hello world~~\n");
            state.toggle_line_strikethrough(0, false, 0);
            assert_eq!(state.text(), "hello world\n", "round-trip is byte-identical");
        }

        #[test]
        fn strikethrough_add_is_single_undo() {
            let mut state = EditorState::new("hello world\n");
            state.toggle_line_strikethrough(0, true, 0);
            assert_eq!(state.text(), "~~hello world~~\n");
            state.buffer.undo();
            assert_eq!(
                state.text(),
                "hello world\n",
                "one undo reverts the whole strikethrough toggle"
            );
            assert!(!state.buffer.can_undo(), "no further undo entries remain");
        }

        #[test]
        fn typing_coalesces_into_word_undo_steps() {
            let mut state = EditorState::new("");
            for c in "hi there".chars() {
                state.insert_text(&c.to_string());
            }
            assert_eq!(state.text(), "hi there");
            // Word-granular undo: "there", then the space, then "hi".
            state.buffer.undo();
            assert_eq!(state.text(), "hi ", "undo removes the whole last word");
            state.buffer.undo();
            assert_eq!(state.text(), "hi", "undo removes the space");
            state.buffer.undo();
            assert_eq!(state.text(), "", "undo removes the first word");
        }

        #[test]
        fn backspace_coalesces_into_one_undo() {
            let mut state = EditorState::new("word\n");
            state.set_cursor(4); // end of "word"
            for _ in 0..4 {
                state.delete_backward();
            }
            assert_eq!(state.text(), "\n");
            state.buffer.undo();
            assert_eq!(state.text(), "word\n", "one undo restores the backspaced word");
        }

        #[test]
        fn paste_is_its_own_undo_step() {
            let mut state = EditorState::new("");
            state.insert_text("ab"); // multi-char (paste-like) — not coalescable
            for c in "cd".chars() {
                state.insert_text(&c.to_string());
            }
            assert_eq!(state.text(), "abcd");
            state.buffer.undo();
            assert_eq!(state.text(), "ab", "typing after a paste undoes separately");
            state.buffer.undo();
            assert_eq!(state.text(), "", "the paste is its own step");
        }

        #[test]
        fn strikethrough_remove_is_single_undo() {
            let mut state = EditorState::new("~~hello world~~\n");
            state.toggle_line_strikethrough(0, false, 0);
            assert_eq!(state.text(), "hello world\n");
            state.buffer.undo();
            assert_eq!(state.text(), "~~hello world~~\n", "one undo restores `~~`");
            assert!(!state.buffer.can_undo(), "no further undo entries remain");
        }

        #[test]
        fn strikethrough_preserves_surrounding_whitespace() {
            // Leading/trailing whitespace must be outside the `~~` wrap.
            let mut state = EditorState::new("  hello  \n");
            state.toggle_line_strikethrough(0, true, 0);
            assert_eq!(state.text(), "  ~~hello~~  \n");
            state.toggle_line_strikethrough(0, false, 0);
            assert_eq!(state.text(), "  hello  \n");
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
