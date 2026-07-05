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

use std::borrow::Cow;
use std::ops::Range;

use crate::buffer::RenderSnapshot;
use crate::editor::EditorTheme;
use crate::inline::{StyledRegion, TextStyle};
use crate::marker::MarkerKind;
use crate::segment_map::{SegmentMap, Special};
use crate::text_engine::{StyleRun, peniko_color};

/// Half-open-on-the-left, closed-on-the-right containment: the cursor is "in" a
/// region when sitting anywhere from its start through its end (so a cursor at the
/// closing delimiter still reveals the region for editing).
fn cursor_in(range: &Range<usize>, cursor: usize) -> bool {
    cursor >= range.start && cursor <= range.end
}

/// A standalone image on a line: a paragraph whose only content is `![alt](url)`.
/// Drawn as an actual image block; `alt` is shown in the pending/broken placeholder.
#[derive(Clone)]
pub struct ImageRef {
    pub url: String,
    pub alt: String,
}

/// An image appearing inline within a line (e.g. a badge row, or mixed with text) —
/// as opposed to a standalone-image paragraph. Its `![alt](url)` span is collapsed to
/// a zero-width point in the display; the layout reserves a Parley inline box there and
/// the draw path paints the image (or a placeholder) at the box's position.
#[derive(Clone)]
pub struct InlineImageRef {
    pub url: String,
    pub alt: String,
    /// Display byte offset of the collapsed image's zero-width point (where the inline
    /// box sits). Ordered left-to-right across a line.
    pub display_offset: usize,
}

/// Fully-resolved styling for one line, ready to hand to `TextEngine::build_line`.
#[derive(Clone)]
pub struct LineRender {
    pub text: String,
    pub font_size: f32,
    pub runs: Vec<StyleRun>,
    /// Maps the display `text` back to buffer bytes (for cursor/click/diff).
    pub map: SegmentMap,
    /// Display byte offset where the line's content begins (after list/quote/heading
    /// markers). Wrapped rows hang-indent under this column; 0 means no marker prefix.
    pub content_start: usize,
    /// Display byte offset of each blockquote marker on the line (outermost first), one
    /// per nesting level. The draw path measures each to an x and paints a continuous
    /// vertical rule there, so the quote bar spans wrapped rows and adjacent lines.
    pub quote_bar_bytes: Vec<usize>,
    /// This line is a thematic break rendered as a horizontal rule (its `---` text is
    /// hidden). False while the cursor is on the line, so the `---` is editable.
    pub is_hr: bool,
    /// Display byte ranges of inline `code` spans, for painting a subtle background
    /// chip behind them (they'd otherwise be indistinguishable in a monospace body).
    pub code_ranges: Vec<Range<usize>>,
    /// This line is a standalone image (`![alt](url)` as the whole paragraph): its
    /// markdown text is hidden and an image block is drawn instead. `None` while the
    /// cursor is on the line, so the raw markdown is revealed for editing.
    pub image: Option<ImageRef>,
    /// Inline images on the line (`![alt](url)` mixed with text or other images), each
    /// collapsed to a zero-width point where the draw path reserves an inline box. Empty
    /// while the cursor is on the line (raw markdown revealed) and on standalone-image
    /// lines (mutually exclusive with `image`). Ordered by `display_offset`.
    pub inline_images: Vec<InlineImageRef>,
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
    let cursor_on_line = cursor_in(&range, cursor_offset);
    let in_code_block = markers.in_code_block || markers.is_fence();

    // Pre-bucketed tree styles (already sorted by start, since `inline_styles` is)
    // plus a synthetic checkbox region from markers, then GitHub extras. The common
    // case — no checkbox, no extras — borrows `line_styles` untouched, skipping the
    // per-line clone+sort on every laid-out line.
    let has_checkbox_style = markers
        .markers
        .iter()
        .any(|m| matches!(m.kind, MarkerKind::Checkbox { .. }));
    let inline: Cow<[StyledRegion]> = if !has_checkbox_style && extra_regions.is_empty() {
        Cow::Borrowed(line_styles)
    } else {
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
        Cow::Owned(inline)
    };

