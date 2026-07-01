//! Display↔buffer segment map (see MIGRATION-PLAN.md, Phase 3b) — the load-bearing
//! invariant of the new render path.
//!
//! Parley lays out the *display* string (markdown markers hidden, some regions
//! collapsed to shorter tokens) while all editing/click/diff math speaks *buffer*
//! bytes. This type is the single source of truth for translating between them.
//!
//! It replaces the old `buffer_to_visual_pos`/`visual_to_buffer_pos` counters,
//! which walked one *byte* at a time and so miscounted multibyte UTF-8. Here,
//! verbatim segments preserve byte offsets exactly (a buffer byte and its display
//! byte differ only by a constant shift), so multibyte text round-trips for free;
//! only intentionally-collapsed tokens are non-1:1, and those are handled explicitly.

use std::ops::Range;

/// A buffer subrange that is transformed on the way to the display string.
#[derive(Debug, Clone, PartialEq)]
pub enum Special {
    /// Buffer bytes that don't appear in the display at all (e.g. `**` markers,
    /// a hidden `# ` heading prefix).
    Hidden(Range<usize>),
    /// Buffer bytes shown as a different, usually shorter token (e.g. a naked URL
    /// rendered as `owner/repo#123`).
    Collapsed {
        buffer: Range<usize>,
        display: String,
    },
}

impl Special {
    fn start(&self) -> usize {
        match self {
            Special::Hidden(r) => r.start,
            Special::Collapsed { buffer, .. } => buffer.start,
        }
    }
    fn end(&self) -> usize {
        match self {
            Special::Hidden(r) => r.end,
            Special::Collapsed { buffer, .. } => buffer.end,
        }
    }
}

/// One contiguous run of the display string and the buffer range it came from.
#[derive(Debug, Clone, PartialEq)]
struct Segment {
    buffer: Range<usize>,
    display: Range<usize>,
    /// display bytes == buffer bytes (a pure shift), so positions map 1:1.
    verbatim: bool,
}

/// Maps between buffer byte offsets and display byte offsets for one line.
#[derive(Debug, Clone)]
pub struct SegmentMap {
    segments: Vec<Segment>,
    display_len: usize,
    buffer_start: usize,
    buffer_end: usize,
}

impl SegmentMap {
    /// Build the display string and map for a line. `line_text` is the raw line
    /// (no trailing newline) starting at buffer offset `line_start`. `specials`
    /// must be non-overlapping and lie within the line; they need not be sorted.
    pub fn build(line_text: &str, line_start: usize, specials: &[Special]) -> (String, SegmentMap) {
        let buffer_end = line_start + line_text.len();
        let mut sp: Vec<&Special> = specials
            .iter()
            .filter(|s| s.start() >= line_start && s.end() <= buffer_end && s.start() < s.end())
            .collect();
        sp.sort_by_key(|s| s.start());

        let mut display = String::new();
        let mut segments = Vec::new();
        let mut pos = line_start;

        let push_verbatim =
            |segments: &mut Vec<Segment>, display: &mut String, from: usize, to: usize| {
                if to <= from {
                    return;
                }
                let slice = &line_text[from - line_start..to - line_start];
                let d0 = display.len();
                display.push_str(slice);
                segments.push(Segment {
                    buffer: from..to,
                    display: d0..display.len(),
                    verbatim: true,
                });
            };

        for special in sp {
            let (s, e) = (special.start(), special.end());
            if s < pos {
                // Overlapping special (e.g. nested emphasis) — skip defensively so
                // the map stays monotonic rather than producing bad offsets.
                continue;
            }
            push_verbatim(&mut segments, &mut display, pos, s);
            match special {
                Special::Hidden(_) => {}
                Special::Collapsed { display: token, .. } => {
                    let d0 = display.len();
                    display.push_str(token);
                    segments.push(Segment {
                        buffer: s..e,
                        display: d0..display.len(),
                        verbatim: false,
                    });
                }
            }
            pos = e;
        }
        push_verbatim(&mut segments, &mut display, pos, buffer_end);

        let display_len = display.len();
        (
            display,
            SegmentMap {
                segments,
                display_len,
                buffer_start: line_start,
                buffer_end,
            },
        )
    }

    /// Identity map for a line with no hidden/collapsed regions.
    pub fn identity(line_text: &str, line_start: usize) -> (String, SegmentMap) {
        Self::build(line_text, line_start, &[])
    }

    pub fn display_len(&self) -> usize {
        self.display_len
    }

    /// Map a buffer byte offset to a display byte offset. Offsets inside a hidden
    /// region collapse to that region's display boundary; offsets inside a
    /// collapsed token clamp to its display start (the region is atomic).
    pub fn buffer_to_display(&self, buffer_off: usize) -> usize {
        if buffer_off <= self.buffer_start {
            return 0;
        }
        if buffer_off >= self.buffer_end {
            return self.display_len;
        }
        for seg in &self.segments {
            if buffer_off < seg.buffer.start {
                // In a hidden gap before this segment.
                return seg.display.start;
            }
            if buffer_off < seg.buffer.end {
                return if seg.verbatim {
                    seg.display.start + (buffer_off - seg.buffer.start)
                } else {
                    seg.display.start
                };
            }
        }
        self.display_len
    }

