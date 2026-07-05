//! Headless, render-only markdown view: turns markdown text into a Vello [`Scene`]
//! (a CPU display list) with no window, GPU device, editor, or diff machinery. It is
//! the reusable core of writ's renderer, meant to be embedded in another app that owns
//! its own surface and rasterizes the scene however it likes.
//!
//! Feed content in one of two ways: [`MarkdownView::set_markdown`] replaces the whole
//! document, and [`MarkdownView::push_str`] appends at the end — the streaming entry
//! point for incrementally growing output (e.g. tokens from an LLM). Then call
//! [`MarkdownView::render`] to draw the document body into a [`Scene`]. The consumer is
//! responsible for creating the GPU device/renderer and painting the scene to pixels.
//!
//! ```ignore
//! use vello::Scene;
//! use writ::MarkdownView;
//!
//! let mut view = MarkdownView::new();
//! view.push_str("# Streaming\n\n");
//! view.push_str("More text arrives later.\n");
//!
//! let mut scene = Scene::new();
//! view.render(&mut scene, 800.0, 600.0, 1.0); // scene is now a display list
//! // ... hand `scene` to a vello::Renderer to rasterize onto your own surface.
//! ```

use vello::Scene;
use vello::kurbo::{Affine, Rect};
use vello::peniko::Fill;

use crate::buffer::Buffer;
use crate::consts::{FONT_SIZE, LINE_HEIGHT, PADDING};
use crate::doc_layout::{DocLayout, HeightCache, LayoutParams, LineCache, RenderCache};
use crate::editor::EditorTheme;
use crate::image_cache::ImageCache;
use crate::text_engine::{TextEngine, peniko_color};

/// A headless markdown renderer. Holds the render-only subset of writ's document
/// engine (text shaping, layout/render caches, theme, buffer, and the laid-out
/// document) without any editor, diff, GitHub, IME, or windowing state.
pub struct MarkdownView {
    text_engine: TextEngine,
    line_cache: LineCache,
    render_cache: RenderCache,
    height_cache: HeightCache,
    theme: EditorTheme,
    buffer: Buffer,
    doc: Option<DocLayout>,
    images: ImageCache,
    scroll_y: f32,
    /// The `(width, scale)` the current `doc` was built at, so `render` can detect a
    /// size change and relayout. `None` whenever `doc` is stale/absent.
    laid_out: Option<(f32, f32)>,
}

impl MarkdownView {
    /// An empty view with the Dracula theme.
    pub fn new() -> Self {
        Self::with_theme(EditorTheme::dracula())
    }

    pub fn with_theme(theme: EditorTheme) -> Self {
        Self {
            text_engine: TextEngine::new(),
            line_cache: LineCache::new(),
            render_cache: RenderCache::new(),
            height_cache: HeightCache::new(),
            theme,
            buffer: Buffer::new(),
            doc: None,
            images: ImageCache::new(),
            scroll_y: 0.0,
            laid_out: None,
        }
    }

    /// Replace the entire document with `text`, discarding any prior content, layout,
    /// and scroll position.
    pub fn set_markdown(&mut self, text: &str) {
        self.buffer = Buffer::new();
        self.buffer.insert(0, text, 0);
        self.doc = None;
        self.laid_out = None;
        self.scroll_y = 0.0;
    }

    /// Append `text` to the end of the document (the streaming entry point). Invalidates
    /// the built layout so the next `layout`/`render` rebuilds; scroll is preserved.
    pub fn push_str(&mut self, text: &str) {
        let end = self.buffer.len_bytes();
        self.buffer.insert(end, text, 0);
        self.doc = None;
        self.laid_out = None;
    }

    /// Lay out the whole document at `width`/`scale` (a single anchor-0, infinite-
    /// viewport build, so every line is materialized). Render-only: no cursor, diff,
    /// GitHub, or IME data.
    pub fn layout(&mut self, width: f32, scale: f32) {
        let version = self.buffer.version();
        let snapshot = self.buffer.render_snapshot();
        let params = LayoutParams {
            device_width: width,
            scale,
            pad_x: PADDING,
            pad_top: PADDING,
            pad_bottom: PADDING * 2.0,
            base_font_size: FONT_SIZE,
            line_height: LINE_HEIGHT,
            fg: peniko_color(self.theme.foreground),
        };
        // `usize::MAX` is a safe "no cursor" sentinel: `build` clamps it to `len_bytes`
        // for the caret-line lookup, and `cursor_key_for` never reports any line as
        // holding the cursor (so no marker is revealed), which is exactly what we want.
        let doc = DocLayout::build(
            &mut self.text_engine,
            &mut self.line_cache,
            &mut self.render_cache,
            &mut self.height_cache,
            version,
            &snapshot,
            &self.theme,
            None,
            None,
            &self.images,
            usize::MAX,
            &params,
            None,
            0,
            f32::INFINITY,
        );
        self.doc = Some(doc);
        self.laid_out = Some((width, scale));
    }

