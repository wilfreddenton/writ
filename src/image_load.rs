//! Standalone-image IO: resolving URLs to local paths, reading/decoding local files,
//! fetching + decoding remote images, and the spawn/blocking entry points that feed
//! decoded images into the shared [`ImageCache`]. Kept off the shell so the god-object
//! doesn't carry the reqwest/decode plumbing.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::image_cache::{ImageCache, LoadedImage, decode};

/// Called when a background image load finishes, so an event-driven consumer can
/// schedule a repaint (an async result sits invisibly in the cache until the UI
/// redraws). Mirrors the Linebender stack's proxy convention; the concrete impl
/// wraps winit's `EventLoopProxy` (see the shell).
pub trait RepaintSignal: Clone + Send + 'static {
    fn notify(&self);
}

/// Resolve a standalone-image URL to a filesystem path: absolute paths as-is, relative
/// paths against the document's directory. Returns None when relative but the doc has
/// no directory (e.g. the in-memory sample).
fn resolve_local_image(doc_dir: Option<&Path>, url: &str) -> Option<PathBuf> {
    let path = Path::new(url.strip_prefix("file://").unwrap_or(url));
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        doc_dir.map(|d| d.join(path))
    }
}

/// One process-wide client so image fetches share a connection pool + TLS session
/// cache instead of paying a cold handshake per image (`Client` is `Arc`-backed).
static IMAGE_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

/// Read + decode a local image URL (relative paths resolved against the doc's dir).
fn load_local_image(doc_dir: Option<&Path>, url: &str) -> Option<LoadedImage> {
    resolve_local_image(doc_dir, url)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| decode(&bytes))
}

/// GET the raw bytes of a remote image (async, on the reactor). Logs a non-2xx status or
/// transport error. Decoding is deliberately separate so it can run off the reactor.
async fn fetch_remote_image(url: &str) -> Option<Vec<u8>> {
    let resp = match IMAGE_CLIENT
        .get(url)
        .header("User-Agent", "writ")
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[writ] image fetch failed ({url}): {e}");
            return None;
        }
    };
    resp.bytes().await.ok().map(|b| b.to_vec())
}

/// Decode image bytes (CPU-heavy — run on a blocking thread, not the async reactor).
/// Logs an undecodable body (e.g. SVG, which the `image` crate can't rasterize).
fn decode_image_bytes(url: &str, bytes: &[u8]) -> Option<LoadedImage> {
    match decode(bytes) {
        Some(img) => Some(img),
        None => {
            eprintln!(
                "[writ] image decode failed ({url}): {} bytes, unsupported format (SVG is not supported)",
                bytes.len()
            );
            None
        }
    }
}

/// GET + decode a remote image inline. Used by the blocking snapshot path; the live GUI
/// path fetches then decodes on a blocking thread (see `spawn_image_loads`).
async fn load_remote_image(url: &str) -> Option<LoadedImage> {
    let bytes = fetch_remote_image(url).await?;
    decode_image_bytes(url, &bytes)
}

/// For each standalone-image URL with no cache entry, mark it loading and spawn a
/// worker: remote `http(s)` via reqwest on the async runtime, else a local file read +
/// decode on a blocking task (so IO/decoding doesn't stall the runtime). Each finish
/// calls `signal.notify()` so an event-driven consumer rebuilds around the now-known height.
pub fn spawn_image_loads(
    doc_dir: Option<PathBuf>,
    urls: &[String],
    cache: &ImageCache,
    runtime: &tokio::runtime::Handle,
    signal: impl RepaintSignal,
) {
    for url in urls {
        if cache.contains(url) {
            continue; // already loading/loaded/failed
        }
        cache.mark_loading(url);
        let cache = cache.clone();
        let signal = signal.clone();
        let url = url.clone();
        if url.starts_with("http://") || url.starts_with("https://") {
            runtime.spawn(async move {
                // Fetch on the reactor, but decode on a blocking thread so CPU-heavy
                // rasterization doesn't stall the async workers shared with GitHub validation.
                let loaded = match fetch_remote_image(&url).await {
                    Some(bytes) => {
                        let u = url.clone();
                        tokio::task::spawn_blocking(move || decode_image_bytes(&u, &bytes))
                            .await
                            .ok()
                            .flatten()
                    }
                    None => None,
                };
                match loaded {
                    Some(img) => cache.set_loaded(&url, img),
                    None => cache.set_failed(&url),
                }
                signal.notify();
            });
        } else {
            let doc_dir = doc_dir.clone();
            runtime.spawn_blocking(move || {
                match load_local_image(doc_dir.as_deref(), &url) {
                    Some(img) => cache.set_loaded(&url, img),
                    None => cache.set_failed(&url),
                }
                signal.notify();
            });
        }
    }
}

/// Synchronously decode standalone images into the cache for the headless snapshot
/// frame: local reads (relative to the doc's directory) plus remote `http(s)` fetched
/// by blocking on the current runtime, so the golden frame reflects the real result.
pub fn load_local_images_blocking(doc_dir: Option<&Path>, urls: &[String], cache: &ImageCache) {
    for url in urls {
        let loaded = if url.starts_with("http://") || url.starts_with("https://") {
            tokio::runtime::Handle::current().block_on(load_remote_image(url))
        } else {
            load_local_image(doc_dir, url)
        };
        match loaded {
            Some(img) => cache.set_loaded(url, img),
            None => cache.set_failed(url),
        }
    }
}
