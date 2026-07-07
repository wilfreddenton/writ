//! Parley text layout + Vello glyph drawing — the core of writ's render path. Lays out a
//! styled line with browser-grade (Chromium) soft-wrap and hanging indent, and paints its
//! glyphs into a Vello scene. Drives the per-line layout cache and the display↔buffer map.

use std::ops::Range;

use parley::{
    Affinity, Alignment, AlignmentOptions, CHROMIUM_LINE_BREAK_OVERRIDE, Cluster, Cursor,
    FontContext, FontFamily, FontFamilyName, FontStyle, FontWeight, GenericFamily, IndentOptions,
    InlineBox, InlineBoxKind, Layout, LayoutContext, LineHeight, OverflowWrap,
    PositionedLayoutItem, Selection, StyleProperty,
};
use vello::Scene;
use vello::kurbo::{Affine, Line, Stroke};
use vello::peniko::{Brush, Color, Fill};

/// A styled sub-range of a line (color + font attributes). Buffer/marker-agnostic;
/// higher layers translate markdown/highlight spans into these.
#[derive(Clone, Debug)]
pub struct StyleRun {
    pub range: Range<usize>,
    pub color: Color,
    pub bold: bool,
    pub italic: bool,
    pub mono: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

impl StyleRun {
    pub fn new(range: Range<usize>, color: Color) -> Self {
        Self {
            range,
            color,
            bold: false,
            italic: false,
            mono: false,
            underline: false,
            strikethrough: false,
        }
    }
}

/// Owns the Parley contexts. One per shell; borrowed `&mut` to build layouts.
pub struct TextEngine {
    fcx: FontContext,
    lcx: LayoutContext<Brush>,
}

impl Default for TextEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TextEngine {
    pub fn new() -> Self {
        Self {
            fcx: FontContext::new(),
            lcx: LayoutContext::new(),
        }
    }

    /// Build a laid-out line. `font_size` is in *logical* pixels; `scale` (the
    /// display's DPI scale factor) multiplies it to device pixels, so the returned
    /// layout is in device px. `max_advance` (the soft-wrap width, None = no wrap)
    /// must therefore be given in *device* px too. `line_height` is a multiple of
    /// the font size (CSS unitless semantics).
    pub fn build_line(
        &mut self,
        text: &str,
        scale: f32,
        font_size: f32,
        line_height: f32,
        base_color: Color,
        max_advance: Option<f32>,
        runs: &[StyleRun],
    ) -> Layout<Brush> {
        self.build_line_hanging(
            text,
            scale,
            font_size,
            line_height,
            base_color,
            max_advance,
            runs,
            0,
            &[],
        )
    }

    /// Like [`build_line`](Self::build_line), but wrapped continuation rows hang-indent
    /// under display byte `content_start` (the column where content begins after
    /// list/blockquote markers), so wrapped list/quote text aligns under its content
    /// instead of returning to the margin. `content_start == 0` (or ≥ `text.len()`)
    /// disables the hang. The prefix width is measured with a throwaway no-wrap break,
    /// then applied via Parley's native `text-indent: hanging`.
    ///
    /// `inline_boxes` are in-flow boxes to reserve space for (inline images): each is a
    /// `(display byte offset, width, height)` in device px; text flows around them and
    /// the line grows to fit the tallest. A box's `id` is its index here, so the caller
    /// can match `PositionedLayoutItem::InlineBox` back to a parallel draw list. Boxes at
    /// a non-char-boundary or past the text end are skipped defensively.
    #[allow(clippy::too_many_arguments)]
    pub fn build_line_hanging(
        &mut self,
        text: &str,
        scale: f32,
        font_size: f32,
        line_height: f32,
        base_color: Color,
        max_advance: Option<f32>,
        runs: &[StyleRun],
        content_start: usize,
        inline_boxes: &[(usize, f32, f32)],
    ) -> Layout<Brush> {
        let mut builder = self.lcx.ranged_builder(&mut self.fcx, text, scale, true);
        builder.push_default(StyleProperty::FontSize(font_size));
        builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(
            line_height,
        )));
        builder.push_default(StyleProperty::FontFamily(FontFamily::Single(
            FontFamilyName::Generic(GenericFamily::Monospace),
        )));
        builder.push_default(StyleProperty::Brush(Brush::Solid(base_color)));
        // A word too long to fit the wrap width breaks at an arbitrary point instead of
        // overflowing off the right edge (CSS `overflow-wrap: anywhere`); normal text
        // still wraps only at word boundaries.
        builder.push_default(StyleProperty::OverflowWrap(OverflowWrap::Anywhere));

        for run in runs {
            builder.push(
                StyleProperty::Brush(Brush::Solid(run.color)),
                run.range.clone(),
            );
            if run.bold {
                builder.push(
                    StyleProperty::FontWeight(FontWeight::BOLD),
                    run.range.clone(),
                );
            }
            if run.italic {
                builder.push(
                    StyleProperty::FontStyle(FontStyle::Italic),
                    run.range.clone(),
                );
            }
            if run.mono {
                builder.push(
                    StyleProperty::FontFamily(FontFamily::Single(FontFamilyName::Generic(
                        GenericFamily::Monospace,
                    ))),
                    run.range.clone(),
                );
            }
            if run.underline {
                builder.push(StyleProperty::Underline(true), run.range.clone());
            }
            if run.strikethrough {
                builder.push(StyleProperty::Strikethrough(true), run.range.clone());
            }
        }