    /// Map a display byte offset back to a buffer byte offset. Offsets inside a
    /// collapsed token clamp to its buffer start.
    pub fn display_to_buffer(&self, display_off: usize) -> usize {
        if display_off == 0 {
            return self.buffer_start;
        }
        if display_off >= self.display_len {
            return self.buffer_end;
        }
        for seg in &self.segments {
            if display_off < seg.display.end {
                return if seg.verbatim {
                    seg.buffer.start + (display_off - seg.display.start)
                } else {
                    seg.buffer.start
                };
            }
        }
        self.buffer_end
    }

    /// Map a buffer range to a display range (clamped, possibly empty).
    pub fn buffer_range_to_display(&self, range: Range<usize>) -> Range<usize> {
        let s = self.buffer_to_display(range.start);
        let e = self.buffer_to_display(range.end);
        s.min(e)..s.max(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_roundtrips() {
        let (display, map) = SegmentMap::identity("hello world", 100);
        assert_eq!(display, "hello world");
        for i in 0..=display.len() {
            let b = map.display_to_buffer(i);
            assert_eq!(b, 100 + i);
            assert_eq!(map.buffer_to_display(b), i);
        }
    }

    #[test]
    fn hidden_emphasis_markers() {
        // "a **b** c" — hide the two `**` at buffer 2..4 and 5..7 (line_start 0).
        let text = "a **b** c";
        let specials = vec![Special::Hidden(2..4), Special::Hidden(5..7)];
        let (display, map) = SegmentMap::build(text, 0, &specials);
        assert_eq!(display, "a b c");
        // Display 'b' is at index 2; its buffer offset is 4.
        assert_eq!(map.display_to_buffer(2), 4);
        assert_eq!(map.buffer_to_display(4), 2);
        // A buffer offset inside the hidden opening `**` (3) collapses to display 2.
        assert_eq!(map.buffer_to_display(3), 2);
        // The space after 'b' in buffer is at 7; display index 3.
        assert_eq!(map.buffer_to_display(7), 3);
        assert_eq!(map.display_to_buffer(3), 7);
    }

    #[test]
    fn collapsed_token_is_shorter() {
        // "see https://github.com/o/r/issues/1 now" collapses the URL to "o/r#1".
        let text = "see LONGURL now";
        let url = 4..11; // "LONGURL"
        let specials = vec![Special::Collapsed {
            buffer: url.clone(),
            display: "o/r#1".to_string(),
        }];
        let (display, map) = SegmentMap::build(text, 0, &specials);
        assert_eq!(display, "see o/r#1 now");
        // Start of the token round-trips exactly.
        assert_eq!(map.buffer_to_display(4), 4);
        assert_eq!(map.display_to_buffer(4), 4);
        // Interior of the collapsed token clamps to its buffer start.
        assert_eq!(map.display_to_buffer(6), 4);
        // Text after the token maps correctly despite the length change.
        assert_eq!(map.buffer_to_display(11), 9); // buffer end of token -> display end
        assert_eq!(map.display_to_buffer(9), 11);
        assert_eq!(map.buffer_to_display(12), 10); // 'n' of "now"
    }

    #[test]
    fn multibyte_verbatim_roundtrips() {
        // Emoji (4 bytes) + accented char (2 bytes); hide a `*` marker in the middle.
        // "é😀*x*" — hide the two `*` around x.
        let text = "é😀 *x*";
        // byte layout: é=0..2, 😀=2..6, ' '=6..7, '*'=7..8, 'x'=8..9, '*'=9..10
        let specials = vec![Special::Hidden(7..8), Special::Hidden(9..10)];
        let (display, map) = SegmentMap::build(text, 0, &specials);
        assert_eq!(display, "é😀 x");
        // 'x' is at buffer 8; in display after "é😀 " (2+4+1 = 7 bytes) => display 7.
        assert_eq!(map.buffer_to_display(8), 7);
        assert_eq!(map.display_to_buffer(7), 8);
        // The emoji start round-trips (buffer 2 -> display 2).
        assert_eq!(map.buffer_to_display(2), 2);
        assert_eq!(map.display_to_buffer(2), 2);
        // End of line.
        assert_eq!(map.buffer_to_display(10), display.len());
    }

    #[test]
    fn line_start_offset_applied() {
        let (_d, map) = SegmentMap::build("abc", 50, &[Special::Hidden(51..52)]);
        // "abc" with 'b' (51..52) hidden => "ac".
        assert_eq!(map.buffer_to_display(50), 0);
        assert_eq!(map.buffer_to_display(52), 1); // 'c'
        assert_eq!(map.display_to_buffer(1), 52);
    }
}
