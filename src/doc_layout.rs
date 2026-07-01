//! Document viewport: per-line Parley layouts stacked by a prefix-sum height
//! model, plus scroll (see MIGRATION-PLAN.md, Phase 3). Replaces gpui's `ListState`
//! (which gave virtualization + scroll-to-reveal for free) with hand-rolled math.
//!
//! The height model is the top defect surface the plan flags: `tops` has length
//! `n + 1` where `tops[i]` is the top y of line `i` and `tops[n]` is the bottom of
//! the last line. One off-by-one here misplaces everything below it, so the pure
//! prefix-sum + visible-range functions are unit-tested independent of any GPU.

use vello::Scene;
use vello::peniko::Brush;

use crate::buffer::RenderSnapshot;
use crate::editor::EditorTheme;
use crate::render::build_line_render;
use crate::text_engine::{TextEngine, peniko_color};

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
    /// Top y of each line; length `layouts.len() + 1`. Device px.
    tops: Vec<f32>,
    pub scroll_y: f32,
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
        let mut heights = Vec::with_capacity(n);
        for i in 0..n {
            let lr = build_line_render(snapshot, i, theme, base_font_size);
            let layout = engine.build_line(
                &lr.text,
                scale,
                lr.font_size,
                line_height,
                fg,
                Some(max_advance),
                &lr.runs,
            );
            heights.push(layout.height());
            layouts.push(layout);
        }
        Self {
            layouts,
            tops: compute_tops(&heights, pad_top * scale),
            scroll_y: 0.0,
            pad_top: pad_top * scale,
            pad_bottom: pad_bottom * scale,
            pad_x: pad_x * scale,
        }
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

    /// Paint visible lines into `scene`, translated by scroll + padding.
    pub fn draw(&self, engine: &TextEngine, scene: &mut Scene, viewport_h: f32) {
        let (first, last) = self.visible_range(viewport_h);
        for i in first..last {
            let y = self.tops[i] - self.scroll_y;
            engine.draw_line(scene, &self.layouts[i], (self.pad_x, y));
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
            tops: compute_tops(heights, pad_top),
            scroll_y: 0.0,
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
}
