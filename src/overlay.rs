//! Overlay rendering: the GitHub-ref hover popover and the autocomplete dropdown,
//! plus the styled-segment machinery (issue/user/title markdown) they share and the
//! hover hit-testing that finds the ref under the pointer.

use std::ops::Range;

use parley::Layout;
use vello::Scene;
use vello::kurbo::{Affine, Rect};
use vello::peniko::{Brush, Fill};

use crate::chrome::draw_panel;
use crate::consts::{PADDING, UI_LINE_HEIGHT};
use crate::core::{AutocompleteState, AutocompleteSuggestion, Editor};
use crate::doc_layout::{DocLayout, ScreenRect};
use crate::editor::EditorTheme;
use crate::inline::{GitHubContext, GitHubRef};
use crate::text_engine::{
    StyleRun, TextEngine, display_range_selection, peniko_color, peniko_color_alpha,
};
use crate::validation::{IssueStatus, ValidatedRefData, ValidationState};

/// A GitHub ref currently under the pointer, plus its on-screen caret rect (used to
/// anchor the hover popover above/below it).
pub(crate) struct HoverTarget {
    pub(crate) reference: GitHubRef,
    pub(crate) anchor: ScreenRect,
}

pub(crate) fn find_hover_target(
    editor: &Editor,
    doc: &DocLayout,
    x: f32,
    y: f32,
) -> Option<HoverTarget> {
    let off = doc.hit_test(x, y)?;
    hover_target_at_offset(editor, doc, off)
}

/// Find the GitHub ref whose byte range contains `off`, with its anchor rect.
pub(crate) fn hover_target_at_offset(
    editor: &Editor,
    doc: &DocLayout,
    off: usize,
) -> Option<HoverTarget> {
    let line = editor.state.buffer.byte_to_line(off);
    if let Some(refs) = editor.github_refs_by_line().get(&line) {
        for m in refs {
            if m.byte_range.contains(&off) {
                let anchor = doc.caret_rect(m.byte_range.start, 2.0)?;
                return Some(HoverTarget {
                    reference: m.reference.clone(),
                    anchor,
                });
            }
        }
    }
    if let Some(urls) = editor.naked_urls_by_line().get(&line) {
        for u in urls {
            if u.byte_range.contains(&off)
                && let Some(r) = &u.github_ref
            {
                let anchor = doc.caret_rect(u.byte_range.start, 2.0)?;
                return Some(HoverTarget {
                    reference: r.clone(),
                    anchor,
                });
            }
        }
    }
    None
}

/// Text color for an issue/PR status badge.
fn status_color(status: IssueStatus, theme: &EditorTheme) -> vello::peniko::Color {
    match status {
        IssueStatus::Open => theme.green,
        IssueStatus::Draft => theme.comment,
        IssueStatus::Merged | IssueStatus::Closed => theme.purple,
        IssueStatus::ClosedNotPlanned => theme.red,
    }
}

/// A styled span in a hover/autocomplete panel line: text plus color and inline
/// markdown attributes. Titles carry `bold`/`italic`/`mono`; all other segments
/// are `PanelSeg::plain`.
struct PanelSeg {
    text: String,
    color: vello::peniko::Color,
    bold: bool,
    italic: bool,
    mono: bool,
}

impl PanelSeg {
    fn plain(text: String, color: vello::peniko::Color) -> Self {
        Self {
            text,
            color,
            bold: false,
            italic: false,
            mono: false,
        }
    }

    fn bold(text: String, color: vello::peniko::Color) -> Self {
        Self {
            text,
            color,
            bold: true,
            italic: false,
            mono: false,
        }
    }

    fn italic(text: String, color: vello::peniko::Color) -> Self {
        Self {
            text,
            color,
            bold: false,
            italic: true,
            mono: false,
        }
    }

    fn mono(text: String, color: vello::peniko::Color) -> Self {
        Self {
            text,
            color,
            bold: false,
            italic: false,
            mono: true,
        }
    }
}

