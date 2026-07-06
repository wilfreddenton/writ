//! Heading-based folding (v1): pure geometry over the document's ATX headings.
//!
//! Folding never mutates the buffer — it is session UI state living on the `Editor`
//! as a set of folded heading *byte offsets*. This module derives, from that set and
//! the current headings, which lines the layout should collapse to zero height. Byte
//! offsets (not line numbers) anchor a fold because a single-splice edit remaps them
//! exactly; the derivation here re-reads live headings each frame, so a fold's reach
//! tracks edits automatically. List-indent folding is a deliberate follow-up — this
//! model generalizes (a fold is any block anchored at a start offset); only the
//! extent derivation below is heading-specific.

use std::collections::HashSet;
use std::ops::Range;

use crate::marker::HeadingInfo;

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
pub fn heading_is_foldable(headings: &[HeadingInfo], idx: usize, line_count: usize) -> bool {
    let r = heading_extent(headings, idx, line_count);
    r.end > r.start
}

/// Merged, sorted hidden-line ranges for every folded heading. Nested folds produce
/// subset ranges, so the coalesce below collapses them into the enclosing range.
pub fn hidden_line_ranges(
    headings: &[HeadingInfo],
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
        assert_eq!(hidden_line_ranges(&hs, &folded, 10), vec![1..6]);
    }

    #[test]
    fn disjoint_folds_stay_separate() {
        let hs = [h(1, 0), h(1, 3), h(1, 6)];
        let folded: HashSet<usize> = [hs[0].byte_offset, hs[2].byte_offset].into();
        assert_eq!(hidden_line_ranges(&hs, &folded, 10), vec![1..3, 7..10]);
    }

    #[test]
    fn section_heading_finds_enclosing() {
        let hs = [h(1, 0), h(2, 4), h(1, 8)];
        assert_eq!(section_heading(&hs, 0), Some(0));
        assert_eq!(section_heading(&hs, 5), Some(1));
        assert_eq!(section_heading(&hs, 9), Some(2));
    }
}
