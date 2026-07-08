//! A hybrid markdown editor: raw text editing with live inline rendering (markers
//! like `**`, `#`, and `-` are hidden when the cursor is elsewhere) plus an inline
//! git diff against HEAD, rendered on winit + wgpu + Vello + Parley.
//!
//! writ ships as both a desktop application (the `writ` binary) and a library. The
//! library's centerpiece is [`MarkdownView`] — a headless, render-only markdown
//! renderer you can embed in your own winit/wgpu/Vello app.
//!
//! # The application
//!
//! - **Live inline rendering**: Markdown syntax is hidden when not editing
//! - **Syntax highlighting**: Code blocks with tree-sitter based highlighting
//! - **Smart continuation**: Shift+Enter continues lists, blockquotes, etc.
//! - **Inline git diff**: Renders the working file's diff against git HEAD, refreshed
//!   live as the file *or* HEAD changes (an external commit updates the diff too)
//!
//! ```ignore
//! // `writ --file notes.md` — the shell reads the path from the CLI and runs the app.
//! writ::shell::run()?;
//! ```
//!
//! # Using writ as a library
//!
//! [`MarkdownView`] turns markdown into a Vello [`Scene`](vello::Scene) (a CPU display
//! list) with no window, GPU device, editor, or diff machinery — the consumer owns its
//! surface and rasterizes the scene however it likes. It supports **streaming**: feed
//! content incrementally with [`MarkdownView::push_str`] (e.g. tokens from an LLM) and
//! re-render.
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
//!
//! See `examples/streaming_markdown.rs` for an end-to-end headless render to PNG (via
//! [`rasterize_scene_to_png`]). For image support the component stays IO-free: it lists
//! referenced URLs via [`MarkdownView::image_urls`] and you push decoded bytes back in
//! with [`MarkdownView::set_image_bytes`].
//!
//! # Feature flags
//!
//! Depend on writ with `default-features = false` to get just the renderer without the
//! app's editor/git/GitHub/networking stack:
//!
//! ```toml
//! writ = { version = "0.16", default-features = false }
//! ```
//!
//! - **(none)** — the render-only base: [`MarkdownView`], [`rasterize_scene_to_png`],
//!   the diff algorithm, layout, and syntax highlighting. Pulls no winit/tokio/reqwest/
//!   gix/notify.
//! - **`editor`** — the headless `Editor` orchestration hub (git diff vs HEAD, GitHub
//!   ref validation, file watching). Enables the `core` module.
//! - **`app`** (default) — the full desktop application: the winit `shell`, clipboard,
//!   config file, mermaid + math + remote images. Implies `editor`.
//! - Individual toggles: `git`, `github`, `watch`, `remote-images`, `mermaid`, `math`.

#[cfg(feature = "editor")]
pub use core::Editor;
pub use editor::{Direction, EditorTheme};
#[cfg(feature = "remote-images")]
pub use image_load::RepaintSignal;
pub use markdown_view::MarkdownView;
pub use raster::rasterize_scene_to_png;

pub mod buffer;
pub mod callout;
#[cfg(feature = "editor")]
pub mod chrome;
pub mod config;
pub mod consts;
#[cfg(feature = "editor")]
pub mod core;
pub mod cursor;
pub mod diff;
pub mod doc_layout;
pub mod editor;
pub mod fold;
#[cfg(feature = "git")]
pub mod git;
#[cfg(feature = "github")]
pub mod github;
pub mod highlight;
pub mod image_cache;
#[cfg(feature = "remote-images")]
pub mod image_load;
pub mod inline;
pub mod markdown_view;
pub mod marker;
#[cfg(feature = "math")]
pub mod math;
#[cfg(feature = "mermaid")]
pub mod mermaid;
#[cfg(feature = "editor")]
pub mod outline;
#[cfg(feature = "app")]
pub mod overlay;
pub mod parser;
pub mod paste;
pub mod raster;
pub mod render;
pub mod segment_map;
#[cfg(feature = "app")]
pub mod shell;
pub mod status_bar;
pub mod table;
pub mod text_engine;
pub mod text_input;
pub mod tokenize;
pub mod validation;
