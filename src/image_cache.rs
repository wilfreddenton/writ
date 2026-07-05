//! Shared, cheaply-cloneable cache of decoded images for standalone-image rendering.
//!
//! Mirrors `validation::GitHubValidationCache`: an `Arc<Mutex<HashMap>>` keyed by the raw
//! URL string. Loads (local file reads or remote fetches) run on worker tasks that
//! write their result here and wake the winit loop; the layout/draw path reads it.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use resvg::tiny_skia;
use resvg::usvg::{self, fontdb};
use vello::peniko::{Blob, ImageAlphaType, ImageBrush, ImageData, ImageFormat};

/// Supersample factor for SVG rasterization: render at 2× the logical size so the
/// vector art stays crisp on hidpi, then downscale to the logical dest at draw time.
const SVG_SUPERSAMPLE: f32 = 2.0;
/// Cap on the rasterized SVG pixel dimensions, so a large SVG doesn't blow up memory.
const SVG_MAX_RASTER: f32 = 4096.0;

/// System-font database, loaded once (the load is expensive) and shared across every
/// SVG parse so badge `<text>` labels resolve to real fonts.
static SVG_FONTS: LazyLock<Arc<fontdb::Database>> = LazyLock::new(|| {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    Arc::new(db)
});

/// A decoded image ready to paint, plus its raster pixel size and logical display
/// size. For raster sources the two match; for SVG the raster size is supersampled
/// while the display size is the SVG's intrinsic logical size (see `decode_svg`).
/// `Arc`-wrapped in the cache so a cache clone stays O(1) (the `Blob` is Arc-backed
/// too, but the `Arc<LoadedImage>` keeps the whole entry a single refcount bump).
pub struct LoadedImage {
    pub brush: ImageBrush,
    pub width: u32,
    pub height: u32,
    pub display_w: f32,
    pub display_h: f32,
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

    /// Presence check without cloning the (image-bearing) state — for the load guard.
    pub fn contains(&self, url: &str) -> bool {
        self.inner.lock().unwrap().contains_key(url)
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

/// Decode encoded image bytes into a paintable `LoadedImage`. Tries the raster path
/// (PNG/JPEG/GIF/WebP) first, then falls back to SVG. Returns `None` on any failure.
/// The RGBA8 buffer from the `image` crate is straight (un-premultiplied) alpha,
/// matching `ImageAlphaType::Alpha`.
pub fn decode(bytes: &[u8]) -> Option<LoadedImage> {
    if let Ok(img) = image::load_from_memory(bytes) {
        let rgba = img.to_rgba8();
        let width = rgba.width();
        let height = rgba.height();
        let data = ImageData {
            data: Blob::new(Arc::new(rgba.into_raw())),
            format: ImageFormat::Rgba8,
            alpha_type: ImageAlphaType::Alpha,
            width,
            height,
        };
        return Some(LoadedImage {
            brush: ImageBrush::new(data),
            width,
            height,
            display_w: width as f32,
            display_h: height as f32,
        });
    }
    looks_like_svg(bytes).then(|| decode_svg(bytes)).flatten()
}

/// Cheap sniff for SVG: after trimming a UTF-8 BOM and leading whitespace, the bytes
/// should start with `<?xml` or `<svg`, or contain `<svg` within the first ~1 KiB.
/// (Gzipped `.svgz` is out of scope and won't match.)
fn looks_like_svg(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let trimmed = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map_or(&[][..], |i| &bytes[i..]);
    if trimmed.starts_with(b"<?xml") || trimmed.starts_with(b"<svg") {
        return true;
    }
    let head = &trimmed[..trimmed.len().min(1024)];
    head.windows(4).any(|w| w == b"<svg")
}

/// Rasterize an SVG at `SVG_SUPERSAMPLE`× its logical size (capped) so it stays crisp
/// on hidpi. `tiny_skia`'s pixmap is premultiplied RGBA8, hence `AlphaPremultiplied`
/// (unlike the raster path's straight `Alpha`).
fn decode_svg(bytes: &[u8]) -> Option<LoadedImage> {
    let opt = usvg::Options {
        fontdb: SVG_FONTS.clone(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_data(bytes, &opt).ok()?;
    let size = tree.size();
    let (dw, dh) = (size.width(), size.height());
    if dw <= 0.0 || dh <= 0.0 {
        return None;
    }
    // Clamp the supersample so neither raster dimension exceeds SVG_MAX_RASTER.
    let k = SVG_SUPERSAMPLE.min(SVG_MAX_RASTER / dw.max(dh));
    let rw = ((dw * k).round() as u32).max(1);
    let rh = ((dh * k).round() as u32).max(1);
    let mut pixmap = tiny_skia::Pixmap::new(rw, rh)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(k, k),
        &mut pixmap.as_mut(),
    );
    let data = ImageData {
        data: Blob::new(Arc::new(pixmap.data().to_vec())),
        format: ImageFormat::Rgba8,
        alpha_type: ImageAlphaType::AlphaPremultiplied,
        width: rw,
        height: rh,
    };
    Some(LoadedImage {
        brush: ImageBrush::new(data),
        width: rw,
        height: rh,
        display_w: dw,
        display_h: dh,
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
    fn decode_rasterizes_svg() {
        let svg = r#"<svg xmlns='http://www.w3.org/2000/svg' width='40' height='20'><rect width='40' height='20' fill='#4c1'/></svg>"#;
        let loaded = decode(svg.as_bytes()).expect("inline SVG should decode");
        assert!((loaded.display_w - 40.0).abs() < 0.5);
        assert!((loaded.display_h - 20.0).abs() < 0.5);
        // 2× supersample → raster is ~80×40.
        assert_eq!((loaded.width, loaded.height), (80, 40));
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
