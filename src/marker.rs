//! Line markers for markdown block-level elements.
//!
//! This module provides types for representing markers (blockquotes, lists,
//! headings, etc.) and functions for extracting them from the parse tree.

use crate::parser::MarkdownParser;
use crate::table::{TableInfo, extract_table};
use ropey::Rope;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use tree_sitter::Node;

/// Owned node info for storing tree structure without lifetimes.
/// Used for efficient lazy LineMarkers computation during rendering.
#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub start_byte: usize,
    pub end_byte: usize,
    pub kind: &'static str,
    pub parent_kind: Option<&'static str>,
    /// For fenced_code_block_delimiter nodes: true if this is the first (opening) delimiter
    pub is_first_fence_delimiter: bool,
    /// True if this node is inside a checked task list item
    pub in_checked_task: bool,
    /// True if this node is inside a fenced code block
    pub in_code_block: bool,
}

/// Information about a fenced code block, collected during tree traversal.
#[derive(Debug, Clone)]
pub struct CodeBlockInfo {
    /// Byte range of the entire code block (including fences)
    pub block_range: Range<usize>,
    /// Byte range of the code content (between fences, excluding fence lines)
    pub content_range: Range<usize>,
    /// Byte range of the info_string (language specifier), if any
    pub info_string_range: Option<Range<usize>>,
}

/// Collected parse information from a tree traversal.
/// Contains both the node list and extracted structural info.
#[derive(Debug, Clone, Default)]
pub struct ParsedNodes {
    /// All nodes in the tree, in preorder traversal order
    pub nodes: Vec<NodeInfo>,
    /// Information about fenced code blocks, sorted by start position
    pub code_blocks: Vec<CodeBlockInfo>,
    /// GFM tables, sorted by `block.start` (traversal order)
    pub tables: Vec<TableInfo>,
}

/// The unordered list marker character.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnorderedMarker {
    Minus,
    Star,
    Plus,
}

/// The ordered list marker style.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OrderedMarker {
    Dot,
    Parenthesis,
}

/// The type of marker on a line.
#[derive(Debug, Clone, PartialEq)]
pub enum MarkerKind {
    BlockQuote,
    ListItem {
        ordered: bool,
        unordered_marker: Option<UnorderedMarker>,
        ordered_marker: Option<OrderedMarker>,
        /// The number for ordered lists (e.g., 1, 2, 3)
        number: Option<u32>,
    },
    /// A checkbox marker: `[ ]` or `[x]` (rendered inline, not as spacer)
    Checkbox {
        checked: bool,
    },
    Heading(u8),
    /// A code block fence (``` or ~~~).
    /// `is_opening` is true for the opening fence, false for the closing fence.
    CodeBlockFence {
        language: Option<String>,
        is_opening: bool,
    },
    ThematicBreak,
    Indent,
}

/// A marker on a line with its byte range.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub kind: MarkerKind,
    pub range: Range<usize>,
}

/// A line with its markers and metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct LineMarkers {
    pub range: Range<usize>,
    pub line_number: usize,
    pub markers: Vec<Marker>,
    /// True if this line is inside a checked task list item
    pub in_checked_task: bool,
    /// True if this line is inside a fenced code block (content lines, not fence lines)
    pub in_code_block: bool,
}

impl LineMarkers {
    /// Combined byte span of the markers matching `pred`, in one pass. Markers are
    /// stored outermost-first, so the first match carries the widest right edge and
    /// the last match the leftmost start — mirroring the old filter-to-Vec then
    /// `last().start .. first().end`, without the intermediate allocation.
    fn span_of(&self, pred: impl Fn(&Marker) -> bool) -> Option<Range<usize>> {
        let mut iter = self.markers.iter().filter(|m| pred(m));
        let first = iter.next()?;
        let end = first.range.end;
        let start = iter.last().map_or(first.range.start, |m| m.range.start);
        Some(start..end)
    }

    /// Returns the combined byte range of spacer markers (excluding Checkbox).
    /// Checkbox markers are rendered inline, not as spacers, so they don't
    /// contribute to wrap indent.
    pub fn marker_range(&self) -> Option<Range<usize>> {
        self.span_of(|m| !matches!(m.kind, MarkerKind::Checkbox { .. }))
    }

    /// Returns the combined byte range of ALL markers (including Checkbox).
    /// Used for determining where content actually starts.
    pub fn full_marker_range(&self) -> Option<Range<usize>> {
        self.span_of(|_| true)
    }

    /// Returns the range of prefix markers (Indent, BlockQuote) that are rendered as spacers.
    /// Excludes CodeBlockFence markers. Used by fence lines to know where fence content starts.
    pub fn prefix_marker_range(&self) -> Option<Range<usize>> {
        self.span_of(|m| matches!(m.kind, MarkerKind::Indent | MarkerKind::BlockQuote))
    }

    /// Returns the byte offset where content starts (after all markers).
    pub fn content_start(&self) -> usize {
        self.full_marker_range()
            .map(|r| r.end)
            .unwrap_or(self.range.start)
    }

    /// Returns the width of the marker (including trailing space) relative to line start.
    /// This is the number of spaces needed to nest under this line.
    /// E.g., "- " = 2, "1. " = 3, "10. " = 4
    pub fn marker_width(&self) -> usize {
        if let Some(range) = self.marker_range() {
            range.end - self.range.start
        } else {
            0
        }
    }

    /// Returns the visual substitution text for all markers.
    /// E.g., "• " for unordered list, "[ ] " for unchecked task.
    /// Computes leading whitespace from line start to the first non-whitespace
    /// character, to respect user's manual indentation.
    pub fn substitution_rope(&self, rope: &Rope) -> String {
        if self.markers.is_empty() {
            return String::new();
        }

        if self.markers.iter().all(|m| {
            matches!(
                m.kind,
                MarkerKind::Indent
                    | MarkerKind::BlockQuote
                    | MarkerKind::ListItem { .. }
                    | MarkerKind::Checkbox { .. }
                    | MarkerKind::CodeBlockFence { .. }
            )
        }) {
            return String::new();
        }

        let spacer_end = self
            .markers
            .iter()
            .filter(|m| matches!(m.kind, MarkerKind::Indent | MarkerKind::BlockQuote))
            .map(|m| m.range.end)
            .max()
            .unwrap_or(self.range.start);

        let ws_scan_start = spacer_end;
        let mut leading_ws_end = ws_scan_start;
        for byte_idx in ws_scan_start..self.range.end {
            if let Some(b) = rope.get_byte(byte_idx) {
                if b == b' ' || b == b'\t' {
                    leading_ws_end = byte_idx + 1;
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        let leading_ws = if leading_ws_end > ws_scan_start {
            rope_slice_cow(rope, ws_scan_start, leading_ws_end)
        } else {
            std::borrow::Cow::Borrowed("")
        };

        let mut result = leading_ws.into_owned();
        for m in self.markers.iter().rev() {
            if !matches!(
                m.kind,
                MarkerKind::Indent
                    | MarkerKind::BlockQuote
                    | MarkerKind::ListItem { .. }
                    | MarkerKind::Checkbox { .. }
            ) {
                result.push_str(m.kind.substitution());
            }
        }
        result
    }

    /// Returns the continuation text to insert on Enter.
    /// E.g., "> " for blockquote, "- " for list, "  " for indent.
    /// Markers are stored innermost to outermost, but continuation should be
    /// in text order (outermost to innermost), so we reverse.
    /// For ordered lists, the number is incremented (e.g., "2. " becomes "3. ").
    pub fn continuation_rope(&self, rope: &Rope) -> String {
        self.markers
            .iter()
            .rev()
            .map(|m| match &m.kind {
                MarkerKind::ListItem {
                    ordered: true,
                    ordered_marker,
                    ..
                } => {
                    let text = rope_slice_cow(rope, m.range.start, m.range.end);
                    increment_ordered_marker(&text, *ordered_marker)
                }
                MarkerKind::Indent
                | MarkerKind::ListItem { ordered: false, .. }
                | MarkerKind::BlockQuote => {
                    rope_slice_cow(rope, m.range.start, m.range.end).into_owned()
                }
                MarkerKind::Checkbox { .. } => {
                    // When continuing, always use unchecked checkbox
                    "[ ] ".to_string()
                }
                _ => m.kind.continuation().to_string(),
            })
            .collect()
    }

    /// Returns true if any marker has a left border (blockquotes).
    pub fn has_border(&self) -> bool {
        self.markers.iter().any(|m| m.kind.has_border())
    }

    /// Returns true if this line has any container markers (list, blockquote, checkbox).
    pub fn has_container(&self) -> bool {
        self.markers.iter().any(|m| m.kind.is_container())
    }

    /// Returns the checkbox state if this line has a checkbox marker.
    pub fn checkbox(&self) -> Option<bool> {
        for m in &self.markers {
            if let MarkerKind::Checkbox { checked } = m.kind {
                return Some(checked);
            }
        }
        None
    }

    /// Returns the leading whitespace before the first marker.
    pub fn leading_whitespace(&self, text: &str) -> String {
        if let Some(first) = self.markers.first()
            && first.range.start > self.range.start
        {
            return text[self.range.start..first.range.start].to_string();
        }
        String::new()
    }

    /// Returns true if this line is a code block fence (opening or closing).
    pub fn is_fence(&self) -> bool {
        self.markers
            .iter()
            .any(|m| matches!(m.kind, MarkerKind::CodeBlockFence { .. }))
    }

    /// Returns true if this line is a thematic break (horizontal rule).
    pub fn is_thematic_break(&self) -> bool {
        self.markers
            .iter()
            .any(|m| matches!(m.kind, MarkerKind::ThematicBreak))
    }

    /// Returns the heading level if this line is a heading.
    pub fn heading_level(&self) -> Option<u8> {
        for m in &self.markers {
            if let MarkerKind::Heading(level) = m.kind {
                return Some(level);
            }
        }
        None
    }

    /// Returns true if this line contains only blockquote/indent markers (no lists).
    pub fn is_blockquote_only(&self) -> bool {
        self.markers
            .iter()
            .all(|m| matches!(m.kind, MarkerKind::BlockQuote | MarkerKind::Indent))
            && self
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::BlockQuote))
    }

    /// Returns the indentation string for a nested paragraph under the current markers.
    /// List markers are converted to equivalent whitespace indentation.
    /// E.g., "- item" -> "  " (2 spaces), "> - item" -> ">   " (blockquote + 2 spaces)
    pub fn nested_paragraph_indent(&self, rope: &Rope) -> String {
        let mut result = String::new();
        for m in self.markers.iter().rev() {
            match &m.kind {
                MarkerKind::BlockQuote => {
                    result.push_str("> ");
                }
                MarkerKind::ListItem { ordered: false, .. } => {
                    result.push_str("  ");
                }
                MarkerKind::Checkbox { .. } => {
                    // Checkbox doesn't contribute to nested indent - it's rendered inline
                }
                MarkerKind::ListItem { ordered: true, .. } => {
                    let indent_len = m.range.end - m.range.start;
                    for _ in 0..indent_len {
                        result.push(' ');
                    }
                }
                MarkerKind::Indent => {
                    result.push_str(&rope_slice_cow(rope, m.range.start, m.range.end));
                }
                _ => {}
            }
        }
        result
    }
}

impl UnorderedMarker {
    /// Visual bullet for this marker type.
    pub fn bullet(&self) -> &'static str {
        match self {
            UnorderedMarker::Minus => "• ", // filled circle
            UnorderedMarker::Star => "◦ ",  // white bullet (small hollow)
            UnorderedMarker::Plus => "‣ ",  // triangular bullet
        }
    }
}

