use ropey::Rope;
use std::ops::{Deref, DerefMut, Range};
use std::rc::Rc;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use tree_sitter::{InputEdit, Point};
use undo::Record;

/// Global counter for unique buffer versions.
static NEXT_VERSION: AtomicU64 = AtomicU64::new(1);

use crate::highlight::{HighlightSpan, Highlighter};
use crate::inline::{StyledRegion, extract_all_inline_styles, styles_in_range};
use crate::marker::{
    LineMarkers, ParsedNodes, collect_node_infos, is_line_in_checked_task, is_line_in_code_block,
    markers_at_from_infos,
};
use crate::parser::{MarkdownParser, MarkdownTree};

/// Compute the byte range for a line (excludes trailing newline).
fn compute_line_byte_range(rope: &Rope, line_idx: usize) -> Range<usize> {
    let start_char = rope.line_to_char(line_idx);
    let end_char = if line_idx + 1 < rope.len_lines() {
        rope.line_to_char(line_idx + 1)
    } else {
        rope.len_chars()
    };
    let start_byte = rope.char_to_byte(start_char);
    let end_byte = rope.char_to_byte(end_char);

    // Exclude trailing newline
    let line_slice = rope.line(line_idx);
    let len = line_slice.len_bytes();
    let has_newline = line_slice.get_byte(len.saturating_sub(1)) == Some(b'\n');
    let adjusted_end = if has_newline {
        end_byte.saturating_sub(1)
    } else {
        end_byte
    };

    start_byte..adjusted_end
}

/// Whether a tree-sitter-md node kind is an ordered-list marker (`1.` or `1)`).
fn is_ordered_list_marker(kind: &str) -> bool {
    kind == "list_marker_dot" || kind == "list_marker_parenthesis"
}

/// Compute `LineMarkers` for `line_idx` from parsed nodes and a rope. Shared by
/// `RenderSnapshot` and `BufferContent`, which differ only in their rope field.
fn line_markers_from(parsed: &ParsedNodes, rope: &Rope, line_idx: usize) -> LineMarkers {
    let range = compute_line_byte_range(rope, line_idx);
    let markers = markers_at_from_infos(&parsed.nodes, rope, range.start, range.end);
    let in_checked_task = is_line_in_checked_task(&parsed.nodes, range.start);
    let in_code_block = is_line_in_code_block(&parsed.nodes, range.start);
    LineMarkers {
        range,
        line_number: line_idx,
        markers,
        in_checked_task,
        in_code_block,
    }
}

/// A snapshot of buffer data for rendering. All fields use Rc for O(1) cloning.
/// LineMarkers are computed lazily per-line using the nodes cache.
#[derive(Clone)]
pub struct RenderSnapshot {
    pub rope: Rope,
    pub inline_styles: Rc<Vec<StyledRegion>>,
    pub code_highlights: Rc<Vec<(Range<usize>, Vec<HighlightSpan>)>>,
    /// Cached parsed nodes for lazy LineMarkers computation and code block queries
    parsed: Rc<ParsedNodes>,
    /// Number of lines in the document
    line_count: usize,
}

impl RenderSnapshot {
    /// Get the number of lines in the document.
    pub fn line_count(&self) -> usize {
        self.line_count
    }

    pub fn line_byte_range(&self, line_idx: usize) -> Range<usize> {
        compute_line_byte_range(&self.rope, line_idx)
    }

    /// Compute LineMarkers for a specific line on demand. O(log n) binary search + marker extraction.
    pub fn line_markers(&self, line_idx: usize) -> LineMarkers {
        line_markers_from(&self.parsed, &self.rope, line_idx)
    }

    /// Tree inline styles bucketed per line (index = line), in one O(n + styles)
    /// pass — the whole-document replacement for calling `inline_styles_in_range`
    /// per line, which is O(n²) because `styles_in_range` scans all earlier styles.
    /// No synthetic checkbox regions (callers that need them inject per line).
    pub fn inline_styles_by_line(&self) -> Vec<Vec<StyledRegion>> {
        let n = self.line_count;
        let mut buckets: Vec<Vec<StyledRegion>> = vec![Vec::new(); n];
        if n == 0 {
            return buckets;
        }
        let line_starts: Vec<usize> = (0..n).map(|i| self.line_byte_range(i).start).collect();
        // Line containing byte `off` (the last line whose start is <= off).
        let line_of = |off: usize| line_starts.partition_point(|&s| s <= off).saturating_sub(1);

        for region in self.inline_styles.iter() {
            let first = line_of(region.full_range.start).min(n - 1);
            // A region overlaps a line iff region.end > line.start, so the last line
            // it touches is the one containing its last byte (end - 1).
            let last_byte = region
                .full_range
                .end
                .saturating_sub(1)
                .max(region.full_range.start);
            let last = line_of(last_byte).max(first).min(n - 1);
            for bucket in buckets.iter_mut().take(last + 1).skip(first) {
                bucket.push(region.clone());
            }
        }
        buckets
    }

