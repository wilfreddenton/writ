//! Window chrome for the new shell (see MIGRATION-PLAN.md, Phase 6): an in-window
//! title bar (filename + dirty marker) and a status bar (nesting context + cursor
//! position). Drawn as opaque strips above/below the editor, which is inset between
//! them. CSD (custom window frame) and the async-blocked overlays are deferred.

use vello::Scene;
use vello::kurbo::{Affine, Line, Rect, RoundedRect, Stroke};
use vello::peniko::{Color, Fill};

use crate::consts::{FIND_ROW_H, PADDING, UI_LINE_HEIGHT};
use crate::core::{FieldFocus, FindMode, FindState};
use crate::doc_layout::ScreenRect;
use crate::editor::EditorTheme;
use crate::marker::MarkerKind;
use crate::status_bar::build_context_display;
use crate::text_engine::{StyleRun, TextEngine, peniko_color, peniko_color_alpha};
use crate::text_input::{TextField, draw_text_field};

/// Everything the status bar shows, gathered from the editor + viewport each frame.
pub struct StatusInfo {
    pub context: Vec<MarkerKind>,
    pub heading_level: Option<u8>,
    pub cursor_line: usize,
    pub cursor_col: usize,
    pub total_lines: usize,
    pub first_visible: usize,
    pub last_visible: usize,
}

/// A screen rect (device px) for a chrome strip.
pub struct BarRect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

fn fill_rect(scene: &mut Scene, color: Color, r: &BarRect) {
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        color,
        None,
        &Rect::new(r.x0, r.y0, r.x1, r.y1),
    );
}

/// Draw a floating overlay panel: a filled rounded rect with a border. Shared by
/// the GitHub hover popover and the autocomplete dropdown.
pub fn draw_panel(
    scene: &mut Scene,
    bg: Color,
    border: Color,
    rect: &Rect,
    radius: f64,
    stroke_w: f64,
) {
    let rr = RoundedRect::from_rect(*rect, radius);
    scene.fill(Fill::NonZero, Affine::IDENTITY, bg, None, &rr);
    scene.stroke(&Stroke::new(stroke_w), Affine::IDENTITY, border, None, &rr);
}

/// The shared chrome-panel look: a `theme.surface` fill with a `theme.selection`
/// hairline border, rounded. Reused by floating in-editor UI (the find bar now;
/// doc map / folding later) so they share one tone with the status bar.
pub fn draw_chrome_panel(scene: &mut Scene, theme: &EditorTheme, rect: &Rect, scale: f32) {
    draw_panel(
        scene,
        peniko_color(theme.surface),
        peniko_color(theme.selection),
        rect,
        6.0 * scale as f64,
        scale as f64,
    );
}

fn hline(scene: &mut Scene, color: Color, x0: f64, x1: f64, y: f64, width: f64) {
    scene.stroke(
        &Stroke::new(width),
        Affine::IDENTITY,
        color,
        None,
        &Line::new((x0, y), (x1, y)),
    );
}

/// Vertically-center a single line of `font_size` (device px) within a bar.
fn baseline_top(bar: &BarRect, font_size: f32) -> f32 {
    (bar.y0 as f32) + ((bar.y1 - bar.y0) as f32 - font_size * UI_LINE_HEIGHT) / 2.0
}

/// Vertically-center a built layout within a bar using its *actual* height, so the
/// glyphs sit on the bar's centerline regardless of the font's line-box metrics.
fn center_top(bar: &BarRect, layout_height: f32) -> f32 {
    (bar.y0 as f32) + ((bar.y1 - bar.y0) as f32 - layout_height) / 2.0
}

/// Draw the top title bar: centered filename with a leading `*` when dirty.
pub fn draw_title_bar(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    bar: &BarRect,
    filename: &str,
    dirty: bool,
    scale: f32,
) {
    fill_rect(scene, peniko_color(theme.background), bar);
    hline(
        scene,
        peniko_color(theme.selection),
        bar.x0,
        bar.x1,
        bar.y1,
        scale as f64,
    );

    let title = if dirty {
        format!("* {filename}")
    } else {
        filename.to_string()
    };
    let font_size = 15.0;
    let color = peniko_color(theme.foreground);
    let layout = engine.build_line(&title, scale, font_size, UI_LINE_HEIGHT, color, None, &[]);
    let w = layout.width();
    let cx = ((bar.x0 + bar.x1) as f32 / 2.0) - w / 2.0;
    engine.draw_line(
        scene,
        &layout,
        (cx.max(bar.x0 as f32), baseline_top(bar, font_size)),
    );
}

