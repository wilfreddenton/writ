//! GFM pipe-table model extraction (headless).
//!
//! tree-sitter-md parses GFM tables into a `pipe_table` node containing a
//! `pipe_table_header`, a `pipe_table_delimiter_row`, and zero or more
//! `pipe_table_row` nodes. Header/body rows hold `pipe_table_cell` children whose
//! byte range is the cell's inline content (plus any trailing space); delimiter
//! cells carry optional `pipe_table_align_left`/`pipe_table_align_right` marks.
//!
//! This module turns that subtree into a flat, position-indexed [`TableInfo`] with
//! no rendering. A lone header row with no delimiter row is NOT a table — it parses
//! as a `paragraph` — so such input yields nothing here.

use ropey::Rope;
use std::ops::Range;
use tree_sitter::Node;

/// Column alignment derived from a delimiter cell. `Left` is also the default when a
/// delimiter cell carries neither alignment mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

/// A single table cell: the byte range of its inline content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCell {
    pub content: Range<usize>,
}

/// A header or body row: the byte range of its line plus its cells in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    pub line: Range<usize>,
    pub cells: Vec<TableCell>,
}

/// The extracted model for one `pipe_table`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    /// Byte range of the whole `pipe_table` node.
    pub block: Range<usize>,
    pub header: TableRow,
    /// Byte range of the `|---|---|` delimiter line.
    pub delimiter_line: Range<usize>,
    /// Per-column alignment, from the delimiter cells. Its length is the delimiter-cell
    /// count, which may differ from `ncols` — index defensively (default `Align::Left`).
    pub aligns: Vec<Align>,
    pub body: Vec<TableRow>,
    /// Max cell count across header and body rows.
    pub ncols: usize,
}

/// The role a buffer line plays within a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Header,
    Delimiter,
    /// Body row; the index is into [`TableInfo::body`].
    Body(usize),
}

/// Which row of `table` line `line_idx` is. Compares by line *number* (not byte
/// containment): tree-sitter's error recovery can start a row node mid-line (after the
/// leading `|`), so a byte-range `contains` would miss the row's own line. `byte_to_line`
/// maps a byte offset to its 0-based line. Shared by `RenderSnapshot::table_row_at_line`
/// and `EditorState::table_context_at_line` so both agree.
pub fn row_kind_at_line(
    table: &TableInfo,
    line_idx: usize,
    byte_to_line: impl Fn(usize) -> usize,
) -> Option<RowKind> {
    if byte_to_line(table.header.line.start) == line_idx {
        return Some(RowKind::Header);
    }
    if byte_to_line(table.delimiter_line.start) == line_idx {
        return Some(RowKind::Delimiter);
    }
    table
        .body
        .iter()
        .position(|r| byte_to_line(r.line.start) == line_idx)
        .map(RowKind::Body)
}

/// Trim leading/trailing ASCII whitespace off a cell node's byte range using the
/// rope, so the stored content range covers just the cell text.
fn trim_cell_range(rope: &Rope, mut start: usize, mut end: usize) -> Range<usize> {
    while start < end && matches!(rope.get_byte(start), Some(b' ') | Some(b'\t')) {
        start += 1;
    }
    while end > start && matches!(rope.get_byte(end - 1), Some(b' ') | Some(b'\t')) {
        end -= 1;
    }
    start..end
}

/// Build a [`TableRow`] from a `pipe_table_header` or `pipe_table_row` node.
fn row_from_node(node: &Node, rope: &Rope) -> TableRow {
    let mut cursor = node.walk();
    let cells = node
        .children(&mut cursor)
        .filter(|c| c.kind() == "pipe_table_cell")
        .map(|c| TableCell {
            content: trim_cell_range(rope, c.start_byte(), c.end_byte()),
        })
        .collect();
    TableRow {
        line: node.start_byte()..node.end_byte(),
        cells,
    }
}

/// Derive alignment from a `pipe_table_delimiter_cell`'s marks: both = Center,
/// left only = Left, right only = Right, neither = Left (default).
fn align_from_delimiter_cell(cell: &Node) -> Align {
    let mut cursor = cell.walk();
    let mut left = false;
    let mut right = false;
    for mark in cell.children(&mut cursor) {
        match mark.kind() {
            "pipe_table_align_left" => left = true,
            "pipe_table_align_right" => right = true,
            _ => {}
        }
    }
    match (left, right) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    }
}

