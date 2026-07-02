//! Per-line styling for the new render path (see MIGRATION-PLAN.md, Phase 3).
//!
//! Turns a [`RenderSnapshot`] line into the inputs `TextEngine::build_line` needs:
//! a display string, a font size (headings are larger → variable line heights),
//! and a set of [`StyleRun`]s carrying color + bold/italic/mono/underline/strike.
//!
//! Phase 3b: markdown markers (emphasis `**`/`*`/`` ` ``, heading `# `) are HIDDEN
//! when the cursor is off their region, and the returned [`SegmentMap`] maps the
//! display text back to buffer bytes. Style runs are mapped onto display offsets
//! through that map. Still verbatim: list/blockquote prefix markers (they want
//! bullet/bar rendering, a later chrome concern).

use crate::buffer::RenderSnapshot;
use crate::editor::EditorTheme;
use crate::inline::{StyledRegion, TextStyle};
use crate::marker::MarkerKind;
use crate::segment_map::{SegmentMap, Special};
use crate::text_engine::{StyleRun, peniko_color};

/// Fully-resolved styling for one line, ready to hand to `TextEngine::build_line`.
#[derive(Clone)]
pub struct LineRender {
    pub text: String,
    pub font_size: f32,
    pub runs: Vec<StyleRun>,
    /// Maps the display `text` back to buffer bytes (for cursor/click/diff).
    pub map: SegmentMap,
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

/// Build the display text (markdown markers hidden when the cursor is off the
/// region), a segment map back to buffer bytes, and style runs over the display,
/// for `line_idx`. `cursor_offset` is the absolute buffer cursor position; markers
/// on the cursor's own region/line stay revealed for editing.
///
/// `extra_regions` are additional inline styled regions to merge in (validated
/// GitHub refs → cyan/underline links, and naked-URL shortening via `display_text`).
/// They carry absolute buffer ranges, like the snapshot's own inline styles.
/// `line_styles` are this line's tree inline styles (bucketed by the caller via
/// `RenderSnapshot::inline_styles_by_line`, without checkbox regions — this fn
/// injects those from `markers`). Passing them in avoids the O(n²) per-line
/// `styles_in_range` scan when laying out a whole document.
pub fn build_line_render(
    snapshot: &RenderSnapshot,
    line_idx: usize,
    theme: &EditorTheme,
    base_font_size: f32,
    cursor_offset: usize,
    line_styles: &[StyledRegion],
    extra_regions: &[StyledRegion],
) -> LineRender {
    let markers = snapshot.line_markers(line_idx);
    let range = markers.range.clone();
    let line_start = range.start;

    let rope = &snapshot.rope;
    // Slice once, excluding a trailing '\n', to avoid a second (trim) allocation.
    let byte_end = if range.end > range.start && rope.get_byte(range.end - 1) == Some(b'\n') {
        range.end - 1
    } else {
        range.end
    };
    let line_text = rope
        .slice(rope.byte_to_char(range.start)..rope.byte_to_char(byte_end))
        .to_string();
    let line_end = line_start + line_text.len();

    let heading_level = markers.heading_level().unwrap_or(0);
    let font_size = base_font_size * heading_scale(heading_level);
    let cursor_on_line = if range.start == range.end {
        cursor_offset == range.start
    } else {
        cursor_offset >= range.start && cursor_offset <= range.end
    };
    let in_code_block = markers.in_code_block || markers.is_fence();

    // Pre-bucketed tree styles + a synthetic checkbox region (from markers), sorted
    // by start to match the old `inline_styles_in_range` order; then GitHub extras.
    let mut inline: Vec<StyledRegion> = line_styles.to_vec();
    for marker in &markers.markers {
        if let MarkerKind::Checkbox { checked } = marker.kind {
            // The checkbox marker range is "[ ] " (4 bytes); style only "[ ]" (3).
            let checkbox_range = marker.range.start..marker.range.start + 3;
            inline.push(StyledRegion {
                full_range: checkbox_range.clone(),
                content_range: checkbox_range,
                style: TextStyle::default(),
                link_url: None,
                is_image: false,
                checkbox: Some(checked),
                display_text: None,
            });
        }
    }
    inline.sort_by_key(|s| s.full_range.start);
    inline.extend_from_slice(extra_regions);

    // Collect the buffer ranges hidden or collapsed on the way to the display.
    // Code blocks and thematic breaks show their markers verbatim.
    let mut specials: Vec<Special> = Vec::new();
    if !in_code_block {
        // Heading `# ` prefix hides when the cursor is elsewhere.
        if heading_level > 0
            && !cursor_on_line
            && let Some(mr) = markers.marker_range()
            && mr.end <= line_end
        {
            specials.push(Special::Hidden(mr));
        }
        // Prefix markers render as bullets / blockquote bars (always on — they're
        // structural), substituting `- `→`• `, `> `→`▎ `. Indent whitespace and
        // ordered-list numbers stay literal; checkbox stays as `[ ]`/`[x]`.
        // On a task item the checkbox is the marker, so suppress the list bullet.
        let has_checkbox = markers
            .markers
            .iter()
            .any(|m| matches!(m.kind, MarkerKind::Checkbox { .. }));
        for marker in &markers.markers {
            let sub = match &marker.kind {
                // Task item: hide the `- ` so the checkbox is the marker.
                MarkerKind::ListItem { ordered: false, .. } if has_checkbox => Some(""),
                MarkerKind::ListItem {
                    ordered: false,
                    unordered_marker,
                    ..
                } => Some(unordered_marker.as_ref().map_or("• ", |m| m.bullet())),
                MarkerKind::BlockQuote => Some("▎ "),
                _ => None,
            };
            if let Some(display) = sub
                && marker.range.start >= line_start
                && marker.range.end <= line_end
            {
                specials.push(Special::Collapsed {
                    buffer: marker.range.clone(),
                    display: display.to_string(),
                });
            }
        }
        for region in &inline {
            let cursor_inside =
                cursor_offset >= region.full_range.start && cursor_offset <= region.full_range.end;
            if cursor_inside {
                continue; // reveal the whole region (markers included) for editing
            }
            if let Some(dt) = &region.display_text {
                specials.push(Special::Collapsed {
                    buffer: region.full_range.clone(),
                    display: dt.clone(),
                });
            } else {
                // Hide the opening and closing delimiters (e.g. `**` … `**`).
                if region.content_range.start > region.full_range.start {
                    specials.push(Special::Hidden(
                        region.full_range.start..region.content_range.start,
                    ));
                }
                if region.full_range.end > region.content_range.end {
                    specials.push(Special::Hidden(
                        region.content_range.end..region.full_range.end,
                    ));
                }
            }
        }
    }

    let (text, map) = SegmentMap::build(&line_text, line_start, &specials);

    let fg = peniko_color(theme.foreground);
    let mut runs = Vec::new();

    if in_code_block {
        // Monospace everywhere, tree-sitter capture colors on top.
        if !text.is_empty() {
            let mut base = StyleRun::new(0..text.len(), fg);
            base.mono = true;
            runs.push(base);
        }
        for span in snapshot.code_highlights_for_line(line_idx) {
            let r = map.buffer_range_to_display(span.range.clone());
            if !r.is_empty() {
                let mut run = StyleRun::new(
                    r,
                    peniko_color(theme.color_for_highlight(span.highlight_id)),
                );
                run.mono = true;
                runs.push(run);
            }
        }
    } else {
        if heading_level > 0 && !text.is_empty() {
            let mut h = StyleRun::new(0..text.len(), peniko_color(theme.purple));
            h.bold = true;
            runs.push(h);
        }
        for region in &inline {
            let cursor_inside =
                cursor_offset >= region.full_range.start && cursor_offset <= region.full_range.end;
            // Style the visible content; when revealed, style the whole region.
            let style_range = if cursor_inside {
                region.full_range.clone()
            } else {
                region.content_range.clone()
            };
            let r = map.buffer_range_to_display(style_range);
            if r.is_empty() {
                continue;
            }
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
        map,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Buffer;
    use crate::inline::TextStyle;
    use std::ops::Range;

    fn link_region(range: Range<usize>) -> StyledRegion {
        StyledRegion {
            full_range: range.clone(),
            content_range: range,
            style: TextStyle::default(),
            link_url: Some("https://github.com/o/r/issues/1".to_string()),
            is_image: false,
            checkbox: None,
            display_text: None,
        }
    }

    /// An extra region carrying `link_url` (a validated GitHub ref) colors its span
    /// as an underlined link — the render half of the GitHub coloring feature.
    #[test]
    fn extra_link_region_styles_underlined_link() {
        let mut buffer: Buffer = "See #1 today\n".parse().unwrap();
        let snapshot = buffer.render_snapshot();
        let theme = EditorTheme::dracula();
        // "#1" is bytes 4..6. Cursor off the line so nothing is revealed.
        let extra = [link_region(4..6)];
        let lr = build_line_render(&snapshot, 0, &theme, 18.0, usize::MAX, &[], &extra);
        let link_run = lr
            .runs
            .iter()
            .find(|r| r.underline)
            .expect("a link run should exist");
        assert_eq!(link_run.color, peniko_color(theme.cyan));
        // The colored display range corresponds to "#1".
        let ds = lr.map.buffer_to_display(4);
        let de = lr.map.buffer_to_display(6);
        assert_eq!(link_run.range, ds..de);
    }

    /// Without any extra regions the same line has no link run (unvalidated refs
    /// stay plain text).
    #[test]
    fn no_extra_regions_no_link() {
        let mut buffer: Buffer = "See #1 today\n".parse().unwrap();
        let snapshot = buffer.render_snapshot();
        let theme = EditorTheme::dracula();
        let lr = build_line_render(&snapshot, 0, &theme, 18.0, usize::MAX, &[], &[]);
        assert!(!lr.runs.iter().any(|r| r.underline));
    }
}