/// Font size (device-independent px) for find-bar labels + match count.
const FIND_LABEL_FONT: f32 = 14.0;
/// Font size for the `.*` / `Aa` toggle chips.
const FIND_CHIP_FONT: f32 = 13.0;
/// Fixed width of the right-aligned label column (`Find`/`Replace`) so both rows align.
const FIND_LABEL_SLOT: f32 = 70.0;

/// The click rects of the replace row's `Replace` and `All` buttons (device px), returned
/// so the shell can hit-test clicks on them. Only present in Replace mode.
pub struct FindButtonRects {
    pub replace: ScreenRect,
    pub all: ScreenRect,
}

/// Draw the bottom-docked find bar (search row, plus a replace row in Replace mode)
/// filling `bar` — the strip between the document and the status bar. Same surface look
/// as the status bar: `theme.surface` fill with a `theme.selection` top hairline.
pub fn draw_find_bar(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    find: &FindState,
    bar: &BarRect,
    scale: f32,
) -> Option<FindButtonRects> {
    fill_rect(scene, peniko_color(theme.surface), bar);
    hline(
        scene,
        peniko_color(theme.selection),
        bar.x0,
        bar.x1,
        bar.y0,
        scale as f64,
    );

    let row_h = FIND_ROW_H * scale;
    let rows = if find.mode == FindMode::Replace {
        2.0
    } else {
        1.0
    };
    let vpad = ((bar.y1 - bar.y0) as f32 - row_h * rows) / 2.0;
    let search_top = bar.y0 as f32 + vpad;

    draw_find_search_row(engine, scene, theme, find, bar, search_top, row_h, scale);
    if find.mode == FindMode::Replace {
        Some(draw_find_replace_row(
            engine,
            scene,
            theme,
            find,
            bar,
            search_top + row_h,
            row_h,
            scale,
        ))
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_find_search_row(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    find: &FindState,
    bar: &BarRect,
    row_top: f32,
    row_h: f32,
    scale: f32,
) {
    let pad = PADDING * scale;
    let gap = 10.0 * scale;
    let slot = FIND_LABEL_SLOT * scale;
    let left = bar.x0 as f32 + pad;
    let right = bar.x1 as f32 - pad;

    draw_find_label(
        engine, scene, theme, "Find", left, slot, row_top, row_h, scale,
    );

    // Right block, laid out right-to-left: match count, then case + regex chips.
    let count_str = if find.matches.is_empty() {
        "0 / 0".to_string()
    } else {
        format!(
            "{} / {}",
            find.active.map(|i| i + 1).unwrap_or(0),
            find.matches.len()
        )
    };
    let mut run = StyleRun::new(0..count_str.len(), peniko_color(theme.comment));
    run.mono = true;
    let count = engine.build_line(
        &count_str,
        scale,
        FIND_LABEL_FONT,
        UI_LINE_HEIGHT,
        peniko_color(theme.comment),
        None,
        &[run],
    );
    let count_x = right - count.width();
    engine.draw_line(
        scene,
        &count,
        (count_x, row_top + (row_h - count.height()) / 2.0),
    );

    let mut rx = count_x - gap;
    rx = draw_find_chip(
        engine,
        scene,
        theme,
        "Aa",
        rx,
        row_top,
        row_h,
        find.case_sensitive,
        scale,
    );
    rx -= gap * 0.5;
    rx = draw_find_chip(
        engine, scene, theme, ".*", rx, row_top, row_h, find.regex, scale,
    );
    let field_right = rx - gap;

    let field_left = left + slot + gap;
    draw_find_field(
        engine,
        scene,
        theme,
        &find.search,
        field_left,
        field_right,
        row_top,
        row_h,
        find.focused && find.focus == FieldFocus::Search,
        scale,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_find_replace_row(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    find: &FindState,
    bar: &BarRect,
    row_top: f32,
    row_h: f32,
    scale: f32,
) -> FindButtonRects {
    let pad = PADDING * scale;
    let gap = 10.0 * scale;
    let slot = FIND_LABEL_SLOT * scale;
    let left = bar.x0 as f32 + pad;
    let right = bar.x1 as f32 - pad;
    draw_find_label(
        engine, scene, theme, "Replace", left, slot, row_top, row_h, scale,
    );

    // Right block, laid out right-to-left: `All` (primary), then `Replace`.
    let (all_left, all) = draw_find_button(
        engine, scene, theme, "All", right, row_top, row_h, true, scale,
    );
    let (replace_left, replace) = draw_find_button(
        engine,
        scene,
        theme,
        "Replace",
        all_left - gap * 0.5,
        row_top,
        row_h,
        false,
        scale,
    );

    draw_find_field(
        engine,
        scene,
        theme,
        &find.replace,
        left + slot + gap,
        replace_left - gap,
        row_top,
        row_h,
        find.focused && find.focus == FieldFocus::Replace,
        scale,
    );
    FindButtonRects { replace, all }
}

/// A boxed action button drawn against its right edge `right_x`; returns its left edge (so
/// buttons chain right-to-left) and its click rect. `primary` buttons (Replace All) read
/// greenish; plain buttons use a subtle neutral fill.
#[allow(clippy::too_many_arguments)]
fn draw_find_button(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    label: &str,
    right_x: f32,
    row_top: f32,
    row_h: f32,
    primary: bool,
    scale: f32,
) -> (f32, ScreenRect) {
    let color = if primary {
        theme.green
    } else {
        theme.foreground
    };
    let layout = engine.build_line(
        label,
        scale,
        FIND_CHIP_FONT,
        UI_LINE_HEIGHT,
        peniko_color(color),
        None,
        &[],
    );
    let cpad = 8.0 * scale;
    let w = layout.width() + 2.0 * cpad;
    let h = row_h * 0.78;
    let x0 = right_x - w;
    let cy = row_top + (row_h - h) / 2.0;
    let rect = Rect::new(x0 as f64, cy as f64, right_x as f64, (cy + h) as f64);
    let rr = RoundedRect::from_rect(rect, 4.0 * scale as f64);
    let bg = if primary {
        peniko_color_alpha(theme.green, 0.20)
    } else {
        peniko_color_alpha(theme.comment, 0.12)
    };
    scene.fill(Fill::NonZero, Affine::IDENTITY, bg, None, &rr);
    let border = if primary {
        theme.green
    } else {
        theme.selection
    };
    scene.stroke(
        &Stroke::new(scale as f64),
        Affine::IDENTITY,
        peniko_color(border),
        None,
        &rr,
    );
    engine.draw_line(
        scene,
        &layout,
        (x0 + cpad, cy + (h - layout.height()) / 2.0),
    );
    (x0, (rect.x0, rect.y0, rect.x1, rect.y1))
}

/// A row label left-aligned at `left` (so `Find`/`Replace` line up with the document's
/// left text edge). `slot` is unused here; the caller reserves it so the fields align.
#[allow(clippy::too_many_arguments)]
fn draw_find_label(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    text: &str,
    left: f32,
    _slot: f32,
    row_top: f32,
    row_h: f32,
    scale: f32,
) {
    let layout = engine.build_line(
        text,
        scale,
        FIND_LABEL_FONT,
        UI_LINE_HEIGHT,
        peniko_color(theme.comment),
        None,
        &[],
    );
    engine.draw_line(
        scene,
        &layout,
        (left, row_top + (row_h - layout.height()) / 2.0),
    );
}

/// A toggle chip drawn against its right edge `right_x`; returns its left edge so the
/// caller can chain chips right-to-left. Active chips read purple with a tinted fill.
#[allow(clippy::too_many_arguments)]
fn draw_find_chip(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    label: &str,
    right_x: f32,
    row_top: f32,
    row_h: f32,
    active: bool,
    scale: f32,
) -> f32 {
    let color = if active { theme.purple } else { theme.comment };
    let layout = engine.build_line(
        label,
        scale,
        FIND_CHIP_FONT,
        UI_LINE_HEIGHT,
        peniko_color(color),
        None,
        &[],
    );
    let cpad = 6.0 * scale;
    let w = layout.width() + 2.0 * cpad;
    let h = row_h * 0.72;
    let x0 = right_x - w;
    let cy = row_top + (row_h - h) / 2.0;
    let rr = RoundedRect::from_rect(
        Rect::new(x0 as f64, cy as f64, right_x as f64, (cy + h) as f64),
        4.0 * scale as f64,
    );
    let bg = if active {
        peniko_color_alpha(theme.purple, 0.22)
    } else {
        peniko_color_alpha(theme.comment, 0.10)
    };
    scene.fill(Fill::NonZero, Affine::IDENTITY, bg, None, &rr);
    engine.draw_line(
        scene,
        &layout,
        (x0 + cpad, cy + (h - layout.height()) / 2.0),
    );
    x0
}

/// A boxed text field: a lighter (document-background) rounded fill with a border —
/// purple when focused, otherwise the surface hairline — then the field's text/caret.
#[allow(clippy::too_many_arguments)]
fn draw_find_field(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    field: &TextField,
    x0: f32,
    x1: f32,
    row_top: f32,
    row_h: f32,
    focused: bool,
    scale: f32,
) {
    let fh = row_h - 4.0 * scale;
    let cy = row_top + (row_h - fh) / 2.0;
    let rect = Rect::new(x0 as f64, cy as f64, x1.max(x0) as f64, (cy + fh) as f64);
    let rr = RoundedRect::from_rect(rect, 4.0 * scale as f64);
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        peniko_color(theme.background),
        None,
        &rr,
    );
    let border = if focused {
        theme.purple
    } else {
        theme.selection
    };
    scene.stroke(
        &Stroke::new(scale as f64),
        Affine::IDENTITY,
        peniko_color(border),
        None,
        &rr,
    );
    draw_text_field(engine, scene, theme, field, &rect, scale, focused);
}

/// Draw the bottom status bar: left = nesting context (depth-colored), right =
/// heading level, cursor position, line count, scroll indicator.
pub fn draw_status_bar(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    bar: &BarRect,
    info: &StatusInfo,
    scale: f32,
) {
    fill_rect(scene, peniko_color(theme.surface), bar);
    hline(
        scene,
        peniko_color(theme.selection),
        bar.x0,
        bar.x1,
        bar.y0,
        scale as f64,
    );

    let font_size = 15.0;
    let pad = PADDING * scale;

    // --- left: context markers, colored by nesting depth ---
    let depth_colors = [
        theme.cyan,
        theme.purple,
        theme.green,
        theme.orange,
        theme.pink,
        theme.yellow,
    ];
    let items = build_context_display(&info.context);
    let mut text = String::new();
    let mut runs = Vec::new();
    for (i, (s, depth)) in items.iter().enumerate() {
        let needs_space = !s.starts_with(' ') && !s.starts_with('x');
        if i > 0 && needs_space {
            text.push(' ');
        }
        let start = text.len();
        text.push_str(s);
        let mut run = StyleRun::new(
            start..text.len(),
            peniko_color(depth_colors[depth % depth_colors.len()]),
        );
        run.mono = true;
        runs.push(run);
    }
    if !text.is_empty() {
        let layout = engine.build_line(
            &text,
            scale,
            font_size,
            UI_LINE_HEIGHT,
            peniko_color(theme.comment),
            None,
            &runs,
        );
        engine.draw_line(
            scene,
            &layout,
            (bar.x0 as f32 + pad, center_top(bar, layout.height())),
        );
    }

    // --- right: H-level · Ln,Col · lines · scroll ---
    let scroll = if info.total_lines <= 1
        || (info.first_visible == 0 && info.last_visible + 1 >= info.total_lines)
    {
        "All".to_string()
    } else if info.first_visible == 0 {
        "Top".to_string()
    } else if info.last_visible + 1 >= info.total_lines {
        "Bot".to_string()
    } else {
        format!(
            "{}%",
            (info.last_visible + 1) * 100 / info.total_lines.max(1)
        )
    };
    let mut segments = Vec::new();
    if let Some(l) = info.heading_level {
        segments.push(format!("H{l}"));
    }
    segments.push(format!("Ln {}, Col {}", info.cursor_line, info.cursor_col));
    segments.push(format!("{} lines", info.total_lines));
    segments.push(scroll);
    let right = segments.join("  ·  ");
    let layout = engine.build_line(
        &right,
        scale,
        font_size,
        UI_LINE_HEIGHT,
        peniko_color(theme.comment),
        None,
        &[],
    );
    let x = bar.x1 as f32 - pad - layout.width();
    engine.draw_line(scene, &layout, (x, center_top(bar, layout.height())));
}