    // Each region's cursor-inside verdict, computed once and reused by the specials
    // and style-run passes below (both would otherwise recompute it).
    let cursor_inside: Vec<bool> = inline
        .iter()
        .map(|r| cursor_in(&r.full_range, cursor_offset))
        .collect();

    // Standalone image: the line's whole content (ignoring surrounding whitespace) is a
    // single `![alt](url)` region. Rendered as a drawn image block, with the markdown
    // text hidden — except while the cursor is on the line (revealed for editing). The
    // "covers the trimmed content" test naturally excludes list/quote-prefixed images
    // (their marker sits before the image, outside its range).
    let trimmed_start = line_start + (line_text.len() - line_text.trim_start().len());
    let trimmed_end = line_start + line_text.trim_end().len();
    let image = if !in_code_block && !cursor_on_line && trimmed_start < trimmed_end {
        let mut images = inline.iter().filter(|r| r.is_image);
        match (images.next(), images.next()) {
            (Some(r), None)
                if r.full_range.start == trimmed_start
                    && r.full_range.end == trimmed_end
                    && r.content_range.start >= line_start
                    && r.content_range.end <= line_end =>
            {
                r.link_url.as_ref().map(|url| {
                    let alt = line_text
                        [r.content_range.start - line_start..r.content_range.end - line_start]
                        .to_string();
                    ImageRef {
                        url: url.clone(),
                        alt,
                    }
                })
            }
            _ => None,
        }
    } else {
        None
    };
    let is_standalone_image = image.is_some();

