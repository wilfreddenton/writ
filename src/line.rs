use std::borrow::Cow;
use std::ops::Range;
use std::path::PathBuf;

use gpui::{
    App, Font, FontStyle, FontWeight, Global, Hsla, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, Pixels, Point, RenderOnce, Rgba, SharedString, StyledText, TextRun, Window,
    black, canvas, div, img, point, prelude::*, px, rems,
};
use ropey::Rope;

use crate::editor::{DispatchEditorAction, EditorAction};
use crate::highlight::HighlightSpan;
use crate::inline::StyledRegion;
use crate::marker::{LineMarkers, MarkerKind};

/// Global state for cursor screen position and content bounds, updated during Line paint.
#[derive(Clone, Default)]
pub struct CursorScreenPosition {
    /// Screen position of the cursor (absolute window coordinates).
    pub position: Option<Point<Pixels>>,
    /// Right edge of the line content area (for popup clamping).
    pub content_right_edge: Option<Pixels>,
}

impl Global for CursorScreenPosition {}

/// Global state for hovered GitHub ref screen position, updated during Line paint.
#[derive(Clone, Default, PartialEq)]
pub struct HoveredRefScreenPosition {
    /// Screen position of the hovered ref (absolute window coordinates).
    pub position: Option<Point<Pixels>>,
    /// Byte range of the hovered ref (used to match against Editor's tracked range).
    pub byte_range: Option<Range<usize>>,
}

impl Global for HoveredRefScreenPosition {}

/// (opening_start, opening_end, closing_start, closing_end) for collapsed markdown syntax.
pub type HiddenRegion = (usize, usize, usize, usize);

/// A display_text region that is currently shown in collapsed form.
/// Used for click position mapping when the user clicks on shortened text.
#[derive(Clone)]
pub struct CollapsedDisplayText {
    /// Visual range in the display string (byte offsets)
    pub visual_range: Range<usize>,
    /// Buffer range of the full content
    pub buffer_range: Range<usize>,
    /// The shortened display text being shown
    pub display_text: String,
    /// The full buffer text (e.g., the full URL)
    pub buffer_text: String,
}

impl CollapsedDisplayText {
    /// Map a pixel x-offset within this collapsed region to a buffer offset.
    /// Uses text measurement to find the proportional position in the full text.
    pub fn map_x_to_buffer_offset(
        &self,
        x_offset: gpui::Pixels,
        font: &Font,
        font_size: gpui::Pixels,
        window: &Window,
    ) -> usize {
        let full_text: SharedString = self.buffer_text.clone().into();
        let run = TextRun {
            len: self.buffer_text.len(),
            font: font.clone(),
            color: black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window
            .text_system()
            .shape_line(full_text, font_size, &[run], None);
        let index = shaped.index_for_x(x_offset.max(px(0.0))).unwrap_or(0);
        self.buffer_range.start + index
    }
}

/// Convert a visual position (index into display text, after prefix) to a buffer offset.
/// Convert a buffer position to a visual index, accounting for hidden regions.
/// This is the inverse of `visual_to_buffer_pos`.
pub fn buffer_to_visual_pos(
    buffer_pos: usize,
    content_range: &Range<usize>,
    heading_marker_len: usize,
    hidden_regions: &[HiddenRegion],
) -> usize {
    if content_range.start >= content_range.end || buffer_pos <= content_range.start {
        return 0;
    }

    let start = content_range.start + heading_marker_len;
    let end = buffer_pos.min(content_range.end);

    if hidden_regions.is_empty() {
        return end.saturating_sub(start);
    }

    // Count visible characters from start to buffer_pos
    let mut visible_count = 0usize;
    for pos in start..end {
        let is_hidden = hidden_regions.iter().any(
            |&(opening_start, opening_end, closing_start, closing_end)| {
                (pos >= opening_start && pos < opening_end)
                    || (pos >= closing_start && pos < closing_end)
            },
        );
        if !is_hidden {
            visible_count += 1;
        }
    }

    visible_count
}

pub fn visual_to_buffer_pos(
    visual_index: usize,
    content_range: &Range<usize>,
    heading_marker_len: usize,
    hidden_regions: &[HiddenRegion],
    line_end: usize,
) -> usize {
    if content_range.start >= content_range.end {
        return content_range.start;
    }

    if hidden_regions.is_empty() {
        return (content_range.start + heading_marker_len + visual_index).min(line_end);
    }

    let mut buffer_pos = content_range.start + heading_marker_len;
    let mut visible_count = 0usize;

    while buffer_pos < content_range.end && visible_count < visual_index {
        let is_hidden = hidden_regions.iter().any(
            |&(opening_start, opening_end, closing_start, closing_end)| {
                (buffer_pos >= opening_start && buffer_pos < opening_end)
                    || (buffer_pos >= closing_start && buffer_pos < closing_end)
            },
        );

        if !is_hidden {
            visible_count += 1;
        }
        buffer_pos += 1;
    }

    buffer_pos.min(line_end)
}

#[derive(Clone)]
pub struct LineTheme {
    pub text_color: Rgba,
    pub cursor_color: Rgba,
    pub link_color: Rgba,
    pub selection_color: Rgba,
    pub border_color: Rgba,
    pub code_color: Rgba,
    pub fence_color: Rgba,
    pub fence_lang_color: Rgba,
    pub checkbox_unchecked_color: Rgba,
    pub checkbox_checked_color: Rgba,
    pub text_font: Font,
    pub code_font: Font,
    /// Width of a single monospace character in the code font.
    /// Used for precise indentation of nested blocks.
    pub monospace_char_width: gpui::Pixels,
    /// Line height for text lines.
    pub line_height: gpui::Rems,
}

#[derive(IntoElement)]
pub struct Line {
    line: LineMarkers,
    rope: Rope,
    cursor_offset: usize,
    inline_styles: Vec<StyledRegion>,
    theme: LineTheme,
    selection_range: Option<Range<usize>>,
    code_highlights: Vec<(HighlightSpan, Rgba)>,
    base_path: Option<PathBuf>,
    substitution: Option<String>,
    /// When set, truncate text with ellipsis at this pixel width.
    truncate_width: Option<Pixels>,
    /// Optional prefix text and runs to prepend before the line content.
    prefix: Option<(String, Vec<TextRun>)>,
    /// Byte ranges of GitHub refs on this line (for hover detection).
    github_ref_ranges: Vec<Range<usize>>,
    /// The currently hovered GitHub ref range (from Editor), if on this line.
    hovered_ref_range: Option<Range<usize>>,
    /// When true, don't dispatch click actions (read-only mode).
    input_blocked: bool,
    /// Maximum width for line content. None means fill container.
    max_line_width: Option<Pixels>,
    /// Optional background color for the entire line.
    line_background: Option<Rgba>,
    /// Byte ranges within the line to highlight more strongly (e.g., changed words).
    inline_highlight_ranges: Vec<Range<usize>>,
    /// Color for inline highlight ranges.
    inline_highlight_color: Option<Rgba>,
}

impl Line {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        line: LineMarkers,
        rope: Rope,
        cursor_offset: usize,
        inline_styles: Vec<StyledRegion>,
        theme: LineTheme,
        selection_range: Option<Range<usize>>,
        code_highlights: Vec<(HighlightSpan, Rgba)>,
        base_path: Option<PathBuf>,
        github_ref_ranges: Vec<Range<usize>>,
        hovered_ref_range: Option<Range<usize>>,
        input_blocked: bool,
        max_line_width: Option<Pixels>,
        line_background: Option<Rgba>,
        inline_highlight_ranges: Vec<Range<usize>>,
        inline_highlight_color: Option<Rgba>,
    ) -> Self {
        let substitution = {
            let s = line.substitution_rope(&rope);
            if s.is_empty() { None } else { Some(s) }
        };
        Self {
            line,
            rope,
            cursor_offset,
            inline_styles,
            theme,
            selection_range,
            code_highlights,
            base_path,
            substitution,
            truncate_width: None,
            prefix: None,
            github_ref_ranges,
            hovered_ref_range,
            input_blocked,
            max_line_width,
            line_background,
            inline_highlight_ranges,
            inline_highlight_color,
        }
    }