/// Extract a [`TableInfo`] from a `pipe_table` node. Returns `None` if the structure
/// is malformed (missing header or delimiter row).
pub fn extract_table(node: &Node, rope: &Rope) -> Option<TableInfo> {
    if node.kind() != "pipe_table" {
        return None;
    }

    let mut header: Option<TableRow> = None;
    let mut delimiter_line: Option<Range<usize>> = None;
    let mut aligns: Vec<Align> = Vec::new();
    let mut body: Vec<TableRow> = Vec::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "pipe_table_header" => header = Some(row_from_node(&child, rope)),
            "pipe_table_delimiter_row" => {
                delimiter_line = Some(child.start_byte()..child.end_byte());
                let mut dc = child.walk();
                aligns = child
                    .children(&mut dc)
                    .filter(|c| c.kind() == "pipe_table_delimiter_cell")
                    .map(|c| align_from_delimiter_cell(&c))
                    .collect();
            }
            "pipe_table_row" => body.push(row_from_node(&child, rope)),
            _ => {}
        }
    }

    let header = header?;
    let delimiter_line = delimiter_line?;

    // Anchor the block to the header's LINE start, not the tree-sitter node's start_byte.
    // On an all-empty-row ERROR, tree-sitter can extend a following table's `pipe_table`
    // node *backwards* over the poisoned rows / a blank line / a heading, so node.start_byte
    // lands far above the real header — the block then overlaps the previous table and the
    // binary-search row lookup mis-attributes lines. The header row is always where the
    // real header begins.
    let block_start = rope.char_to_byte(rope.line_to_char(rope.byte_to_line(header.line.start)));

    // Heal rows tree-sitter dropped. An all-empty row followed by a newline
    // (`|  |  |  |\n`) makes tree-sitter-md emit an ERROR that truncates the `pipe_table`,
    // silently dropping that row AND the real row before it (verified). That's exactly
    // what `Editor::insert_table_row_after_line` produces, so a Tab-added empty row would
    // otherwise render the last filled row as raw pipes. Recover by scanning the source
    // lines immediately after the parsed table and re-attaching any that still look like
    // table rows (non-blank + contains a pipe) — per GFM those ARE part of the table.
    let mut block_end = node.end_byte();
    let start_line = body
        .last()
        .map(|r| r.line.start)
        .unwrap_or(delimiter_line.start);
    let mut line = rope.byte_to_line(start_line);
    while line + 1 < rope.len_lines() {
        line += 1;
        let ls = rope.char_to_byte(rope.line_to_char(line));
        let le = if line + 1 < rope.len_lines() {
            rope.char_to_byte(rope.line_to_char(line + 1))
        } else {
            rope.len_bytes()
        };
        let text = rope.slice(rope.byte_to_char(ls)..rope.byte_to_char(le));
        // A GFM table continues over consecutive non-blank lines that contain a pipe; a
        // blank line (or a pipe-less line) ends it. Healing an all-empty `|  |  |` row too
        // means a mid-table empty row doesn't cut off the real rows below it.
        let is_row = text.chars().any(|c| c == '|') && text.chars().any(|c| !c.is_whitespace());
        if !is_row {
            break;
        }
        let content_end = if le > ls && rope.get_byte(le - 1) == Some(b'\n') {
            le - 1
        } else {
            le
        };
        body.push(row_from_raw_line(rope, ls, content_end));
        block_end = content_end;
    }

    let ncols = std::iter::once(header.cells.len())
        .chain(body.iter().map(|r| r.cells.len()))
        .max()
        .unwrap_or(0);

    Some(TableInfo {
        block: block_start..block_end,
        header,
        delimiter_line,
        aligns,
        body,
        ncols,
    })
}

