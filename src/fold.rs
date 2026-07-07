//! Folding: pure geometry over the document's ATX headings and list items.
//!
//! Folding never mutates the buffer — it is session UI state living on the `Editor`
//! as a set of folded *byte offsets* (line-start of a heading or a list-item bullet).
//! This module derives, from that set plus the current headings/list-items, which lines
//! the layout should collapse to zero height. Byte offsets (not line numbers) anchor a
//! fold because a single-splice edit remaps them exactly; the derivation re-reads the
//! live parse each frame, so a fold's reach tracks edits automatically. A fold is any
//! block anchored at a start offset — headings and list items differ only in how their
//! hidden extent is derived (`heading_extent` by heading level; `list_item_extent` from
//! the item's precomputed nested span).

use std::collections::HashSet;
use std::ops::Range;

use crate::marker::{HeadingInfo, ListItemInfo};

/// Lines hidden when `headings[idx]` folds: from the line after the heading down to
/// (but not including) the next heading of the same-or-higher level, else EOF.
/// Half-open `[start, end)`; `start == end` when the heading has no body.
pub fn heading_extent(headings: &[HeadingInfo], idx: usize, line_count: usize) -> Range<usize> {
    let h = &headings[idx];
    let start = h.line + 1;
    let mut end = line_count;
    for next in &headings[idx + 1..] {
        if next.level <= h.level {
            end = next.line;
            break;
        }
    }
    start..end.max(start)
}

/// Whether folding this heading would hide at least one line (drives chevron display).
/// O(1): the extent is non-empty iff the next heading is a deeper child, or the next
/// sibling/higher heading (or EOF) leaves at least one body line — no forward scan needed.
pub fn heading_is_foldable(headings: &[HeadingInfo], idx: usize, line_count: usize) -> bool {
    let h = &headings[idx];
    match headings.get(idx + 1) {
        None => h.line + 1 < line_count,
        Some(next) if next.level > h.level => true,
        Some(next) => next.line > h.line + 1,
    }
}

/// Lines hidden when `list_items[idx]` folds: its nested/continuation lines, from the
/// line after the bullet down to (but not including) `fold_end_line`.
pub fn list_item_extent(items: &[ListItemInfo], idx: usize) -> Range<usize> {
    let item = &items[idx];
    (item.line + 1)..item.fold_end_line.max(item.line + 1)
}

/// Whether folding this list item would hide at least one line (has nested content).
pub fn list_item_is_foldable(items: &[ListItemInfo], idx: usize) -> bool {
    items[idx].fold_end_line > items[idx].line + 1
}

/// The hidden-line extent for a folded offset, whichever kind of foldable it anchors —
/// heading or list item. `None` if `offset` no longer matches either (stale after an edit).
pub fn extent_for_offset(
    headings: &[HeadingInfo],
    list_items: &[ListItemInfo],
    offset: usize,
    line_count: usize,
) -> Option<Range<usize>> {
    if let Some(idx) = headings.iter().position(|h| h.byte_offset == offset) {
        return Some(heading_extent(headings, idx, line_count));
    }
    let idx = list_items.iter().position(|i| i.byte_offset == offset)?;
    Some(list_item_extent(list_items, idx))
}

/// Merged, sorted hidden-line ranges for every folded heading AND list item. Nested
/// folds (a folded parent + a folded child, or a heading enclosing a folded list)
/// produce subset ranges, so the coalesce below collapses them into the enclosing range.
pub fn hidden_line_ranges(
    headings: &[HeadingInfo],
    list_items: &[ListItemInfo],
    folded: &HashSet<usize>,
    line_count: usize,
) -> Vec<Range<usize>> {
    if folded.is_empty() {
        return Vec::new();
    }
    let mut ranges: Vec<Range<usize>> = headings
        .iter()
        .enumerate()
        .filter(|(_, h)| folded.contains(&h.byte_offset))
        .map(|(i, _)| heading_extent(headings, i, line_count))
        .chain(
            list_items
                .iter()
                .enumerate()
                .filter(|(_, li)| folded.contains(&li.byte_offset))
                .map(|(i, _)| list_item_extent(list_items, i)),
        )
        .filter(|r| r.end > r.start)
        .collect();
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    merged
}