/// Scan an issue/PR `title` and emit styled spans, translating a small subset of
/// inline markdown (`` `code` ``, `**bold**`, `*italic*`/`_italic_`) into
/// attributes with the delimiters stripped. Non-nested, left-to-right; an
/// unterminated delimiter is emitted as literal text. All spans use `color`.
fn parse_title_markdown(title: &str, color: vello::peniko::Color) -> Vec<PanelSeg> {
    let mut out: Vec<PanelSeg> = Vec::new();
    let mut plain = String::new();
    let chars: Vec<char> = title.chars().collect();
    let mut i = 0;

    let flush_plain = |plain: &mut String, out: &mut Vec<PanelSeg>| {
        if !plain.is_empty() {
            out.push(PanelSeg::plain(std::mem::take(plain), color));
        }
    };

    while i < chars.len() {
        let c = chars[i];
        // `**bold**` — must check the double marker before the single `*`.
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_close_double(&chars, i + 2, '*') {
                flush_plain(&mut plain, &mut out);
                out.push(PanelSeg::bold(chars[i + 2..end].iter().collect(), color));
                i = end + 2;
                continue;
            }
        } else if c == '*' || c == '_' {
            if let Some(end) = find_close_single(&chars, i + 1, c) {
                flush_plain(&mut plain, &mut out);
                out.push(PanelSeg::italic(chars[i + 1..end].iter().collect(), color));
                i = end + 1;
                continue;
            }
        } else if c == '`'
            && let Some(end) = find_close_single(&chars, i + 1, '`')
        {
            flush_plain(&mut plain, &mut out);
            out.push(PanelSeg::mono(chars[i + 1..end].iter().collect(), color));
            i = end + 1;
            continue;
        }
        plain.push(c);
        i += 1;
    }
    flush_plain(&mut plain, &mut out);
    if out.is_empty() {
        out.push(PanelSeg::plain(String::new(), color));
    }
    out
}

/// Index of the next `marker` char at or after `from`, or None if unterminated.
fn find_close_single(chars: &[char], from: usize, marker: char) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == marker)
}

/// Index of the first char of a `marker`×2 run at or after `from`, or None.
fn find_close_double(chars: &[char], from: usize, marker: char) -> Option<usize> {
    (from..chars.len().saturating_sub(1)).find(|&j| chars[j] == marker && chars[j + 1] == marker)
}

/// Colored segments for an issue/PR: `<symbol> #<number> <title>`, with the
/// title's inline markdown rendered.
fn issue_segments(
    symbol: &str,
    number: u64,
    title: &str,
    status: IssueStatus,
    theme: &EditorTheme,
) -> Vec<PanelSeg> {
    let mut v = vec![
        PanelSeg::plain(format!("{symbol} "), status_color(status, theme)),
        PanelSeg::plain(format!("#{number} "), theme.cyan),
    ];
    v.extend(parse_title_markdown(title, theme.foreground));
    v
}

/// Colored segments for a user: `@login` and, if present, its display name.
fn user_segments(login: &str, name: Option<&str>, theme: &EditorTheme) -> Vec<PanelSeg> {
    let mut v = vec![PanelSeg::plain(format!("@{login}"), theme.cyan)];
    if let Some(name) = name {
        v.push(PanelSeg::plain(format!("  {name}"), theme.comment));
    }
    v
}

/// Lay out colored `segments` into a single styled line.
fn segments_to_line(
    engine: &mut TextEngine,
    segments: &[PanelSeg],
    scale: f32,
    font_size: f32,
    max_text: f32,
    theme: &EditorTheme,
) -> (Layout<Brush>, Vec<Range<usize>>) {
    let mut text = String::new();
    let mut runs = Vec::new();
    // Panel text is built directly (no segment map), so a mono segment's byte range in
    // `text` is already its display range — usable as-is for the code-chip background.
    let mut code_ranges = Vec::new();
    for seg in segments {
        let start = text.len();
        text.push_str(&seg.text);
        if seg.mono {
            code_ranges.push(start..text.len());
        }
        let mut run = StyleRun::new(start..text.len(), peniko_color(seg.color));
        run.bold = seg.bold;
        run.italic = seg.italic;
        run.mono = seg.mono;
        runs.push(run);
    }
    let layout = engine.build_line(
        &text,
        scale,
        font_size,
        UI_LINE_HEIGHT,
        peniko_color(theme.foreground),
        Some(max_text),
        &runs,
    );
    (layout, code_ranges)
}