/// Parse a raw table-row line (`| a | b | c |`) into cells, for rows tree-sitter dropped
/// (see the healing in `extract_table`). Cells are the spans between consecutive unescaped
/// `|`; the outer leading/trailing pipes bound the first/last cell. `start`/`end` are the
/// line's buffer byte range (excluding the trailing newline).
fn row_from_raw_line(rope: &Rope, start: usize, end: usize) -> TableRow {
    let text: String = rope
        .slice(rope.byte_to_char(start)..rope.byte_to_char(end))
        .to_string();
    let bytes = text.as_bytes();
    let mut pipes: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2, // skip an escaped char (e.g. `\|`)
            b'|' => {
                pipes.push(start + i);
                i += 1;
            }
            _ => i += 1,
        }
    }
    // Cells are the spans between consecutive pipes, PLUS the text before the first
    // pipe and after the last: per GFM the outer leading/trailing pipes are optional
    // borders, so `1 | 2` is two cells, not one. Build every segment, then drop the
    // first/last only when they are the empty span produced by a border pipe (keeping
    // a genuinely empty first/last cell like `| | b |`).
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut seg_start = start;
    for &p in &pipes {
        segments.push((seg_start, p));
        seg_start = p + 1;
    }
    segments.push((seg_start, end));
    if segments.len() > 1 {
        if trim_cell_range(rope, segments[0].0, segments[0].1).is_empty() {
            segments.remove(0);
        }
        if let Some(&(s, e)) = segments.last()
            && trim_cell_range(rope, s, e).is_empty()
        {
            segments.pop();
        }
    }
    let cells = segments
        .into_iter()
        .map(|(s, e)| TableCell {
            content: trim_cell_range(rope, s, e),
        })
        .collect();
    TableRow {
        line: start..end,
        cells,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;

    fn cell_text<'a>(text: &'a str, cell: &TableCell) -> &'a str {
        text[cell.content.clone()].trim()
    }

    fn only_table(buf: &Buffer) -> TableInfo {
        let tables = &buf.parsed().tables;
        assert_eq!(tables.len(), 1, "expected exactly one table");
        tables[0].clone()
    }

    #[test]
    fn row_from_raw_line_handles_missing_outer_pipes() {
        // A healed continuation row may omit its border pipes; GFM still reads it as
        // multiple cells. `1 | 2` must be two cells, not one.
        let buf: Buffer = "1 | 2 | 3".parse().unwrap();
        let text = buf.text();
        let row = row_from_raw_line(buf.rope(), 0, text.trim_end().len());
        assert_eq!(row.cells.len(), 3);
        assert_eq!(cell_text(&text, &row.cells[0]), "1");
        assert_eq!(cell_text(&text, &row.cells[1]), "2");
        assert_eq!(cell_text(&text, &row.cells[2]), "3");
    }

    #[test]
    fn row_from_raw_line_keeps_empty_interior_cell() {
        // `| | b |` — the empty first cell is real (between border and first inner pipe),
        // only the border segments are dropped.
        let buf: Buffer = "| | b |".parse().unwrap();
        let text = buf.text();
        let row = row_from_raw_line(buf.rope(), 0, text.trim_end().len());
        assert_eq!(row.cells.len(), 2);
        assert_eq!(cell_text(&text, &row.cells[0]), "");
        assert_eq!(cell_text(&text, &row.cells[1]), "b");
    }

    #[test]
    fn trailing_empty_row_is_healed() {
        // An all-empty trailing row makes tree-sitter drop it AND the filled row before it;
        // healing recovers both so the whole table still grid-renders.
        let buf: Buffer = "| A | B |\n| - | - |\n| 1 | 2 |\n|   |   |\n"
            .parse()
            .unwrap();
        let t = only_table(&buf);
        assert_eq!(t.body.len(), 2, "filled + empty row both recovered");
        assert_eq!(t.ncols, 2);
        let text = buf.text();
        assert_eq!(cell_text(&text, &t.body[0].cells[0]), "1");
        assert_eq!(cell_text(&text, &t.body[0].cells[1]), "2");
        assert!(
            t.body[1]
                .cells
                .iter()
                .all(|c| cell_text(&text, c).is_empty())
        );
        // The empty row is inside the block, so it renders as an (empty) grid row.
        assert!(t.block.contains(&buf.line_to_byte(3)));
    }

    #[test]
    fn mid_table_empty_row_does_not_truncate() {
        let buf: Buffer = "| A | B |\n| - | - |\n| 1 | 2 |\n|   |   |\n| 3 | 4 |\n"
            .parse()
            .unwrap();
        let t = only_table(&buf);
        assert_eq!(
            t.body.len(),
            3,
            "an empty row mid-table keeps the rows below it"
        );
        let text = buf.text();
        assert_eq!(cell_text(&text, &t.body[2].cells[0]), "3");
    }

    #[test]
    fn healing_stops_at_blank_line() {
        // A blank line ends the table; a following pipe-containing paragraph isn't absorbed.
        let buf: Buffer = "| A | B |\n| - | - |\n| 1 | 2 |\n\nafter | pipe\n"
            .parse()
            .unwrap();
        let t = only_table(&buf);
        assert_eq!(t.body.len(), 1);
    }

    #[test]
    fn empty_row_does_not_corrupt_next_table() {
        // An empty row in the first table used to make tree-sitter splice a phantom table
        // whose block started inside the first table and swallowed the second — garbling it.
        let src =
            "| A | B |\n| - | - |\n| x | y |\n|   |   |\n\n## H\n| C | D |\n| - | - |\n| 1 | 2 |\n";
        let buf: Buffer = src.parse().unwrap();
        let ts = &buf.parsed().tables;
        assert_eq!(ts.len(), 2, "two distinct tables");
        assert!(
            ts[0].block.end <= ts[1].block.start,
            "table blocks must not overlap: {:?} vs {:?}",
            ts[0].block,
            ts[1].block
        );
        let text = buf.text();
        assert_eq!(cell_text(&text, &ts[1].header.cells[0]), "C");
        assert_eq!(cell_text(&text, &ts[1].body[0].cells[1]), "2");
    }

    #[test]
    fn healed_row_cells_parse() {
        let buf: Buffer = "| A | B |\n| - | - |\n| hi | yo |\n|   |   |\n"
            .parse()
            .unwrap();
        let t = only_table(&buf);
        let text = buf.text();
        assert_eq!(cell_text(&text, &t.body[0].cells[0]), "hi");
        assert_eq!(cell_text(&text, &t.body[0].cells[1]), "yo");
    }

    #[test]
    fn basic_two_col() {
        let buf: Buffer = "| a | b |\n|:--|--:|\n| 1 | 2 |\n".parse().unwrap();
        let text = buf.text();
        let t = only_table(&buf);

        assert_eq!(t.ncols, 2);
        assert_eq!(t.aligns, vec![Align::Left, Align::Right]);

        assert_eq!(t.header.cells.len(), 2);
        assert_eq!(cell_text(&text, &t.header.cells[0]), "a");
        assert_eq!(cell_text(&text, &t.header.cells[1]), "b");

        assert_eq!(t.body.len(), 1);
        assert_eq!(cell_text(&text, &t.body[0].cells[0]), "1");
        assert_eq!(cell_text(&text, &t.body[0].cells[1]), "2");
    }

    #[test]
    fn center_alignment() {
        let buf: Buffer = "| a | b |\n|:-:|:-:|\n| 1 | 2 |\n".parse().unwrap();
        let t = only_table(&buf);
        assert_eq!(t.aligns, vec![Align::Center, Align::Center]);
    }

    #[test]
    fn default_alignment() {
        let buf: Buffer = "| a | b |\n|---|---|\n| 1 | 2 |\n".parse().unwrap();
        let t = only_table(&buf);
        assert_eq!(t.aligns, vec![Align::Left, Align::Left]);
    }

    #[test]
    fn multi_row_and_ragged() {
        // Second body row has only one cell; ncols stays the header count (2).
        let buf: Buffer = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 |\n".parse().unwrap();
        let text = buf.text();
        let t = only_table(&buf);

        assert_eq!(t.ncols, 2);
        assert_eq!(t.body.len(), 2);
        assert_eq!(t.body[0].cells.len(), 2);
        assert_eq!(t.body[1].cells.len(), 1);
        assert_eq!(cell_text(&text, &t.body[1].cells[0]), "3");
    }

    #[test]
    fn spike_a_lone_header_is_not_a_table() {
        // A header row alone (no delimiter) parses as a paragraph, not a table.
        let buf: Buffer = "| a | b |\n".parse().unwrap();
        assert!(buf.parsed().tables.is_empty());

        // Adding the delimiter line makes it exactly one table.
        let buf2: Buffer = "| a | b |\n|---|---|\n".parse().unwrap();
        assert_eq!(buf2.parsed().tables.len(), 1);
    }

    #[test]
    fn snapshot_offset_and_row_lookup() {
        let mut buf: Buffer = "| a | b |\n|:--|--:|\n| 1 | 2 |\n| 3 | 4 |\n"
            .parse()
            .unwrap();
        let snap = buf.render_snapshot();

        // Inside the table block resolves to the table; outside is None.
        let inside = snap.table_containing_offset(15);
        assert!(inside.is_some());
        assert_eq!(inside.unwrap().ncols, 2);
        assert!(snap.table_containing_offset(1000).is_none());

        // Line roles: header / delimiter / body rows.
        assert!(matches!(
            snap.table_row_at_line(0),
            Some((_, RowKind::Header))
        ));
        assert!(matches!(
            snap.table_row_at_line(1),
            Some((_, RowKind::Delimiter))
        ));
        assert!(matches!(
            snap.table_row_at_line(2),
            Some((_, RowKind::Body(0)))
        ));
        assert!(matches!(
            snap.table_row_at_line(3),
            Some((_, RowKind::Body(1)))
        ));
    }
}