    /// Tree inline styles for one line (no synthetic checkbox regions), matching one
    /// bucket of `inline_styles_by_line` but computed in isolation (O(log n)). For
    /// callers that need only a handful of lines — e.g. the few visible ghost lines —
    /// and shouldn't pay to bucket the whole document each rebuild.
    pub fn tree_styles_for_line(&self, line_idx: usize) -> Vec<StyledRegion> {
        let range = self.line_byte_range(line_idx);
        styles_in_range(&self.inline_styles, &range)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Get inline styles for a specific line. O(log n) binary search.
    /// Also injects a synthetic StyledRegion for any checkbox marker on the line.
    pub fn inline_styles_for_line(&self, line_idx: usize) -> Vec<StyledRegion> {
        let range = self.line_byte_range(line_idx);
        let markers = self.line_markers(line_idx);
        self.inline_styles_in_range(&range, &markers)
    }

    /// Inline styles for a line whose byte `range` and `markers` are already known.
    /// Lets callers that hold both (e.g. `build_line_render`) skip recomputing the
    /// markers — the redundant recompute was ~25% of the per-line render cost.
    pub fn inline_styles_in_range(
        &self,
        range: &Range<usize>,
        markers: &LineMarkers,
    ) -> Vec<StyledRegion> {
        let mut styles: Vec<StyledRegion> = styles_in_range(&self.inline_styles, range)
            .into_iter()
            .cloned()
            .collect();

        // Inject synthetic StyledRegion for checkbox markers
        for marker in &markers.markers {
            if let crate::marker::MarkerKind::Checkbox { checked } = marker.kind {
                // The checkbox marker range is "[ ] " (4 bytes), but we only
                // want to style "[ ]" (3 bytes) with the checkbox style.
                let checkbox_range = marker.range.start..marker.range.start + 3;
                styles.push(StyledRegion {
                    full_range: checkbox_range.clone(),
                    content_range: checkbox_range,
                    style: crate::inline::TextStyle::default(),
                    link_url: None,
                    is_image: false,
                    checkbox: Some(checked),
                    display_text: None,
                });
            }
        }

        // Re-sort by start position to maintain order
        styles.sort_by_key(|s| s.full_range.start);
        styles
    }

    /// Get code highlights for a specific line. O(code_blocks) scan.
    pub fn code_highlights_for_line(&self, line_idx: usize) -> Vec<HighlightSpan> {
        let range = self.line_byte_range(line_idx);
        let mut result = Vec::new();
        for (block_range, highlights) in self.code_highlights.iter() {
            if range.start < block_range.end && range.end > block_range.start {
                for span in highlights {
                    if span.range.start < range.end && span.range.end > range.start {
                        result.push(span.clone());
                    }
                }
            }
        }
        result
    }
}

#[derive(Clone, Debug)]
struct CodeHighlightCache {
    highlights: Rc<Vec<(Range<usize>, Vec<HighlightSpan>)>>,
    valid: bool,
}

impl Default for CodeHighlightCache {
    fn default() -> Self {
        Self {
            highlights: Rc::new(Vec::new()),
            valid: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TextEdit {
    offset: usize,
    deleted: String,
    inserted: String,
    cursor_before: usize,
    cursor_after: usize,
    /// Single-character typing/backspace, eligible to merge into a run so one Ctrl+Z
    /// undoes a whole word rather than one character. Multi-char (paste), replacements,
    /// and programmatic edits are false and stay their own undo step.
    coalescable: bool,
}

impl TextEdit {
    pub fn new(
        offset: usize,
        deleted: String,
        inserted: String,
        cursor_before: usize,
        cursor_after: usize,
    ) -> Self {
        // A pure single-character insert or delete is coalescable.
        let coalescable = deleted.chars().count() + inserted.chars().count() == 1;
        Self {
            offset,
            deleted,
            inserted,
            cursor_before,
            cursor_after,
            coalescable,
        }
    }
}

/// Word characters extend the current typing group; anything else (space, punctuation,
/// newline) starts a fresh undo step — the common word-granular undo behavior.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl undo::Edit for TextEdit {
    type Target = BufferContent;
    type Output = usize; // Returns cursor position

    fn edit(&mut self, target: &mut BufferContent) -> Self::Output {
        target.apply_edit(self.offset, &self.deleted, &self.inserted);
        self.cursor_after
    }

    fn undo(&mut self, target: &mut BufferContent) -> Self::Output {
        target.apply_edit(self.offset, &self.inserted, &self.deleted);
        self.cursor_before
    }

    /// Coalesce contiguous single-character word typing (or backspacing) into one undo
    /// step. `self` is the edit already on the stack; `other` the incoming one.
    fn merge(&mut self, other: Self) -> undo::Merged<Self> {
        if !self.coalescable || !other.coalescable {
            return undo::Merged::No(other);
        }
        // Forward typing: `other` inserts a word char right where `self`'s insert ended.
        if self.deleted.is_empty() && other.deleted.is_empty() {
            let ends_at = self.offset + self.inserted.len();
            let last = self.inserted.chars().next_back();
            let next = other.inserted.chars().next();
            if ends_at == other.offset
                && matches!((last, next), (Some(l), Some(n)) if is_word_char(l) && is_word_char(n))
            {
                self.inserted.push_str(&other.inserted);
                self.cursor_after = other.cursor_after;
                return undo::Merged::Yes;
            }
        }
        // Backspacing: `other` deletes the word char immediately before `self`'s deletion.
        if self.inserted.is_empty() && other.inserted.is_empty() {
            let first = self.deleted.chars().next();
            let prev = other.deleted.chars().next();
            if other.offset + other.deleted.len() == self.offset
                && matches!((first, prev), (Some(f), Some(p)) if is_word_char(f) && is_word_char(p))
            {
                self.deleted.insert_str(0, &other.deleted);
                self.offset = other.offset;
                self.cursor_after = other.cursor_after;
                return undo::Merged::Yes;
            }
        }
        undo::Merged::No(other)
    }
}

pub struct BufferContent {
    text: Rope,
    parser: MarkdownParser,
    tree: Option<MarkdownTree>,
    highlighter: Highlighter,
    code_highlight_cache: CodeHighlightCache,
    /// Cached parsed nodes for lazy LineMarkers computation and code block queries.
    /// Updated when tree changes. Wrapped in Rc for O(1) cloning in render snapshots.
    parsed: Rc<ParsedNodes>,
    /// Cached inline styles, updated when tree changes.
    /// Sorted by start position for efficient binary search lookup.
    /// Wrapped in Rc for O(1) cloning in render snapshots.
    inline_styles: Rc<Vec<StyledRegion>>,
    /// Version counter, incremented on each edit. Used by Editor to detect changes.
    version: u64,
    /// When set, `apply_edit` skips the whole-doc `update_caches` (nodes + inline styles).
    /// Used to batch a compound action (a checkbox toggle cascades through many
    /// revert/reapply edits via history) so the O(n) caches rebuild once at the end
    /// instead of per sub-edit. No consumer may read `parsed`/`inline_styles` while set.
    suspend_caches: bool,
}

impl BufferContent {
    pub fn new() -> Self {
        Self {
            text: Rope::new(),
            parser: MarkdownParser::default(),
            highlighter: Highlighter::new(),
            tree: None,
            code_highlight_cache: CodeHighlightCache::default(),
            parsed: Rc::new(ParsedNodes::default()),
            inline_styles: Rc::new(Vec::new()),
            version: NEXT_VERSION.fetch_add(1, Ordering::Relaxed),
            suspend_caches: false,
        }
    }

    /// Returns the current version number. Incremented on each edit.
    pub fn version(&self) -> u64 {
        self.version
    }

    fn update_caches(&mut self) {
        self.parsed = Rc::new(
            self.tree
                .as_ref()
                .map(|t| collect_node_infos(&t.block_tree().root_node()))
                .unwrap_or_default(),
        );

        self.inline_styles = Rc::new(if let Some(ref tree) = self.tree {
            extract_all_inline_styles(tree, &self.text)
        } else {
            Vec::new()
        });
    }

    fn apply_edit(&mut self, offset: usize, to_delete: &str, to_insert: &str) {
        let delete_len = to_delete.len();
        let insert_len = to_insert.len();

        // Build the edit description for tree-sitter before modifying the rope
        let start_point = self.byte_to_point(offset);
        let old_end_point = if delete_len > 0 {
            self.byte_to_point(offset + delete_len)
        } else {
            start_point
        };

        let new_end_position = if insert_len > 0 {
            self.compute_new_end_point(start_point, to_insert)
        } else {
            start_point
        };

        let edit = InputEdit {
            start_byte: offset,
            old_end_byte: offset + delete_len,
            new_end_byte: offset + insert_len,
            start_position: start_point,
            old_end_position: old_end_point,
            new_end_position,
        };

        if delete_len > 0 {
            let char_start = self.text.byte_to_char(offset);
            let char_end = self.text.byte_to_char(offset + delete_len);
            self.text.remove(char_start..char_end);
        }
        if insert_len > 0 {
            let char_offset = self.text.byte_to_char(offset);
            self.text.insert(char_offset, to_insert);
        }

        if let Some(ref mut tree) = self.tree {
            tree.edit(&edit);
        }
        self.tree = self.parser.parse_rope(&self.text, self.tree.as_ref());

        // Renumbering only matters when the edit lands inside an ordered list, which
        // is the uncommon case — skip the tree walk + reparse otherwise.
        if self.edit_in_ordered_list(offset, offset + insert_len) {
            self.normalize_ordered_lists();
        }
        if !self.suspend_caches {
            self.update_caches();
        }
        self.code_highlight_cache.valid = false;
        self.version += 1;
    }

    /// True if the byte range `start..end` lies within (or on the boundary of) an
    /// ordered list node in the current tree. Used to gate `normalize_ordered_lists`.
    fn edit_in_ordered_list(&self, start: usize, end: usize) -> bool {
        let Some(tree) = &self.tree else {
            return false;
        };
        let root = tree.block_tree().root_node();
        let Some(node) = root.descendant_for_byte_range(start, end) else {
            return false;
        };
        let mut current = Some(node);
        while let Some(n) = current {
            if n.kind() == "list" && self.is_ordered_list(&n) {
                return true;
            }
            current = n.parent();
        }
        false
    }

    /// Normalize ordered list numbering - ensure sequential numbers (1, 2, 3...).
    /// Modifies the rope directly and re-parses incrementally if changes were made.
    fn normalize_ordered_lists(&mut self) -> bool {
        let corrections = {
            let Some(tree) = &self.tree else {
                return false;
            };
            self.find_ordered_list_corrections(tree.block_tree().root_node())
        };

        if corrections.is_empty() {
            return false;
        }

        // Apply corrections in reverse order to preserve byte offsets. Each marker
        // rewrite is fed to `tree.edit` so the follow-up parse stays incremental.
        for (marker_range, correct_number, is_parenthesis) in corrections.into_iter().rev() {
            let new_marker = if is_parenthesis {
                format!("{correct_number}) ")
            } else {
                format!("{correct_number}. ")
            };
            let new_len = new_marker.len();

            let start_point = self.byte_to_point(marker_range.start);
            let old_end_point = self.byte_to_point(marker_range.end);
            let new_end_point = self.compute_new_end_point(start_point, &new_marker);

            let char_start = self.text.byte_to_char(marker_range.start);
            let char_end = self.text.byte_to_char(marker_range.end);
            self.text.remove(char_start..char_end);

            let char_offset = self.text.byte_to_char(marker_range.start);
            self.text.insert(char_offset, &new_marker);

            if let Some(tree) = self.tree.as_mut() {
                tree.edit(&InputEdit {
                    start_byte: marker_range.start,
                    old_end_byte: marker_range.end,
                    new_end_byte: marker_range.start + new_len,
                    start_position: start_point,
                    old_end_position: old_end_point,
                    new_end_position: new_end_point,
                });
            }
        }

        self.tree = self.parser.parse_rope(&self.text, self.tree.as_ref());
        true
    }

    /// Returns corrections as (range, correct_number, is_parenthesis_style)
    fn find_ordered_list_corrections(
        &self,
        root: tree_sitter::Node,
    ) -> Vec<(Range<usize>, usize, bool)> {
        let mut corrections = Vec::new();
        self.collect_list_corrections(&root, &mut corrections);
        corrections
    }

    fn collect_list_corrections(
        &self,
        node: &tree_sitter::Node,
        corrections: &mut Vec<(Range<usize>, usize, bool)>,
    ) {
        if node.kind() == "list" && self.is_ordered_list(node) {
            let mut item_number = 1;
            for child in node.children(&mut node.walk()) {
                if child.kind() == "list_item"
                    && let Some((marker_range, current_number, is_parenthesis)) =
                        self.extract_ordered_marker(&child)
                {
                    if current_number != item_number {
                        corrections.push((marker_range, item_number, is_parenthesis));
                    }
                    item_number += 1;
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.collect_list_corrections(&child, corrections);
        }
    }

    fn is_ordered_list(&self, list_node: &tree_sitter::Node) -> bool {
        for child in list_node.children(&mut list_node.walk()) {
            if child.kind() == "list_item"
                && let Some(marker) = child.children(&mut child.walk()).next()
            {
                return is_ordered_list_marker(marker.kind());
            }
        }
        false
    }

    /// Extract ordered list marker info: (range, current_number, is_parenthesis_style)
    fn extract_ordered_marker(
        &self,
        list_item: &tree_sitter::Node,
    ) -> Option<(Range<usize>, usize, bool)> {
        for marker in list_item.children(&mut list_item.walk()) {
            if is_ordered_list_marker(marker.kind()) {
                let start = marker.start_byte();
                let end = marker.end_byte();
                let is_parenthesis = marker.kind() == "list_marker_parenthesis";

                // Extract digits from the marker using rope slice
                let char_start = self.text.byte_to_char(start);
                let char_end = self.text.byte_to_char(end);
                let slice = self.text.slice(char_start..char_end);

                let number: usize = slice
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(1);

                return Some((start..end, number, is_parenthesis));
            }
        }
        None
    }

    fn byte_to_point(&self, byte_offset: usize) -> Point {
        let byte_offset = byte_offset.min(self.text.len_bytes());
        let char_offset = self.text.byte_to_char(byte_offset);
        let line = self.text.char_to_line(char_offset);
        let line_start_char = self.text.line_to_char(line);
        let line_start_byte = self.text.char_to_byte(line_start_char);
        let column = byte_offset - line_start_byte;
        Point::new(line, column)
    }

    fn compute_new_end_point(&self, start: Point, text: &str) -> Point {
        let newlines: Vec<_> = text.match_indices('\n').collect();
        if newlines.is_empty() {
            Point::new(start.row, start.column + text.len())
        } else {
            let last_newline_pos = newlines.last().unwrap().0;
            let column = text.len() - last_newline_pos - 1;
            Point::new(start.row + newlines.len(), column)
        }
    }

    pub fn text(&self) -> String {
        self.text.to_string()
    }

    /// Whether the content equals `other`, comparing the rope in place (no `String` alloc).
    pub fn content_eq(&self, other: &str) -> bool {
        self.text == *other
    }

    pub fn len_bytes(&self) -> usize {
        self.text.len_bytes()
    }

    pub fn len_chars(&self) -> usize {
        self.text.len_chars()
    }

    pub fn is_empty(&self) -> bool {
        self.text.len_bytes() == 0
    }

    /// Check if buffer ends with the given string (efficient, doesn't copy whole buffer).
    pub fn ends_with(&self, suffix: &str) -> bool {
        let len = self.text.len_bytes();
        let suffix_len = suffix.len();
        if len < suffix_len {
            return false;
        }
        let start = len - suffix_len;
        self.text
            .byte_slice(start..len)
            .as_str()
            .map(|s| s == suffix)
            .unwrap_or(false)
    }

    /// Get a single byte at the given offset, if it exists.
    pub fn byte_at(&self, offset: usize) -> Option<u8> {
        if offset >= self.text.len_bytes() {
            return None;
        }
        self.text
            .byte_slice(offset..offset + 1)
            .as_str()
            .and_then(|s| s.bytes().next())
    }

    pub fn rope(&self) -> &Rope {
        &self.text
    }

    pub fn tree(&self) -> Option<&MarkdownTree> {
        self.tree.as_ref()
    }

    /// Get a reference to the parsed nodes for structural queries.
    pub fn parsed(&self) -> &ParsedNodes {
        &self.parsed
    }

    /// Compute LineMarkers for a specific line on demand.
    pub fn line_markers(&self, line_idx: usize) -> LineMarkers {
        line_markers_from(&self.parsed, &self.text, line_idx)
    }

    /// Compute LineMarkers for all lines. Only available in tests.
    #[cfg(test)]
    pub fn lines(&self) -> Vec<LineMarkers> {
        (0..self.line_count())
            .map(|i| self.line_markers(i))
            .collect()
    }

    /// Create a render snapshot for efficient virtualized rendering.
    /// Ensures code highlight cache is valid before creating the snapshot.
    /// All Rc clones are O(1). LineMarkers are computed lazily per-line.
    pub fn render_snapshot(&mut self) -> RenderSnapshot {
        if !self.code_highlight_cache.valid {
            self.rebuild_code_highlight_cache();
        }

        RenderSnapshot {
            rope: self.text.clone(),
            inline_styles: Rc::clone(&self.inline_styles),
            code_highlights: Rc::clone(&self.code_highlight_cache.highlights),
            parsed: Rc::clone(&self.parsed),
            line_count: self.text.len_lines(),
        }
    }

    pub fn byte_to_line(&self, byte_offset: usize) -> usize {
        let char_offset = self.text.byte_to_char(byte_offset);
        self.text.char_to_line(char_offset)
    }

    pub fn line_to_byte(&self, line: usize) -> usize {
        let char_offset = self.text.line_to_char(line);
        self.text.char_to_byte(char_offset)
    }

    pub fn line_count(&self) -> usize {
        self.text.len_lines()
    }

    /// Returns true if the line has no content (only markers or whitespace).
    /// Code fences are always considered content.
    pub fn is_line_empty(&self, line_idx: usize) -> bool {
        if line_idx >= self.line_count() {
            return true;
        }
        let line = self.line_markers(line_idx);

        if line.is_fence() {
            return false;
        }

        self.slice_cow(line.content_start()..line.range.end)
            .trim()
            .is_empty()
    }

    #[cfg(test)]
    pub fn line(&self, line_idx: usize) -> String {
        let line = self.text.line(line_idx);
        // Avoid double allocation - trim in place
        let mut s = line.to_string();
        if s.ends_with('\n') {
            s.pop();
        }
        s
    }

    pub fn line_byte_range(&self, line_idx: usize) -> Range<usize> {
        compute_line_byte_range(&self.text, line_idx)
    }

    fn slice(&self, range: Range<usize>) -> String {
        let char_start = self.text.byte_to_char(range.start);
        let char_end = self.text.byte_to_char(range.end);
        self.text.slice(char_start..char_end).to_string()
    }

    /// Get a byte slice from the rope, borrowing if possible.
    /// Returns a Cow that borrows if the slice fits in one chunk, allocates otherwise.
    pub fn slice_cow(&self, range: Range<usize>) -> std::borrow::Cow<'_, str> {
        let len = self.text.len_bytes();
        // Clamp range to valid bounds
        let start = range.start.min(len);
        let end = range.end.min(len);
        if start >= end {
            return std::borrow::Cow::Borrowed("");
        }
        let char_start = self.text.byte_to_char(start);
        let char_end = self.text.byte_to_char(end);
        let slice = self.text.slice(char_start..char_end);
        match slice.as_str() {
            Some(s) => std::borrow::Cow::Borrowed(s),
            None => std::borrow::Cow::Owned(slice.to_string()),
        }
    }

    fn rebuild_code_highlight_cache(&mut self) {
        let highlights = Rc::make_mut(&mut self.code_highlight_cache.highlights);
        highlights.clear();

        for code_block in &self.parsed.code_blocks {
            let language = code_block.info_string_range.as_ref().and_then(|range| {
                let char_start = self.text.byte_to_char(range.start);
                let char_end = self.text.byte_to_char(range.end);
                let slice = self.text.slice(char_start..char_end);
                let lang = slice.to_string().trim().to_string();
                if lang.is_empty() { None } else { Some(lang) }
            });

            if let Some(lang) = language
                && !code_block.content_range.is_empty()
            {
                let char_start = self.text.byte_to_char(code_block.content_range.start);
                let char_end = self.text.byte_to_char(code_block.content_range.end);
                let slice = self.text.slice(char_start..char_end);
                let code_content = slice.to_string();

                let mut spans = self.highlighter.highlight(&code_content, &lang);
                for span in &mut spans {
                    span.range.start += code_block.content_range.start;
                    span.range.end += code_block.content_range.start;
                }
                highlights.push((code_block.block_range.clone(), spans));
            }
        }

        self.code_highlight_cache.valid = true;
    }
}

impl Default for BufferContent {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Buffer {
    content: BufferContent,
    history: Record<TextEdit>,
}

impl Deref for Buffer {
    type Target = BufferContent;

    fn deref(&self) -> &Self::Target {
        &self.content
    }
}

impl DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.content
    }
}

impl Buffer {
    pub fn new() -> Self {
        Self {
            content: BufferContent::new(),
            history: Record::new(),
        }
    }

    pub fn is_dirty(&self) -> bool {
        !self.history.is_saved()
    }

    pub fn mark_clean(&mut self) {
        self.history.set_saved();
    }

    pub fn insert(&mut self, byte_offset: usize, text: &str, cursor_before: usize) -> usize {
        let cursor_after = byte_offset + text.len();
        let edit = TextEdit::new(
            byte_offset,
            String::new(),
            text.to_string(),
            cursor_before,
            cursor_after,
        );
        self.history.edit(&mut self.content, edit)
    }

    pub fn delete(&mut self, byte_range: Range<usize>, cursor_before: usize) -> usize {
        let deleted = self.content.slice(byte_range.clone());
        let cursor_after = byte_range.start;
        let edit = TextEdit::new(
            byte_range.start,
            deleted,
            String::new(),
            cursor_before,
            cursor_after,
        );
        self.history.edit(&mut self.content, edit)
    }

    pub fn replace(&mut self, byte_range: Range<usize>, text: &str, cursor_before: usize) -> usize {
        let deleted = self.content.slice(byte_range.clone());
        let cursor_after = byte_range.start + text.len();
        let edit = TextEdit::new(
            byte_range.start,
            deleted,
            text.to_string(),
            cursor_before,
            cursor_after,
        );
        self.history.edit(&mut self.content, edit)
    }

    /// Current head index into the undo history. Pair with `coalesce_since` to
    /// collapse a batch of edits into a single undo entry.
    pub fn undo_head(&self) -> usize {
        self.history.head()
    }

    /// Collapse every edit made since `head` (a prior `undo_head` value) into one
    /// undo entry mapping the state at `head` (`text_before`) directly to
    /// `text_after`. Reverts the intervening edits, then splices a single minimal
    /// replace, so undo/redo treat the whole batch as one step. `cursor_before`/
    /// `cursor_after` are recorded on the entry so undo/redo restore the cursor.
    pub fn coalesce_since(
        &mut self,
        head: usize,
        text_before: &str,
        text_after: &str,
        cursor_before: usize,
        cursor_after: usize,
    ) {
        let (start, old_end, replacement) = minimal_diff(text_before, text_after);
        // Suspend the whole-doc cache rebuild across the batch's revert + reapply; rebuild
        // once at the end. Every exit path below runs the reset+rebuild, so the caches can
        // never stay frozen. Discard the reverted batch either way; only push a new entry
        // if the batch produced a net visible change.
        self.content.suspend_caches = true;
        self.history.go_to(&mut self.content, head);
        if start != old_end || !replacement.is_empty() {
            let mut edit = TextEdit::new(
                start,
                text_before[start..old_end].to_string(),
                replacement,
                cursor_before,
                cursor_after,
            );
            // A coalesced batch is one discrete action — never fold it into prior typing.
            edit.coalescable = false;
            self.history.edit(&mut self.content, edit);
        }
        self.content.suspend_caches = false;
        self.content.update_caches();
    }

    pub fn undo(&mut self) -> Option<usize> {
        self.history.undo(&mut self.content)
    }

    pub fn redo(&mut self) -> Option<usize> {
        self.history.redo(&mut self.content)
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Load a buffer from a file. Returns the buffer and whether the file was modified.
    /// Currently always returns false for modified since we don't normalize on load.
    pub fn from_file(path: &std::path::Path) -> std::io::Result<(Self, bool)> {
        let content = std::fs::read_to_string(path).unwrap_or_default();

        // Parse the file exactly as-is - no normalization
        let mut buffer: Buffer = content.parse().expect("Buffer parsing is infallible");

        // Mark as clean since we just loaded
        buffer.history.set_saved();

        // Second return value is false - we never modify the file on load
        Ok((buffer, false))
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Minimal replace transforming `old` into `new`: strip the common prefix and
/// suffix, returning `(start, old_end, replacement)` where replacing
/// `old[start..old_end]` with `replacement` yields `new`. Boundaries are snapped
/// to UTF-8 char boundaries so the result is safe to apply to a rope.
fn minimal_diff(old: &str, new: &str) -> (usize, usize, String) {
    let old_b = old.as_bytes();
    let new_b = new.as_bytes();

    let max_pre = old_b.len().min(new_b.len());
    let mut start = 0;
    while start < max_pre && old_b[start] == new_b[start] {
        start += 1;
    }
    while start > 0 && !old.is_char_boundary(start) {
        start -= 1;
    }

    let max_suf = old_b.len().min(new_b.len()) - start;
    let mut suf = 0;
    while suf < max_suf && old_b[old_b.len() - 1 - suf] == new_b[new_b.len() - 1 - suf] {
        suf += 1;
    }
    let mut old_end = old_b.len() - suf;
    while old_end < old_b.len() && !old.is_char_boundary(old_end) {
        old_end += 1;
    }

    let new_end = new_b.len() - (old_b.len() - old_end);
    (start, old_end, new[start..new_end].to_string())
}

impl FromStr for Buffer {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let text = Rope::from_str(s);
        let mut parser = MarkdownParser::default();
        let tree = parser.parse_rope(&text, None);

        let mut content = BufferContent {
            text,
            parser,
            highlighter: Highlighter::new(),
            tree,
            code_highlight_cache: CodeHighlightCache::default(),
            parsed: Rc::new(ParsedNodes::default()),
            inline_styles: Rc::new(Vec::new()),
            version: NEXT_VERSION.fetch_add(1, Ordering::Relaxed),
            suspend_caches: false,
        };

        content.update_caches();

        let mut history = Record::new();
        history.set_saved();

        Ok(Self { content, history })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_diff(old: &str, new: &str) -> String {
        let (start, old_end, replacement) = minimal_diff(old, new);
        let mut result = String::new();
        result.push_str(&old[..start]);
        result.push_str(&replacement);
        result.push_str(&old[old_end..]);
        result
    }

    #[test]
    fn minimal_diff_reconstructs_and_is_minimal() {
        // Single-char change in the middle: tight span, empty ends.
        let (start, old_end, repl) = minimal_diff("- [ ] task", "- [x] task");
        assert_eq!((start, old_end, repl.as_str()), (3, 4, "x"));

        // Pure insertion and reconstruction across several shapes.
        for (old, new) in [
            ("- [ ] task", "- [x] ~~task~~"),
            ("~~hello~~", "hello"),
            ("abc", "abc"),
            ("", "xyz"),
            ("xyz", ""),
        ] {
            assert_eq!(apply_diff(old, new), new, "reconstruct {old:?} -> {new:?}");
        }
    }

    #[test]
    fn minimal_diff_respects_utf8_boundaries() {
        // Edits adjacent to multibyte chars must land on char boundaries.
        let old = "café ☕ done";
        let new = "café ☕ DONE";
        let (start, old_end, _) = minimal_diff(old, new);
        assert!(old.is_char_boundary(start) && old.is_char_boundary(old_end));
        assert_eq!(apply_diff(old, new), new);

        let old2 = "α β γ";
        let new2 = "α X γ";
        assert_eq!(apply_diff(old2, new2), new2);
    }

    /// The linear `inline_styles_by_line` bucketing must match the original
    /// per-line `styles_in_range` (tree styles only), including multi-line emphasis
    /// that spans a soft line break and an enclosing region over nested ones.
    #[test]
    fn inline_styles_by_line_matches_styles_in_range() {
        let src = "**bold** and *italic*\n\
                   a `code` span here\n\
                   soft **wrapped\n\
                   emphasis** across lines\n\
                   plain final line\n";
        let mut buf: Buffer = src.parse().unwrap();
        let snap = buf.render_snapshot();
        let buckets = snap.inline_styles_by_line();
        assert_eq!(buckets.len(), snap.line_count());
        for (i, bucket) in buckets.iter().enumerate() {
            let range = snap.line_byte_range(i);
            let expected: Vec<StyledRegion> = styles_in_range(&snap.inline_styles, &range)
                .into_iter()
                .cloned()
                .collect();
            assert_eq!(*bucket, expected, "line {i} bucket mismatch (range {range:?})");
        }
        // The multi-line emphasis must appear in both spanned lines' buckets.
        let spans_line2 = buckets[2].iter().any(|s| s.style.bold);
        let spans_line3 = buckets[3].iter().any(|s| s.style.bold);
        assert!(
            spans_line2 && spans_line3,
            "multi-line bold should bucket into both lines"
        );
    }

    #[test]
    fn test_new_buffer_is_empty() {
        let buf = Buffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.len_bytes(), 0);
    }

    #[test]
    fn test_from_str() {
        let buf: Buffer = "hello world".parse().unwrap();
        assert_eq!(buf.text(), "hello world");
        assert_eq!(buf.len_bytes(), 11);
        assert!(!buf.is_empty());
    }

    #[test]
    fn test_insert() {
        let mut buf: Buffer = "hello world".parse().unwrap();
        buf.insert(5, " beautiful", 5);
        assert_eq!(buf.text(), "hello beautiful world");
    }

    #[test]
    fn test_delete() {
        let mut buf: Buffer = "hello world".parse().unwrap();
        buf.delete(5..11, 11);
        assert_eq!(buf.text(), "hello");
    }

    #[test]
    fn test_replace() {
        let mut buf: Buffer = "hello world".parse().unwrap();
        buf.replace(6..11, "rust", 11);
        assert_eq!(buf.text(), "hello rust");
    }

    #[test]
    fn test_line_operations() {
        let buf: Buffer = "line one\nline two\nline three".parse().unwrap();
        assert_eq!(buf.line_count(), 3);
        assert_eq!(buf.line(0), "line one");
        assert_eq!(buf.line(1), "line two");
        assert_eq!(buf.line(2), "line three");
    }

    #[test]
    fn test_byte_to_line() {
        let buf: Buffer = "abc\ndef\nghi".parse().unwrap();
        assert_eq!(buf.byte_to_line(0), 0);
        assert_eq!(buf.byte_to_line(3), 0);
        assert_eq!(buf.byte_to_line(4), 1);
        assert_eq!(buf.byte_to_line(8), 2);
    }

    #[test]
    fn test_tree_exists_after_parse() {
        let buf: Buffer = "# Hello\n\nSome **bold** text.".parse().unwrap();
        assert!(buf.tree().is_some());
    }

    #[test]
    fn test_tree_updated_after_edit() {
        let mut buf: Buffer = "# Hello".parse().unwrap();
        let tree1 = buf.tree().map(|t| t.block_tree().root_node().to_sexp());

        buf.insert(7, " World", 7);
        let tree2 = buf.tree().map(|t| t.block_tree().root_node().to_sexp());

        assert_ne!(tree1, tree2);
    }

    #[test]
    fn test_undo_insert() {
        let mut buf: Buffer = "hello".parse().unwrap();
        buf.insert(5, " world", 5);
        assert_eq!(buf.text(), "hello world");

        let cursor = buf.undo();
        assert_eq!(cursor, Some(5));
        assert_eq!(buf.text(), "hello");
    }

    #[test]
    fn test_redo_insert() {
        let mut buf: Buffer = "hello".parse().unwrap();
        buf.insert(5, " world", 5);
        buf.undo();

        let cursor = buf.redo();
        assert_eq!(cursor, Some(11));
        assert_eq!(buf.text(), "hello world");
    }

    #[test]
    fn test_undo_delete() {
        let mut buf: Buffer = "hello world".parse().unwrap();
        buf.delete(5..11, 11);
        assert_eq!(buf.text(), "hello");

        let cursor = buf.undo();
        assert_eq!(cursor, Some(11));
        assert_eq!(buf.text(), "hello world");
    }

    #[test]
    fn test_dirty_state() {
        let mut buf: Buffer = "hello".parse().unwrap();
        assert!(!buf.is_dirty());

        buf.insert(5, " world", 5);
        assert!(buf.is_dirty());

        buf.mark_clean();
        assert!(!buf.is_dirty());

        buf.insert(11, "!", 11);
        assert!(buf.is_dirty());
    }

    #[test]
    fn test_dirty_after_undo_to_save_point() {
        let mut buf: Buffer = "hello".parse().unwrap();
        buf.insert(5, " world", 5);
        buf.mark_clean();
        assert!(!buf.is_dirty());

        buf.insert(11, "!", 11);
        assert!(buf.is_dirty());

        buf.undo();
        assert!(!buf.is_dirty());
    }

    #[test]
    fn test_dirty_save_point_unreachable() {
        let mut buf: Buffer = "hello".parse().unwrap();
        buf.insert(5, " world", 5);
        buf.mark_clean();

        buf.undo();
        assert!(buf.is_dirty());

        buf.insert(5, " rust", 5);
        assert!(buf.is_dirty());

        buf.undo();
        assert!(buf.is_dirty());
    }

    #[test]
    fn test_multiple_undo_redo() {
        // Multi-char inserts are non-coalescable, so each is its own undo step — this
        // exercises the undo/redo *stack* mechanics (single-char typing coalesces, which
        // is covered separately by the editor coalescing tests).
        let mut buf: Buffer = "a".parse().unwrap();
        buf.insert(1, "bb", 1);
        buf.insert(3, "cc", 3);
        buf.insert(5, "dd", 5);
        assert_eq!(buf.text(), "abbccdd");

        buf.undo();
        assert_eq!(buf.text(), "abbcc");
        buf.undo();
        assert_eq!(buf.text(), "abb");
        buf.undo();
        assert_eq!(buf.text(), "a");

        buf.redo();
        assert_eq!(buf.text(), "abb");
        buf.redo();
        assert_eq!(buf.text(), "abbcc");
        buf.redo();
        assert_eq!(buf.text(), "abbccdd");
    }

    #[test]
    fn test_ordered_list_no_normalization() {
        // Numbers are preserved as-is (no renumbering)
        let buf: Buffer = "1. First\n5. Second\n9. Third\n".parse().unwrap();
        assert_eq!(buf.text(), "1. First\n5. Second\n9. Third\n");
    }

    #[test]
    fn test_ordered_list_parenthesis_no_normalization() {
        // Parenthesis style numbers are preserved as-is
        let buf: Buffer = "1) First\n5) Second\n9) Third\n".parse().unwrap();
        assert_eq!(buf.text(), "1) First\n5) Second\n9) Third\n");
    }

    #[test]
    fn test_unordered_list_unchanged() {
        let buf: Buffer = "- First\n- Second\n- Third\n".parse().unwrap();
        assert_eq!(buf.text(), "- First\n- Second\n- Third\n");
    }

    /// Structural edits that MERGE two ordered lists still renumber, even though the
    /// gate only runs `normalize_ordered_lists` for edits landing in an ordered list:
    /// a merge leaves the edit offset at the seam, which is inside the resulting list,
    /// so `edit_in_ordered_list` still fires. Guards the `normalize` gating (F2) against
    /// a false-negative where merged lists would keep stale numbers.
    #[test]
    fn merging_ordered_lists_renumbers() {
        // Blank-line-separated lists each restarting at 1.; delete the blank to merge.
        let mut buf: Buffer = "1. a\n\n1. b\n".parse().unwrap();
        let blank = buf.text().find("a\n\n").unwrap() + 2; // the second '\n'
        buf.delete(blank..blank + 1, blank);
        assert_eq!(buf.text(), "1. a\n2. b\n", "merged lists renumber");

        // A malformed list (2., 5.) brought to byte 0 by a leading-paragraph delete:
        // the offset-0 descendant must still resolve into the list.
        let mut buf2: Buffer = "x\n\n2. a\n5. b\n".parse().unwrap();
        let end = buf2.text().find("2. a").unwrap();
        buf2.delete(0..end, 0);
        assert_eq!(buf2.text(), "1. a\n2. b\n", "list at offset 0 renumbers");
    }

    #[test]
    fn edit_in_ordered_list_renumbers() {
        // A non-sequential list gets renumbered when the edit lands inside it.
        let mut buf: Buffer = "1. a\n5. b\n".parse().unwrap();
        let off = buf.text().find('a').unwrap() + 1;
        buf.insert(off, "X", off);
        assert_eq!(buf.text(), "1. aX\n2. b\n", "in-list edit renumbers");
    }

    #[test]
    fn edit_outside_ordered_list_does_not_renumber() {
        // Editing a paragraph after a non-sequential list must not perturb its numbers.
        let mut buf: Buffer = "1. a\n5. b\n\nhello\n".parse().unwrap();
        let off = buf.text().find("hello").unwrap() + 5;
        buf.insert(off, "X", off);
        assert_eq!(
            buf.text(),
            "1. a\n5. b\n\nhelloX\n",
            "out-of-list edit leaves list numbering untouched"
        );
    }

    #[test]
    fn delete_in_ordered_list_renumbers() {
        // Deleting a middle item renumbers the remainder (empty edit range inside list).
        let mut buf: Buffer = "1. a\n2. b\n3. c\n".parse().unwrap();
        let start = buf.text().find("2.").unwrap();
        let end = buf.text().find("3.").unwrap();
        buf.delete(start..end, start);
        assert_eq!(buf.text(), "1. a\n2. c\n", "in-list delete renumbers");
    }

    // Line extraction tests (moved from lines.rs)

    use crate::marker::MarkerKind;

    #[test]
    fn test_lines_empty_buffer() {
        let buf: Buffer = "".parse().unwrap();
        let lines = buf.lines();

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].range, 0..0);
        assert!(lines[0].markers.is_empty());
    }

    #[test]
    fn test_lines_single_newline() {
        let buf: Buffer = "\n".parse().unwrap();
        let lines = buf.lines();

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].range, 0..0);
        assert_eq!(lines[1].range, 1..1);
    }

