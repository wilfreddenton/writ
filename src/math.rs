//! LaTeX math rendering: turn `$…$` / `$$…$$` source into a rasterized image via RaTeX
//! (parse → layout → tiny-skia PNG, with the KaTeX fonts embedded), then route it through
//! writ's existing image pipeline (`image_cache::decode` → `LoadedImage` → the standalone-
//! /inline-image draw path). Glyph color is set to the theme foreground at layout time so
//! math is legible on the dark editor; the background is transparent. Rendering runs off
//! the UI thread, cached by a key over `(latex, display, size, color)`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};

use ratex_layout::{LayoutOptions, layout, to_display_list};
use ratex_parser::parser::parse;
use ratex_render::{RenderOptions, render_to_png};
use ratex_types::color::Color;
use ratex_types::math_style::MathStyle;

use crate::image_cache::{ImageCache, LoadedImage, decode};

/// Logical-px padding baked around the glyphs so ascenders/descenders don't clip.
const PAD: f32 = 2.0;
/// Keep the raster under Vello's per-frame large-image cliff (see `image_cache::SVG_MAX_RASTER`).
const MAX_RASTER: f32 = 900.0;

/// A `$$…$$` display-math block. `block` is the whole delimited span (for the reveal
/// test); `content` is the LaTeX source; `anchor_line` is the first line, which carries
/// the rendered image (other block lines collapse to zero height).
pub struct MathBlock {
    pub block: Range<usize>,
    pub content: Range<usize>,
    pub anchor_line: usize,
}

/// A request to render one math span.
#[derive(Clone)]
pub struct MathJob {
    pub latex: String,
    /// `$$` display style (large, centered) vs `$` inline text style.
    pub display: bool,
    /// Body/heading font size the math should match, in logical px.
    pub font_px: f32,
    /// Device pixel ratio (render at this × for hidpi crispness).
    pub scale: f32,
    /// Glyph color (theme foreground) as linear-ish RGB in 0..1.
    pub fg: (f32, f32, f32),
}

/// Cache key for a math render. Includes display/size/color so inline vs block and a theme
/// switch don't collide on one entry.
pub fn key_for(job: &MathJob) -> String {
    let mut h = DefaultHasher::new();
    job.latex.hash(&mut h);
    job.display.hash(&mut h);
    job.font_px.to_bits().hash(&mut h);
    job.scale.to_bits().hash(&mut h);
    let (r, g, b) = job.fg;
    (r.to_bits(), g.to_bits(), b.to_bits()).hash(&mut h);
    format!("math:{:016x}", h.finish())
}

/// Longest error kept for the placeholder — enough for a RaTeX parse error without letting a
/// pathological message blow up the box.
const MAX_ERR_LEN: usize = 200;

/// Collapse whitespace and cap length so a parse error fits the placeholder box.
fn truncate_err(msg: &str) -> String {
    let one_line = msg.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(MAX_ERR_LEN) {
        Some((byte, _)) => format!("{}…", &one_line[..byte]),
        None => one_line,
    }
}