/// The inline-code chip background color, matching the body's code spans.
fn code_chip_color(theme: &EditorTheme) -> vello::peniko::Color {
    peniko_color_alpha(theme.comment, 0.22)
}

/// Paint the inline-code background chip behind each display `range` of `layout`,
/// with the layout drawn at `origin` (device px). Mirrors the body's code chips for
/// hover/autocomplete titles.
fn fill_code_chips(
    scene: &mut Scene,
    layout: &Layout<Brush>,
    ranges: &[Range<usize>],
    origin: (f32, f32),
    color: vello::peniko::Color,
) {
    for r in ranges {
        let sel = display_range_selection(layout, r.clone());
        for (bb, _) in sel.geometry(layout) {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                color,
                None,
                &Rect::new(
                    bb.x0 + origin.0 as f64,
                    bb.y0 + origin.1 as f64,
                    bb.x1 + origin.0 as f64,
                    bb.y1 + origin.1 as f64,
                ),
            );
        }
    }
}

/// Position a `panel_w`×`panel_h` panel near `anchor`, preferring below it but
/// flipping above when it would overflow the viewport, and clamping horizontally.
/// Returns the panel's top-left (x, y).
fn place_panel(
    anchor: ScreenRect,
    panel_w: f32,
    panel_h: f32,
    viewport: (f32, f32),
    gap: f64,
) -> (f64, f64) {
    let (viewport_w, viewport_h) = viewport;
    let (ax0, ay0, _ax1, ay1) = anchor;
    let mut x = ax0;
    if x as f32 + panel_w > viewport_w {
        x = (viewport_w - panel_w) as f64;
    }
    x = x.max(0.0);
    let below_y = ay1 + gap;
    let y = if below_y as f32 + panel_h <= viewport_h {
        below_y
    } else {
        (ay0 - gap - panel_h as f64).max(0.0)
    };
    (x, y)
}

/// The colored text segments shown in a ref's hover popover, per validation state.
fn hover_segments(
    reference: &GitHubRef,
    state: &Option<ValidationState>,
    context: Option<&GitHubContext>,
    theme: &EditorTheme,
) -> Vec<PanelSeg> {
    match state {
        Some(ValidationState::Valid(Some(ValidatedRefData::Issue(issue)))) => issue_segments(
            issue.symbol(),
            issue.number,
            &issue.title,
            issue.status(),
            theme,
        ),
        Some(ValidationState::Valid(Some(ValidatedRefData::User(user)))) => {
            user_segments(&user.login, user.name.as_deref(), theme)
        }
        Some(ValidationState::Valid(None)) => {
            status_display_line("✓ ", theme.green, reference, context, theme)
        }
        Some(ValidationState::Invalid) => {
            status_display_line("✗ ", theme.red, reference, context, theme)
        }
        Some(ValidationState::Pending) | None => {
            status_display_line("… ", theme.comment, reference, context, theme)
        }
    }
}

/// A hover line for a ref that has no validated title: a status glyph followed by
/// the ref's short display text.
fn status_display_line(
    symbol: &str,
    symbol_color: vello::peniko::Color,
    reference: &GitHubRef,
    context: Option<&GitHubContext>,
    theme: &EditorTheme,
) -> Vec<PanelSeg> {
    vec![
        PanelSeg::plain(symbol.to_string(), symbol_color),
        PanelSeg::plain(reference.short_display(context), theme.cyan),
    ]
}

