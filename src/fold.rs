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

use crate::callout::CalloutInfo;
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

/// Which kind of block a [`FoldAnchor`] came from. Matters only for the two per-kind
/// gestures: the recursive fold gathers same-kind descendants, and "fold all at this
/// level" groups by heading *level* vs list/callout nesting *depth*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldKind {
    Heading,
    List,
    Callout,
}

/// A foldable block, projected to the fields every fold operation needs, so headings,
/// list items, and callouts share one code path (extent lookup, hidden ranges, the
/// recursive/level gestures, chevron placement). Only blocks that hide ≥1 line when
/// folded become anchors.
#[derive(Debug, Clone)]
pub struct FoldAnchor {
    /// Byte offset of the anchor line's start — the fold-set key (survives edits).
    pub byte_offset: usize,
    /// 0-based line of the anchor (heading / bullet / callout header).
    pub line: usize,
    /// Lines hidden when this anchor folds (half-open).
    pub extent: Range<usize>,
    pub kind: FoldKind,
    /// Grouping key for "fold all at this level": heading level (1–6) or list/callout depth.
    pub level: u32,
}

/// Every foldable block in the document — headings, list items, and callouts — projected
/// to [`FoldAnchor`]s, grouped by kind (all headings, then list items, then callouts), not
/// in document order. Consumers look anchors up by `byte_offset`/kind/extent, so order is
/// irrelevant. Non-foldable blocks (no hidden body) are omitted.
pub fn fold_anchors(
    headings: &[HeadingInfo],
    list_items: &[ListItemInfo],
    callouts: &[CalloutInfo],
    line_count: usize,
) -> Vec<FoldAnchor> {
    let mut anchors = Vec::new();
    for (i, h) in headings.iter().enumerate() {
        if heading_is_foldable(headings, i, line_count) {
            anchors.push(FoldAnchor {
                byte_offset: h.byte_offset,
                line: h.line,
                extent: heading_extent(headings, i, line_count),
                kind: FoldKind::Heading,
                level: h.level as u32,
            });
        }
    }
    for (i, li) in list_items.iter().enumerate() {
        if list_item_is_foldable(list_items, i) {
            anchors.push(FoldAnchor {
                byte_offset: li.byte_offset,
                line: li.line,
                extent: list_item_extent(list_items, i),
                kind: FoldKind::List,
                level: li.depth as u32,
            });
        }
    }
    for c in callouts {
        if c.is_foldable() {
            anchors.push(FoldAnchor {
                byte_offset: c.byte_offset,
                line: c.header_line,
                extent: c.body_extent(),
                kind: FoldKind::Callout,
                level: c.depth as u32,
            });
        }
    }
    anchors
}

/// The hidden-line extent for a folded offset. `None` if it no longer anchors a foldable
/// block (stale after an edit).
pub fn extent_for_offset(anchors: &[FoldAnchor], offset: usize) -> Option<Range<usize>> {
    anchors
        .iter()
        .find(|a| a.byte_offset == offset)
        .map(|a| a.extent.clone())
}

/// Merged, sorted hidden-line ranges for every folded anchor. Nested folds (a folded
/// parent + a folded child) produce subset ranges, coalesced into the enclosing range.
pub fn hidden_line_ranges(anchors: &[FoldAnchor], folded: &HashSet<usize>) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = anchors
        .iter()
        .filter(|a| folded.contains(&a.byte_offset))
        .map(|a| a.extent.clone())
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

    fn callout(header_line: usize, end_line: usize, foldable: bool) -> CalloutInfo {
        CalloutInfo {
            kind: crate::callout::CalloutKind::Note,
            title: "Note".into(),
            foldable,
            default_collapsed: false,
            header_line,
            byte_offset: header_line * 100,
            end_line,
            depth: 1,
        }
    }

    #[test]
    fn callout_extent_and_union() {
        // A foldable callout on line 5 (body 6..8) unions with a folded heading.
        let hs = [h(1, 0)];
        let cs = [callout(5, 8, true)];
        let anchors = fold_anchors(&hs, &[], &cs, 3);
        let folded: HashSet<usize> = [hs[0].byte_offset, cs[0].byte_offset].into();
        assert_eq!(hidden_line_ranges(&anchors, &folded), vec![1..3, 6..8]);
        assert_eq!(extent_for_offset(&anchors, 500), Some(6..8));
    }

    #[test]
    fn non_opted_in_callout_never_folds() {
        // A callout without `+`/`-` isn't a foldable anchor, so it can't fold.
        let cs = [callout(2, 5, false)];
        let anchors = fold_anchors(&[], &[], &cs, 10);
        assert!(anchors.is_empty());
        let folded: HashSet<usize> = [cs[0].byte_offset].into();
        assert!(hidden_line_ranges(&anchors, &folded).is_empty());
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
        assert_eq!(
            hidden_line_ranges(&fold_anchors(&hs, &[], &[], 10), &folded),
            vec![1..6]
        );
    }

    #[test]
    fn disjoint_folds_stay_separate() {
        let hs = [h(1, 0), h(1, 3), h(1, 6)];
        let folded: HashSet<usize> = [hs[0].byte_offset, hs[2].byte_offset].into();
        assert_eq!(
            hidden_line_ranges(&fold_anchors(&hs, &[], &[], 10), &folded),
            vec![1..3, 7..10]
        );
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
        assert_eq!(
            hidden_line_ranges(&fold_anchors(&[], &items, &[], 10), &folded),
            vec![1..3]
        );
    }

    #[test]
    fn mixed_heading_and_list_union() {
        // A folded heading and a disjoint folded list item both appear, sorted.
        let hs = [h(1, 0)]; // heading at line 0 → extent 1..3
        let items = [li(5, 8)]; // list item at line 5 → extent 6..8
        let folded: HashSet<usize> = [hs[0].byte_offset, items[0].byte_offset].into();
        assert_eq!(
            hidden_line_ranges(&fold_anchors(&hs, &items, &[], 3), &folded),
            vec![1..3, 6..8]
        );
    }

    #[test]
    fn extent_for_offset_resolves_all_kinds() {
        let hs = [h(1, 0)];
        let items = [li(5, 8)];
        let cs = [callout(9, 12, true)];
        let anchors = fold_anchors(&hs, &items, &cs, 3);
        assert_eq!(extent_for_offset(&anchors, 0), Some(1..3)); // heading
        assert_eq!(extent_for_offset(&anchors, 500), Some(6..8)); // list (line 5 * 100)
        assert_eq!(extent_for_offset(&anchors, 900), Some(10..12)); // callout (line 9 * 100)
        assert_eq!(extent_for_offset(&anchors, 999), None); // stale
    }

    #[test]
    fn section_heading_finds_enclosing() {
        let hs = [h(1, 0), h(2, 4), h(1, 8)];
        assert_eq!(section_heading(&hs, 0), Some(0));
        assert_eq!(section_heading(&hs, 5), Some(1));
        assert_eq!(section_heading(&hs, 9), Some(2));
    }
}
