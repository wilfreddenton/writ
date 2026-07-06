//! Mermaid diagram rendering: detect a ```` ```mermaid ```` fence, render its source to
//! SVG via `mermaid-rs-renderer`, and route the SVG through writ's existing image
//! pipeline (`image_cache::decode` → `LoadedImage` → the standalone-image draw path).
//! The diagram is cached under a synthetic `mermaid:<hash>` key so it renders once per
//! distinct source, not per frame; rendering runs off the UI thread.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};

use mermaid_rs_renderer::{RenderOptions, Theme, render_with_options};

use crate::image_cache::{ImageCache, LoadedImage, decode};

/// A ```` ```mermaid ```` fence in the document. `block` is the whole fenced range (for
/// the reveal-on-cursor test); `content` is the diagram source; `anchor_line` is the
/// fence's first line, which carries the drawn image (other block lines collapse to 0).
pub struct MermaidBlock {
    pub block: Range<usize>,
    pub content: Range<usize>,
    pub anchor_line: usize,
}

/// Synthetic cache key for a diagram, derived from its source only — the same source
/// reuses one decode across widths (the image draw path fits-to-width, see `decode_svg`).
pub fn key_for(source: &str) -> String {
    let mut h = DefaultHasher::new();
    source.hash(&mut h);
    format!("mermaid:{:016x}", h.finish())
}

/// Render mermaid source to a paintable image: source → SVG string → `decode`. Returns
/// `None` on a render error, a panic (bad input can panic the renderer), or an SVG that
/// won't decode.
fn render_to_image(source: &str) -> Option<LoadedImage> {
    // Render in mermaid's dark theme so diagrams sit in the dark editor instead of on a
    // glaring white canvas.
    let opts = RenderOptions {
        theme: Theme::dark(),
        ..Default::default()
    };
    let svg = catch_unwind(AssertUnwindSafe(|| render_with_options(source, opts)))
        .ok()?
        .ok()?;
    decode(svg.as_bytes())
}

/// For each `(key, source)` with no cache entry, mark it loading and render on a worker
/// thread (mermaid layout is CPU-heavy — never on the render thread), then `notify()` so
/// the event-driven shell rebuilds around the now-known diagram size. Mirrors
/// `image_load::spawn_image_loads`, but the "load" is a local CPU render, so a plain OS
/// thread suffices (no async runtime).
pub fn spawn_mermaid_renders(
    sources: &[(String, String)],
    cache: &ImageCache,
    notify: impl Fn() + Send + Clone + 'static,
) {
    for (key, source) in sources {
        if cache.contains(key) {
            continue; // already loading/loaded/failed
        }
        cache.mark_loading(key);
        let cache = cache.clone();
        let notify = notify.clone();
        let key = key.clone();
        let source = source.clone();
        std::thread::spawn(move || {
            match render_to_image(&source) {
                Some(img) => cache.set_loaded(&key, img),
                None => cache.set_failed(&key),
            }
            notify();
        });
    }
}

/// Synchronously render diagrams into the cache for the headless snapshot frame.
pub fn render_mermaid_blocking(sources: &[(String, String)], cache: &ImageCache) {
    for (key, source) in sources {
        if cache.contains(key) {
            continue;
        }
        match render_to_image(source) {
            Some(img) => cache.set_loaded(key, img),
            None => cache.set_failed(key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_cache::ImageState;

    #[test]
    fn renders_diagram_into_cache() {
        let src = "flowchart TD\n  A[Start] --> B[End]";
        let key = key_for(src);
        let cache = ImageCache::new();
        render_mermaid_blocking(&[(key.clone(), src.to_string())], &cache);
        assert!(
            matches!(cache.get(&key), Some(ImageState::Loaded(_))),
            "a valid flowchart should render to SVG and decode"
        );
    }

    #[test]
    fn bad_source_fails_without_panicking() {
        let src = "%%%% definitely not mermaid %%%%";
        let key = key_for(src);
        let cache = ImageCache::new();
        render_mermaid_blocking(&[(key.clone(), src.to_string())], &cache);
        // Must not panic; the entry resolves to some state (Failed, or an error diagram).
        assert!(cache.get(&key).is_some());
    }

    #[test]
    fn key_is_stable_per_source() {
        assert_eq!(key_for("graph LR; A-->B"), key_for("graph LR; A-->B"));
        assert_ne!(key_for("graph LR; A-->B"), key_for("graph LR; A-->C"));
    }
}
