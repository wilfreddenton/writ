//! Document viewport: per-line Parley layouts stacked by a prefix-sum height
//! model, plus scroll (see MIGRATION-PLAN.md, Phase 3). Replaces gpui's `ListState`
//! (which gave virtualization + scroll-to-reveal for free) with hand-rolled math.
//!
//! The height model is the top defect surface the plan flags: `tops` has length
//! `n + 1` where `tops[i]` is the top y of line `i` and `tops[n]` is the bottom of
//! the last line. One off-by-one here misplaces everything below it, so the pure
//! prefix-sum + visible-range functions are unit-tested independent of any GPU.

use std::ops::Range;

use parley::{Affinity, Cursor, Selection};
use vello::Scene;
use vello::kurbo::{Affine, Rect};
use vello::peniko::{Brush, Color, Fill};

use crate::buffer::RenderSnapshot;
use crate::diff::DiffState;
use crate::editor::EditorTheme;
use crate::render::build_line_render;
use crate::segment_map::SegmentMap;
use crate::text_engine::{TextEngine, peniko_color, peniko_color_alpha};

/// A screen-space rectangle (device px), already offset by padding + scroll.
pub type ScreenRect = (f64, f64, f64, f64);

/// Inline git-diff decorations for one real line (added-line bg + word ranges).
#[derive(Default)]
struct LineDiff {
    is_addition: bool,
    /// Display byte ranges of word-level additions within the line.
    inline: Vec<Range<usize>>,
}

/// A deleted (ghost) line, rendered from the HEAD snapshot above the buffer line
/// it was removed before. Inert: not in the hit-test/cursor index.
struct Ghost {
    layout: parley::Layout<Brush>,
    height: f32,
    /// Display byte ranges of word-level deletions within the ghost line.
    inline: Vec<Range<usize>>,
}

/// The four translucent inline-diff colors, baked from the theme at build time.
struct DiffColors {
    added_bg: Color,
    added_inline: Color,
    deleted_bg: Color,
    deleted_inline: Color,
}