/// Index of the heading whose section contains `line`: the nearest heading at or
/// before `line`. `None` when `line` precedes the first heading.
pub fn section_heading(headings: &[HeadingInfo], line: usize) -> Option<usize> {
    headings.iter().rposition(|h| h.line <= line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(level: u8, line: usize) -> HeadingInfo {
        HeadingInfo {
            level,
            text: String::new(),
            line,
            // Unique, monotonic offsets so tests can fold by offset unambiguously.
            byte_offset: line * 100,
        }
    }

    fn li(line: usize, fold_end_line: usize) -> ListItemInfo {
        ListItemInfo {
            line,
            byte_offset: line * 100,
            fold_end_line,
            depth: 1,
        }
    }

    #[test]
    fn extent_stops_at_same_level_sibling() {
        // # A(0)  body(1..2)  # B(3)
        let hs = [h(1, 0), h(1, 3)];
        assert_eq!(heading_extent(&hs, 0, 10), 1..3);
        assert_eq!(heading_extent(&hs, 1, 10), 4..10); // last heading → EOF
    }

    #[test]
    fn extent_swallows_deeper_subheadings() {
        // # A(0)  ## B(2)  ### C(4)  # D(6)
        let hs = [h(1, 0), h(2, 2), h(3, 4), h(1, 6)];
        assert_eq!(heading_extent(&hs, 0, 10), 1..6); // A swallows B and C
        assert_eq!(heading_extent(&hs, 1, 10), 3..6); // B swallows C, stops at D
        assert_eq!(heading_extent(&hs, 3, 10), 7..10);
    }

    #[test]
    fn extent_empty_when_no_body() {
        // # A(0) immediately followed by # B(1)
        let hs = [h(1, 0), h(1, 1)];
        assert_eq!(heading_extent(&hs, 0, 5), 1..1);
        assert!(!heading_is_foldable(&hs, 0, 5));
        assert!(heading_is_foldable(&hs, 1, 5));
    }

    #[test]
    fn nested_folds_coalesce() {
        let hs = [h(1, 0), h(2, 2), h(3, 4), h(1, 6)];
        // Folding both A and its child B: A's range 1..6 subsumes B's 3..6.
        let folded: HashSet<usize> = [hs[0].byte_offset, hs[1].byte_offset].into();
        assert_eq!(hidden_line_ranges(&hs, &[], &folded, 10), vec![1..6]);
    }

    #[test]
    fn disjoint_folds_stay_separate() {
        let hs = [h(1, 0), h(1, 3), h(1, 6)];
        let folded: HashSet<usize> = [hs[0].byte_offset, hs[2].byte_offset].into();
        assert_eq!(hidden_line_ranges(&hs, &[], &folded, 10), vec![1..3, 7..10]);
    }

    #[test]
    fn list_item_extent_and_foldability() {
        // Parent on line 0 (children on 1,2), a nested child on line 1 (its child on 2),
        // and a leaf on line 2. fold_end_line is the first line NOT in the item.
        let items = [li(0, 3), li(1, 3), li(2, 3)];
        assert_eq!(list_item_extent(&items, 0), 1..3);
        assert!(list_item_is_foldable(&items, 0));
        assert_eq!(list_item_extent(&items, 1), 2..3);
        assert!(list_item_is_foldable(&items, 1));
        assert!(!list_item_is_foldable(&items, 2)); // 3..3 empty → leaf
    }

    #[test]
    fn nested_list_folds_coalesce() {
        // Parent li(0,3) subsumes child li(1,3) when both folded.
        let items = [li(0, 3), li(1, 3)];
        let folded: HashSet<usize> = [items[0].byte_offset, items[1].byte_offset].into();
        assert_eq!(hidden_line_ranges(&[], &items, &folded, 10), vec![1..3]);
    }

    #[test]
    fn mixed_heading_and_list_union() {
        // A folded heading and a disjoint folded list item both appear, sorted.
        let hs = [h(1, 0)]; // heading at line 0 → extent 1..3
        let items = [li(5, 8)]; // list item at line 5 → extent 6..8
        let folded: HashSet<usize> = [hs[0].byte_offset, items[0].byte_offset].into();
        assert_eq!(
            hidden_line_ranges(&hs, &items, &folded, 3),
            vec![1..3, 6..8]
        );
    }

    #[test]
    fn extent_for_offset_resolves_both_kinds() {
        let hs = [h(1, 0)];
        let items = [li(5, 8)];
        assert_eq!(extent_for_offset(&hs, &items, 0, 3), Some(1..3)); // heading
        assert_eq!(extent_for_offset(&hs, &items, 500, 3), Some(6..8)); // list (line 5 * 100)
        assert_eq!(extent_for_offset(&hs, &items, 999, 3), None); // stale
    }

    #[test]
    fn section_heading_finds_enclosing() {
        let hs = [h(1, 0), h(2, 4), h(1, 8)];
        assert_eq!(section_heading(&hs, 0), Some(0));
        assert_eq!(section_heading(&hs, 5), Some(1));
        assert_eq!(section_heading(&hs, 9), Some(2));
    }
}