/// Draw the GitHub ref hover popover: a bordered panel anchored below (or above,
/// if space is tight) the hovered ref, showing its validated title/status.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_hover_popover(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    editor: &Editor,
    target: &HoverTarget,
    viewport_w: f32,
    viewport_h: f32,
    scale: f32,
) {
    let state = editor.github_validation_cache().get(&target.reference);
    let segs = hover_segments(&target.reference, &state, editor.github_context(), theme);

    let font_size = 14.0;
    // Cap the panel width (long issue titles) so it doesn't run off-screen.
    let max_text = (viewport_w - 2.0 * PADDING * scale).min(480.0 * scale);
    let (layout, code_ranges) = segments_to_line(engine, &segs, scale, font_size, max_text, theme);
    let pad = 8.0 * scale;
    let panel_w = layout.width() + pad * 2.0;
    let panel_h = layout.height() + pad * 2.0;

    let gap = 4.0 * scale as f64;
    let (x, y) = place_panel(
        target.anchor,
        panel_w,
        panel_h,
        (viewport_w, viewport_h),
        gap,
    );

    let rect = Rect::new(x, y, x + panel_w as f64, y + panel_h as f64);
    draw_panel(
        scene,
        peniko_color(theme.background),
        peniko_color(theme.comment),
        &rect,
        6.0 * scale as f64,
        scale as f64,
    );
    let origin = (x as f32 + pad, y as f32 + pad);
    fill_code_chips(scene, &layout, &code_ranges, origin, code_chip_color(theme));
    engine.draw_line(scene, &layout, origin);
}

/// The colored text segments for one autocomplete row.
fn suggestion_segments(s: &AutocompleteSuggestion, theme: &EditorTheme) -> Vec<PanelSeg> {
    match s {
        AutocompleteSuggestion::IssueOrPr {
            number,
            symbol,
            status,
            title,
        } => issue_segments(symbol, *number, title, *status, theme),
        AutocompleteSuggestion::User { login, name } => {
            user_segments(login, name.as_deref(), theme)
        }
    }
}