impl MarkerKind {
    /// Visual substitution text for this marker kind.
    pub fn substitution(&self) -> &'static str {
        match self {
            MarkerKind::BlockQuote => "  ", // Replace "> " with spaces, border shows visually
            MarkerKind::ListItem {
                ordered: false,
                unordered_marker,
                ..
            } => unordered_marker.map_or("• ", |m| m.bullet()),
            MarkerKind::ListItem { ordered: true, .. } => "",
            MarkerKind::Checkbox { checked: false } => "[ ] ",
            MarkerKind::Checkbox { checked: true } => "[x] ",
            MarkerKind::Heading(_) => "",
            MarkerKind::CodeBlockFence { .. } => "",
            MarkerKind::ThematicBreak => "",
            MarkerKind::Indent => "  ",
        }
    }

    /// Continuation text to insert on Enter.
    pub fn continuation(&self) -> &'static str {
        match self {
            MarkerKind::BlockQuote => "> ",
            MarkerKind::ListItem { ordered: false, .. } => "- ",
            MarkerKind::ListItem { ordered: true, .. } => "1. ",
            MarkerKind::Checkbox { .. } => "[ ] ",
            MarkerKind::Heading(_) => "",
            MarkerKind::CodeBlockFence { .. } => "",
            MarkerKind::ThematicBreak => "",
            MarkerKind::Indent => "",
        }
    }

    /// Whether this marker has a left border.
    pub fn has_border(&self) -> bool {
        matches!(self, MarkerKind::BlockQuote)
    }

    /// Whether this marker represents an active container.
    /// Containers are structures where Enter creates siblings or exits,
    /// as opposed to plain text where Enter creates paragraph breaks.
    pub fn is_container(&self) -> bool {
        matches!(self, MarkerKind::ListItem { .. } | MarkerKind::BlockQuote)
    }

    /// Returns true if this is a list marker (ordered or unordered).
    pub fn is_list_item(&self) -> bool {
        matches!(self, MarkerKind::ListItem { .. })
    }

    /// Returns true if this is a block-level marker that increases nesting depth.
    pub fn is_block_level(&self) -> bool {
        matches!(
            self,
            MarkerKind::BlockQuote
                | MarkerKind::ListItem { .. }
                | MarkerKind::CodeBlockFence { .. }
        )
    }

    /// Returns true if this is a checkbox marker.
    pub fn is_checkbox(&self) -> bool {
        matches!(self, MarkerKind::Checkbox { .. })
    }

    /// Convert marker to its status bar string representation.
    pub fn status_bar_str(&self) -> String {
        match self {
            MarkerKind::BlockQuote => ">".to_string(),
            MarkerKind::ListItem {
                ordered: false,
                unordered_marker,
                ..
            } => match unordered_marker {
                Some(UnorderedMarker::Minus) => "-".to_string(),
                Some(UnorderedMarker::Star) => "*".to_string(),
                Some(UnorderedMarker::Plus) => "+".to_string(),
                None => "-".to_string(),
            },
            MarkerKind::ListItem {
                ordered: true,
                ordered_marker,
                number,
                ..
            } => {
                let n = number.unwrap_or(1);
                match ordered_marker {
                    Some(OrderedMarker::Dot) | None => format!("{}.", n),
                    Some(OrderedMarker::Parenthesis) => format!("{})", n),
                }
            }
            MarkerKind::Checkbox { checked: false } => "[ ]".to_string(),
            MarkerKind::Checkbox { checked: true } => "[x]".to_string(),
            MarkerKind::CodeBlockFence { language, .. } => language
                .as_ref()
                .map(|l| format!("```{}", l))
                .unwrap_or_else(|| "```".to_string()),
            MarkerKind::Heading(level) => format!("H{}", level),
            MarkerKind::ThematicBreak => "---".to_string(),
            MarkerKind::Indent => "".to_string(),
        }
    }
}

/// Find the index of the first NodeInfo with start_byte >= target.
fn find_node_info_index(nodes: &[NodeInfo], target_byte: usize) -> usize {
    nodes
        .binary_search_by_key(&target_byte, |n| n.start_byte)
        .unwrap_or_else(|idx| idx)
}

/// Extend a marker's end byte to include a single trailing space, if present.
fn extend_over_space(rope: &Rope, end: usize) -> usize {
    if rope.get_byte(end) == Some(b' ') {
        end + 1
    } else {
        end
    }
}

/// Get a byte slice from a Rope, borrowing if possible.
/// Returns a Cow that borrows if the slice fits in one chunk, allocates otherwise.
fn rope_slice_cow(rope: &Rope, start: usize, end: usize) -> std::borrow::Cow<'_, str> {
    let slice = rope.byte_slice(start..end);
    match slice.as_str() {
        Some(s) => std::borrow::Cow::Borrowed(s),
        None => std::borrow::Cow::Owned(slice.to_string()),
    }
}

/// Parse an ordered list marker (e.g., "2. " or "10) ") and return the next number.
/// Returns the incremented marker string, preserving the style (dot vs parenthesis).
fn increment_ordered_marker(text: &str, ordered_marker: Option<OrderedMarker>) -> String {
    let num_end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let num_str = &text[..num_end];

    let num: u32 = num_str.parse().unwrap_or(0);
    let next_num = num + 1;

    let suffix = match ordered_marker {
        Some(OrderedMarker::Parenthesis) => ") ",
        _ => ". ",
    };

    format!("{}{}", next_num, suffix)
}