    // Collect the buffer ranges hidden or collapsed on the way to the display.
    // Code blocks show their markers verbatim; thematic breaks hide their `---` and
    // render as a drawn rule (unless the cursor is on the line, for editing).
    let is_hr = markers.is_thematic_break() && !cursor_on_line && !in_code_block;
    let mut specials: Vec<Special> = Vec::new();
    // Inline images collected during the specials pass: (buffer offset of the collapsed
    // `![alt](url)` span, url, alt). Resolved to display offsets once the map is built.
    let mut inline_image_specs: Vec<(usize, String, String)> = Vec::new();
    if !in_code_block {
        // Heading `# ` prefix hides when the cursor is elsewhere.
        if heading_level > 0
            && !cursor_on_line
            && let Some(mr) = markers.marker_range()
            && mr.end <= line_end
        {
            specials.push(Special::Hidden(mr));
        }
        // Thematic break: hide the `---` text; the draw path paints a horizontal rule.
        if is_hr && line_end > line_start {
            specials.push(Special::Hidden(line_start..line_end));
        }
        // Standalone image: hide the whole `![alt](url)` line; the draw path paints the
        // image block. Hiding the entire content leaves `text` empty (no double render).
        if is_standalone_image && line_end > line_start {
            specials.push(Special::Hidden(line_start..line_end));
        }
        // Prefix markers render as bullets / blockquote gutters (always on — they're
        // structural), substituting `- `→`• `, `> `→`  ` (the blockquote bar is painted
        // as a continuous rule in the draw path, not a per-line glyph). Indent whitespace and
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
                MarkerKind::BlockQuote => Some("  "),
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
        for (i, region) in inline.iter().enumerate() {
            if is_standalone_image {
                break; // the whole line is hidden; the image block is drawn instead
            }
            if cursor_inside[i] {
                continue; // reveal the whole region (markers included) for editing
            }
            // Inline image: collapse the whole `![alt](url)` span to a zero-width point
            // and record it. The draw path reserves an inline box there and paints the
            // image; unlike the delimiter-hiding fallback, no alt/link text shows.
            // Reveal is per-image, not per-line (the `cursor_inside[i]` skip above
            // already reveals the one the caret is in), so editing one badge in a row
            // leaves the others rendered.
            if region.is_image
                && region.content_range.start >= line_start
                && region.content_range.end <= line_end
                && let Some(url) = &region.link_url
            {
                specials.push(Special::Hidden(region.full_range.clone()));
                let alt = line_text[region.content_range.start - line_start
                    ..region.content_range.end - line_start]
                    .to_string();
                inline_image_specs.push((region.full_range.start, url.clone(), alt));
                continue;
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

    // Resolve each collapsed inline image to its zero-width display point, ordered
    // left-to-right. The `![alt](url)` span was hidden, so this offset is where the
    // draw path anchors the inline box.
    let mut inline_images: Vec<InlineImageRef> = inline_image_specs
        .into_iter()
        .map(|(buffer_start, url, alt)| InlineImageRef {
            url,
            alt,
            display_offset: map.buffer_to_display(buffer_start),
        })
        .collect();
    inline_images.sort_by_key(|i| i.display_offset);

    // Display offsets of each blockquote marker (outermost first), for the drawn gutter
    // rules. Skipped inside code blocks, where `> ` isn't a quote marker.
    let quote_bar_bytes: Vec<usize> = if in_code_block {
        Vec::new()
    } else {
        let mut bars: Vec<usize> = markers
            .markers
            .iter()
            .filter(|m| matches!(m.kind, MarkerKind::BlockQuote))
            .map(|m| map.buffer_to_display(m.range.start))
            .collect();
        bars.sort_unstable(); // markers are innermost-first; want outermost (leftmost) first
        bars
    };

    let fg = peniko_color(theme.foreground);
    let mut runs = Vec::new();
    let mut code_ranges: Vec<Range<usize>> = Vec::new();

    if in_code_block {
        // Monospace everywhere, tree-sitter capture colors on top.
        if !text.is_empty() {
            let mut base = StyleRun::new(0..text.len(), fg);
            base.mono = true;
            runs.push(base);
        }
        if markers.is_fence() {
            // Fence line: the ```/~~~ delimiter reads as comment, the language/info
            // string as green (matching the old gpui rendering). Overlays the base run.
            let bytes = text.as_bytes();
            if let Some(start) = bytes.iter().position(|&b| b == b'`' || b == b'~') {
                let fc = bytes[start];
                let end = start + bytes[start..].iter().take_while(|&&b| b == fc).count();
                let mut delim = StyleRun::new(start..end, peniko_color(theme.comment));
                delim.mono = true;
                runs.push(delim);
                if end < text.len() {
                    let mut lang = StyleRun::new(end..text.len(), peniko_color(theme.green));
                    lang.mono = true;
                    runs.push(lang);
                }
            }
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
        // A completed task's content reads as "done": dimmed (its checkbox stays green).
        // A base run under everything catches plain text between styled spans.
        let dim = markers.checkbox() == Some(true);
        let content_color = if dim { peniko_color(theme.comment) } else { fg };
        if dim && !text.is_empty() {
            runs.push(StyleRun::new(0..text.len(), content_color));
        }
        if heading_level > 0 && !text.is_empty() {
            let mut h = StyleRun::new(0..text.len(), peniko_color(theme.purple));
            h.bold = true;
            runs.push(h);
        }
        for (i, region) in inline.iter().enumerate() {
            // Style the visible content; when revealed, style the whole region.
            let style_range = if cursor_inside[i] {
                region.full_range.clone()
            } else {
                region.content_range.clone()
            };
            let r = map.buffer_range_to_display(style_range);
            if r.is_empty() {
                continue;
            }
            let style = &region.style;
            let color = match region.checkbox {
                // A checked box reads as "done" (green); an empty box is muted.
                Some(true) => peniko_color(theme.green),
                Some(false) => peniko_color(theme.comment),
                None if region.link_url.is_some() => peniko_color(theme.cyan),
                None if style.code => peniko_color(theme.green),
                None => content_color, // dimmed on a completed task, else fg
            };
            // Inline code (not a link/checkbox) gets a background chip behind its span.
            if style.code && region.checkbox.is_none() && region.link_url.is_none() {
                code_ranges.push(r.clone());
            }
            let mut run = StyleRun::new(r, color);
            run.bold = style.bold || heading_level > 0 || region.checkbox == Some(true);
            run.italic = style.italic;
            run.mono = style.code;
            run.strikethrough = style.strikethrough;
            run.underline = region.link_url.is_some();
            runs.push(run);
        }
    }

    // Display column where content begins (after markers) — drives the hanging indent
    // so wrapped rows align under it. Hidden markers (e.g. heading `# `) collapse to 0.
    let content_start = map.buffer_to_display(markers.content_start());

    LineRender {
        text,
        font_size,
        runs,
        map,
        content_start,
        quote_bar_bytes,
        is_hr,
        image,
        code_ranges,
        inline_images,
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

    /// A completed task dims its content (muted, spanning the line) while its checkbox
    /// stays green; an active task's content is not dimmed.
    #[test]
    fn completed_task_dims_content() {
        let theme = EditorTheme::dracula();
        let comment = peniko_color(theme.comment);
        let mut buf: Buffer = "- [x] done\n- [ ] todo\n".parse().unwrap();
        let snap = buf.render_snapshot();

        let done = build_line_render(&snap, 0, &theme, 18.0, usize::MAX, &[], &[]);
        assert!(
            done.runs
                .iter()
                .any(|r| r.color == comment && r.range.len() >= done.text.len()),
            "completed task content is dimmed across the line"
        );
        assert!(
            done.runs
                .iter()
                .any(|r| r.color == peniko_color(theme.green)),
            "the checkbox itself stays green"
        );

        let todo = build_line_render(&snap, 1, &theme, 18.0, usize::MAX, &[], &[]);
        assert!(
            !todo
                .runs
                .iter()
                .any(|r| r.color == comment && r.range.len() >= todo.text.len()),
            "active task content is not dimmed (only its empty box is muted)"
        );
    }

    /// Inline code spans expose their display ranges (for the background chip); a line
    /// with no inline code exposes none.
    #[test]
    fn inline_code_exposes_code_ranges() {
        let theme = EditorTheme::dracula();
        let mut buf: Buffer = "call `foo()` now\nplain line\n".parse().unwrap();
        let snap = buf.render_snapshot();
        let styles = snap.inline_styles_by_line();

        let coded = build_line_render(&snap, 0, &theme, 18.0, usize::MAX, &styles[0], &[]);
        assert_eq!(coded.code_ranges.len(), 1, "one code span");
        // The chip covers the visible `foo()` (backticks hidden off-cursor).
        let (s, e) = (coded.code_ranges[0].start, coded.code_ranges[0].end);
        assert_eq!(&coded.text[s..e], "foo()");

        let plain = build_line_render(&snap, 1, &theme, 18.0, usize::MAX, &styles[1], &[]);
        assert!(plain.code_ranges.is_empty(), "no code span, no chip");
    }

    /// A thematic break hides its `---` and flags `is_hr` for the drawn rule — but
    /// reveals the text (no rule) while the cursor is on the line, for editing.
    #[test]
    fn thematic_break_hides_text_and_flags_rule() {
        let theme = EditorTheme::dracula();
        // Blank lines around `---` so it parses as a thematic break, not a setext
        // heading underline (`a\n---` would make "a" an H2). The break is line 2.
        let mut buffer: Buffer = "a\n\n---\n\nb\n".parse().unwrap();
        let snap = buffer.render_snapshot();

        // Cursor off the `---` line: text hidden, rule flagged.
        let lr = build_line_render(&snap, 2, &theme, 18.0, 0, &[], &[]);
        assert!(lr.is_hr, "off-line thematic break renders as a rule");
        assert!(lr.text.is_empty(), "the `---` text is hidden");

        // Cursor on the `---` line: text revealed, no rule.
        let on = snap.line_markers(2).range.start;
        let lr = build_line_render(&snap, 2, &theme, 18.0, on, &[], &[]);
        assert!(!lr.is_hr, "revealed for editing when cursor is on it");
        assert_eq!(lr.text, "---");
    }

    /// A paragraph that is only `![alt](url)` flags `image: Some` (with url + alt) and
    /// hides its markdown text, so the draw path paints an image block instead. When the
    /// cursor is on the line, `image` is `None` and the raw markdown is revealed.
    #[test]
    fn standalone_image_detected_off_line_revealed_on_line() {
        let theme = EditorTheme::dracula();
        let mut buffer: Buffer = "![a badge](img/badge.png)\n".parse().unwrap();
        let snap = buffer.render_snapshot();
        let styles = snap.inline_styles_by_line();

        let off = build_line_render(&snap, 0, &theme, 18.0, usize::MAX, &styles[0], &[]);
        let img = off
            .image
            .as_ref()
            .expect("standalone image detected off-line");
        assert_eq!(img.url, "img/badge.png");
        assert_eq!(img.alt, "a badge");
        assert!(off.text.is_empty(), "the `![alt](url)` markdown is hidden");

        // Cursor on the line: reveal the raw markdown, no image block.
        let on = build_line_render(&snap, 0, &theme, 18.0, 0, &styles[0], &[]);
        assert!(
            on.image.is_none(),
            "cursor on the line reveals raw markdown"
        );
        assert_eq!(on.text, "![a badge](img/badge.png)");
    }

    /// An image that is only part of a paragraph (surrounding text) is not standalone:
    /// it stays inline text/link, `image` is `None`.
    #[test]
    fn inline_image_is_not_standalone() {
        let theme = EditorTheme::dracula();
        let mut buffer: Buffer = "see ![a](x.png) here\n".parse().unwrap();
        let snap = buffer.render_snapshot();
        let styles = snap.inline_styles_by_line();
        let lr = build_line_render(&snap, 0, &theme, 18.0, usize::MAX, &styles[0], &[]);
        assert!(
            lr.image.is_none(),
            "image amid other text is not standalone"
        );
    }

    /// Two images on one line (a badge row) become inline-image refs off-cursor: their
    /// `![...](...)` markdown is hidden and their display offsets ascend. Reveal is
    /// per-image: the caret inside one reveals only that one (the other stays a box); a
    /// caret elsewhere on the line keeps both rendered.
    #[test]
    fn inline_images_reveal_per_image() {
        let theme = EditorTheme::dracula();
        // "![a](x.png) and ![b](y.png)": image 1 spans 0..11, " and " 11..16, image 2 16..27.
        let mut buffer: Buffer = "![a](x.png) and ![b](y.png)\n".parse().unwrap();
        let snap = buffer.render_snapshot();
        let styles = snap.inline_styles_by_line();

        let off = build_line_render(&snap, 0, &theme, 18.0, usize::MAX, &styles[0], &[]);
        assert_eq!(off.inline_images.len(), 2, "both inline images detected");
        assert!(
            off.inline_images[0].display_offset <= off.inline_images[1].display_offset,
            "display offsets ascend left-to-right"
        );
        assert!(
            !off.text.contains("!["),
            "markdown hidden, got {:?}",
            off.text
        );
        assert!(off.image.is_none(), "not a standalone image");

        // Caret inside the first image: it reveals, the second stays a box.
        let in_first = build_line_render(&snap, 0, &theme, 18.0, 2, &styles[0], &[]);
        assert_eq!(
            in_first.inline_images.len(),
            1,
            "only the other image stays a box"
        );
        assert!(in_first.text.contains("![a]"), "caret's image shown raw");

        // Caret on the line but between the images: both stay boxes.
        let between = build_line_render(&snap, 0, &theme, 18.0, 13, &styles[0], &[]);
        assert_eq!(
            between.inline_images.len(),
            2,
            "off-caret images stay boxes"
        );
    }

    /// The blockquote bar is no longer a `▎` glyph in the text — the marker collapses
    /// to spacing and one gutter offset per nesting level is exposed for the drawn rule.
    #[test]
    fn blockquote_exposes_gutter_offsets_not_glyph() {
        let theme = EditorTheme::dracula();

        let mut single: Buffer = "> quoted line\n".parse().unwrap();
        let snap = single.render_snapshot();
        let lr = build_line_render(&snap, 0, &theme, 18.0, usize::MAX, &[], &[]);
        assert!(!lr.text.contains('▎'), "bar must not be a text glyph");
        assert_eq!(lr.quote_bar_bytes, vec![0], "one gutter at the line start");

        let mut nested: Buffer = "> > deeply quoted\n".parse().unwrap();
        let snap = nested.render_snapshot();
        let lr = build_line_render(&snap, 0, &theme, 18.0, usize::MAX, &[], &[]);
        assert_eq!(lr.quote_bar_bytes.len(), 2, "one gutter per nesting level");
        assert_eq!(lr.quote_bar_bytes[0], 0, "outer gutter at line start");
        assert!(
            lr.quote_bar_bytes[1] > lr.quote_bar_bytes[0],
            "inner gutter sits to the right of the outer"
        );
    }
}