        // Browser-grade line breaking: don't break before closing punctuation or
        // after opening punctuation (the fix we couldn't land in gpui).
        builder.set_line_break_override(Some(CHROMIUM_LINE_BREAK_OVERRIDE));

        for (i, &(offset, width, height)) in inline_boxes.iter().enumerate() {
            if offset > text.len() || !text.is_char_boundary(offset) {
                continue;
            }
            builder.push_inline_box(InlineBox {
                id: i as u64,
                kind: InlineBoxKind::InFlow,
                index: offset,
                width,
                height,
            });
        }

        let mut layout = builder.build(text);

        // Hanging indent: measure the marker/quote prefix width on a throwaway no-wrap
        // break (cluster x-offset at content start), then let Parley offset every
        // continuation row by it. Must be set before the real break + align.
        if content_start > 0 && content_start < text.len() {
            layout.break_all_lines(None);
            let hang = Cluster::from_byte_index(&layout, content_start)
                .and_then(|c| c.visual_offset())
                .unwrap_or(0.0);
            if hang > 0.0 {
                layout.set_text_indent(
                    hang,
                    IndentOptions {
                        each_line: false,
                        hanging: true,
                    },
                );
            }
        }

        layout.break_all_lines(max_advance);
        layout.align(Alignment::Start, AlignmentOptions::default());
        layout
    }

    /// Paint a laid-out line's glyphs (and any underline/strikethrough) into `scene`,
    /// translated so the layout's top-left sits at `origin`.
    pub fn draw_line(&self, scene: &mut Scene, layout: &Layout<Brush>, origin: (f32, f32)) {
        let transform = Affine::translate((origin.0 as f64, origin.1 as f64));
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                let style = glyph_run.style();
                let run = glyph_run.run();
                let font = run.font();
                let font_size = run.font_size();
                let synthesis = run.synthesis();
                let glyph_xform = synthesis
                    .skew()
                    .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));

                scene
                    .draw_glyphs(font)
                    .brush(&style.brush)
                    .hint(true)
                    .transform(transform)
                    .glyph_transform(glyph_xform)
                    .font_size(font_size)
                    .normalized_coords(run.normalized_coords())
                    .draw(
                        Fill::NonZero,
                        glyph_run.positioned_glyphs().map(|g| vello::Glyph {
                            id: g.id,
                            x: g.x,
                            y: g.y,
                        }),
                    );

                let run_metrics = run.metrics();
                let x0 = glyph_run.offset() as f64;
                let x1 = (glyph_run.offset() + glyph_run.advance()) as f64;
                let baseline = glyph_run.baseline();
                if style.underline.is_some() {
                    let y =
                        baseline - run_metrics.underline_offset + run_metrics.underline_size / 2.0;
                    stroke_decoration(
                        scene,
                        transform,
                        &style.brush,
                        x0,
                        x1,
                        y,
                        run_metrics.underline_size,
                    );
                }
                if style.strikethrough.is_some() {
                    let y = baseline - run_metrics.strikethrough_offset
                        + run_metrics.strikethrough_size / 2.0;
                    stroke_decoration(
                        scene,
                        transform,
                        &style.brush,
                        x0,
                        x1,
                        y,
                        run_metrics.strikethrough_size,
                    );
                }
            }
        }
    }
}

fn stroke_decoration(
    scene: &mut Scene,
    transform: Affine,
    brush: &Brush,
    x0: f64,
    x1: f64,
    y: f32,
    width: f32,
) {
    let line = Line::new((x0, y as f64), (x1, y as f64));
    scene.stroke(&Stroke::new(width.into()), transform, brush, None, &line);
}

/// Identity pass-through (theme colors are already `peniko::Color`). Retained to
/// avoid churning every `peniko_color(theme.x)` call site; can be inlined later.
pub fn peniko_color(c: Color) -> Color {
    c
}

/// Build a Parley `Selection` spanning a display byte range (Downstream at the
/// start, Upstream at the end) for geometry/highlight queries.
pub(crate) fn display_range_selection(layout: &Layout<Brush>, range: Range<usize>) -> Selection {
    Selection::new(
        Cursor::from_byte_index(layout, range.start, Affinity::Downstream),
        Cursor::from_byte_index(layout, range.end, Affinity::Upstream),
    )
}

