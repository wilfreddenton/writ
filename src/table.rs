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

    let ncols = std::iter::once(header.cells.len())
        .chain(body.iter().map(|r| r.cells.len()))
        .max()
        .unwrap_or(0);

    Some(TableInfo {
        block: node.start_byte()..node.end_byte(),
        header,
        delimiter_line,
        aligns,
        body,
        ncols,
    })
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
