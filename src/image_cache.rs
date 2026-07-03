//! Shared, cheaply-cloneable cache of decoded images for standalone-image rendering.
//!
//! Mirrors `github::GitHubValidationCache`: an `Arc<Mutex<HashMap>>` keyed by the raw
//! URL string. Loads (local file reads or remote fetches) run on worker tasks that
//! write their result here and wake the winit loop; the layout/draw path reads it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use vello::peniko::{Blob, ImageAlphaType, ImageBrush, ImageData, ImageFormat};

/// A decoded image ready to paint, plus its intrinsic pixel size (for aspect math).
/// `Arc`-wrapped in the cache so a cache clone stays O(1) (the `Blob` is Arc-backed
/// too, but the `Arc<LoadedImage>` keeps the whole entry a single refcount bump).
pub struct LoadedImage {
    pub brush: ImageBrush,
    pub width: u32,
    pub height: u32,
}

/// Load state for one image URL.
#[derive(Clone)]
pub enum ImageState {
    /// A load has been spawned but hasn't finished.
    Loading,
    /// Decoded and ready to paint.
    Loaded(Arc<LoadedImage>),
    /// The fetch or decode failed (missing file, network error, bad bytes).
    Failed,
}

/// Thread-safe cache of image load states, shared across clones (like the GitHub
/// caches). Cheap to clone: just bumps the inner `Arc`.
#[derive(Clone, Default)]
pub struct ImageCache {
    inner: Arc<Mutex<HashMap<String, ImageState>>>,
}

impl ImageCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, url: &str) -> Option<ImageState> {
        self.inner.lock().unwrap().get(url).cloned()
    }

    pub fn mark_loading(&self, url: &str) {
        self.inner
            .lock()
            .unwrap()
            .insert(url.to_string(), ImageState::Loading);
    }

    pub fn set_loaded(&self, url: &str, image: LoadedImage) {
        self.inner
            .lock()
            .unwrap()
            .insert(url.to_string(), ImageState::Loaded(Arc::new(image)));
    }

    pub fn set_failed(&self, url: &str) {
        self.inner
            .lock()
            .unwrap()
            .insert(url.to_string(), ImageState::Failed);
    }
}

/// Decode encoded image bytes (PNG/JPEG/GIF/WebP) into a paintable `LoadedImage`.
/// Returns `None` on any decode failure. The RGBA8 buffer from the `image` crate is
/// straight (un-premultiplied) alpha, matching `ImageAlphaType::Alpha`.
pub fn decode(bytes: &[u8]) -> Option<LoadedImage> {
    let rgba = image::load_from_memory(bytes).ok()?.to_rgba8();
    let width = rgba.width();
    let height = rgba.height();
    let data = ImageData {
        data: Blob::new(Arc::new(rgba.into_raw())),
        format: ImageFormat::Rgba8,
        alpha_type: ImageAlphaType::Alpha,
        width,
        height,
    };
    Some(LoadedImage {
        brush: ImageBrush::new(data),
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat as CrateImageFormat, RgbaImage};
    use std::io::Cursor;

    /// Encode a tiny solid RGBA image to PNG bytes for round-trip decode tests.
    fn tiny_png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), CrateImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn decode_reports_dimensions() {
        let png = tiny_png(7, 3);
        let loaded = decode(&png).expect("tiny PNG should decode");
        assert_eq!((loaded.width, loaded.height), (7, 3));
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode(b"not an image").is_none());
    }

    #[test]
    fn cache_shares_state_across_clones() {
        let cache = ImageCache::new();
        let other = cache.clone();
        assert!(cache.get("x.png").is_none());
        cache.mark_loading("x.png");
        assert!(matches!(other.get("x.png"), Some(ImageState::Loading)));
        let loaded = decode(&tiny_png(2, 2)).unwrap();
        cache.set_loaded("x.png", loaded);
        assert!(matches!(other.get("x.png"), Some(ImageState::Loaded(_))));
    }
}
