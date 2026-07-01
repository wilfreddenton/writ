//! Per-line styling for the new render path (see MIGRATION-PLAN.md, Phase 3).
//!
//! Turns a [`RenderSnapshot`] line into the inputs `TextEngine::build_line` needs:
//! a display string, a font size (headings are larger → variable line heights),
//! and a set of [`StyleRun`]s carrying color + bold/italic/mono/underline/strike.
//!
//! Phase 3a: the display string is the raw line text (markers still visible), so
//! byte offsets are line-relative and the display↔buffer map is the identity.
//! Phase 3b replaces this with the marker-hiding `build_styled_content` port and a
//! real segment map.

use crate::buffer::RenderSnapshot;
use crate::editor::EditorTheme;
use crate::text_engine::{StyleRun, peniko_color};

/// Fully-resolved styling for one line, ready to hand to `TextEngine::build_line`.
pub struct LineRender {
    pub text: String,
    pub font_size: f32,
    pub runs: Vec<StyleRun>,
}

/// Font-size multiplier for a heading level (1 = largest). 0 = body text.
fn heading_scale(level: u8) -> f32 {
    match level {
        1 => 1.8,
        2 => 1.5,
        3 => 1.3,
        4 => 1.15,
        5 => 1.05,
        _ => 1.0,
    }
}

/// Build the display text + style runs for `line_idx`. `base_font_size` is the
/// body text size in logical px; headings scale up from it.
pub fn build_line_render(
    snapshot: &RenderSnapshot,
    line_idx: usize,
    theme: &EditorTheme,
    base_font_size: f32,
) -> LineRender {
    let markers = snapshot.line_markers(line_idx);
    let range = markers.range.clone();
    let line_start = range.start;

    let rope = &snapshot.rope;
    let text = rope
        .slice(rope.byte_to_char(range.start)..rope.byte_to_char(range.end))
        .to_string();
    let text = text.trim_end_matches('\n').to_string();
    let text_len = text.len();

    let heading_level = markers.heading_level().unwrap_or(0);
    let font_size = base_font_size * heading_scale(heading_level);

    // Clamp a buffer range to a line-relative range within the display text.
    let rel = |start: usize, end: usize| -> Option<std::ops::Range<usize>> {
        let s = start.saturating_sub(line_start).min(text_len);
        let e = end.saturating_sub(line_start).min(text_len);
        (s < e).then_some(s..e)
    };

    let fg = peniko_color(theme.foreground);
    let mut runs = Vec::new();

    let in_code_block = markers.in_code_block || markers.is_fence();
    if in_code_block {
        // Code block: monospace everywhere, tree-sitter capture colors on top.
        if !text.is_empty() {
            let mut base = StyleRun::new(0..text_len, fg);
            base.mono = true;
            runs.push(base);
        }
        for span in snapshot.code_highlights_for_line(line_idx) {
            if let Some(r) = rel(span.range.start, span.range.end) {
                let mut run = StyleRun::new(
                    r,
                    peniko_color(theme.color_for_highlight(span.highlight_id)),
                );
                run.mono = true;
                runs.push(run);
            }
        }
    } else {
        // Prose: heading tint + inline emphasis/link/code spans.
        if heading_level > 0 && !text.is_empty() {
            let mut h = StyleRun::new(0..text_len, peniko_color(theme.purple));
            h.bold = true;
            runs.push(h);
        }
        for region in snapshot.inline_styles_for_line(line_idx) {
            let Some(r) = rel(region.full_range.start, region.full_range.end) else {
                continue;
            };
            let style = &region.style;
            let color = if region.link_url.is_some() {
                peniko_color(theme.cyan)
            } else if style.code {
                peniko_color(theme.green)
            } else {
                fg
            };
            let mut run = StyleRun::new(r, color);
            run.bold = style.bold || heading_level > 0;
            run.italic = style.italic;
            run.mono = style.code;
            run.strikethrough = style.strikethrough;
            run.underline = region.link_url.is_some();
            runs.push(run);
        }
    }

    LineRender {
        text,
        font_size,
        runs,
    }
}