    /// The full content height of the built document (0 if nothing is laid out yet).
    pub fn content_height(&self) -> f32 {
        self.doc.as_ref().map(|d| d.content_height()).unwrap_or(0.0)
    }

    /// Set the absolute scroll offset. The lower bound is clamped here; the upper bound
    /// is clamped in `scroll_by`/`render`, which know the viewport height.
    pub fn set_scroll(&mut self, y: f32) {
        self.scroll_y = y.max(0.0);
        if let Some(doc) = self.doc.as_mut() {
            doc.scroll_y = self.scroll_y;
        }
    }

    /// Scroll by `dy`, clamped to `[0, max_scroll]` for `viewport_h` (via the doc).
    pub fn scroll_by(&mut self, dy: f32, viewport_h: f32) {
        if let Some(doc) = self.doc.as_mut() {
            doc.scroll_y = self.scroll_y;
            doc.scroll_by(dy, viewport_h);
            self.scroll_y = doc.scroll_y;
        } else {
            self.scroll_y = (self.scroll_y + dy).max(0.0);
        }
    }

    pub fn theme(&self) -> &EditorTheme {
        &self.theme
    }

    /// Draw the document body into `scene`, relaying out first if the target size
    /// changed since the last build. Draws backgrounds, quote gutters, rules, images,
    /// and glyphs — no caret, selection, status bar, or other chrome. `height` is the
    /// viewport extent used for scroll clamping and off-screen culling.
    pub fn render(&mut self, scene: &mut Scene, width: f32, height: f32, scale: f32) {
        if self.laid_out != Some((width, scale)) || self.doc.is_none() {
            self.layout(width, scale);
        }
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        doc.scroll_y = self.scroll_y;
        doc.clamp_scroll(height);
        self.scroll_y = doc.scroll_y;

        // Mirror `paint_document`: clip to the viewport so lines partially scrolled past
        // the top/bottom edges don't bleed outside it.
        let clip = Rect::new(0.0, 0.0, width as f64, height as f64);
        scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
        doc.draw_added_backgrounds(scene, height);
        doc.draw_blockquote_gutters(scene, height);
        doc.draw_horizontal_rules(scene, height);
        doc.draw_images(&mut self.text_engine, scene, height);
        doc.draw(&self.text_engine, scene, height);
        scene.pop_layer();
    }
}

impl Default for MarkdownView {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_grows_content_height() {
        let mut view = MarkdownView::new();
        view.push_str("# Hello\n\n");
        view.layout(800.0, 1.0);
        let first = view.content_height();

        view.push_str("more paragraph text\n\nand another\n");
        view.layout(800.0, 1.0);
        let second = view.content_height();

        assert!(
            second > first,
            "streaming append should grow content height: {first} -> {second}"
        );
    }

    #[test]
    fn set_markdown_replaces_content() {
        let mut view = MarkdownView::new();
        view.set_markdown("# A heading\n\nwith a paragraph beneath it\n");
        view.layout(800.0, 1.0);
        let full = view.content_height();
        assert!(full > 0.0);

        view.set_markdown("");
        view.layout(800.0, 1.0);
        let empty = view.content_height();
        assert!(empty < full, "empty doc should be shorter: {empty} vs {full}");

        view.set_markdown("# A\n");
        view.layout(800.0, 1.0);
        assert!(view.content_height() > 0.0);
    }

    #[test]
    fn render_into_scene_does_not_panic() {
        let mut view = MarkdownView::new();
        view.push_str("# Title\n\n");
        view.push_str("- one\n- two\n\n");
        view.push_str("> a quote\n\n");
        view.push_str("```rust\nfn main() {}\n```\n");

        let mut scene = Scene::new();
        view.render(&mut scene, 800.0, 600.0, 1.0);
    }

    #[test]
    fn render_relayouts_on_width_change() {
        let mut view = MarkdownView::new();
        view.push_str("A longer paragraph that will wrap differently at narrow and wide widths so the two layouts genuinely differ.\n");

        let mut scene = Scene::new();
        view.render(&mut scene, 1200.0, 600.0, 1.0);

        let mut scene = Scene::new();
        view.render(&mut scene, 400.0, 600.0, 1.0);

        assert_eq!(view.laid_out, Some((400.0, 1.0)));
        // A narrower width wraps more, so the doc is at least as tall as the wide one.
        assert!(view.content_height() > 0.0);
    }
}