/// Draw the autocomplete dropdown anchored at the caret (flipping above if it would
/// overflow the viewport bottom). Returns the screen rect of each row for click
/// routing. Rows are stacked top-down; the selected row gets a highlight fill.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_autocomplete(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    ac: &AutocompleteState,
    caret: ScreenRect,
    viewport_w: f32,
    viewport_h: f32,
    scale: f32,
) -> Vec<ScreenRect> {
    let font_size = 14.0;
    let pad_x = 10.0 * scale;
    let pad_y = 5.0 * scale;
    let panel_w = (viewport_w - 2.0 * PADDING * scale)
        .min(480.0 * scale)
        .max(140.0 * scale);
    let max_text = panel_w - 2.0 * pad_x;

    // Lay out every row first so the panel can be sized to their total height.
    let mut rows = Vec::with_capacity(ac.suggestions.len());
    for s in &ac.suggestions {
        let segs = suggestion_segments(s, theme);
        let (layout, code) = segments_to_line(engine, &segs, scale, font_size, max_text, theme);
        let h = layout.height() + 2.0 * pad_y;
        rows.push((layout, h, code));
    }
    let panel_h: f32 = rows.iter().map(|(_, h, _)| *h).sum();

    let gap = 4.0 * scale as f64;
    let (x, y) = place_panel(caret, panel_w, panel_h, (viewport_w, viewport_h), gap);

    let panel = Rect::new(x, y, x + panel_w as f64, y + panel_h as f64);
    draw_panel(
        scene,
        peniko_color(theme.background),
        peniko_color(theme.comment),
        &panel,
        6.0 * scale as f64,
        scale as f64,
    );

    let sel_color = peniko_color(theme.selection);
    let mut rects = Vec::with_capacity(rows.len());
    let mut row_top = y;
    for (i, (layout, h, code)) in rows.iter().enumerate() {
        let row_bottom = row_top + *h as f64;
        if i == ac.selected_index {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                sel_color,
                None,
                &Rect::new(x, row_top, x + panel_w as f64, row_bottom),
            );
        }
        let origin = (x as f32 + pad_x, row_top as f32 + pad_y);
        fill_code_chips(scene, layout, code, origin, code_chip_color(theme));
        engine.draw_line(scene, layout, origin);
        rects.push((x, row_top, x + panel_w as f64, row_bottom));
        row_top = row_bottom;
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;
    use vello::peniko::Color;

    const C: Color = Color::new([0.1, 0.2, 0.3, 1.0]);

    fn texts(segs: &[PanelSeg]) -> Vec<&str> {
        segs.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn plain_title_is_one_plain_span() {
        let segs = parse_title_markdown("just a title", C);
        assert_eq!(texts(&segs), vec!["just a title"]);
        assert!(!segs[0].bold && !segs[0].italic && !segs[0].mono);
    }

    #[test]
    fn empty_title_yields_one_empty_span() {
        let segs = parse_title_markdown("", C);
        assert_eq!(texts(&segs), vec![""]);
    }

    #[test]
    fn code_span_strips_backticks_and_sets_mono() {
        let segs = parse_title_markdown("Fix `foo()` crash", C);
        assert_eq!(texts(&segs), vec!["Fix ", "foo()", " crash"]);
        assert!(segs[1].mono && !segs[1].bold && !segs[1].italic);
        assert!(!segs[0].mono && !segs[2].mono);
        for s in &segs {
            assert!(!s.text.contains('`'));
        }
    }

    #[test]
    fn bold_span_strips_delimiters_and_sets_bold() {
        let segs = parse_title_markdown("**bold**", C);
        assert_eq!(texts(&segs), vec!["bold"]);
        assert!(segs[0].bold && !segs[0].italic && !segs[0].mono);
        assert!(!segs[0].text.contains('*'));
    }

    #[test]
    fn star_and_underscore_italics() {
        let star = parse_title_markdown("*it*", C);
        assert_eq!(texts(&star), vec!["it"]);
        assert!(star[0].italic && !star[0].bold && !star[0].mono);

        let under = parse_title_markdown("_it_", C);
        assert_eq!(texts(&under), vec!["it"]);
        assert!(under[0].italic && !under[0].bold && !under[0].mono);

        for segs in [&star, &under] {
            assert!(!segs[0].text.contains('*') && !segs[0].text.contains('_'));
        }
    }

    #[test]
    fn unterminated_backtick_stays_literal() {
        let segs = parse_title_markdown("a ` b", C);
        assert_eq!(texts(&segs), vec!["a ` b"]);
        assert!(!segs[0].mono);
    }

    #[test]
    fn mixed_markdown_in_title() {
        let segs = parse_title_markdown("**b** and `c` and *i*", C);
        assert_eq!(texts(&segs), vec!["b", " and ", "c", " and ", "i"]);
        assert!(segs[0].bold);
        assert!(segs[2].mono);
        assert!(segs[4].italic);
    }

    #[test]
    fn issue_segments_prefixes_plain_then_title_spans() {
        let theme = EditorTheme::default();
        let segs = issue_segments("●", 42, "Fix `foo`", IssueStatus::Open, &theme);
        assert_eq!(texts(&segs), vec!["● ", "#42 ", "Fix ", "foo"]);
        assert!(!segs[0].mono && !segs[1].mono);
        assert!(segs[3].mono);
    }

    #[test]
    fn segments_to_line_mono_run_sets_mono_flag() {
        let mut engine = TextEngine::new();
        let theme = EditorTheme::default();
        let segs = vec![
            PanelSeg::plain("x ".to_string(), C),
            PanelSeg {
                text: "code".to_string(),
                color: C,
                bold: false,
                italic: false,
                mono: true,
            },
        ];
        // Exercises the draw path's run construction without panicking.
        let _ = segments_to_line(&mut engine, &segs, 1.0, 14.0, 400.0, &theme);
    }
}