    #[test]
    fn test_lines_paragraph() {
        let buf: Buffer = "Hello\n\nWorld\n".parse().unwrap();
        let lines = buf.lines();

        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].range, 0..5);
        assert_eq!(lines[1].range, 6..6); // blank line
        assert_eq!(lines[2].range, 7..12);
    }

    #[test]
    fn test_heading_markers() {
        let buf: Buffer = "# Hello\n".parse().unwrap();
        let lines = buf.lines();

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].heading_level(), Some(1));
        assert_eq!(lines[0].marker_range(), Some(0..2));
    }

    #[test]
    fn test_heading_levels() {
        let buf: Buffer = "# H1\n## H2\n### H3\n".parse().unwrap();
        let lines = buf.lines();

        assert_eq!(lines[0].heading_level(), Some(1));
        assert_eq!(lines[1].heading_level(), Some(2));
        assert_eq!(lines[2].heading_level(), Some(3));
    }

    #[test]
    fn test_unordered_list_markers() {
        let buf: Buffer = "- Item 1\n- Item 2\n".parse().unwrap();
        let lines = buf.lines();

        assert!(
            lines[0]
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::ListItem { ordered: false, .. }))
        );
        assert_eq!(lines[0].marker_range(), Some(0..2));
    }

    #[test]
    fn test_ordered_list_markers() {
        let buf: Buffer = "1. First\n2. Second\n".parse().unwrap();
        let lines = buf.lines();

        assert!(
            lines[0]
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::ListItem { ordered: true, .. }))
        );
    }

    #[test]
    fn test_checkbox_markers() {
        let buf: Buffer = "- [ ] Unchecked\n- [x] Checked\n".parse().unwrap();
        let lines = buf.lines();

        assert_eq!(lines[0].checkbox(), Some(false));
        assert_eq!(lines[1].checkbox(), Some(true));
    }

    #[test]
    fn test_blockquote_markers() {
        let buf: Buffer = "> Quote\n".parse().unwrap();
        let lines = buf.lines();

        assert!(lines[0].has_border());
        assert!(
            lines[0]
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::BlockQuote))
        );
    }

    #[test]
    fn test_is_line_empty_blockquote_only() {
        // A line with just "> " (blockquote marker, no content) should be empty
        let buf: Buffer = "> hey\n> \n> > hey".parse().unwrap();
        assert!(!buf.is_line_empty(0)); // "> hey" has content
        assert!(buf.is_line_empty(1)); // "> " is empty (marker only)
        assert!(!buf.is_line_empty(2)); // "> > hey" has content
    }

    #[test]
    fn test_marker_range_includes_trailing_space() {
        // "> hey" - marker should be "> " (bytes 0..2)
        let buf: Buffer = "> hey".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(lines[0].marker_range(), Some(0..2)); // "> " includes the space

        // "- item" - marker should be "- " (bytes 0..2)
        let buf: Buffer = "- item".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(lines[0].marker_range(), Some(0..2)); // "- " includes the space

        // "> > hey" - marker should be "> > " (bytes 0..4)
        let buf: Buffer = "> > hey".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(lines[0].marker_range(), Some(0..4)); // "> > " includes both spaces
    }

    #[test]
    fn test_nested_blockquote_lines() {
        let buf: Buffer = "> Level 1\n> > Level 2\n".parse().unwrap();
        let lines = buf.lines();

        assert_eq!(
            lines[0]
                .markers
                .iter()
                .filter(|m| matches!(m.kind, MarkerKind::BlockQuote))
                .count(),
            1
        );
        assert_eq!(
            lines[1]
                .markers
                .iter()
                .filter(|m| matches!(m.kind, MarkerKind::BlockQuote))
                .count(),
            2
        );
    }

    #[test]
    fn test_list_in_blockquote_lines() {
        let buf: Buffer = "> - Item\n".parse().unwrap();
        let lines = buf.lines();

        assert!(
            lines[0]
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::BlockQuote))
        );
        assert!(
            lines[0]
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::ListItem { .. }))
        );
    }

    #[test]
    fn test_code_block_fence_lines() {
        let buf: Buffer = "```rust\nlet x = 1;\n```\n".parse().unwrap();
        let lines = buf.lines();

        assert!(lines[0].is_fence());
        assert!(!lines[1].is_fence());
        assert!(lines[2].is_fence());
    }

    #[test]
    fn test_thematic_break_lines() {
        let buf: Buffer = "---\n".parse().unwrap();
        let lines = buf.lines();

        assert!(
            lines[0]
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::ThematicBreak))
        );
    }

    #[test]
    fn test_nested_list_continuation() {
        let buf: Buffer = "- Item 1\n  - Nested\n".parse().unwrap();
        let lines = buf.lines();

        let continuation = lines[1].continuation_rope(buf.rope());
        assert!(continuation.contains("- "));
    }

    #[test]
    fn test_substitution() {
        let buf: Buffer = "- Item\n".parse().unwrap();
        let lines = buf.lines();

        // List markers are now rendered as spacers, not substitution
        let sub = lines[0].substitution_rope(buf.rope());
        assert_eq!(sub, "");
    }

    #[test]
    fn test_list_in_blockquote_continuation() {
        let buf: Buffer = "> - Item\n".parse().unwrap();
        let lines = buf.lines();

        let continuation = lines[0].continuation_rope(buf.rope());
        assert_eq!(continuation, "> - ");
    }

    #[test]
    fn test_multiline_blockquote_with_list_continuation() {
        let buf: Buffer = "> hey\n>\n> - foo\n".parse().unwrap();
        let lines = buf.lines();

        let continuation = lines[2].continuation_rope(buf.rope());
        assert_eq!(continuation, "> - ");
    }

    #[test]
    fn test_code_block_in_blockquote() {
        let buf: Buffer = "> ```rust\n> fn main() {}\n> ```\n".parse().unwrap();
        let lines = buf.lines();

        assert!(lines[0].is_fence());
        assert!(lines[0].has_border());
    }
}