    /// Enable truncation with ellipsis at the given pixel width.
    pub fn truncate(mut self, width: Pixels) -> Self {
        self.truncate_width = Some(width);
        self
    }

    /// Add a prefix with styled text runs before the line content.
    pub fn with_prefix(mut self, text: String, runs: Vec<TextRun>) -> Self {
        self.prefix = Some((text, runs));
        self
    }

    /// Get a slice from the rope as a Cow<str>.
    fn slice(&self, range: Range<usize>) -> Cow<'_, str> {
        let start = self.rope.byte_to_char(range.start);
        let end = self.rope.byte_to_char(range.end);
        let slice = self.rope.slice(start..end);
        match slice.as_str() {
            Some(s) => Cow::Borrowed(s),
            None => Cow::Owned(slice.to_string()),
        }
    }

    fn cursor_on_line(&self) -> bool {
        let range = &self.line.range;
        if range.start == range.end {
            self.cursor_offset == range.start
        } else {
            self.cursor_offset >= range.start && self.cursor_offset <= range.end
        }
    }

    fn selection_on_line(&self) -> bool {
        if let Some(ref sel) = self.selection_range {
            let line_range = &self.line.range;
            sel.start < line_range.end && sel.end > line_range.start
        } else {
            false
        }
    }

    fn marker_in_selection(&self, marker_range: &Range<usize>) -> bool {
        if let Some(ref sel) = self.selection_range {
            sel.start < marker_range.end && sel.end > marker_range.start
        } else {
            false
        }
    }

    fn is_code_block_line(&self) -> bool {
        self.line.in_code_block || self.line.is_fence()
    }

    fn standalone_image_url(&self) -> Option<&str> {
        // Always show image, even when cursor is on line
        if !self.line.markers.is_empty() {
            return None;
        }

        if self.inline_styles.len() != 1 {
            return None;
        }

        let style = &self.inline_styles[0];
        if !style.is_image {
            return None;
        }

        let line_content = self.slice(self.line.range.clone());
        let line_content = line_content.trim_end();
        let image_text = self.slice(style.full_range.clone());
        if line_content != image_text.as_ref() {
            return None;
        }

        style.link_url.as_deref()
    }

    fn content_range(&self) -> Range<usize> {
        let range = &self.line.range;

        // Use marker_range() which excludes Checkbox markers.
        // The checkbox text "[ ] " will be part of the content and rendered
        // inline (not as a spacer), which gives correct wrap indent.
        if let Some(marker_range) = self.line.marker_range() {
            marker_range.end..range.end
        } else {
            range.clone()
        }
    }

    fn get_substitution(&self) -> Option<&str> {
        self.substitution.as_deref()
    }

    fn text_run(&self, len: usize, font: Font, color: Rgba) -> TextRun {
        TextRun {
            len,
            font,
            color: color.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }
    }

    /// Check if this line should be rendered as a heading.
    /// Returns the heading level only if the marker includes a space after the `#`.
    /// This prevents `#` alone from being rendered as a large heading while typing.
    fn display_heading_level(&self) -> Option<u8> {
        let level = self.line.heading_level()?;

        // Find the heading marker
        for m in &self.line.markers {
            if let MarkerKind::Heading(lvl) = m.kind {
                // The marker range includes "# " (hash + space) when valid.
                // For just "#" alone, the marker length equals the heading level (number of #).
                // For "# " or "## ", the marker length is level + 1 (includes the space).
                let marker_len = m.range.len();
                if marker_len > lvl as usize {
                    // Marker includes space after the #s
                    return Some(level);
                }
                // No space in marker - don't render as heading
                return None;
            }
        }
        None
    }

    fn line_font(&self) -> Font {
        if self.display_heading_level().is_some() {
            Font {
                weight: FontWeight::BOLD,
                ..self.theme.text_font.clone()
            }
        } else {
            self.theme.text_font.clone()
        }
    }

    fn get_highlight_color_for_range(&self, start: usize, end: usize) -> Option<Rgba> {
        for (span, color) in &self.code_highlights {
            if span.range.start <= start && end <= span.range.end {
                return Some(*color);
            }
        }
        None
    }

    /// Apply selection background color to text runs that overlap with the selection range.
    /// This modifies runs in-place, splitting them if needed to correctly highlight
    /// only the selected portion.
    fn apply_selection_to_runs(
        &self,
        runs: Vec<TextRun>,
        selection_range: Range<usize>,
    ) -> Vec<TextRun> {
        let selection_color: Hsla = self.theme.selection_color.into();
        self.apply_background_to_runs(runs, &[selection_range], selection_color)
    }

    /// Apply inline highlight to runs.
    /// The ranges are byte offsets within the line content.
    fn apply_inline_highlight_to_runs(
        &self,
        runs: Vec<TextRun>,
        display_text: &str,
        highlight_color: Rgba,
    ) -> Vec<TextRun> {
        let line_start = self.line.range.start;

        // Convert inline_highlight_ranges (relative to line start) to visual positions.
        // Use buffer_to_visual_pos to account for hidden regions (collapsed markers like **).
        let display_ranges: Vec<Range<usize>> = self
            .inline_highlight_ranges
            .iter()
            .filter_map(|range| {
                // The range is relative to line start in buffer coordinates
                let abs_start = line_start + range.start;
                let abs_end = line_start + range.end;

                // Convert buffer positions to visual positions
                let visual_start = self.buffer_to_visual_pos(abs_start, display_text);
                let visual_end = self.buffer_to_visual_pos(abs_end, display_text);

                if visual_start < visual_end {
                    Some(visual_start..visual_end)
                } else {
                    None
                }
            })
            .collect();

        self.apply_background_to_runs(runs, &display_ranges, highlight_color.into())
    }

    /// Apply background color to runs for the given ranges.
    fn apply_background_to_runs(
        &self,
        runs: Vec<TextRun>,
        ranges: &[Range<usize>],
        bg_color: Hsla,
    ) -> Vec<TextRun> {
        if ranges.is_empty() {
            return runs;
        }

        let mut result = Vec::new();
        let mut pos = 0;

        for run in runs {
            let run_start = pos;
            let run_end = pos + run.len;

            // Check if any range overlaps this run
            let overlapping: Vec<_> = ranges
                .iter()
                .filter(|r| r.start < run_end && r.end > run_start)
                .cloned()
                .collect();

            if overlapping.is_empty() {
                result.push(run);
            } else {
                // Split the run at range boundaries
                let mut current_pos = run_start;
                let mut remaining_len = run.len;

                while remaining_len > 0 {
                    // Find if we're inside a highlighted range
                    let in_highlight = overlapping
                        .iter()
                        .any(|r| current_pos >= r.start && current_pos < r.end);

                    // Find next boundary
                    let next_boundary = overlapping
                        .iter()
                        .filter_map(|r| {
                            if r.start > current_pos && r.start < current_pos + remaining_len {
                                Some(r.start)
                            } else if r.end > current_pos && r.end < current_pos + remaining_len {
                                Some(r.end)
                            } else {
                                None
                            }
                        })
                        .min()
                        .unwrap_or(current_pos + remaining_len);

                    let segment_len = next_boundary - current_pos;
                    if segment_len > 0 {
                        result.push(TextRun {
                            len: segment_len,
                            font: run.font.clone(),
                            color: run.color,
                            background_color: if in_highlight {
                                Some(bg_color)
                            } else {
                                run.background_color
                            },
                            underline: run.underline,
                            strikethrough: run.strikethrough,
                        });
                        remaining_len -= segment_len;
                        current_pos = next_boundary;
                    } else {
                        break;
                    }
                }
            }

            pos = run_end;
        }

        result
    }

    fn build_styled_content(&self) -> (String, Vec<TextRun>, Vec<CollapsedDisplayText>) {
        let content_range = if self.line.heading_level().is_some() {
            self.line.range.clone()
        } else {
            self.content_range()
        };

        let mut display_text = String::new();
        let mut runs: Vec<TextRun> = Vec::new();
        let mut collapsed_regions: Vec<CollapsedDisplayText> = Vec::new();

        if let Some(prefix) = self.get_substitution()
            && !prefix.is_empty()
        {
            if self.line.checkbox().is_some() {
                if let Some(checkbox_start) = prefix.find('[') {
                    let checkbox_end = prefix.find(']').map(|i| i + 2).unwrap_or(prefix.len());

                    if checkbox_start > 0 {
                        let before = &prefix[..checkbox_start];
                        display_text.push_str(before);
                        runs.push(self.text_run(
                            before.len(),
                            self.theme.code_font.clone(),
                            self.theme.text_color,
                        ));
                    }

                    let checkbox = &prefix[checkbox_start..checkbox_end.min(prefix.len())];
                    display_text.push_str(checkbox);
                    runs.push(self.text_run(
                        checkbox.len(),
                        self.theme.code_font.clone(),
                        self.theme.link_color,
                    ));

                    if checkbox_end < prefix.len() {
                        let after = &prefix[checkbox_end..];
                        display_text.push_str(after);
                        runs.push(self.text_run(
                            after.len(),
                            self.theme.code_font.clone(),
                            self.theme.text_color,
                        ));
                    }
                } else {
                    display_text.push_str(prefix);
                    runs.push(self.text_run(
                        prefix.len(),
                        self.theme.code_font.clone(),
                        self.theme.text_color,
                    ));
                }
            } else {
                display_text.push_str(prefix);
                runs.push(self.text_run(
                    prefix.len(),
                    self.theme.code_font.clone(),
                    self.theme.text_color,
                ));
            }
        }

        if self.line.is_fence() {
            // Use content after prefix markers (Indent, BlockQuote) but include the fence itself
            let fence_start = self
                .line
                .prefix_marker_range()
                .map(|r| r.end)
                .unwrap_or(self.line.range.start);
            let fence_text = self.slice(fence_start..self.line.range.end);
            // Fence can be ``` or ~~~
            let fence_char = fence_text.chars().next().unwrap_or('`');
            let fence_markers: String = fence_text
                .chars()
                .take_while(|&c| c == fence_char && (c == '`' || c == '~'))
                .collect();
            let language = &fence_text[fence_markers.len()..];

            if !fence_markers.is_empty() {
                display_text.push_str(&fence_markers);
                runs.push(self.text_run(
                    fence_markers.len(),
                    self.theme.code_font.clone(),
                    self.theme.fence_color,
                ));
            }

            if !language.is_empty() {
                display_text.push_str(language);
                runs.push(self.text_run(
                    language.len(),
                    self.theme.code_font.clone(),
                    self.theme.fence_lang_color,
                ));
            }

            return (display_text, runs, collapsed_regions);
        }

        if self.line.is_thematic_break() {
            let is_visible = self.cursor_on_line() || self.selection_on_line();
            let break_text = self.slice(self.line.range.clone());

            let transparent = Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            };
            let color = if is_visible {
                self.theme.text_color
            } else {
                transparent
            };

            display_text.push_str(&break_text);
            runs.push(self.text_run(break_text.len(), self.theme.text_font.clone(), color));

            return (display_text, runs, collapsed_regions);
        }

        if content_range.start >= content_range.end {
            return (display_text, runs, collapsed_regions);
        }

        let mut boundaries: Vec<usize> = vec![content_range.start, content_range.end];

        if self.line.heading_level().is_some()
            && let Some(marker_range) = self.line.marker_range()
        {
            boundaries.push(marker_range.start);
            boundaries.push(marker_range.end);
        }

        for region in &self.inline_styles {
            // Add boundaries for checkbox regions (from synthetic StyledRegion)
            if region.checkbox.is_some() {
                boundaries.push(region.full_range.start.max(content_range.start));
                boundaries.push(region.full_range.end.min(content_range.end));
            }
            if region.full_range.end > content_range.start
                && region.full_range.start < content_range.end
            {
                let cursor_inside = self.cursor_offset >= region.full_range.start
                    && self.cursor_offset <= region.full_range.end;

                if cursor_inside {
                    boundaries.push(region.full_range.start.max(content_range.start));
                    boundaries.push(region.full_range.end.min(content_range.end));
                } else {
                    boundaries.push(region.full_range.start.max(content_range.start));
                    boundaries.push(region.content_range.start.max(content_range.start));
                    boundaries.push(region.content_range.end.min(content_range.end));
                    boundaries.push(region.full_range.end.min(content_range.end));
                }
            }
        }

        for (span, _) in &self.code_highlights {
            if span.range.end > content_range.start && span.range.start < content_range.end {
                boundaries.push(span.range.start.max(content_range.start));
                boundaries.push(span.range.end.min(content_range.end));
            }
        }

        boundaries.retain(|&b| b >= content_range.start && b <= content_range.end);
        boundaries.sort();
        boundaries.dedup();

        let mut hidden_ranges: Vec<(usize, usize)> = Vec::new();
        let mut style_ranges: Vec<(Range<usize>, &StyledRegion)> = Vec::new();

        if self.line.heading_level().is_some()
            && !self.cursor_on_line()
            && !self.selection_on_line()
            && let Some(marker_range) = self.line.marker_range()
        {
            hidden_ranges.push((marker_range.start, marker_range.end));
        }

        let is_code_block = self.is_code_block_line();
        let base_code_font = &self.theme.code_font;
        let base_text_font = &self.theme.text_font;

        let ordered_marker_range = self
            .line
            .markers
            .iter()
            .find(|m| matches!(m.kind, MarkerKind::ListItem { ordered: true, .. }))
            .map(|m| m.range.clone());

        for region in &self.inline_styles {
            let cursor_inside = self.cursor_offset >= region.full_range.start
                && self.cursor_offset <= region.full_range.end;

            if !cursor_inside {
                let opening_start = region.full_range.start.max(content_range.start);
                let opening_end = region.content_range.start.min(content_range.end);
                if opening_end > opening_start {
                    hidden_ranges.push((opening_start, opening_end));
                }

                let closing_start = region.content_range.end.max(content_range.start);
                let closing_end = region.full_range.end.min(content_range.end);
                if closing_end > closing_start {
                    hidden_ranges.push((closing_start, closing_end));
                }
            }

            let style_range = if cursor_inside {
                region.full_range.clone()
            } else {
                region.content_range.clone()
            };
            style_ranges.push((style_range, region));
        }

        for window in boundaries.windows(2) {
            let start = window[0];
            let end = window[1];

            if start >= end {
                continue;
            }

            let is_hidden = hidden_ranges
                .iter()
                .any(|&(h_start, h_end)| start >= h_start && end <= h_end);

            if is_hidden {
                continue;
            }

            // Check if this span exactly matches a display_text region
            // (naked URLs create a single boundary window matching their full_range)
            // Only show display_text when cursor is NOT inside the region;
            // when cursor is inside, expand to show the full buffer content.
            let display_text_region = self.inline_styles.iter().find(|region| {
                region.display_text.is_some()
                    && region.full_range.start == start
                    && region.full_range.end == end
                    && !(self.cursor_offset >= region.full_range.start
                        && self.cursor_offset <= region.full_range.end)
            });

            if let Some(region) = display_text_region {
                // Emit the display_text instead of the buffer content
                let substitution = region.display_text.as_ref().unwrap();
                let visual_start = display_text.len();
                display_text.push_str(substitution);
                let visual_end = display_text.len();

                // Track this collapsed region for click position mapping
                let buffer_text = self.slice(region.full_range.start..region.full_range.end);
                collapsed_regions.push(CollapsedDisplayText {
                    visual_range: visual_start..visual_end,
                    buffer_range: region.full_range.clone(),
                    display_text: substitution.clone(),
                    buffer_text: buffer_text.to_string(),
                });

                // Style it as a link
                runs.push(TextRun {
                    len: substitution.len(),
                    font: self.theme.text_font.clone(),
                    color: self.theme.link_color.into(),
                    background_color: None,
                    underline: Some(gpui::UnderlineStyle {
                        thickness: px(1.0),
                        color: Some(self.theme.link_color.into()),
                        wavy: false,
                    }),
                    strikethrough: None,
                });
                continue;
            }

            let span_text = self.slice(start..end);
            let span_len = span_text.len();
            display_text.push_str(&span_text);

            let mut is_bold = false;
            let mut is_italic = false;
            let mut is_code = false;
            let mut is_strikethrough = false;
            let mut is_link = false;
            let mut checkbox_state: Option<bool> = None; // None = not checkbox, Some(false) = unchecked, Some(true) = checked

            for (style_range, region) in &style_ranges {
                if style_range.start <= start && end <= style_range.end {
                    is_bold = is_bold || region.style.bold;
                    is_italic = is_italic || region.style.italic;
                    is_code = is_code || region.style.code;
                    is_strikethrough = is_strikethrough || region.style.strikethrough;
                    is_link = is_link || region.link_url.is_some();
                    if checkbox_state.is_none() && region.checkbox.is_some() {
                        checkbox_state = region.checkbox;
                    }

                    if is_bold
                        && is_italic
                        && is_code
                        && is_strikethrough
                        && is_link
                        && checkbox_state.is_some()
                    {
                        break;
                    }
                }
            }
            let is_checkbox = checkbox_state.is_some();

            let in_ordered_marker = ordered_marker_range
                .as_ref()
                .is_some_and(|r| start < r.end && end > r.start);

            let base_font = if is_code || is_code_block || in_ordered_marker || is_checkbox {
                base_code_font
            } else {
                base_text_font
            };

            let font = Font {
                weight: if is_bold || self.display_heading_level().is_some() {
                    FontWeight::BOLD
                } else {
                    base_font.weight
                },
                style: if is_italic {
                    FontStyle::Italic
                } else {
                    base_font.style
                },
                ..base_font.clone()
            };

            let color: Hsla = if is_strikethrough {
                self.theme.border_color.into()
            } else if let Some(checked) = checkbox_state {
                if checked {
                    self.theme.checkbox_checked_color.into()
                } else {
                    self.theme.checkbox_unchecked_color.into()
                }
            } else if is_link {
                self.theme.link_color.into()
            } else if let Some(highlight_color) = self.get_highlight_color_for_range(start, end) {
                highlight_color.into()
            } else if is_code && !is_code_block {
                self.theme.code_color.into()
            } else {
                self.theme.text_color.into()
            };

            let underline = if is_link {
                Some(gpui::UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(self.theme.link_color.into()),
                    wavy: false,
                })
            } else {
                None
            };

            let strikethrough = if is_strikethrough {
                Some(gpui::StrikethroughStyle {
                    thickness: px(1.0),
                    color: Some(self.theme.border_color.into()),
                })
            } else {
                None
            };

            runs.push(TextRun {
                len: span_len,
                font,
                color,
                background_color: None,
                underline,
                strikethrough,
            });
        }

        (display_text, runs, collapsed_regions)
    }

    fn resolve_image_source(&self, url: &str) -> gpui::ImageSource {
        if url.starts_with("http://") || url.starts_with("https://") {
            return gpui::ImageSource::from(url.to_string());
        }

        let path = std::path::Path::new(url);
        let resolved_path = if path.is_absolute() {
            PathBuf::from(url)
        } else if let Some(base) = &self.base_path {
            base.join(url)
        } else {
            PathBuf::from(url)
        };

        gpui::ImageSource::from(resolved_path)
    }

    fn hidden_bytes_before(&self, offset: usize, content_range: &Range<usize>) -> usize {
        let mut hidden = 0usize;

        // For headings, the marker is hidden when cursor and selection are not on line
        if self.line.heading_level().is_some()
            && !self.cursor_on_line()
            && !self.selection_on_line()
            && let Some(marker_range) = self.line.marker_range()
            && offset > marker_range.end
        {
            hidden += marker_range.end - marker_range.start;
        }

        for region in &self.inline_styles {
            let cursor_inside = self.cursor_offset >= region.full_range.start
                && self.cursor_offset <= region.full_range.end;
            if cursor_inside {
                continue;
            }

            let opening_start = region.full_range.start.max(content_range.start);
            let opening_end = region.content_range.start.min(content_range.end);
            if opening_end > opening_start && offset > opening_end {
                hidden += opening_end - opening_start;
            }

            let closing_start = region.content_range.end.max(content_range.start);
            let closing_end = region.full_range.end.min(content_range.end);
            if closing_end > closing_start && offset > closing_end {
                hidden += closing_end - closing_start;
            }
        }
        hidden
    }

    /// Returns true if the cursor is in the marker area (before content starts)
    fn cursor_in_marker_area(&self) -> bool {
        if !self.cursor_on_line() {
            return false;
        }
        if self.line.is_fence()
            || self.line.is_thematic_break()
            || self.line.heading_level().is_some()
        {
            return false;
        }
        let content_range = self.content_range();
        self.cursor_offset < content_range.start
    }

    fn compute_visual_cursor_pos(&self, display_text: &str) -> Option<usize> {
        if !self.cursor_on_line() {
            return None;
        }
        if self.cursor_in_marker_area() {
            return None;
        }
        Some(self.buffer_to_visual_pos(self.cursor_offset, display_text))
    }

    fn buffer_to_visual_pos(&self, buffer_offset: usize, display_text: &str) -> usize {
        let content_range = if self.line.is_fence() {
            // Fence content starts after prefix markers (Indent, BlockQuote)
            let start = self
                .line
                .prefix_marker_range()
                .map(|r| r.end)
                .unwrap_or(self.line.range.start);
            start..self.line.range.end
        } else if self.line.is_thematic_break()
            || (self.line.heading_level().is_some()
                && (self.cursor_on_line() || self.selection_on_line()))
        {
            self.line.range.clone()
        } else {
            self.content_range()
        };

        if content_range.start >= content_range.end {
            return 0;
        }

        let clamped_offset = buffer_offset.min(content_range.end);

        let hidden = self.hidden_bytes_before(clamped_offset, &content_range);
        let buffer_pos_in_content = clamped_offset.saturating_sub(content_range.start);
        let visual_pos = buffer_pos_in_content.saturating_sub(hidden);

        visual_pos.min(display_text.len())
    }

    fn compute_visual_selection_range(&self, display_text: &str) -> Option<Range<usize>> {
        let selection = self.selection_range.as_ref()?;
        let line_range = &self.line.range;

        if selection.end <= line_range.start || selection.start >= line_range.end {
            return None;
        }

        let sel_start = selection.start.max(line_range.start);
        let sel_end = selection.end.min(line_range.end);
        let visual_start = self.buffer_to_visual_pos(sel_start, display_text);
        let visual_end = self.buffer_to_visual_pos(sel_end, display_text);

        if visual_start < visual_end {
            Some(visual_start..visual_end)
        } else {
            None
        }
    }

    fn render_cursor(&self, cursor_pos: usize, text_layout: gpui::TextLayout) -> impl IntoElement {
        let cursor_color = self.theme.cursor_color;

        canvas(
            move |_bounds, _window, _cx| {
                // For cursor positioning, we want the position at the START of the character
                // at cursor_pos. At wrap boundaries, position_for_index(n) returns end-of-row
                // position. Check if next char exists and is on a different row - if so, use
                // the y from next char's position with x=0 (start of that row).
                if cursor_pos < text_layout.len()
                    && let Some(next_pos) = text_layout.position_for_index(cursor_pos + 1)
                    && let Some(curr_pos) = text_layout.position_for_index(cursor_pos)
                {
                    let line_height = text_layout.line_height();
                    let curr_row = (curr_pos.y / line_height).floor();
                    let next_row = (next_pos.y / line_height).floor();
                    // At wrap boundary: curr is at end of row, next is at start of new row
                    if next_row > curr_row {
                        // The cursor should appear at the start of the wrapped row.
                        // Use x from first char (index 0) to get the row start position.
                        let row_start_x = text_layout
                            .position_for_index(0)
                            .map(|p| p.x)
                            .unwrap_or(px(0.0));
                        return Some(point(row_start_x, next_pos.y));
                    }
                }
                text_layout.position_for_index(cursor_pos)
            },
            move |bounds, cursor_pos_result, window: &mut Window, cx| {
                let pos =
                    cursor_pos_result.unwrap_or_else(|| point(bounds.origin.x, bounds.origin.y));

                // Store cursor position and content bounds for autocomplete popup positioning
                // pos from position_for_index appears to already be in absolute coords
                cx.set_global(CursorScreenPosition {
                    position: Some(pos),
                    content_right_edge: Some(bounds.origin.x + bounds.size.width),
                });

                let text_style = window.text_style();
                let font_size = text_style.font_size.to_pixels(window.rem_size());
                let line_height = text_style
                    .line_height
                    .to_pixels(font_size.into(), window.rem_size());

                let cursor_char: SharedString = "\u{258F}".into();
                let cursor_font_size = font_size * 1.4;
                let cursor_run = TextRun {
                    len: cursor_char.len(),
                    font: text_style.font(),
                    color: cursor_color.into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };

                let shaped_cursor = window.text_system().shape_line(
                    cursor_char,
                    cursor_font_size,
                    &[cursor_run],
                    None,
                );

                let cursor_height = cursor_font_size * 1.2;
                let y_offset = (line_height - cursor_height) / 2.0;
                let cursor_pos = point(pos.x, pos.y + y_offset);
                let _ = shaped_cursor.paint(cursor_pos, cursor_height, gpui::TextAlign::Left, None, window, cx);
            },
        )
        .absolute()
        .size_full()
    }

    /// Render an invisible element that sets the HoveredRefScreenPosition global during paint.
    fn render_hovered_ref_position_tracker(
        &self,
        ref_visual_start: usize,
        ref_byte_range: Range<usize>,
        text_layout: gpui::TextLayout,
    ) -> impl IntoElement {
        canvas(
            move |_bounds, _window, _cx| text_layout.position_for_index(ref_visual_start),
            move |bounds, pos_result, _window: &mut Window, cx| {
                let pos = pos_result.unwrap_or_else(|| point(bounds.origin.x, bounds.origin.y));
                cx.set_global(HoveredRefScreenPosition {
                    position: Some(pos),
                    byte_range: Some(ref_byte_range.clone()),
                });
            },
        )
        .absolute()
        .size_full()
    }

    fn render_spacer_cursor(&self, char_offset: usize) -> impl IntoElement {
        let cursor_color = self.theme.cursor_color;
        let cursor_font = self.theme.text_font.clone();
        let char_width = self.theme.monospace_char_width;
        let x_pos = char_width * char_offset as f32;

        canvas(
            move |_bounds, _window, _cx| (),
            move |bounds, _, window: &mut Window, cx| {
                let text_style = window.text_style();
                let font_size = text_style.font_size.to_pixels(window.rem_size());
                let line_height = text_style
                    .line_height
                    .to_pixels(font_size.into(), window.rem_size());

                let cursor_char: SharedString = "\u{258F}".into();
                let cursor_font_size = font_size * 1.4;
                let cursor_run = TextRun {
                    len: cursor_char.len(),
                    font: cursor_font.clone(),
                    color: cursor_color.into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };

                let shaped_cursor = window.text_system().shape_line(
                    cursor_char,
                    cursor_font_size,
                    &[cursor_run],
                    None,
                );

                let cursor_height = cursor_font_size * 1.2;
                let y_offset = (line_height - cursor_height) / 2.0;
                let cursor_pos = point(bounds.origin.x + x_pos, bounds.origin.y + y_offset);
                let _ = shaped_cursor.paint(cursor_pos, cursor_height, gpui::TextAlign::Left, None, window, cx);
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .w(char_width * (char_offset as f32 + 2.0))
        .h(self.theme.line_height)
    }

    /// Attach the shared click/drag mouse handlers to a marker spacer element:
    /// Click on mouse-down, Drag on a left-button drag, both targeting
    /// `marker_start` and both gated on `input_blocked`. Kept in one place so
    /// the blockquote / indent / list spacers can't drift apart (they did).
    fn attach_spacer_handlers(&self, spacer: gpui::Div, marker_start: usize) -> gpui::Div {
        let input_blocked = self.input_blocked;
        spacer
            .on_mouse_down(
                MouseButton::Left,
                move |event: &MouseDownEvent, window, cx| {
                    if input_blocked {
                        return;
                    }
                    cx.stop_propagation();
                    window.dispatch_action(
                        Box::new(DispatchEditorAction(EditorAction::Click {
                            offset: marker_start,
                            shift: event.modifiers.shift,
                            click_count: event.click_count,
                        })),
                        cx,
                    );
                },
            )
            .on_mouse_move(move |event: &MouseMoveEvent, window, cx| {
                if input_blocked {
                    return;
                }
                if event.pressed_button == Some(MouseButton::Left) {
                    cx.stop_propagation();
                    window.dispatch_action(
                        Box::new(DispatchEditorAction(EditorAction::Drag {
                            offset: marker_start,
                        })),
                        cx,
                    );
                }
            })
    }
}

fn line_base(line_number: usize, max_width: Option<Pixels>) -> gpui::Stateful<gpui::Div> {
    let d = div().id(("line", line_number)).w_full();
    match max_width {
        Some(w) => d.max_w(w).mx_auto(),
        None => d,
    }
}

impl RenderOnce for Line {
    fn render(self, window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let line_number = self.line.line_number;
        let line_range = self.line.range.clone();
        let input_blocked = self.input_blocked;
        let max_line_width = self.max_line_width;

        let standalone_image = self.standalone_image_url().map(|url| {
            let source = self.resolve_image_source(url);
            let line_end = line_range.end;
            let open_url = if url.starts_with("http://") || url.starts_with("https://") {
                url.to_string()
            } else {
                let path = std::path::Path::new(url);
                if path.is_absolute() {
                    url.to_string()
                } else if let Some(base) = &self.base_path {
                    base.join(url).to_string_lossy().to_string()
                } else {
                    url.to_string()
                }
            };
            (source, line_end, open_url)
        });

        if let Some((source, line_end, open_url)) = standalone_image.clone()
            && !self.cursor_on_line()
            && !self.selection_on_line()
        {
            return line_base(line_number, max_line_width).child(
                img(source).max_w_full().on_mouse_down(
                    MouseButton::Left,
                    move |event: &MouseDownEvent, window, cx| {
                        if input_blocked {
                            return;
                        }
                        if event.modifiers.control || event.modifiers.platform {
                            window.dispatch_action(
                                Box::new(DispatchEditorAction(EditorAction::OpenLink {
                                    url: open_url.clone(),
                                })),
                                cx,
                            );
                            return;
                        }
                        window.dispatch_action(
                            Box::new(DispatchEditorAction(EditorAction::Click {
                                offset: line_end,
                                shift: event.modifiers.shift,
                                click_count: event.click_count,
                            })),
                            cx,
                        );
                    },
                ),
            );
        }

        let (display_text, runs, collapsed_regions) = self.build_styled_content();

        // Prepend prefix if set
        let (display_text, mut runs) = if let Some((prefix_text, prefix_runs)) = &self.prefix {
            let mut new_text = prefix_text.clone();
            new_text.push_str(&display_text);
            let mut new_runs = prefix_runs.clone();
            new_runs.extend(runs);
            (new_text, new_runs)
        } else {
            (display_text, runs)
        };

        let display_text = if display_text.is_empty() {
            runs.push(self.text_run(1, self.line_font(), self.theme.text_color));
            " ".to_string()
        } else {
            display_text
        };

        let visual_cursor_pos = self.compute_visual_cursor_pos(&display_text);

        let visual_selection = self.compute_visual_selection_range(&display_text);

        let mut runs = if let Some(ref sel_range) = visual_selection {
            self.apply_selection_to_runs(runs, sel_range.clone())
        } else {
            runs
        };

        // Apply inline highlighting (e.g., word-level diff changes)
        if !self.inline_highlight_ranges.is_empty()
            && let Some(color) = self.inline_highlight_color
        {
            runs = self.apply_inline_highlight_to_runs(runs, &display_text, color);
        }

        // Truncate text with ellipsis if width is specified
        let shared_text: SharedString = if let Some(truncate_width) = self.truncate_width {
            let text_style = window.text_style();
            let font_size = text_style.font_size.to_pixels(window.rem_size());
            let mut line_wrapper = window
                .text_system()
                .line_wrapper(self.theme.text_font.clone(), font_size);
            let (truncated, truncated_runs) = line_wrapper.truncate_line(
                display_text.into(),
                truncate_width,
                "…",
                &runs,
                gpui::TruncateFrom::End,
            );
            runs = truncated_runs.into_owned();
            truncated
        } else {
            display_text.into()
        };
        let styled_text = StyledText::new(shared_text).with_runs(runs);
        let text_layout = styled_text.layout().clone();

        let mut line_div = line_base(line_number, max_line_width)
            .relative()
            .flex()
            .flex_row()
            .when_some(self.line_background, |d, bg| d.bg(bg));

        let mut spacers: Vec<gpui::Div> = Vec::new();
        let cursor_in_markers = self.cursor_in_marker_area();

        for marker in self.line.markers.iter().rev() {
            let cursor_in_this_marker = cursor_in_markers
                && self.cursor_offset >= marker.range.start
                && self.cursor_offset < marker.range.end;
            let cursor_char_offset = if cursor_in_this_marker {
                self.cursor_offset - marker.range.start
            } else {
                0
            };

            match &marker.kind {
                MarkerKind::Heading(level) => {
                    // Only apply heading styling if there's whitespace after the marker.
                    // This prevents `#` alone from rendering large while typing.
                    if self.display_heading_level().is_some() {
                        line_div = line_div.font_weight(FontWeight::BOLD);
                        line_div = match level {
                            1 => line_div.text_size(rems(2.0)),
                            2 => line_div.text_size(rems(1.75)),
                            3 => line_div.text_size(rems(1.5)),
                            4 => line_div.text_size(rems(1.25)),
                            5 => line_div.text_size(rems(1.1)),
                            _ => line_div,
                        };
                    }
                }
                MarkerKind::BlockQuote => {
                    let marker_chars = marker.range.len();
                    let spacer_width = self.theme.monospace_char_width * marker_chars as f32;
                    let border_color = if self.line.in_checked_task {
                        self.theme.selection_color
                    } else {
                        self.theme.border_color
                    };
                    let border_element = div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .w(px(2.0))
                        .bg(border_color);
                    let mut spacer = div()
                        .relative()
                        .w(spacer_width)
                        .min_h_full()
                        .child(border_element);
                    if self.marker_in_selection(&marker.range) {
                        spacer = spacer.bg(self.theme.selection_color);
                    }
                    if cursor_in_this_marker {
                        spacer = spacer.child(self.render_spacer_cursor(cursor_char_offset));
                    }
                    spacer = self.attach_spacer_handlers(spacer, marker.range.start);
                    spacers.push(spacer);
                }
                MarkerKind::CodeBlockFence { .. } => {}
                MarkerKind::ThematicBreak => {
                    if !self.cursor_on_line() && !self.selection_on_line() {
                        let line_text = self.slice(self.line.range.clone());
                        let invisible_run = TextRun {
                            len: line_text.len(),
                            font: self.theme.text_font.clone(),
                            color: gpui::transparent_black(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        let shared_text: SharedString = line_text.into_owned().into();
                        let styled_text =
                            StyledText::new(shared_text).with_runs(vec![invisible_run]);
                        let text_layout = styled_text.layout().clone();

                        let hr_line = div()
                            .absolute()
                            .top_1_2()
                            .left_0()
                            .right_0()
                            .h(px(1.0))
                            .bg(self.theme.border_color);

                        let mut hr_div = line_base(line_number, max_line_width)
                            .relative()
                            .child(styled_text)
                            .child(hr_line);

                        hr_div = hr_div.on_mouse_down(
                            MouseButton::Left,
                            move |event: &MouseDownEvent, window, cx| {
                                if input_blocked {
                                    return;
                                }
                                let visual_index =
                                    match text_layout.index_for_position(event.position) {
                                        Ok(idx) => idx,
                                        Err(idx) => idx,
                                    };
                                let buffer_offset = line_range.start + visual_index;
                                let buffer_offset = buffer_offset.min(line_range.end);
                                window.dispatch_action(
                                    Box::new(DispatchEditorAction(EditorAction::Click {
                                        offset: buffer_offset,
                                        shift: event.modifiers.shift,
                                        click_count: event.click_count,
                                    })),
                                    cx,
                                );
                            },
                        );

                        return hr_div;
                    }
                }
                MarkerKind::Indent => {
                    let indent_chars = marker.range.len();
                    let spacer_width = self.theme.monospace_char_width * indent_chars as f32;
                    let mut spacer = div().relative().w(spacer_width).min_h_full();
                    if self.marker_in_selection(&marker.range) {
                        spacer = spacer.bg(self.theme.selection_color);
                    }
                    if cursor_in_this_marker {
                        spacer = spacer.child(self.render_spacer_cursor(cursor_char_offset));
                    }
                    spacer = self.attach_spacer_handlers(spacer, marker.range.start);
                    spacers.push(spacer);
                }
                MarkerKind::ListItem {
                    ordered,
                    unordered_marker,
                    ..
                } => {
                    let marker_chars = marker.range.len();
                    let spacer_width = self.theme.monospace_char_width * marker_chars as f32;

                    let marker_text = if *ordered {
                        self.slice(marker.range.clone()).into_owned()
                    } else {
                        unordered_marker.map_or("• ", |m| m.bullet()).to_string()
                    };

                    let marker_color = if self.line.in_checked_task {
                        self.theme.selection_color
                    } else {
                        self.theme.text_color
                    };

                    let mut marker_label = div()
                        .relative()
                        .w(spacer_width)
                        .min_h_full()
                        .font_family(self.theme.code_font.family.clone())
                        .text_color(marker_color)
                        .child(marker_text);

                    if self.marker_in_selection(&marker.range) {
                        marker_label = marker_label.bg(self.theme.selection_color);
                    }
                    if cursor_in_this_marker {
                        marker_label =
                            marker_label.child(self.render_spacer_cursor(cursor_char_offset));
                    }

                    marker_label = self.attach_spacer_handlers(marker_label, marker.range.start);
                    spacers.push(marker_label);
                }
                MarkerKind::Checkbox { .. } => {
                    // Checkbox is rendered inline with the text, not as a spacer.
                    // The "[ ] " text is part of content_range and will be rendered
                    // in the styled text section. Click handling is done there.
                    // This gives correct wrap indent (only list marker width, not checkbox).
                }
            }
        }

        if self.is_code_block_line() {
            line_div = line_div.text_size(rems(0.9));
        }

        let mut text_container = div().relative().flex_1().min_w_0().child(styled_text);

        if let Some(cursor_pos) = visual_cursor_pos {
            text_container =
                text_container.child(self.render_cursor(cursor_pos, text_layout.clone()));
        }

        let content_range_for_handlers = if self.line.is_fence() {
            // Fence content starts after prefix markers (Indent, BlockQuote)
            let start = self
                .line
                .prefix_marker_range()
                .map(|r| r.end)
                .unwrap_or(self.line.range.start);
            start..self.line.range.end
        } else if self.line.is_thematic_break() || self.line.heading_level().is_some() {
            self.line.range.clone()
        } else {
            self.content_range()
        };

        let heading_marker_len = if self.line.heading_level().is_some()
            && !self.cursor_on_line()
            && !self.selection_on_line()
        {
            self.line.marker_range().map(|r| r.len()).unwrap_or(0)
        } else {
            0
        };

        let cursor_offset = self.cursor_offset;
        let hidden_regions: Vec<(usize, usize, usize, usize)> = self
            .inline_styles
            .iter()
            .filter_map(|region| {
                let cursor_inside = cursor_offset >= region.full_range.start
                    && cursor_offset <= region.full_range.end;
                if cursor_inside {
                    None
                } else {
                    let opening_start = region
                        .full_range
                        .start
                        .max(content_range_for_handlers.start);
                    let opening_end = region
                        .content_range
                        .start
                        .min(content_range_for_handlers.end);
                    let closing_start = region
                        .content_range
                        .end
                        .max(content_range_for_handlers.start);
                    let closing_end = region.full_range.end.min(content_range_for_handlers.end);
                    Some((opening_start, opening_end, closing_start, closing_end))
                }
            })
            .collect();

        let prefix_len = self.get_substitution().map(|s| s.len()).unwrap_or(0);

        // Add hovered ref position tracker if this line contains the hovered ref
        if let Some(ref hovered_range) = self.hovered_ref_range {
            // Convert buffer position to visual index, accounting for hidden markdown syntax.
            let ref_visual_start = buffer_to_visual_pos(
                hovered_range.start,
                &content_range_for_handlers,
                heading_marker_len,
                &hidden_regions,
            ) + prefix_len;
            text_container = text_container.child(self.render_hovered_ref_position_tracker(
                ref_visual_start,
                hovered_range.clone(),
                text_layout.clone(),
            ));
        }

        if !input_blocked {
            let layout_for_click = text_layout.clone();
            let content_range = content_range_for_handlers.clone();
            let line_number = self.line.line_number;
            let hidden_regions_for_click = hidden_regions.clone();
            let hidden_regions_for_move = hidden_regions;
            let collapsed_regions_for_click = collapsed_regions.clone();
            let text_font_for_click = self.theme.text_font.clone();

            // Find the checkbox click range in the content.
            // The checkbox "[ ]" or "[x]" is now rendered as part of content, not substitution.
            let checkbox_click_range: Option<std::ops::Range<usize>> =
                if self.line.checkbox().is_some() {
                    // The checkbox is at the start of the content (after list marker spacer)
                    // It's 4 chars: "[ ] " or "[x] "
                    Some(prefix_len..prefix_len + 4)
                } else {
                    None
                };

            let link_regions: Vec<_> = self
                .inline_styles
                .iter()
                .filter_map(|region| {
                    region
                        .link_url
                        .as_ref()
                        .map(|url| (region.content_range.clone(), url.clone()))
                })
                .collect();

            text_container = text_container.on_mouse_down(
                MouseButton::Left,
                move |event: &MouseDownEvent, window, cx| {
                    let visual_index = match layout_for_click.index_for_position(event.position) {
                        Ok(idx) => idx,
                        Err(idx) => idx,
                    };

                    if let Some(ref range) = checkbox_click_range
                        && visual_index >= range.start
                        && visual_index < range.end
                    {
                        window.dispatch_action(
                            Box::new(DispatchEditorAction(EditorAction::ToggleCheckbox {
                                line_number,
                            })),
                            cx,
                        );
                        return;
                    }

                    let content_visual_index = visual_index.saturating_sub(prefix_len);

                    // Check if click is within a collapsed display_text region
                    // If so, use text measurement to map the click position proportionally
                    let collapsed_region = collapsed_regions_for_click.iter().find(|r| {
                        content_visual_index >= r.visual_range.start
                            && content_visual_index < r.visual_range.end
                    });

                    let buffer_offset = if let Some(region) = collapsed_region {
                        // Get pixel position relative to start of the collapsed text
                        if let Some(visual_start_pos) = layout_for_click
                            .position_for_index(prefix_len + region.visual_range.start)
                        {
                            let x_offset = event.position.x - visual_start_pos.x;
                            let font_size = window.rem_size(); // Use rem as base font size
                            region.map_x_to_buffer_offset(
                                x_offset,
                                &text_font_for_click,
                                font_size,
                                window,
                            )
                        } else {
                            region.buffer_range.start
                        }
                    } else {
                        visual_to_buffer_pos(
                            content_visual_index,
                            &content_range,
                            heading_marker_len,
                            &hidden_regions_for_click,
                            line_range.end,
                        )
                    };

                    if event.modifiers.control || event.modifiers.platform {
                        for (range, url) in &link_regions {
                            if buffer_offset >= range.start && buffer_offset <= range.end {
                                window.dispatch_action(
                                    Box::new(DispatchEditorAction(EditorAction::OpenLink {
                                        url: url.clone(),
                                    })),
                                    cx,
                                );
                                return;
                            }
                        }
                    }

                    window.prevent_default();
                    window.dispatch_action(
                        Box::new(DispatchEditorAction(EditorAction::Click {
                            offset: buffer_offset,
                            shift: event.modifiers.shift,
                            click_count: event.click_count,
                        })),
                        cx,
                    );
                },
            );

            let layout_for_move = text_layout;
            let line_range_for_move = self.line.range.clone();
            let content_range = content_range_for_handlers;
            let collapsed_regions_for_move = collapsed_regions;
            let text_font_for_move = self.theme.text_font.clone();

            // Checkbox hover range - checkbox is now at start of content (after spacer)
            let checkbox_hover_range: Option<Range<usize>> = if self.line.checkbox().is_some() {
                // The checkbox is 4 chars: "[ ] " or "[x] "
                Some(prefix_len..prefix_len + 4)
            } else {
                None
            };

            let link_content_ranges: Vec<Range<usize>> = self
                .inline_styles
                .iter()
                .filter(|region| region.link_url.is_some())
                .map(|region| region.content_range.clone())
                .collect();

            let github_ref_ranges = self.github_ref_ranges.clone();

            text_container =
                text_container.on_mouse_move(move |event: &MouseMoveEvent, window, cx| {
                    if event.pressed_button == Some(MouseButton::Left) {
                        let visual_index = match layout_for_move.index_for_position(event.position)
                        {
                            Ok(idx) => idx,
                            Err(idx) => idx,
                        };

                        let content_visual_index = visual_index.saturating_sub(prefix_len);

                        // Check if drag is within a collapsed display_text region
                        let collapsed_region = collapsed_regions_for_move.iter().find(|r| {
                            content_visual_index >= r.visual_range.start
                                && content_visual_index < r.visual_range.end
                        });

                        let buffer_offset = if let Some(region) = collapsed_region {
                            if let Some(visual_start_pos) = layout_for_move
                                .position_for_index(prefix_len + region.visual_range.start)
                            {
                                let x_offset = event.position.x - visual_start_pos.x;
                                let font_size = window.rem_size();
                                region.map_x_to_buffer_offset(
                                    x_offset,
                                    &text_font_for_move,
                                    font_size,
                                    window,
                                )
                            } else {
                                region.buffer_range.start
                            }
                        } else {
                            visual_to_buffer_pos(
                                content_visual_index,
                                &content_range,
                                heading_marker_len,
                                &hidden_regions_for_move,
                                line_range_for_move.end,
                            )
                        };

                        window.dispatch_action(
                            Box::new(DispatchEditorAction(EditorAction::Drag {
                                offset: buffer_offset,
                            })),
                            cx,
                        );
                    }

                    let visual_index = match layout_for_move.index_for_position(event.position) {
                        Ok(idx) => idx,
                        Err(idx) => idx,
                    };

                    let hovering_checkbox = checkbox_hover_range.as_ref().is_some_and(|range| {
                        visual_index >= range.start && visual_index < range.end
                    });

                    let content_visual_index = visual_index.saturating_sub(prefix_len);
                    let buffer_offset = visual_to_buffer_pos(
                        content_visual_index,
                        &content_range,
                        heading_marker_len,
                        &hidden_regions_for_move,
                        line_range_for_move.end,
                    );
                    let hovering_link_region = link_content_ranges
                        .iter()
                        .any(|range| buffer_offset >= range.start && buffer_offset < range.end);

                    // Find if hovering over a GitHub ref
                    let hovered_github_ref_range = github_ref_ranges
                        .iter()
                        .find(|range| buffer_offset >= range.start && buffer_offset < range.end)
                        .cloned();

                    // Calculate the screen position of the ref's start (not mouse position)
                    let hovered_ref_position =
                        hovered_github_ref_range.as_ref().and_then(|range| {
                            // Convert ref's start byte offset to visual index
                            let ref_visual_index = buffer_to_visual_pos(
                                range.start,
                                &content_range,
                                heading_marker_len,
                                &hidden_regions_for_move,
                            );
                            // Add prefix length to get absolute visual index
                            let absolute_visual_index = prefix_len + ref_visual_index;
                            // Get screen position for that index
                            layout_for_move.position_for_index(absolute_visual_index)
                        });

                    window.dispatch_action(
                        Box::new(DispatchEditorAction(EditorAction::UpdateHover {
                            over_checkbox: hovering_checkbox,
                            over_link: hovering_link_region,
                            hovered_github_ref_range,
                            hovered_ref_position,
                        })),
                        cx,
                    );
                });
        }

        // Add spacers and text container to line_div
        for spacer in spacers {
            line_div = line_div.child(spacer);
        }
        line_div = line_div.child(text_container);

        if let Some((source, _, open_url)) = standalone_image {
            let mut container = div().id(line_number).w_full().flex().flex_col();
            if let Some(w) = max_line_width {
                container = container.max_w(w).mx_auto();
            }
            return container
                .child(line_div)
                .child(img(source).max_w_full().on_mouse_down(
                    MouseButton::Left,
                    move |event: &MouseDownEvent, _, _| {
                        if event.modifiers.control || event.modifiers.platform {
                            let _ = open::that(&open_url);
                        }
                    },
                ));
        }

        line_div
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_visual_to_buffer_pos_no_hidden() {
        // Simple case: no hidden regions
        let content_range = 10..30;
        let hidden: Vec<HiddenRegion> = vec![];

        // Visual index 0 -> buffer pos 10
        assert_eq!(visual_to_buffer_pos(0, &content_range, 0, &hidden, 30), 10);
        // Visual index 5 -> buffer pos 15
        assert_eq!(visual_to_buffer_pos(5, &content_range, 0, &hidden, 30), 15);
        // Visual index 20 -> buffer pos 30 (end)
        assert_eq!(visual_to_buffer_pos(20, &content_range, 0, &hidden, 30), 30);
    }

    #[test]
    fn test_visual_to_buffer_pos_with_hidden() {
        // Content: "**bold** text" at buffer 0..13
        // Hidden regions: 0..2 (opening **) and 6..8 (closing **)
        let content_range = 0..13;
        let hidden: Vec<HiddenRegion> = vec![(0, 2, 6, 8)];

        // Visual "bold text" = 9 chars, buffer has 13
        // Visual 0 -> cursor before first char -> buffer 0 (loop doesn't run)
        assert_eq!(visual_to_buffer_pos(0, &content_range, 0, &hidden, 13), 0);
        // Visual 1 -> after 'b' (first visible) -> counts 1 visible, stops at buffer 3
        assert_eq!(visual_to_buffer_pos(1, &content_range, 0, &hidden, 13), 3);
        // Visual 4 -> after "bold" -> buffer 6 (counts 4 visible: b,o,l,d at 2,3,4,5)
        assert_eq!(visual_to_buffer_pos(4, &content_range, 0, &hidden, 13), 6);
        // Visual 5 -> after space -> buffer 9 (skips hidden 6,7, counts space at 8)
        assert_eq!(visual_to_buffer_pos(5, &content_range, 0, &hidden, 13), 9);
    }

    #[test]
    fn test_buffer_to_visual_pos_no_hidden() {
        // Simple case: no hidden regions
        let content_range = 10..30;
        let hidden: Vec<HiddenRegion> = vec![];

        // Buffer pos 10 -> visual 0
        assert_eq!(buffer_to_visual_pos(10, &content_range, 0, &hidden), 0);
        // Buffer pos 15 -> visual 5
        assert_eq!(buffer_to_visual_pos(15, &content_range, 0, &hidden), 5);
        // Buffer pos 30 -> visual 20
        assert_eq!(buffer_to_visual_pos(30, &content_range, 0, &hidden), 20);
    }

    #[test]
    fn test_buffer_to_visual_pos_with_hidden() {
        // Content: "**bold** text" at buffer 0..13
        // Hidden regions: 0..2 (opening **) and 6..8 (closing **)
        let content_range = 0..13;
        let hidden: Vec<HiddenRegion> = vec![(0, 2, 6, 8)];

        // Buffer 0 -> visual 0 (at start, before any visible char)
        assert_eq!(buffer_to_visual_pos(0, &content_range, 0, &hidden), 0);
        // Buffer 2 -> visual 0 (first visible char 'b')
        assert_eq!(buffer_to_visual_pos(2, &content_range, 0, &hidden), 0);
        // Buffer 6 -> visual 4 (after "bold", before hidden **)
        assert_eq!(buffer_to_visual_pos(6, &content_range, 0, &hidden), 4);
        // Buffer 8 -> visual 4 (after hidden **, space char)
        assert_eq!(buffer_to_visual_pos(8, &content_range, 0, &hidden), 4);
        // Buffer 9 -> visual 5 (space counted)
        assert_eq!(buffer_to_visual_pos(9, &content_range, 0, &hidden), 5);
    }

    #[test]
    fn test_buffer_visual_roundtrip() {
        // Test that the functions are inverses (for visible positions)
        let content_range = 0..20;
        let hidden: Vec<HiddenRegion> = vec![(2, 4, 10, 12)]; // hide 2 ranges of 2 chars each

        // For each visual index, converting to buffer and back should give same visual
        for visual in 0..16 {
            // 20 - 4 hidden = 16 visible
            let buffer = visual_to_buffer_pos(visual, &content_range, 0, &hidden, 20);
            let back = buffer_to_visual_pos(buffer, &content_range, 0, &hidden);
            assert_eq!(
                back, visual,
                "roundtrip failed for visual {}: buffer={}, back={}",
                visual, buffer, back
            );
        }
    }

    #[test]
    fn test_collapsed_display_text_ranges() {
        // Simulate a GitHub URL at buffer positions 10..54 (44 chars)
        // displayed as "rust-lang/rust#123" (18 chars) at visual positions 5..23
        let collapsed = CollapsedDisplayText {
            visual_range: 5..23,
            buffer_range: 10..54,
            display_text: "rust-lang/rust#123".to_string(),
            buffer_text: "https://github.com/rust-lang/rust/issues/123".to_string(),
        };

        // Verify the display text is shorter than buffer text
        assert!(collapsed.display_text.len() < collapsed.buffer_text.len());

        // Verify ranges are consistent
        assert_eq!(
            collapsed.visual_range.len(),
            collapsed.display_text.len(),
            "visual_range length should match display_text length"
        );
        assert_eq!(
            collapsed.buffer_range.len(),
            collapsed.buffer_text.len(),
            "buffer_range length should match buffer_text length"
        );
    }

    #[test]
    fn test_collapsed_region_detection_logic() {
        // Test the logic used to find collapsed regions during click handling
        let collapsed_regions = [
            CollapsedDisplayText {
                visual_range: 0..18,
                buffer_range: 0..44,
                display_text: "rust-lang/rust#123".to_string(),
                buffer_text: "https://github.com/rust-lang/rust/issues/123".to_string(),
            },
            CollapsedDisplayText {
                visual_range: 25..43,
                buffer_range: 51..95,
                display_text: "rust-lang/rust#456".to_string(),
                buffer_text: "https://github.com/rust-lang/rust/issues/456".to_string(),
            },
        ];

        // Click at visual index 10 should find first region
        let content_visual_index = 10;
        let found = collapsed_regions.iter().find(|r| {
            content_visual_index >= r.visual_range.start
                && content_visual_index < r.visual_range.end
        });
        assert!(found.is_some());
        assert_eq!(found.unwrap().buffer_range, 0..44);

        // Click at visual index 30 should find second region
        let content_visual_index = 30;
        let found = collapsed_regions.iter().find(|r| {
            content_visual_index >= r.visual_range.start
                && content_visual_index < r.visual_range.end
        });
        assert!(found.is_some());
        assert_eq!(found.unwrap().buffer_range, 51..95);

        // Click at visual index 20 (between regions) should find nothing
        let content_visual_index = 20;
        let found = collapsed_regions.iter().find(|r| {
            content_visual_index >= r.visual_range.start
                && content_visual_index < r.visual_range.end
        });
        assert!(found.is_none());
    }
}