/// Render math to a paintable image. `Ok(img)` on success; `Err(Some(msg))` on a LaTeX parse
/// error (shown in the block-math placeholder so the author sees the mistake); `Err(None)` on
/// a panic, empty result, or decode/render failure. The returned image's `display_w/display_h`
/// are LOGICAL px (raster ÷ dpr, matching the SVG-decode convention so `build_image_block`/
/// inline sizing scale it right on hidpi), and `baseline` is the logical px from the image top
/// to the math baseline (for inline placement).
fn render_to_image(job: &MathJob) -> Result<LoadedImage, Option<String>> {
    let out = catch_unwind(AssertUnwindSafe(
        || -> Result<LoadedImage, Option<String>> {
            let nodes = parse(&job.latex).map_err(|e| Some(truncate_err(&e.to_string())))?;
            let (r, g, b) = job.fg;
            let opts = LayoutOptions {
                style: if job.display {
                    MathStyle::Display
                } else {
                    MathStyle::Text
                },
                color: Color { r, g, b, a: 1.0 },
                ..Default::default()
            };
            let dl = to_display_list(&layout(&nodes, &opts));
            let total_h = (dl.height + dl.depth) as f32;
            if dl.width <= 0.0 || total_h <= 0.0 {
                return Err(None);
            }
            let em = job.font_px;
            // Clamp the device-pixel-ratio so neither raster dimension crosses MAX_RASTER.
            let base_w = dl.width as f32 * em + 2.0 * PAD;
            let base_h = total_h * em + 2.0 * PAD;
            let dpr = job.scale.min(MAX_RASTER / base_w.max(base_h)).max(0.01);
            let ropts = RenderOptions {
                font_size: em,
                padding: PAD,
                background_color: Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.0,
                },
                font_dir: String::new(),
                device_pixel_ratio: dpr,
            };
            let png = render_to_png(&dl, &ropts).map_err(|_| None)?;
            let mut img = decode(&png).ok_or(None)?;
            img.display_w = img.width as f32 / dpr;
            img.display_h = img.height as f32 / dpr;
            img.baseline = Some(dl.height as f32 * em + PAD);
            Ok(img)
        },
    ));
    match out {
        Ok(r) => r,
        Err(_) => Err(None), // layout/render panicked on adversarial input
    }
}

/// For each `(key, job)` with no cache entry, mark it loading and render on a worker thread
/// (parse+layout is CPU-heavy), then `notify()` so the shell relays out around the image.
/// Mirrors `mermaid::spawn_mermaid_renders`.
pub fn spawn_math_renders(
    jobs: &[(String, MathJob)],
    cache: &ImageCache,
    notify: impl Fn() + Send + Clone + 'static,
) {
    for (key, job) in jobs {
        if cache.contains(key) {
            continue;
        }
        cache.mark_loading(key);
        let cache = cache.clone();
        let notify = notify.clone();
        let key = key.clone();
        let job = job.clone();
        std::thread::spawn(move || {
            match render_to_image(&job) {
                Ok(img) => cache.set_loaded(&key, img),
                Err(Some(msg)) => cache.set_failed_with(&key, msg),
                Err(None) => cache.set_failed(&key),
            }
            notify();
        });
    }
}

/// Synchronously render math into the cache for the headless snapshot frame.
pub fn render_math_blocking(jobs: &[(String, MathJob)], cache: &ImageCache) {
    for (key, job) in jobs {
        if cache.contains(key) {
            continue;
        }
        match render_to_image(job) {
            Ok(img) => cache.set_loaded(key, img),
            Err(Some(msg)) => cache.set_failed_with(key, msg),
            Err(None) => cache.set_failed(key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_cache::ImageState;

    fn job(latex: &str, display: bool) -> MathJob {
        MathJob {
            latex: latex.to_string(),
            display,
            font_px: 16.0,
            scale: 2.0,
            fg: (0.9, 0.9, 0.9),
        }
    }

    #[test]
    fn renders_math_into_cache() {
        let j = job("x^2 + \\frac{1}{2}", true);
        let key = key_for(&j);
        let cache = ImageCache::new();
        render_math_blocking(&[(key.clone(), j)], &cache);
        assert!(
            matches!(cache.get(&key), Some(ImageState::Loaded(_))),
            "valid LaTeX should render + decode"
        );
    }

    #[test]
    fn bad_latex_fails_without_panicking() {
        let j = job("\\frac{ oops unbalanced", false);
        let key = key_for(&j);
        let cache = ImageCache::new();
        render_math_blocking(&[(key.clone(), j)], &cache);
        assert!(cache.get(&key).is_some());
    }

    #[test]
    fn key_stable_and_distinguishes_display() {
        assert_eq!(key_for(&job("x", true)), key_for(&job("x", true)));
        assert_ne!(key_for(&job("x", true)), key_for(&job("x", false)));
        assert_ne!(key_for(&job("x", true)), key_for(&job("y", true)));
    }
}
