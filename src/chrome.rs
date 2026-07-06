//! Window chrome for the new shell (see MIGRATION-PLAN.md, Phase 6): an in-window
//! title bar (filename + dirty marker) and a status bar (nesting context + cursor
//! position). Drawn as opaque strips above/below the editor, which is inset between
//! them. CSD (custom window frame) and the async-blocked overlays are deferred.

use vello::Scene;
use vello::kurbo::{Affine, Line, Rect, RoundedRect, Stroke};
use vello::peniko::{Color, Fill};

use crate::consts::{PADDING, UI_LINE_HEIGHT};
use crate::editor::EditorTheme;
use crate::marker::MarkerKind;
use crate::status_bar::build_context_display;
use crate::text_engine::{StyleRun, TextEngine, peniko_color};

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