/// A theme color at an explicit alpha (0..1) — for translucent diff backgrounds.
pub fn peniko_color_alpha(c: Color, alpha: f32) -> Color {
    let [r, g, b, _] = c.components;
    Color::new([r, g, b, alpha.clamp(0.0, 1.0)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The visual byte-range covered by each laid-out line (union of its glyph runs).
    fn line_ranges(layout: &Layout<Brush>) -> Vec<Range<usize>> {
        layout
            .lines()
            .map(|line| {
                let mut start = usize::MAX;
                let mut end = 0usize;
                for item in line.items() {
                    if let PositionedLayoutItem::GlyphRun(gr) = item {
                        let r = gr.run().text_range();
                        start = start.min(r.start);
                        end = end.max(r.end);
                    }
                }
                start..end
            })
            .collect()
    }

    /// The migration's headline goal: browser-grade wrapping. `LineBreakContext` is
    /// `#[non_exhaustive]` (can't construct it to unit-test the override fn directly),
    /// so exercise the override through a real narrow layout and assert its contract
    /// on the actual break points: no wrapped line starts with closing punctuation,
    /// none ends with opening punctuation. (The differential UAX-14-vs-Chromium
    /// correctness is owned + tested by parley upstream; this pins that we wired it in.)
    #[test]
    fn chromium_override_no_orphaned_punctuation() {
        let mut engine = TextEngine::new();
        // Monospace so advances are uniform and wrapping is deterministic on-machine.
        let text = "alpha (beta) gamma, delta; epsilon: zeta. eta [theta] iota {kappa} lambda";
        let mono = StyleRun {
            range: 0..text.len(),
            color: Color::WHITE,
            bold: false,
            italic: false,
            mono: true,
            underline: false,
            strikethrough: false,
        };
        let full = engine.build_line(
            text,
            1.0,
            16.0,
            1.4,
            Color::WHITE,
            None,
            std::slice::from_ref(&mono),
        );
        let full_width = full.width();
        // Force several wraps.
        let layout = engine.build_line(
            text,
            1.0,
            16.0,
            1.4,
            Color::WHITE,
            Some(full_width * 0.4),
            &[mono],
        );
        assert!(layout.lines().count() > 1, "expected the line to wrap");

        for range in line_ranges(&layout) {
            let slice = text[range].trim();
            if slice.is_empty() {
                continue;
            }
            let first = slice.chars().next().unwrap();
            let last = slice.chars().last().unwrap();
            assert!(
                !matches!(first, ')' | ']' | '}' | '.' | ',' | ':' | ';'),
                "line starts with orphaned closing punctuation: {slice:?}"
            );
            assert!(
                !matches!(last, '(' | '[' | '{'),
                "line ends with dangling opening punctuation: {slice:?}"
            );
        }
    }

    #[test]
    fn build_line_lays_out_glyphs() {
        let mut engine = TextEngine::new();
        let text = "Hello world";
        let layout = engine.build_line(text, 1.0, 16.0, 1.4, Color::WHITE, None, &[]);
        assert!(layout.lines().count() >= 1);
        let glyphs: usize = layout
            .lines()
            .flat_map(|l| l.items())
            .filter_map(|it| match it {
                PositionedLayoutItem::GlyphRun(gr) => Some(gr.positioned_glyphs().count()),
                _ => None,
            })
            .sum();
        assert!(glyphs > 0, "expected glyphs to be laid out");
    }

    /// With a `content_start`, wrapped continuation rows hang-indent under the content
    /// column (offset > 0); without it they stay flush at the margin (offset ~0).
    #[test]
    fn hanging_indent_offsets_wrapped_rows() {
        let mut engine = TextEngine::new();
        let text = "- a fairly long list item that will certainly wrap onto several visual rows";
        let content_start = 2; // just past "- "
        let run = StyleRun {
            range: 0..text.len(),
            color: Color::WHITE,
            bold: false,
            italic: false,
            mono: true,
            underline: false,
            strikethrough: false,
        };
        let runs = std::slice::from_ref(&run);
        let wrap = engine
            .build_line(text, 1.0, 16.0, 1.4, Color::WHITE, None, runs)
            .width()
            * 0.45;

        // First glyph-run x of each visual row.
        let row_x = |layout: &Layout<Brush>| -> Vec<f32> {
            layout
                .lines()
                .map(|line| {
                    line.items()
                        .find_map(|it| match it {
                            PositionedLayoutItem::GlyphRun(gr) => Some(gr.offset()),
                            _ => None,
                        })
                        .unwrap_or(0.0)
                })
                .collect()
        };

        let hung = engine.build_line_hanging(
            text,
            1.0,
            16.0,
            1.4,
            Color::WHITE,
            Some(wrap),
            runs,
            content_start,
            &[],
        );
        let flat = engine.build_line(text, 1.0, 16.0, 1.4, Color::WHITE, Some(wrap), runs);
        let hung_x = row_x(&hung);
        let flat_x = row_x(&flat);

        assert!(hung_x.len() > 1, "expected the line to wrap");
        assert_eq!(hung_x.len(), flat_x.len(), "same wrap points either way");
        assert!(hung_x[0].abs() < 0.5, "first row flush at margin");
        assert!(
            hung_x[1] > 1.0,
            "continuation row should hang, got {}",
            hung_x[1]
        );
        assert!(
            flat_x[1].abs() < 0.5,
            "without content_start the continuation stays flush, got {}",
            flat_x[1]
        );
    }
}