/// Extract a marker from a tree-sitter node.
/// Handles block_quote_marker and list_marker_* nodes.
/// Returns (main_marker, optional_indent_for_leading_whitespace).
fn marker_from_node(
    node_kind: &str,
    rope: &Rope,
    start: usize,
    end: usize,
) -> (Option<Marker>, Option<Marker>) {
    let content = rope_slice_cow(rope, start, end);
    let bytes = content.as_bytes();

    let mut marker_start = 0;
    while marker_start < bytes.len()
        && (bytes[marker_start] == b' ' || bytes[marker_start] == b'\t')
    {
        marker_start += 1;
    }

    let indent_marker = if marker_start > 0 {
        Some(Marker {
            kind: MarkerKind::Indent,
            range: start..(start + marker_start),
        })
    } else {
        None
    };

    let marker = match node_kind {
        "block_quote_marker" => {
            if let Some(rel_gt) = content[marker_start..].find('>') {
                let gt_pos = marker_start + rel_gt;
                let range_end = if bytes.get(gt_pos + 1) == Some(&b' ') {
                    gt_pos + 2
                } else {
                    gt_pos + 1
                };
                Some(Marker {
                    kind: MarkerKind::BlockQuote,
                    range: (start + gt_pos)..(start + range_end),
                })
            } else {
                None
            }
        }
        "list_marker_minus" | "list_marker_plus" | "list_marker_star" => {
            let unordered_marker = Some(match node_kind {
                "list_marker_minus" => UnorderedMarker::Minus,
                "list_marker_star" => UnorderedMarker::Star,
                "list_marker_plus" => UnorderedMarker::Plus,
                _ => unreachable!(),
            });
            Some(Marker {
                kind: MarkerKind::ListItem {
                    ordered: false,
                    unordered_marker,
                    ordered_marker: None,
                    number: None,
                },
                range: (start + marker_start)..end,
            })
        }
        "list_marker_dot" | "list_marker_parenthesis" => {
            let ordered_marker = Some(match node_kind {
                "list_marker_dot" => OrderedMarker::Dot,
                "list_marker_parenthesis" => OrderedMarker::Parenthesis,
                _ => unreachable!(),
            });
            // Extract the number from the marker text (e.g., "1. " -> 1)
            let number = content[marker_start..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<u32>()
                .ok();
            Some(Marker {
                kind: MarkerKind::ListItem {
                    ordered: true,
                    unordered_marker: None,
                    ordered_marker,
                    number,
                },
                range: (start + marker_start)..end,
            })
        }
        _ => None,
    };

    (marker, indent_marker)
}

/// Shift every marker range by `to - from` (all ranges are ≥ `from`). Used to move
/// cached markers between the canonical prefix-relative frame (base 0) and the live
/// `start` offset.
fn rebase_markers(markers: &[Marker], from: usize, to: usize) -> Vec<Marker> {
    markers
        .iter()
        .map(|m| Marker {
            kind: m.kind.clone(),
            range: (m.range.start - from + to)..(m.range.end - from + to),
        })
        .collect()
}

/// Parse a continuation string into markers using tree-sitter.
/// Returns markers innermost-to-outermost (reverse document order) for use by markers_at.
///
/// The marker set for a prefix depends only on its text, not on where it sits in the
/// document, so results are memoized per prefix string (ranges stored relative to the
/// prefix, then re-offset to `start`). Continuation prefixes are short and highly
/// repetitive (`> `, `- `, `  `, …), so this turns almost every call into a lookup.
pub fn parse_continuation(rope: &Rope, start: usize, end: usize) -> Vec<Marker> {
    thread_local! {
        static CACHE: RefCell<HashMap<String, Vec<Marker>>> = RefCell::new(HashMap::new());
    }

    let content = rope_slice_cow(rope, start, end);
    if let Some(rel) = CACHE.with(|c| c.borrow().get(content.as_ref()).cloned()) {
        return rebase_markers(&rel, 0, start);
    }

    let markers = parse_continuation_uncached(rope, start, end);
    let rel = rebase_markers(&markers, start, 0);
    CACHE.with(|c| c.borrow_mut().insert(content.into_owned(), rel));
    markers
}

/// Uncached tree-sitter parse of the continuation prefix at `[start, end)`.
fn parse_continuation_uncached(rope: &Rope, start: usize, end: usize) -> Vec<Marker> {
    thread_local! {
        static PARSER: RefCell<MarkdownParser> = RefCell::new(MarkdownParser::default());
    }

    let mut markers = Vec::new();

    let content = rope_slice_cow(rope, start, end);
    let bytes = content.as_bytes();
    let tree =
        PARSER.with_borrow_mut(|parser| parser.parse_with(&mut |byte, _| &bytes[byte..], None));
    let Some(tree) = tree else {
        return markers;
    };

    let root = tree.block_tree().root_node();
    let mut last_marker_end = 0usize;
    let mut first_marker_seen = false;

    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        let kind = node.kind();

        if matches!(
            kind,
            "block_quote_marker"
                | "list_marker_minus"
                | "list_marker_plus"
                | "list_marker_star"
                | "list_marker_dot"
                | "list_marker_parenthesis"
        ) {
            let node_start = node.start_byte();
            let node_end = node.end_byte();
            let (marker, indent) =
                marker_from_node(kind, rope, start + node_start, start + node_end);

            if let Some(ref ind) = indent {
                let indent_starts_at_zero = ind.range.start == start;
                if first_marker_seen || indent_starts_at_zero {
                    markers.insert(0, ind.clone());
                }
            }
            if let Some(m) = marker {
                first_marker_seen = true;
                last_marker_end = m.range.end - start;
                markers.insert(0, m);
            }
        }

        if cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                if last_marker_end > 0 && last_marker_end < content.len() {
                    let trailing = &content[last_marker_end..];
                    if !trailing.is_empty() && trailing.chars().all(|c| c.is_whitespace()) {
                        markers.insert(
                            0,
                            Marker {
                                kind: MarkerKind::Indent,
                                range: (start + last_marker_end)..(start + content.len()),
                            },
                        );
                    }
                }
                return markers;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Check if a list_item node contains a checked task marker.
fn list_item_is_checked_task(node: &Node) -> bool {
    let mut child_cursor = node.walk();
    for child in node.children(&mut child_cursor) {
        if child.kind() == "task_list_marker_checked" {
            return true;
        }
    }
    false
}

/// Collect all nodes as owned NodeInfo structs (no lifetimes).
/// Used for lazy LineMarkers computation during rendering.
pub fn collect_node_infos(root: &Node, rope: &Rope) -> ParsedNodes {
    let mut cursor = root.walk();
    let mut nodes = Vec::new();
    let mut code_blocks = Vec::new();
    let mut tables = Vec::new();
    let mut checked_task_stack: Vec<(usize, bool)> = Vec::new();
    let mut code_block_end: Option<usize> = None;
    // Ancestor kinds, pushed on descent and popped on ascent, so the current node's
    // parent kind is `kind_stack.last()` in O(1) (avoids re-descending from root).
    let mut kind_stack: Vec<&'static str> = Vec::new();
    // Whether the current fenced_code_block's first delimiter has been emitted yet.
    // Reset on entering each fence (fences don't nest), so the first delimiter child is
    // flagged without re-walking the parent's children.
    let mut fence_first_delim_seen = false;

    loop {
        let node = cursor.node();

        while let Some(&(end_byte, _)) = checked_task_stack.last() {
            if node.start_byte() >= end_byte {
                checked_task_stack.pop();
            } else {
                break;
            }
        }

        if let Some(end_byte) = code_block_end
            && node.start_byte() >= end_byte
        {
            code_block_end = None;
        }

        if node.kind() == "list_item" {
            let is_checked = list_item_is_checked_task(&node);
            checked_task_stack.push((node.end_byte(), is_checked));
        }

        if node.kind() == "fenced_code_block" {
            code_block_end = Some(node.end_byte());
            fence_first_delim_seen = false;

            let block_range = node.start_byte()..node.end_byte();
            let mut content_start: Option<usize> = None;
            let mut content_end: Option<usize> = None;
            let mut info_string_range: Option<Range<usize>> = None;
            let mut fence_count = 0;

            let mut child_cursor = node.walk();
            for child in node.children(&mut child_cursor) {
                match child.kind() {
                    "info_string" => {
                        info_string_range = Some(child.start_byte()..child.end_byte());
                    }
                    "fenced_code_block_delimiter" => {
                        fence_count += 1;
                        if fence_count == 2 {
                            content_end = Some(child.start_byte());
                        }
                    }
                    "code_fence_content" => {
                        if content_start.is_none() {
                            content_start = Some(child.start_byte());
                        }
                        content_end = Some(child.end_byte());
                    }
                    _ => {
                        if fence_count == 1 && content_start.is_none() {
                            content_start = Some(child.start_byte());
                        }
                    }
                }
            }

            let content_range =
                content_start.unwrap_or(node.end_byte())..content_end.unwrap_or(node.end_byte());

            code_blocks.push(CodeBlockInfo {
                block_range,
                content_range,
                info_string_range,
            });
        }

        if node.kind() == "pipe_table"
            && let Some(table) = extract_table(&node, rope)
        {
            tables.push(table);
        }

        let in_checked_task = checked_task_stack.iter().any(|(_, checked)| *checked);
        let in_code_block = code_block_end.is_some();
        let parent_kind = kind_stack.last().copied();

        let is_first_fence_delimiter = if node.kind() == "fenced_code_block_delimiter" {
            if parent_kind == Some("fenced_code_block") {
                let first = !fence_first_delim_seen;
                fence_first_delim_seen = true;
                first
            } else {
                true
            }
        } else {
            false
        };

        nodes.push(NodeInfo {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            kind: node.kind(),
            parent_kind,
            is_first_fence_delimiter,
            in_checked_task,
            in_code_block,
        });

        if cursor.goto_first_child() {
            kind_stack.push(node.kind());
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return ParsedNodes {
                    nodes,
                    code_blocks,
                    tables,
                };
            }
            kind_stack.pop();
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Find all markers for a line by scanning NodeInfo structs that start within the line.
/// Returns markers innermost to outermost (reverse document order).
/// Takes a pre-computed nodes vec from `collect_node_infos()` for efficiency.
/// This version works with owned NodeInfo instead of borrowed Node<'a>.
pub fn markers_at_from_infos(
    nodes: &[NodeInfo],
    rope: &Rope,
    line_start: usize,
    line_end: usize,
) -> Vec<Marker> {
    let mut markers = Vec::new();
    let mut pending_task: Option<(bool, Range<usize>)> = None;

    let end_idx = find_node_info_index(nodes, line_end + 1);

    for node in nodes[..end_idx].iter().rev() {
        let start = node.start_byte;
        if start < line_start {
            break;
        }
        let end = node.end_byte;
        let kind = node.kind;

        match kind {
            "block_quote_marker" | "block_continuation" => {
                if kind == "block_continuation" && node.parent_kind == Some("indented_code_block") {
                    continue;
                }
                let content = rope_slice_cow(rope, start, end);
                if content.contains('>') {
                    markers.extend(parse_continuation(rope, start, end));
                } else if !content.is_empty() && content.chars().all(|c| c.is_whitespace()) {
                    markers.push(Marker {
                        kind: MarkerKind::Indent,
                        range: start..end,
                    });
                }
            }
            "list_marker_minus" | "list_marker_plus" | "list_marker_star" => {
                let (marker, indent) = marker_from_node(kind, rope, start, end);

                if let Some(ind) = indent {
                    markers.push(ind);
                }

                // If there's a pending checkbox, add it as a separate Checkbox marker
                if let Some((checked, checkbox_range)) = pending_task.take() {
                    markers.push(Marker {
                        kind: MarkerKind::Checkbox { checked },
                        range: checkbox_range,
                    });
                }

                // Always add the list item marker
                if let Some(m) = marker {
                    markers.push(m);
                }
            }
            "list_marker_dot" | "list_marker_parenthesis" => {
                let (marker, indent) = marker_from_node(kind, rope, start, end);

                if let Some(ind) = indent {
                    markers.push(ind);
                }

                if let Some(m) = marker {
                    markers.push(m);
                }
            }
            "task_list_marker_unchecked" | "task_list_marker_checked" => {
                let checked = kind == "task_list_marker_checked";
                let checkbox_start = start;
                let range_end = extend_over_space(rope, end);
                pending_task = Some((checked, checkbox_start..range_end));
            }
            "atx_h1_marker" | "atx_h2_marker" | "atx_h3_marker" | "atx_h4_marker"
            | "atx_h5_marker" | "atx_h6_marker" => {
                let level = match kind {
                    "atx_h1_marker" => 1,
                    "atx_h2_marker" => 2,
                    "atx_h3_marker" => 3,
                    "atx_h4_marker" => 4,
                    "atx_h5_marker" => 5,
                    _ => 6,
                };
                let range_end = extend_over_space(rope, end);
                markers.push(Marker {
                    kind: MarkerKind::Heading(level),
                    range: start..range_end,
                });
            }
            "thematic_break" => {
                markers.push(Marker {
                    kind: MarkerKind::ThematicBreak,
                    range: start..end,
                });
            }
            "fenced_code_block_delimiter" => {
                let language = markers.iter().find_map(|m| {
                    if let MarkerKind::CodeBlockFence { language, .. } = &m.kind {
                        language.clone()
                    } else {
                        None
                    }
                });
                markers.retain(|m| !matches!(m.kind, MarkerKind::CodeBlockFence { .. }));

                let is_opening = node.is_first_fence_delimiter;

                markers.push(Marker {
                    kind: MarkerKind::CodeBlockFence {
                        language,
                        is_opening,
                    },
                    range: start..end,
                });
            }
            "info_string" => {
                let lang = rope_slice_cow(rope, start, end);
                let lang = lang.trim();
                let language = if lang.is_empty() {
                    None
                } else {
                    Some(lang.to_string())
                };
                markers.push(Marker {
                    kind: MarkerKind::CodeBlockFence {
                        language,
                        is_opening: true,
                    },
                    range: start..end,
                });
            }

            _ => {}
        }
    }

    // Fallback: tree-sitter doesn't emit fenced_code_block_delimiter for closing
    // fences without a trailing newline. Detect them manually.
    // Note: this doesn't handle fences inside blockquotes - that's a tree-sitter limitation.
    if !markers
        .iter()
        .any(|m| matches!(m.kind, MarkerKind::CodeBlockFence { .. }))
    {
        let line_text = rope_slice_cow(rope, line_start, line_end);
        let trimmed = line_text.trim();
        if (trimmed.starts_with("```") && trimmed.chars().skip(3).all(|c| c == '`'))
            || (trimmed.starts_with("~~~") && trimmed.chars().skip(3).all(|c| c == '~'))
        {
            markers.push(Marker {
                kind: MarkerKind::CodeBlockFence {
                    language: None,
                    is_opening: false,
                },
                range: line_start..line_end,
            });
        }
    }

    markers
}

/// Check if a line is inside a checked task by finding the first node that starts
/// within the line range.
pub fn is_line_in_checked_task(nodes: &[NodeInfo], line_start: usize) -> bool {
    let idx = find_node_info_index(nodes, line_start);
    nodes.get(idx).map(|n| n.in_checked_task).unwrap_or(false)
}

/// Check if a line is inside a fenced code block by finding the first node that starts
/// within the line range.
pub fn is_line_in_code_block(nodes: &[NodeInfo], line_start: usize) -> bool {
    let idx = find_node_info_index(nodes, line_start);
    nodes.get(idx).map(|n| n.in_code_block).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    fn kinds(markers: &[Marker]) -> Vec<&MarkerKind> {
        markers.iter().map(|m| &m.kind).collect()
    }

    // Helper to check if marker is an unordered list item
    fn is_unordered_list(kind: &MarkerKind) -> bool {
        matches!(kind, MarkerKind::ListItem { ordered: false, .. })
    }

    // Helper to check if marker is an ordered list item
    fn is_ordered_list(kind: &MarkerKind) -> bool {
        matches!(kind, MarkerKind::ListItem { ordered: true, .. })
    }

    // Helper to check if marker is an unchecked checkbox
    fn is_checkbox_unchecked(kind: &MarkerKind) -> bool {
        matches!(kind, MarkerKind::Checkbox { checked: false })
    }

    // Helper to check if marker is a checked checkbox
    fn is_checkbox_checked(kind: &MarkerKind) -> bool {
        matches!(kind, MarkerKind::Checkbox { checked: true })
    }

    fn print_tree(node: &tree_sitter::Node, text: &str, indent: usize) {
        let spacing = "  ".repeat(indent);
        let preview: String = text[node.byte_range()]
            .chars()
            .take(20)
            .flat_map(|c| if c == '\n' { vec!['\\', 'n'] } else { vec![c] })
            .collect();
        println!(
            "{}{} [{}-{}] {:?}",
            spacing,
            node.kind(),
            node.start_byte(),
            node.end_byte(),
            preview,
        );
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                print_tree(&child, text, indent + 1);
            }
        }
    }

    fn print_nodes_by_position(root: &tree_sitter::Node, text: &str) {
        let mut cursor = root.walk();
        let mut nodes = Vec::new();

        loop {
            nodes.push((
                cursor.node().start_byte(),
                cursor.node().end_byte(),
                cursor.node().kind().to_string(),
            ));

            if cursor.goto_first_child() {
                continue;
            }
            if cursor.goto_next_sibling() {
                continue;
            }
            loop {
                if !cursor.goto_parent() {
                    // Print sorted by start position
                    nodes.sort_by_key(|(start, _, _)| *start);
                    println!("\nNodes by position:");
                    for (start, end, kind) in &nodes {
                        let preview: String = text[*start..*end]
                            .chars()
                            .take(15)
                            .flat_map(|c| if c == '\n' { vec!['\\', 'n'] } else { vec![c] })
                            .collect();
                        println!("  [{}-{}] {} {:?}", start, end, kind, preview);
                    }
                    return;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    #[test]
    fn test_block_continuation_structure() {
        // Understand where block_continuation nodes appear
        let buf: Buffer = "> Line 1\n> Line 2\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();

        println!("\n=== Multiline blockquote ===");
        println!("Text: {:?}", text);
        print_tree(&root, &text, 0);

        // Line 2 is bytes 9-17 ("> Line 2")
        // Probe at end of line (16) - where do we land?
        let probe = 16;
        let node = root.descendant_for_byte_range(probe, probe);
        println!(
            "\nProbe at {}: {:?}",
            probe,
            node.map(|n| (n.kind(), n.byte_range()))
        );

        // What is the first child of inline?
        if let Some(inline) = node {
            println!(
                "inline first child: {:?}",
                inline.child(0).map(|c| (c.kind(), c.byte_range()))
            );
        }

        // What about probing at 10 (inside block_continuation)?
        let probe = 10;
        let node = root.descendant_for_byte_range(probe, probe);
        println!(
            "Probe at {}: {:?}",
            probe,
            node.map(|n| (n.kind(), n.byte_range()))
        );
    }

    #[test]
    fn test_simple_list() {
        let buf: Buffer = "- Item\n".parse().unwrap();
        let lines = buf.lines();
        assert!(matches!(
            lines[0].markers.first().map(|m| &m.kind),
            Some(MarkerKind::ListItem { ordered: false, .. })
        ));
    }

    #[test]
    fn test_empty_list_item() {
        // "- " with no content - should still be recognized as a list item
        let buf: Buffer = "- \n".parse().unwrap();
        let lines = buf.lines();
        assert!(matches!(
            lines[0].markers.first().map(|m| &m.kind),
            Some(MarkerKind::ListItem { ordered: false, .. })
        ));
    }

    #[test]
    fn test_list_items_with_paragraph_break() {
        // Two list items with a blank line between them
        let buf: Buffer = "- hey\n\n- \n".parse().unwrap();
        let lines = buf.lines();

        // Line 0: first list item
        assert_eq!(lines[0].markers.len(), 1);
        assert!(is_unordered_list(&lines[0].markers[0].kind));

        // Line 1: blank line
        assert!(lines[1].markers.is_empty());

        // Line 2: second list item (empty)
        assert_eq!(lines[2].markers.len(), 1);
        assert!(is_unordered_list(&lines[2].markers[0].kind));
    }

    #[test]
    fn test_nested_paragraph_under_task_list() {
        // Task list with nested paragraph - uses 2-space indent (not 6)
        // because 4+ spaces triggers indented code block detection
        let buf: Buffer = "- [ ] task\n\n  nested\n".parse().unwrap();
        let lines = buf.lines();

        // Line 0: task list item - now has 2 markers (Checkbox + ListItem)
        assert_eq!(lines[0].markers.len(), 2);
        assert!(is_checkbox_unchecked(&lines[0].markers[0].kind));
        assert!(is_unordered_list(&lines[0].markers[1].kind));

        // Line 1: blank
        assert!(lines[1].markers.is_empty());

        // Line 2: nested paragraph with 2-space indent
        assert_eq!(kinds(&lines[2].markers), vec![&MarkerKind::Indent]);
        assert_eq!(lines[2].markers[0].range.len(), 2);
    }

    #[test]
    fn test_nested_paragraph_indent_ordered_list() {
        // Ordered lists need marker-width indent (3 for "1. ", 4 for "10. ")
        // 2-space indent breaks out of the list.
        let buf: Buffer = "1. item\n".parse().unwrap();
        let lines = buf.lines();
        // Should produce 3-space indent to match "1. "
        assert_eq!(lines[0].nested_paragraph_indent(buf.rope()), "   ");
    }

    #[test]
    fn test_nested_paragraph_indent_double_digit_ordered_list() {
        // Need 10 items to get a double-digit marker (Buffer normalizes "10. " to "1. ")
        let buf: Buffer = "1. a\n2. b\n3. c\n4. d\n5. e\n6. f\n7. g\n8. h\n9. i\n10. j\n"
            .parse()
            .unwrap();
        let lines = buf.lines();
        // Line 9 (0-indexed) is "10. j" - should produce 4-space indent
        assert_eq!(lines[9].nested_paragraph_indent(buf.rope()), "    ");
    }

    #[test]
    fn test_nested_paragraph_indent_unordered_list() {
        // Unordered lists use 2-space indent (not marker width)
        // to avoid triggering indented code block detection
        let buf: Buffer = "- item\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(lines[0].nested_paragraph_indent(buf.rope()), "  ");
    }

    #[test]
    fn test_multiline_blockquote() {
        let buf: Buffer = "> Line 1\n> Line 2\n".parse().unwrap();
        let lines = buf.lines();

        println!("Line 0 markers: {:?}", lines[0].markers);
        println!("Line 1 markers: {:?}", lines[1].markers);

        assert_eq!(kinds(&lines[0].markers), vec![&MarkerKind::BlockQuote]);
        assert_eq!(kinds(&lines[1].markers), vec![&MarkerKind::BlockQuote]);
    }

    #[test]
    fn test_nested_blockquote() {
        let buf: Buffer = "> Level 1\n> > Level 2\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();

        println!("\n=== Nested blockquote ===");
        println!("Text: {:?}", text);
        print_tree(&root, &text, 0);
        print_nodes_by_position(&root, &text);

        let lines = buf.lines();
        println!("Line 0 markers: {:?}", lines[0].markers);
        println!("Line 1 markers: {:?}", lines[1].markers);

        assert_eq!(kinds(&lines[0].markers), vec![&MarkerKind::BlockQuote]);
        assert_eq!(
            kinds(&lines[1].markers),
            vec![&MarkerKind::BlockQuote, &MarkerKind::BlockQuote]
        );
    }

    #[test]
    fn test_list_in_blockquote() {
        let buf: Buffer = "> - Item\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(lines[0].markers.len(), 2);
        assert!(is_unordered_list(&lines[0].markers[0].kind));
        assert!(matches!(lines[0].markers[1].kind, MarkerKind::BlockQuote));
    }

    #[test]
    fn test_ordered_list() {
        let buf: Buffer = "1. First\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(lines[0].markers.len(), 1);
        assert!(is_ordered_list(&lines[0].markers[0].kind));
    }

    #[test]
    fn test_task_list() {
        let buf: Buffer = "- [ ] Todo\n- [x] Done\n".parse().unwrap();
        let lines = buf.lines();

        // Task list now has 2 markers: Checkbox + ListItem
        assert_eq!(lines[0].markers.len(), 2);
        assert!(is_checkbox_unchecked(&lines[0].markers[0].kind));
        assert!(is_unordered_list(&lines[0].markers[1].kind));

        assert_eq!(lines[1].markers.len(), 2);
        assert!(is_checkbox_checked(&lines[1].markers[0].kind));
        assert!(is_unordered_list(&lines[1].markers[1].kind));
    }

    #[test]
    fn test_heading() {
        let buf: Buffer = "## Heading\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(kinds(&lines[0].markers), vec![&MarkerKind::Heading(2)]);
    }

    #[test]
    fn test_fenced_code_block() {
        let buf: Buffer = "```rust\nlet x = 1;\n```\n".parse().unwrap();
        let lines = buf.lines();

        // Opening fence with language
        assert_eq!(
            kinds(&lines[0].markers),
            vec![&MarkerKind::CodeBlockFence {
                language: Some("rust".to_string()),
                is_opening: true,
            }]
        );
        // Content lines have no markers (code block detection handled separately)
        assert_eq!(kinds(&lines[1].markers), vec![] as Vec<&MarkerKind>);
        // Closing fence
        assert_eq!(
            kinds(&lines[2].markers),
            vec![&MarkerKind::CodeBlockFence {
                language: None,
                is_opening: false,
            }]
        );
    }

    #[test]
    fn test_fenced_code_block_with_indentation() {
        let buf: Buffer = "```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n"
            .parse()
            .unwrap();
        let lines = buf.lines();

        assert_eq!(
            kinds(&lines[0].markers),
            vec![&MarkerKind::CodeBlockFence {
                language: Some("rust".to_string()),
                is_opening: true,
            }]
        );
        // Content lines have no markers
        assert_eq!(kinds(&lines[1].markers), vec![] as Vec<&MarkerKind>);
        assert_eq!(kinds(&lines[2].markers), vec![] as Vec<&MarkerKind>);
        assert_eq!(kinds(&lines[3].markers), vec![] as Vec<&MarkerKind>);
        // Closing fence
        assert_eq!(
            kinds(&lines[4].markers),
            vec![&MarkerKind::CodeBlockFence {
                language: None,
                is_opening: false,
            }]
        );
    }

    #[test]
    fn test_closing_fence_without_trailing_newline() {
        // Without trailing newline - the closing fence should still be detected
        let buf: Buffer = "```rust\ncode\n```".parse().unwrap();
        let lines = buf.lines();

        // Opening fence
        assert_eq!(
            kinds(&lines[0].markers),
            vec![&MarkerKind::CodeBlockFence {
                language: Some("rust".to_string()),
                is_opening: true,
            }]
        );
        // Content line
        assert_eq!(kinds(&lines[1].markers), vec![] as Vec<&MarkerKind>);
        // Closing fence should be detected even without trailing newline
        assert_eq!(
            kinds(&lines[2].markers),
            vec![&MarkerKind::CodeBlockFence {
                language: None,
                is_opening: false,
            }]
        );
    }

    #[test]
    fn test_indented_code_block() {
        // Indented code blocks have no markers - detection handled separately
        let buf: Buffer = "    let x = 1;\n    let y = 2;\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();
        print_nodes_by_position(&root, &text);

        let lines = buf.lines();
        println!("Line 0: {:?}", lines[0].markers);
        println!("Line 1: {:?}", lines[1].markers);

        assert_eq!(kinds(&lines[0].markers), vec![] as Vec<&MarkerKind>);
        assert_eq!(kinds(&lines[1].markers), vec![] as Vec<&MarkerKind>);
    }

    #[test]
    fn test_indented_code_block_in_blockquote() {
        // Blockquote containing an indented code block - should still have blockquote marker
        let buf: Buffer = ">     code\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();
        print_nodes_by_position(&root, &text);

        let lines = buf.lines();
        println!("Line 0: {:?}", lines[0].markers);

        // Should have the blockquote marker even though content is indented code
        assert_eq!(kinds(&lines[0].markers), vec![&MarkerKind::BlockQuote]);
    }

    #[test]
    fn test_closing_fence_no_trailing_newline() {
        // Closing fence without trailing newline should still be detected
        // (tree-sitter doesn't emit the delimiter node without newline, so we detect manually)
        let buf: Buffer = "```rust\ncode\n```".parse().unwrap();
        assert!(buf.lines()[2].is_fence());
    }

    #[test]
    fn test_thematic_break() {
        let buf: Buffer = "---\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(kinds(&lines[0].markers), vec![&MarkerKind::ThematicBreak]);
    }

    #[test]
    fn test_soft_wrapped_list_item() {
        let buf: Buffer = "- First line\n  continuation\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();

        println!("\n=== Soft wrapped list item ===");
        println!("Text: {:?}", text);
        print_tree(&root, &text, 0);

        let lines = buf.lines();
        println!("Line 0 markers: {:?}", lines[0].markers);
        println!("Line 1 markers: {:?}", lines[1].markers);

        assert_eq!(lines[0].markers.len(), 1);
        assert!(is_unordered_list(&lines[0].markers[0].kind));
        // Line 2: continuation has Indent marker for the "  " prefix
        assert_eq!(kinds(&lines[1].markers), vec![&MarkerKind::Indent]);
    }

    #[test]
    fn test_multi_paragraph_list_item() {
        let buf: Buffer = "- First line\n\n  Second paragraph\n".parse().unwrap();
        let lines = buf.lines();

        assert_eq!(lines[0].markers.len(), 1);
        assert!(is_unordered_list(&lines[0].markers[0].kind));
        // Line 2: empty line - no markers
        assert_eq!(kinds(&lines[1].markers), vec![] as Vec<&MarkerKind>);
        // Line 3: second paragraph with indent
        assert_eq!(kinds(&lines[2].markers), vec![&MarkerKind::Indent]);
    }

    #[test]
    fn test_nested_blockquote_with_indent() {
        // ">    > x" - outer blockquote with indent, then inner blockquote with content
        // Markers should be in reverse byte order: inner > first, then indent, then outer >
        let buf: Buffer = "> 1. hey\n>\n>    > x\n".parse().unwrap();
        let lines = buf.lines();

        // Line 2: ">    > x" has three markers
        // Bytes: "> 1. hey\n>\n>    > x\n"
        //        01234567890123456789
        // Line 2 is bytes 11-20: ">    > x\n"
        // Outer > at 11-13, indent at 13-16, inner > at 16-18
        assert_eq!(lines[2].markers.len(), 3);
        assert_eq!(
            kinds(&lines[2].markers),
            vec![
                &MarkerKind::BlockQuote, // inner "> " (16-18)
                &MarkerKind::Indent,     // spaces (13-16)
                &MarkerKind::BlockQuote, // outer "> " (11-13)
            ]
        );
        // Verify byte order is descending (innermost first)
        assert_eq!(lines[2].markers[0].range, 16..18); // inner "> "
        assert_eq!(lines[2].markers[1].range, 13..16); // "   "
        assert_eq!(lines[2].markers[2].range, 11..13); // outer "> "
    }

    #[test]
    fn test_nested_list_in_nested_blockquote() {
        // Nested blockquote with list inside ordered list paragraph
        // Line 2 and Line 4 should have matching markers (both are ">    > - hey")
        let buf: Buffer = "> 1. item 1\n>\n>    > - hey\n>    >\n>    > - hey\n"
            .parse()
            .unwrap();
        let lines = buf.lines();

        // Both list item lines should have 4 markers:
        // [ListItem, BlockQuote(inner), Indent, BlockQuote(outer)]
        assert_eq!(lines[2].markers.len(), 4);
        assert_eq!(lines[4].markers.len(), 4);
        assert_eq!(kinds(&lines[2].markers), kinds(&lines[4].markers));
    }

    #[test]
    fn test_blockquote_inside_list_paragraph() {
        let buf: Buffer = "1. item\n\n   > quote\n".parse().unwrap();
        let lines = buf.lines();

        // Line 0: ordered list item
        assert_eq!(lines[0].markers.len(), 1);
        assert!(is_ordered_list(&lines[0].markers[0].kind));
        // Line 1: empty line - no markers
        assert_eq!(kinds(&lines[1].markers), vec![] as Vec<&MarkerKind>);
        // Line 2: blockquote inside list's nested paragraph - has BlockQuote + Indent
        assert_eq!(
            kinds(&lines[2].markers),
            vec![&MarkerKind::BlockQuote, &MarkerKind::Indent]
        );
        // Check ranges: BlockQuote should be at byte 9 ("> "), Indent at byte 9
        // "1. item\n\n   > quote\n"
        //  0123456 7 8901234567
        // Line 2 starts at byte 9 ("   > quote\n")
        // BlockQuote marker is "> " at positions 12-14
        // Indent marker is "   " at positions 9-12
        let bq_marker = &lines[2].markers[0];
        let indent_marker = &lines[2].markers[1];
        assert_eq!(bq_marker.range, 12..14); // "> "
        assert_eq!(indent_marker.range, 9..12); // "   "

        // Continuation should include both: "   > "
        assert_eq!(lines[2].continuation_rope(buf.rope()), "   > ");
    }

    // ========================================================================
    // Tests for Line struct methods
    // ========================================================================

    fn make_line(range: Range<usize>, markers: Vec<Marker>) -> LineMarkers {
        LineMarkers {
            range,
            line_number: 0,
            markers,
            in_checked_task: false,
            in_code_block: false,
        }
    }

    #[test]
    fn test_line_marker_range_empty() {
        let line = make_line(0..10, vec![]);
        assert_eq!(line.marker_range(), None);
    }

    #[test]
    fn test_line_marker_range_single() {
        let line = make_line(
            0..10,
            vec![Marker {
                kind: MarkerKind::ListItem {
                    ordered: false,
                    unordered_marker: Some(UnorderedMarker::Minus),
                    ordered_marker: None,
                    number: None,
                },
                range: 0..2,
            }],
        );
        assert_eq!(line.marker_range(), Some(0..2));
    }

    #[test]
    fn test_line_marker_range_multiple() {
        // Markers are innermost to outermost (ListItem inside BlockQuote)
        let line = make_line(
            0..15,
            vec![
                Marker {
                    kind: MarkerKind::ListItem {
                        ordered: false,
                        unordered_marker: Some(UnorderedMarker::Minus),
                        ordered_marker: None,
                        number: None,
                    },
                    range: 2..4,
                },
                Marker {
                    kind: MarkerKind::BlockQuote,
                    range: 0..2,
                },
            ],
        );
        assert_eq!(line.marker_range(), Some(0..4));
    }

    #[test]
    fn test_line_substitution() {
        let buf: Buffer = "> - Item text here\n".parse().unwrap();
        let lines = buf.lines();
        // All markers (blockquote, list) are now rendered as spacers, not substitution
        assert_eq!(lines[0].substitution_rope(buf.rope()), "");
    }

    #[test]
    fn test_line_substitution_task_list() {
        let buf: Buffer = "- [ ] Task item\n".parse().unwrap();
        let lines = buf.lines();
        // Task list markers are rendered as spacers, not substitution
        assert_eq!(lines[0].substitution_rope(buf.rope()), "");
    }

    #[test]
    fn test_line_substitution_different_markers() {
        // All list markers are now rendered as spacers, not substitution
        let buf_minus: Buffer = "- item\n".parse().unwrap();
        let buf_star: Buffer = "* item\n".parse().unwrap();
        let buf_plus: Buffer = "+ item\n".parse().unwrap();

        assert_eq!(buf_minus.lines()[0].substitution_rope(buf_minus.rope()), "");
        assert_eq!(buf_star.lines()[0].substitution_rope(buf_star.rope()), "");
        assert_eq!(buf_plus.lines()[0].substitution_rope(buf_plus.rope()), "");
    }

    #[test]
    fn test_task_list_substitution_different_markers() {
        // Task list markers are now rendered as spacers, not substitution
        let buf_minus: Buffer = "- [ ] task\n".parse().unwrap();
        let buf_star: Buffer = "* [ ] task\n".parse().unwrap();
        let buf_plus: Buffer = "+ [ ] task\n".parse().unwrap();

        assert_eq!(buf_minus.lines()[0].substitution_rope(buf_minus.rope()), "");
        assert_eq!(buf_star.lines()[0].substitution_rope(buf_star.rope()), "");
        assert_eq!(buf_plus.lines()[0].substitution_rope(buf_plus.rope()), "");
    }

    #[test]
    fn test_line_continuation() {
        let buf: Buffer = "> - Item text here\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(lines[0].continuation_rope(buf.rope()), "> - ");
    }

    #[test]
    fn test_line_continuation_with_indent() {
        // Indent markers appear in nested paragraphs under list items
        let buf: Buffer = "- item\n\n  Second paragraph\n".parse().unwrap();
        let lines = buf.lines();
        // Line 2 is the indented paragraph
        assert_eq!(lines[2].continuation_rope(buf.rope()), "  ");
    }

    #[test]
    fn test_line_continuation_nested_list() {
        // Nested list: "  - Nested" where marker includes leading whitespace
        let buf: Buffer = "- Top\n  - Nested\n".parse().unwrap();
        let lines = buf.lines();
        // ListItem marker extracts actual text including indent
        assert_eq!(lines[1].continuation_rope(buf.rope()), "  - ");
    }

    #[test]
    fn test_line_has_border() {
        let line_with_quote = make_line(
            0..10,
            vec![Marker {
                kind: MarkerKind::BlockQuote,
                range: 0..2,
            }],
        );
        assert!(line_with_quote.has_border());

        let line_with_list = make_line(
            0..10,
            vec![Marker {
                kind: MarkerKind::ListItem {
                    ordered: false,
                    unordered_marker: Some(UnorderedMarker::Minus),
                    ordered_marker: None,
                    number: None,
                },
                range: 0..2,
            }],
        );
        assert!(!line_with_list.has_border());
    }

    #[test]
    fn test_line_checkbox() {
        let line_unchecked = make_line(
            0..15,
            vec![Marker {
                kind: MarkerKind::Checkbox { checked: false },
                range: 2..6,
            }],
        );
        assert_eq!(line_unchecked.checkbox(), Some(false));

        let line_checked = make_line(
            0..15,
            vec![Marker {
                kind: MarkerKind::Checkbox { checked: true },
                range: 2..6,
            }],
        );
        assert_eq!(line_checked.checkbox(), Some(true));

        let line_no_checkbox = make_line(
            0..10,
            vec![Marker {
                kind: MarkerKind::ListItem {
                    ordered: false,
                    unordered_marker: Some(UnorderedMarker::Minus),
                    ordered_marker: None,
                    number: None,
                },
                range: 0..2,
            }],
        );
        assert_eq!(line_no_checkbox.checkbox(), None);
    }

    #[test]
    fn test_line_leading_whitespace() {
        let text = "  - Item\n";
        let line = make_line(
            0..8,
            vec![Marker {
                kind: MarkerKind::ListItem {
                    ordered: false,
                    unordered_marker: Some(UnorderedMarker::Minus),
                    ordered_marker: None,
                    number: None,
                },
                range: 2..4,
            }],
        );
        assert_eq!(line.leading_whitespace(text), "  ");
    }

    #[test]
    fn test_line_leading_whitespace_none() {
        let text = "- Item\n";
        let line = make_line(
            0..6,
            vec![Marker {
                kind: MarkerKind::ListItem {
                    ordered: false,
                    unordered_marker: Some(UnorderedMarker::Minus),
                    ordered_marker: None,
                    number: None,
                },
                range: 0..2,
            }],
        );
        assert_eq!(line.leading_whitespace(text), "");
    }

    #[test]
    fn test_nested_list() {
        let buf: Buffer = "- First\n    - Nested\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();

        print_nodes_by_position(&root, &text);

        let lines = buf.lines();
        println!("Line 0: {:?}", lines[0].markers);
        println!("Line 1: {:?}", lines[1].markers);

        // Line 1: "    - Nested" - nested list item
        // block_continuation [8-10] creates Indent for "  "
        // list_marker_minus [10-14] has "  - " but we strip leading whitespace:
        //   - Indent [10-12] for the leading "  "
        //   - ListItem [12-14] for "- "
        // This gives us non-overlapping markers where spacers and markers are 1:1
        assert_eq!(lines[1].markers.len(), 3);
        assert!(matches!(lines[1].markers[0].kind, MarkerKind::Indent));
        assert!(is_unordered_list(&lines[1].markers[1].kind));
        assert!(matches!(lines[1].markers[2].kind, MarkerKind::Indent));
        assert_eq!(&text[lines[1].markers[0].range.clone()], "  "); // from list_marker
        assert_eq!(&text[lines[1].markers[1].range.clone()], "- "); // actual marker
        assert_eq!(&text[lines[1].markers[2].range.clone()], "  "); // from block_continuation
    }

    #[test]
    fn test_two_nested_items_same_level() {
        // Both nested items have 4-space indent, should render at same level
        let buf: Buffer = "- test\n    - hey\n    - hey\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();

        print_tree(&root, &text, 0);
        print_nodes_by_position(&root, &text);

        let lines = buf.lines();
        for (i, line) in lines.iter().enumerate() {
            let line_text = &text[line.range.clone()];
            let leading = line.leading_whitespace(&text);
            let sub = line.substitution_rope(buf.rope());
            println!(
                "Line {}: {:?}\n  markers={:?}\n  leading_whitespace={:?} substitution={:?}",
                i, line_text, line.markers, leading, sub
            );
        }

        // Both line 1 and line 2 should have the same substitution
        // (indentation is now included in substitution via Indent markers)
        assert_eq!(
            lines[1].substitution_rope(buf.rope()),
            lines[2].substitution_rope(buf.rope())
        );
    }

    #[test]
    fn test_marker_width_unordered() {
        let buf: Buffer = "- item\n".parse().unwrap();
        let lines = buf.lines();
        // "- " is 2 chars
        assert_eq!(lines[0].marker_width(), 2);
    }

    #[test]
    fn test_marker_width_ordered_single_digit() {
        let buf: Buffer = "1. item\n".parse().unwrap();
        let lines = buf.lines();
        // "1. " is 3 chars
        assert_eq!(lines[0].marker_width(), 3);
    }

    #[test]
    fn test_marker_width_ordered_double_digit() {
        // Need 10 items to get a double-digit marker (Buffer normalizes "10. " to "1. ")
        let buf: Buffer = "1. a\n2. b\n3. c\n4. d\n5. e\n6. f\n7. g\n8. h\n9. i\n10. j\n"
            .parse()
            .unwrap();
        let lines = buf.lines();
        // Line 9 (0-indexed) is "10. j" - marker is "10. " = 4 chars
        assert_eq!(lines[9].marker_width(), 4);
    }

    #[test]
    fn test_marker_width_no_marker() {
        let buf: Buffer = "just text\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(lines[0].marker_width(), 0);
    }

    #[test]
    fn test_marker_width_task_list() {
        // Task list "- [ ] " is 6 chars total, but marker_width should be 2
        // because only the list marker "- " contributes to wrap indent.
        // The checkbox "[ ] " is rendered inline and doesn't contribute.
        let buf: Buffer = "- [ ] task\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(lines[0].marker_width(), 2); // Not 6!
    }

    #[test]
    fn test_nesting_threshold_unordered() {
        // "- " is 2 chars, so 2 spaces should nest
        let buf: Buffer = "- top\n  - nested\n".parse().unwrap();
        let lines = buf.lines();

        // Line 1 should have an Indent marker (it's nested)
        assert!(
            lines[1]
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::Indent))
        );
    }

    #[test]
    fn test_nesting_threshold_unordered_insufficient() {
        // "- " is 2 chars, so 1 space should NOT nest (becomes sibling)
        let buf: Buffer = "- top\n - not nested\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();

        print_tree(&root, &text, 0);

        let lines = buf.lines();
        println!("Line 0: {:?}", lines[0].markers);
        println!("Line 1: {:?}", lines[1].markers);

        // Line 1 should NOT have an Indent marker (it's a sibling, not nested)
        assert!(
            !lines[1]
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::Indent))
        );
    }

    #[test]
    fn test_nesting_threshold_ordered_single_digit() {
        // "1. " is 3 chars, so 3 spaces should nest
        let buf: Buffer = "1. top\n   - nested\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();

        print_tree(&root, &text, 0);

        let lines = buf.lines();
        println!("Line 0: {:?}", lines[0].markers);
        println!("Line 1: {:?}", lines[1].markers);

        // Line 1 should have an Indent marker (it's nested)
        assert!(
            lines[1]
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::Indent))
        );
    }

    #[test]
    fn test_nesting_threshold_ordered_double_digit() {
        // "10. " is 4 chars, so 4 spaces should nest
        let buf: Buffer = "10. top\n    - nested\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();

        print_tree(&root, &text, 0);

        let lines = buf.lines();
        println!("Line 0: {:?}", lines[0].markers);
        println!("Line 1: {:?}", lines[1].markers);

        // Line 1 should have an Indent marker (it's nested)
        assert!(
            lines[1]
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::Indent))
        );
    }

    #[test]
    fn test_nesting_threshold_ordered_triple_digit() {
        // "100. " is 5 chars, so 5 spaces should nest
        let buf: Buffer = "100. top\n     - nested\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();

        print_tree(&root, &text, 0);

        let lines = buf.lines();
        println!("Line 0: {:?}", lines[0].markers);
        println!("Line 1: {:?}", lines[1].markers);

        // Line 1 should have an Indent marker (it's nested)
        assert!(
            lines[1]
                .markers
                .iter()
                .any(|m| matches!(m.kind, MarkerKind::Indent))
        );
    }

    // ========================================================================
    // Tests for marker detection with/without trailing whitespace
    // ========================================================================

    #[test]
    fn test_blockquote_with_space() {
        let buf: Buffer = "> text\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(kinds(&lines[0].markers), vec![&MarkerKind::BlockQuote]);
    }

    #[test]
    fn test_blockquote_without_space() {
        // Does ">text" (no space after >) get recognized as blockquote?
        let buf: Buffer = ">text\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();
        print_tree(&root, &text, 0);

        let lines = buf.lines();
        println!("Markers for '>text': {:?}", lines[0].markers);
        // Result: YES, blockquote is recognized without space
        assert_eq!(kinds(&lines[0].markers), vec![&MarkerKind::BlockQuote]);
    }

    #[test]
    fn test_unordered_list_minus_with_space() {
        let buf: Buffer = "- text\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(lines[0].markers.len(), 1);
        assert!(is_unordered_list(&lines[0].markers[0].kind));
    }

    #[test]
    fn test_unordered_list_minus_without_space() {
        // Does "-text" (no space after -) get recognized as list?
        let buf: Buffer = "-text\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();
        print_tree(&root, &text, 0);

        let lines = buf.lines();
        println!("Markers for '-text': {:?}", lines[0].markers);
        // Result: NO, list is NOT recognized without space
        assert_eq!(kinds(&lines[0].markers), vec![] as Vec<&MarkerKind>);
    }

    #[test]
    fn test_unordered_list_star_with_space() {
        let buf: Buffer = "* text\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(lines[0].markers.len(), 1);
        assert!(is_unordered_list(&lines[0].markers[0].kind));
    }

    #[test]
    fn test_unordered_list_star_without_space() {
        // Does "*text" (no space after *) get recognized as list?
        let buf: Buffer = "*text\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();
        print_tree(&root, &text, 0);

        let lines = buf.lines();
        println!("Markers for '*text': {:?}", lines[0].markers);
        // Result: NO, list is NOT recognized without space (parsed as emphasis)
        assert_eq!(kinds(&lines[0].markers), vec![] as Vec<&MarkerKind>);
    }

    #[test]
    fn test_unordered_list_plus_with_space() {
        let buf: Buffer = "+ text\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(lines[0].markers.len(), 1);
        assert!(is_unordered_list(&lines[0].markers[0].kind));
    }

    #[test]
    fn test_unordered_list_plus_without_space() {
        // Does "+text" (no space after +) get recognized as list?
        let buf: Buffer = "+text\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();
        print_tree(&root, &text, 0);

        let lines = buf.lines();
        println!("Markers for '+text': {:?}", lines[0].markers);
        // Result: NO, list is NOT recognized without space
        assert_eq!(kinds(&lines[0].markers), vec![] as Vec<&MarkerKind>);
    }

    #[test]
    fn test_ordered_list_with_space() {
        let buf: Buffer = "1. text\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(lines[0].markers.len(), 1);
        assert!(is_ordered_list(&lines[0].markers[0].kind));
    }

    #[test]
    fn test_ordered_list_without_space() {
        // Does "1.text" (no space after .) get recognized as list?
        let buf: Buffer = "1.text\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();
        print_tree(&root, &text, 0);

        let lines = buf.lines();
        println!("Markers for '1.text': {:?}", lines[0].markers);
        // Result: NO, list is NOT recognized without space
        assert_eq!(kinds(&lines[0].markers), vec![] as Vec<&MarkerKind>);
    }

    #[test]
    fn test_heading_with_space() {
        let buf: Buffer = "# text\n".parse().unwrap();
        let lines = buf.lines();
        assert_eq!(kinds(&lines[0].markers), vec![&MarkerKind::Heading(1)]);
    }

    #[test]
    fn test_heading_without_space() {
        // Does "#text" (no space after #) get recognized as heading?
        let buf: Buffer = "#text\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();
        print_tree(&root, &text, 0);

        let lines = buf.lines();
        println!("Markers for '#text': {:?}", lines[0].markers);
        // Result: NO, heading is NOT recognized without space
        assert_eq!(kinds(&lines[0].markers), vec![] as Vec<&MarkerKind>);
    }

    #[test]
    fn test_heading_h2_without_space() {
        // Does "##text" (no space after ##) get recognized as heading?
        let buf: Buffer = "##text\n".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();
        print_tree(&root, &text, 0);

        let lines = buf.lines();
        println!("Markers for '##text': {:?}", lines[0].markers);
        // Result: NO, heading is NOT recognized without space
        assert_eq!(kinds(&lines[0].markers), vec![] as Vec<&MarkerKind>);
    }

    #[test]
    fn test_mixed_list_markers_tree_structure() {
        // Test whether tree-sitter treats different unordered list markers as separate lists
        // Per CommonMark spec, different markers should create separate lists
        let buf: Buffer = "- a\n* b\n+ c".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();

        println!("\n=== Mixed list markers ===");
        println!("Text: {:?}", text);
        print_tree(&root, &text, 0);

        // Count how many 'list' nodes are direct children of the section/document
        let mut list_count = 0;
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() == "section" {
                let mut section_cursor = child.walk();
                for section_child in child.children(&mut section_cursor) {
                    if section_child.kind() == "list" {
                        list_count += 1;
                        println!("Found list: {:?}", &text[section_child.byte_range()]);
                    }
                }
            } else if child.kind() == "list" {
                list_count += 1;
                println!("Found list: {:?}", &text[child.byte_range()]);
            }
        }

        println!("Total list count: {}", list_count);

        // If tree-sitter creates 3 separate lists (one for each marker type),
        // we should normalize markers. If it creates 1 list, no need.
        // This test documents the actual behavior.
        assert!(
            list_count == 1 || list_count == 3,
            "Expected either 1 unified list or 3 separate lists, got {}",
            list_count
        );
    }

    #[test]
    fn test_ordered_list_marker_styles() {
        // Test whether tree-sitter distinguishes 1. vs 1) ordered list styles
        let buf_dot: Buffer = "1. item\n".parse().unwrap();
        let buf_paren: Buffer = "1) item\n".parse().unwrap();

        let text_dot = buf_dot.text();
        let tree_dot = buf_dot.tree().unwrap();
        let root_dot = tree_dot.block_tree().root_node();

        let text_paren = buf_paren.text();
        let tree_paren = buf_paren.tree().unwrap();
        let root_paren = tree_paren.block_tree().root_node();

        println!("\n=== Ordered list with dot ===");
        print_tree(&root_dot, &text_dot, 0);

        println!("\n=== Ordered list with paren ===");
        print_tree(&root_paren, &text_paren, 0);

        // Check that both are recognized as ordered lists
        assert!(is_ordered_list(&buf_dot.lines()[0].markers[0].kind));
        assert!(is_ordered_list(&buf_paren.lines()[0].markers[0].kind));
    }

    #[test]
    fn test_increment_ordered_marker() {
        // Dot style
        assert_eq!(
            increment_ordered_marker("1. ", Some(OrderedMarker::Dot)),
            "2. "
        );
        assert_eq!(
            increment_ordered_marker("9. ", Some(OrderedMarker::Dot)),
            "10. "
        );
        assert_eq!(
            increment_ordered_marker("99. ", Some(OrderedMarker::Dot)),
            "100. "
        );

        // Parenthesis style
        assert_eq!(
            increment_ordered_marker("1) ", Some(OrderedMarker::Parenthesis)),
            "2) "
        );
        assert_eq!(
            increment_ordered_marker("5) ", Some(OrderedMarker::Parenthesis)),
            "6) "
        );

        // Default to dot if no marker specified
        assert_eq!(increment_ordered_marker("3. ", None), "4. ");
    }

    #[test]
    fn test_ordered_list_continuation_increments() {
        let buf: Buffer = "1. First\n2. Second".parse().unwrap();
        let lines = buf.lines();

        // Line 1 (index 1) has "2. Second", continuation should be "3. "
        let continuation = lines[1].continuation_rope(buf.rope());
        assert_eq!(continuation, "3. ");
    }

    #[test]
    fn test_ordered_list_in_blockquote_continuation() {
        let buf: Buffer = "> 1. First\n> 2. Second".parse().unwrap();
        let lines = buf.lines();

        // Continuation should be "> 3. "
        let continuation = lines[1].continuation_rope(buf.rope());
        assert_eq!(continuation, "> 3. ");
    }

    #[test]
    fn debug_ordered_list_tree_structure() {
        let buf: Buffer = "1. First\n2. Second\n3. Third".parse().unwrap();
        let text = buf.text();
        let tree = buf.tree().unwrap();
        let root = tree.block_tree().root_node();

        println!("\n=== Ordered list ===");
        println!("Text: {:?}", text);
        print_tree(&root, &text, 0);
    }
}
