//! Window chrome for the new shell (see MIGRATION-PLAN.md, Phase 6): an in-window
//! title bar (filename + dirty marker) and a status bar (nesting context + cursor
//! position). Drawn as opaque strips above/below the editor, which is inset between
//! them. CSD (custom window frame) and the async-blocked overlays are deferred.

use vello::Scene;
use vello::kurbo::{Affine, Line, Rect, Stroke};
use vello::peniko::{Color, Fill};

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
    (bar.y0 as f32) + ((bar.y1 - bar.y0) as f32 - font_size * 1.3) / 2.0
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
    let layout = engine.build_line(&title, scale, font_size, 1.3, color, None, &[]);
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
    fill_rect(scene, peniko_color(theme.background), bar);
    hline(
        scene,
        peniko_color(theme.selection),
        bar.x0,
        bar.x1,
        bar.y0,
        scale as f64,
    );

    let font_size = 13.0;
    let pad = 24.0 * scale;
    let y = baseline_top(bar, font_size);

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
        let mut run = StyleRun::new(start..text.len(), peniko_color(depth_colors[depth % 6]));
        run.mono = true;
        runs.push(run);
    }
    if !text.is_empty() {
        let layout = engine.build_line(
            &text,
            scale,
            font_size,
            1.3,
            peniko_color(theme.comment),
            None,
            &runs,
        );
        engine.draw_line(scene, &layout, (bar.x0 as f32 + pad, y));
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
    let heading = info
        .heading_level
        .map(|l| format!("H{l}  "))
        .unwrap_or_default();
    let right = format!(
        "{heading}Ln {}, Col {}  ·  {} lines  ·  {scroll}",
        info.cursor_line, info.cursor_col, info.total_lines
    );
    let layout = engine.build_line(
        &right,
        scale,
        font_size,
        1.3,
        peniko_color(theme.comment),
        None,
        &[],
    );
    let x = bar.x1 as f32 - pad - layout.width();
    engine.draw_line(scene, &layout, (x, y));
}