/// Build the ghost (deleted) lines that render above buffer line `new_line`,
/// laying out each from the HEAD snapshot. `usize::MAX` cursor keeps every marker
/// hidden in ghosts (the cursor is never "on" a ghost line).
#[allow(clippy::too_many_arguments)]
fn build_ghosts_before(
    engine: &mut TextEngine,
    diff: Option<&DiffState>,
    new_line: usize,
    theme: &EditorTheme,
    scale: f32,
    base_font_size: f32,
    line_height: f32,
    max_advance: f32,
    fg: Color,
) -> Vec<Ghost> {
    let Some(d) = diff else { return Vec::new() };
    let Some(old_range) = d.ghost_lines_before(new_line) else {
        return Vec::new();
    };
    let old = &d.old_snapshot;
    let mut out = Vec::new();
    for old_line in old_range {
        if old_line >= old.line_count() {
            break;
        }
        let lr = build_line_render(old, old_line, theme, base_font_size, usize::MAX);
        let layout = engine.build_line(
            &lr.text,
            scale,
            lr.font_size,
            line_height,
            fg,
            Some(max_advance),
            &lr.runs,
        );
        let line_start = old.line_markers(old_line).range.start;
        let inline = d
            .old_inline_changes(old_line)
            .map(|changes| {
                changes
                    .iter()
                    .filter_map(|c| {
                        let dr = lr.map.buffer_range_to_display(
                            line_start + c.range.start..line_start + c.range.end,
                        );
                        (!dr.is_empty()).then_some(dr)
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(Ghost {
            height: layout.height(),
            layout,
            inline,
        });
    }
    out
}

/// Prefix-sum tops for `heights`: `out[0] = pad_top`, `out[i+1] = out[i] +
/// heights[i]`. Length is `heights.len() + 1`.
fn compute_tops(heights: &[f32], pad_top: f32) -> Vec<f32> {
    let mut tops = Vec::with_capacity(heights.len() + 1);
    let mut y = pad_top;
    tops.push(y);
    for &h in heights {
        y += h;
        tops.push(y);
    }
    tops
}

pub struct DocLayout {
    layouts: Vec<parley::Layout<Brush>>,
    /// Per-line display↔buffer maps, parallel to `layouts` (Phase 4 cursor/click).
    maps: Vec<SegmentMap>,
    /// Per-line buffer byte ranges (incl. trailing newline), parallel to `layouts`.
    line_ranges: Vec<Range<usize>>,
    /// Per-line inline git-diff decorations, parallel to `layouts`.
    line_diffs: Vec<LineDiff>,
    /// Ghost (deleted) lines rendered *above* each real line, parallel to `layouts`.
    ghosts: Vec<Vec<Ghost>>,
    /// Total ghost-block height above each real line, parallel to `layouts`.
    ghost_height: Vec<f32>,
    /// Top y of each line's *ghost block*; the real line begins at
    /// `tops[i] + ghost_height[i]`. Length `layouts.len() + 1`. Device px.
    tops: Vec<f32>,
    diff_colors: DiffColors,
    pub scroll_y: f32,
    /// Surface width in device px (for full-width diff row backgrounds).
    width: f32,
    pad_top: f32,
    pad_bottom: f32,
    pad_x: f32,
}

impl DocLayout {
    /// Lay out every line of `snapshot` at the current width. `pad_*`/`font_size`
    /// are logical px; `scale` converts to device px (matching the layouts).
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        engine: &mut TextEngine,
        snapshot: &RenderSnapshot,
        theme: &EditorTheme,
        diff: Option<&DiffState>,
        cursor_offset: usize,
        device_width: f32,
        scale: f32,
        pad_x: f32,
        pad_top: f32,
        pad_bottom: f32,
        base_font_size: f32,
        line_height: f32,
    ) -> Self {
        let fg = peniko_color(theme.foreground);
        let max_advance = (device_width - 2.0 * pad_x * scale).max(1.0);
        let n = snapshot.line_count();
        let mut layouts = Vec::with_capacity(n);
        let mut maps = Vec::with_capacity(n);
        let mut line_ranges = Vec::with_capacity(n);
        let mut line_diffs = Vec::with_capacity(n);
        let mut ghosts = Vec::with_capacity(n);
        let mut ghost_height = Vec::with_capacity(n);
        // Each line's total height = its ghost block above + the real line.
        let mut heights = Vec::with_capacity(n);
        for i in 0..n {
            // Ghost (deleted) lines rendered before this line, from the HEAD snapshot.
            let line_ghosts = build_ghosts_before(
                engine,
                diff,
                i,
                theme,
                scale,
                base_font_size,
                line_height,
                max_advance,
                fg,
            );
            let gh: f32 = line_ghosts.iter().map(|g| g.height).sum();

            let lr = build_line_render(snapshot, i, theme, base_font_size, cursor_offset);
            let layout = engine.build_line(
                &lr.text,
                scale,
                lr.font_size,
                line_height,
                fg,
                Some(max_advance),
                &lr.runs,
            );
            let range = snapshot.line_markers(i).range;
            // Inline diff: map added word ranges (line-relative buffer bytes) through
            // this line's segment map into display ranges.
            let line_diff = match diff {
                Some(d) if d.is_addition(i) => {
                    let inline = d
                        .new_inline_changes(i)
                        .map(|changes| {
                            changes
                                .iter()
                                .filter_map(|c| {
                                    let dr = lr.map.buffer_range_to_display(
                                        range.start + c.range.start..range.start + c.range.end,
                                    );
                                    (!dr.is_empty()).then_some(dr)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    LineDiff {
                        is_addition: true,
                        inline,
                    }
                }
                _ => LineDiff::default(),
            };
            heights.push(gh + layout.height());
            layouts.push(layout);
            maps.push(lr.map);
            line_ranges.push(range);
            line_diffs.push(line_diff);
            ghosts.push(line_ghosts);
            ghost_height.push(gh);
        }
        let diff_colors = DiffColors {
            added_bg: peniko_color_alpha(theme.green, 0.05),
            added_inline: peniko_color_alpha(theme.green, 0.25),
            deleted_bg: peniko_color_alpha(theme.red, 0.05),
            deleted_inline: peniko_color_alpha(theme.red, 0.25),
        };
        Self {
            layouts,
            maps,
            line_ranges,
            line_diffs,
            ghosts,
            ghost_height,
            diff_colors,
            tops: compute_tops(&heights, pad_top * scale),
            scroll_y: 0.0,
            width: device_width,
            pad_top: pad_top * scale,
            pad_bottom: pad_bottom * scale,
            pad_x: pad_x * scale,
        }
    }

    /// The display↔buffer map for a line (Phase 4 cursor/click math).
    pub fn line_map(&self, line: usize) -> Option<&SegmentMap> {
        self.maps.get(line)
    }

    /// Buffer line index containing `buffer_off` (clamped to the last line).
    fn line_of(&self, buffer_off: usize) -> usize {
        let n = self.line_ranges.len();
        if n == 0 {
            return 0;
        }
        self.line_ranges
            .partition_point(|r| r.start <= buffer_off)
            .saturating_sub(1)
            .min(n - 1)
    }

    /// Top y (device px, before scroll) of the *real* text of line `i` — i.e. below
    /// any ghost (deleted) rows stacked above it.
    fn real_top(&self, line: usize) -> f32 {
        self.tops[line] + self.ghost_height[line]
    }

    /// Screen-space caret rectangle for a buffer offset, or None if empty doc.
    /// `caret_width` is in device px.
    pub fn caret_rect(&self, buffer_off: usize, caret_width: f32) -> Option<ScreenRect> {
        let line = self.line_of(buffer_off);
        let layout = self.layouts.get(line)?;
        let display_off = self.maps[line].buffer_to_display(buffer_off);
        let cursor = Cursor::from_byte_index(layout, display_off, Affinity::Downstream);
        let bb = cursor.geometry(layout, caret_width);
        let dx = self.pad_x as f64;
        let dy = (self.real_top(line) - self.scroll_y) as f64;
        Some((bb.x0 + dx, bb.y0 + dy, bb.x1 + dx, bb.y1 + dy))
    }

    /// Screen-space fill rectangles covering the buffer selection range. Handles
    /// multi-line and wrapped-line selections (parley yields one rect per visual row).
    pub fn selection_rects(&self, selection: Range<usize>) -> Vec<ScreenRect> {
        if selection.start >= selection.end {
            return Vec::new();
        }
        let first = self.line_of(selection.start);
        let last = self.line_of(selection.end);
        let mut rects = Vec::new();
        for line in first..=last.min(self.layouts.len().saturating_sub(1)) {
            let range = &self.line_ranges[line];
            let s = selection.start.max(range.start);
            let e = selection.end.min(range.end);
            if s >= e {
                continue;
            }
            let map = &self.maps[line];
            let ds = map.buffer_to_display(s);
            let de = map.buffer_to_display(e);
            if ds >= de {
                continue;
            }
            let layout = &self.layouts[line];
            let sel = Selection::new(
                Cursor::from_byte_index(layout, ds, Affinity::Downstream),
                Cursor::from_byte_index(layout, de, Affinity::Upstream),
            );
            let dx = self.pad_x as f64;
            let dy = (self.real_top(line) - self.scroll_y) as f64;
            for (bb, _) in sel.geometry(layout) {
                rects.push((bb.x0 + dx, bb.y0 + dy, bb.x1 + dx, bb.y1 + dy));
            }
        }
        rects
    }

    /// Map a screen point (device px) to a buffer offset via Parley hit-testing.
    /// Points landing in a ghost (deleted) block are inert — they map to the start
    /// of the real line the ghosts precede.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<usize> {
        let n = self.layouts.len();
        if n == 0 {
            return None;
        }
        let cy = y + self.scroll_y;
        let line = self.tops[..n]
            .partition_point(|&t| t <= cy)
            .saturating_sub(1)
            .min(n - 1);
        let real_top = self.real_top(line);
        if cy < real_top {
            // In the ghost block above the real line: inert.
            return Some(self.line_ranges[line].start);
        }
        let layout = &self.layouts[line];
        let lx = (x - self.pad_x).max(0.0);
        let ly = (cy - real_top).max(0.0);
        let display_off = Cursor::from_point(layout, lx, ly).index();
        Some(self.maps[line].display_to_buffer(display_off))
    }

    /// Scroll the minimum amount so the line containing `buffer_off` is visible.
    /// Reveals the ghost block above too (so a hunk's deletions come into view).
    pub fn scroll_to(&mut self, buffer_off: usize, viewport_h: f32) {
        let line = self.line_of(buffer_off);
        let top = self.tops[line];
        let bottom = self.tops[line + 1];
        if top < self.scroll_y {
            self.scroll_y = top;
        } else if bottom > self.scroll_y + viewport_h {
            self.scroll_y = bottom - viewport_h;
        }
        self.clamp_scroll(viewport_h);
    }

    pub fn line_count(&self) -> usize {
        self.layouts.len()
    }

    /// Total document height in device px (last line bottom + bottom padding).
    pub fn content_height(&self) -> f32 {
        self.tops.last().copied().unwrap_or(self.pad_top) + self.pad_bottom
    }

    /// Largest valid scroll offset so the document bottom aligns to the viewport.
    pub fn max_scroll(&self, viewport_h: f32) -> f32 {
        (self.content_height() - viewport_h).max(0.0)
    }

    pub fn scroll_by(&mut self, dy: f32, viewport_h: f32) {
        self.scroll_y = (self.scroll_y + dy).clamp(0.0, self.max_scroll(viewport_h));
    }

    pub fn clamp_scroll(&mut self, viewport_h: f32) {
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll(viewport_h));
    }

    /// Half-open range of lines intersecting `[scroll_y, scroll_y + viewport_h)`.
    pub fn visible_range(&self, viewport_h: f32) -> (usize, usize) {
        let n = self.layouts.len();
        if n == 0 {
            return (0, 0);
        }
        let top = self.scroll_y;
        let bottom = self.scroll_y + viewport_h;
        // First line whose *bottom* (tops[i+1]) is past the viewport top.
        let first = self.tops[1..].partition_point(|&b| b <= top).min(n);
        // First line whose *top* (tops[i]) is at/after the viewport bottom.
        let last = self.tops[..n].partition_point(|&t| t < bottom).min(n);
        (first, last.max(first))
    }

    /// Fill the word-change rects of `ranges` within `layout`, offset to `top_y`.
    fn fill_word_ranges(
        &self,
        scene: &mut Scene,
        layout: &parley::Layout<Brush>,
        ranges: &[Range<usize>],
        top_y: f64,
        color: Color,
    ) {
        for r in ranges {
            let sel = Selection::new(
                Cursor::from_byte_index(layout, r.start, Affinity::Downstream),
                Cursor::from_byte_index(layout, r.end, Affinity::Upstream),
            );
            for (bb, _) in sel.geometry(layout) {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    color,
                    None,
                    &Rect::new(
                        bb.x0 + self.pad_x as f64,
                        bb.y0 + top_y,
                        bb.x1 + self.pad_x as f64,
                        bb.y1 + top_y,
                    ),
                );
            }
        }
    }

    /// Paint visible lines: ghost (deleted) rows above each line, then real glyphs.
    pub fn draw(&self, engine: &TextEngine, scene: &mut Scene, viewport_h: f32) {
        let (first, last) = self.visible_range(viewport_h);
        for i in first..last {
            // Ghost (deleted) rows stacked in this line's ghost block.
            let mut gy = self.tops[i] - self.scroll_y;
            for ghost in &self.ghosts[i] {
                let top = gy as f64;
                let bottom = (gy + ghost.height) as f64;
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    self.diff_colors.deleted_bg,
                    None,
                    &Rect::new(0.0, top, self.width as f64, bottom),
                );
                self.fill_word_ranges(
                    scene,
                    &ghost.layout,
                    &ghost.inline,
                    top,
                    self.diff_colors.deleted_inline,
                );
                engine.draw_line(scene, &ghost.layout, (self.pad_x, gy));
                gy += ghost.height;
            }
            // The real line, below its ghost block.
            engine.draw_line(scene, &self.layouts[i], (self.pad_x, gy));
        }
    }

    /// Paint inline-diff backgrounds for added lines (faint full-row bg + stronger
    /// word-change bg). Call BEFORE `draw` so glyphs sit on top.
    pub fn draw_added_backgrounds(&self, scene: &mut Scene, viewport_h: f32) {
        let (first, last) = self.visible_range(viewport_h);
        for i in first..last {
            let d = &self.line_diffs[i];
            if !d.is_addition {
                continue;
            }
            let top = (self.real_top(i) - self.scroll_y) as f64;
            let bottom = (self.tops[i + 1] - self.scroll_y) as f64;
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                self.diff_colors.added_bg,
                None,
                &Rect::new(0.0, top, self.width as f64, bottom),
            );
            self.fill_word_ranges(
                scene,
                &self.layouts[i],
                &d.inline,
                top,
                self.diff_colors.added_inline,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tops_prefix_sum_has_n_plus_one() {
        let heights = [10.0, 20.0, 5.0];
        let tops = compute_tops(&heights, 4.0);
        assert_eq!(tops.len(), 4); // n + 1
        assert_eq!(tops, vec![4.0, 14.0, 34.0, 39.0]);
    }

    #[test]
    fn tops_empty_doc() {
        let tops = compute_tops(&[], 4.0);
        assert_eq!(tops, vec![4.0]); // just the padding top
    }

    /// Build a DocLayout-like fixture from raw heights to test visibility math
    /// without a GPU/font stack.
    fn fixture(heights: &[f32], pad_top: f32, pad_bottom: f32) -> DocLayout {
        DocLayout {
            layouts: heights.iter().map(|_| parley::Layout::new()).collect(),
            maps: heights
                .iter()
                .map(|_| SegmentMap::identity("", 0).1)
                .collect(),
            line_ranges: heights.iter().map(|_| 0..0).collect(),
            line_diffs: heights.iter().map(|_| LineDiff::default()).collect(),
            ghosts: heights.iter().map(|_| Vec::new()).collect(),
            ghost_height: heights.iter().map(|_| 0.0).collect(),
            diff_colors: DiffColors {
                added_bg: Color::TRANSPARENT,
                added_inline: Color::TRANSPARENT,
                deleted_bg: Color::TRANSPARENT,
                deleted_inline: Color::TRANSPARENT,
            },
            tops: compute_tops(heights, pad_top),
            scroll_y: 0.0,
            width: 100.0,
            pad_top,
            pad_bottom,
            pad_x: 0.0,
        }
    }

    #[test]
    fn visible_range_covers_viewport() {
        // 5 lines of height 10, pad_top 0 => tops [0,10,20,30,40,50]
        let doc = fixture(&[10.0; 5], 0.0, 0.0);
        // Viewport [0,25) => lines 0,1,2 (line 2 spans 20..30, intersects).
        assert_eq!(doc.visible_range(25.0), (0, 3));
        // Scrolled to 25, viewport height 20 => [25,45) => lines 2,3,4.
        let mut doc = doc;
        doc.scroll_y = 25.0;
        assert_eq!(doc.visible_range(20.0), (2, 5));
    }

    #[test]
    fn scroll_clamps_to_content() {
        let mut doc = fixture(&[10.0; 5], 0.0, 0.0); // content height 50
        doc.scroll_by(1000.0, 20.0);
        assert_eq!(doc.scroll_y, 30.0); // 50 - 20
        doc.scroll_by(-1000.0, 20.0);
        assert_eq!(doc.scroll_y, 0.0);
    }

    #[test]
    fn short_doc_has_no_scroll() {
        let mut doc = fixture(&[10.0; 2], 0.0, 0.0); // content 20 < viewport 100
        doc.scroll_by(50.0, 100.0);
        assert_eq!(doc.scroll_y, 0.0);
        assert_eq!(doc.max_scroll(100.0), 0.0);
    }

    /// The load-bearing Phase 4 path: a real layout, caret geometry, and click
    /// hit-testing must round-trip through Parley + the segment map. Runs headless
    /// (fonts, no GPU).
    #[test]
    fn caret_and_hit_test_roundtrip() {
        use crate::buffer::Buffer;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        let mut buffer: Buffer = "hello world\nsecond line here\n".parse().unwrap();
        let snapshot = buffer.render_snapshot();
        let doc = DocLayout::build(
            &mut engine,
            &snapshot,
            &theme,
            None,
            0,
            1200.0,
            1.0,
            24.0,
            24.0,
            48.0,
            18.0,
            1.5,
        );
        // Several buffer offsets (incl. second line) should map to a caret rect
        // whose center hit-tests back to the same offset.
        for &off in &[0usize, 6, 11, 12, 19, 27] {
            let (x0, y0, x1, y1) = doc.caret_rect(off, 2.0).expect("caret rect");
            let cx = ((x0 + x1) / 2.0) as f32;
            let cy = ((y0 + y1) / 2.0) as f32;
            let got = doc.hit_test(cx, cy).expect("hit test");
            assert_eq!(got, off, "offset {off} round-trip (got {got})");
        }
    }

    /// Ghost (deleted) rows offset the real line below them, and clicks in a ghost
    /// block are inert. Locks the Phase 5b interleave math (the plan's defect surface).
    #[test]
    fn ghost_rows_offset_and_are_inert() {
        use crate::buffer::Buffer;
        use crate::diff::DiffState;
        let mut engine = TextEngine::new();
        let theme = EditorTheme::dracula();
        let old_text = "line one\nline two\n";
        let new_text = "line one changed here\nline two\n";
        let mut base: Buffer = old_text.parse().unwrap();
        let diff = DiffState::compute(base.render_snapshot(), old_text, new_text);
        assert!(diff.has_hunks());

        let mut buf: Buffer = new_text.parse().unwrap();
        let snapshot = buf.render_snapshot();
        let doc = DocLayout::build(
            &mut engine,
            &snapshot,
            &theme,
            Some(&diff),
            usize::MAX,
            1200.0,
            1.0,
            24.0,
            24.0,
            48.0,
            18.0,
            1.5,
        );
        // Line 0's changed version has a deleted ghost stacked above it.
        assert!(doc.ghost_height[0] > 0.0, "expected a ghost above line 0");
        // The real line begins below its ghost block, and the caret follows.
        assert!(doc.real_top(0) > doc.tops[0]);
        let (_, cy0, _, _) = doc.caret_rect(0, 2.0).unwrap();
        assert!(cy0 as f32 >= doc.real_top(0) - 1.0);
        // A click in the ghost block is inert → maps to the real line start (offset 0).
        let ghost_mid = doc.tops[0] + doc.ghost_height[0] * 0.5;
        assert_eq!(doc.hit_test(50.0, ghost_mid), Some(0));
    }
}
