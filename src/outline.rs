//! The document-map / outline panel: a right-docked list of the document's ATX
//! headings, the current section highlighted, click a heading to scroll to it.

use crate::marker::HeadingInfo;

/// Index of the section the cursor is in: the last heading with `line <= cursor_line`.
/// `None` when the cursor sits before the first heading. Assumes `headings` is sorted
/// by `line` ascending (document order), which `collect_node_infos` guarantees.
pub fn current_heading_index(headings: &[HeadingInfo], cursor_line: usize) -> Option<usize> {
    headings
        .partition_point(|h| h.line <= cursor_line)
        .checked_sub(1)
}

#[cfg(feature = "app")]
pub use app::draw_outline;

#[cfg(feature = "app")]
mod app {
    use parley::Layout;
    use vello::Scene;
    use vello::kurbo::{Affine, BezPath, Point, Rect};
    use vello::peniko::{Brush, Fill};

    use crate::chrome::BarRect;
    use crate::consts::UI_LINE_HEIGHT;
    use crate::doc_layout::ScreenRect;
    use crate::editor::EditorTheme;
    use crate::marker::HeadingInfo;
    use crate::text_engine::{TextEngine, peniko_color, peniko_color_alpha};

    const FONT_SIZE: f32 = 13.0;
    /// Per-level horizontal indent (logical px).
    const INDENT: f32 = 12.0;

    /// Level color: H1 in foreground, deeper levels stepping through the accent palette
    /// and fading toward the muted comment color, keeping each depth legible in Dracula.
    fn level_color(theme: &EditorTheme, level: u8) -> vello::peniko::Color {
        let palette = [
            theme.foreground,
            theme.cyan,
            theme.green,
            theme.orange,
            theme.pink,
            theme.comment,
        ];
        palette[(level.saturating_sub(1) as usize).min(palette.len() - 1)]
    }

    /// Draw the outline panel: fill + left hairline, then a stacked, indented, level-colored
    /// heading list with the current section highlighted (and a fainter hover tint). Returns
    /// one device-px `ScreenRect` per drawn row for click routing. Rows overflowing the panel
    /// height are clipped and not returned (outline-internal scrolling is a deferred follow-up).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_outline(
        engine: &mut TextEngine,
        scene: &mut Scene,
        theme: &EditorTheme,
        headings: &[HeadingInfo],
        current: Option<usize>,
        hover: Option<usize>,
        // Parallel to `headings`: whether each heading's own fold is active in the
        // document, so the outline can mark collapsed sections with a ▸.
        folded: &[bool],
        panel: &BarRect,
        scale: f32,
    ) -> Vec<ScreenRect> {
        let panel_rect = Rect::new(panel.x0, panel.y0, panel.x1, panel.y1);
        // Docked side strip: a `theme.surface` fill (the left hairline is drawn last, on
        // top of the rows, so the current-row highlight can span the full width without
        // painting over it).
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            peniko_color(theme.surface),
            None,
            &panel_rect,
        );

        let pad_x = 12.0 * scale;
        let pad_y = 4.0 * scale;
        let row_h = FONT_SIZE * UI_LINE_HEIGHT * scale + 2.0 * pad_y;
        let sel = peniko_color(theme.selection);
        let hover_tint = peniko_color_alpha(theme.selection, 0.45);

        scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &panel_rect);
        let mut rects = Vec::new();
        let mut row_top = panel.y0;
        for (i, h) in headings.iter().enumerate() {
            if row_top >= panel.y1 {
                break;
            }
            let row_bottom = row_top + row_h as f64;
            let row_rect = Rect::new(panel.x0, row_top, panel.x1, row_bottom);
            if current == Some(i) {
                scene.fill(Fill::NonZero, Affine::IDENTITY, sel, None, &row_rect);
            } else if hover == Some(i) {
                scene.fill(Fill::NonZero, Affine::IDENTITY, hover_tint, None, &row_rect);
            }

            let level_indent = (h.level.saturating_sub(1) as f32) * INDENT * scale;
            let tx = panel.x0 as f32 + pad_x + level_indent;
            let max_text = (panel.x1 as f32 - tx - pad_x).max(4.0 * scale);
            let color = peniko_color(level_color(theme, h.level));
            // A collapsed section gets a small ▸ in the gap left of its text (mirroring
            // the editor gutter), so the map shows folded state at a glance.
            if folded.get(i).copied().unwrap_or(false) {
                let cy = (row_top + row_bottom) / 2.0;
                let cx = (tx - 8.0 * scale) as f64;
                let g = (6.0 * scale) as f64;
                let mut tri = BezPath::new();
                tri.move_to(Point::new(cx - g * 0.3, cy - g * 0.5));
                tri.line_to(Point::new(cx - g * 0.3, cy + g * 0.5));
                tri.line_to(Point::new(cx + g * 0.45, cy));
                tri.close_path();
                scene.fill(Fill::NonZero, Affine::IDENTITY, color, None, &tri);
            }
            let layout = ellipsize(engine, &h.text, scale, color, max_text);
            engine.draw_line(scene, &layout, (tx, row_top as f32 + pad_y));

            rects.push((panel.x0, row_top, panel.x1, row_bottom));
            row_top = row_bottom;
        }
        scene.pop_layer();

        // Left hairline, drawn last so it stays crisp over any full-width row highlight.
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            sel,
            None,
            &Rect::new(panel.x0, panel.y0, panel.x0 + scale as f64, panel.y1),
        );
        rects
    }

    /// Lay out one heading row on a single line, truncating with a trailing `…` when it
    /// exceeds `max_text`. Monospace, so a first char-budget estimate lands within a char
    /// or two; a short shrink loop corrects any residual overhang (e.g. wide/CJK glyphs).
    fn ellipsize(
        engine: &mut TextEngine,
        text: &str,
        scale: f32,
        color: vello::peniko::Color,
        max_text: f32,
    ) -> Layout<Brush> {
        let full = engine.build_line(text, scale, FONT_SIZE, UI_LINE_HEIGHT, color, None, &[]);
        let char_count = text.chars().count();
        if full.width() <= max_text || char_count == 0 {
            return full;
        }
        let char_w = full.width() / char_count as f32;
        let budget = (max_text / char_w).floor() as usize;
        let keep = budget.saturating_sub(1).min(char_count);
        let mut s: String = text.chars().take(keep).collect();
        s.push('…');
        let mut layout = engine.build_line(&s, scale, FONT_SIZE, UI_LINE_HEIGHT, color, None, &[]);
        while layout.width() > max_text && s.chars().count() > 1 {
            let mut chars: Vec<char> = s.chars().collect();
            let n = chars.len();
            chars.remove(n - 2); // drop the char just before the ellipsis
            s = chars.into_iter().collect();
            layout = engine.build_line(&s, scale, FONT_SIZE, UI_LINE_HEIGHT, color, None, &[]);
        }
        layout
    }
}
