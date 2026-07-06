//! A hybrid markdown editor: raw text editing with live inline rendering (markers
//! like `**`, `#`, and `-` are hidden when the cursor is elsewhere) plus an inline
//! git diff against HEAD, rendered on winit + wgpu + Vello + Parley.
//!
//! # Features
//!
//! - **Live inline rendering**: Markdown syntax is hidden when not editing
//! - **Syntax highlighting**: Code blocks with tree-sitter based highlighting
//! - **Smart continuation**: Shift+Enter continues lists, blockquotes, etc.
//! - **Inline git diff**: Renders the working file's diff against git HEAD
//!
//! # Quick Start
//!
//! ```ignore
//! let editor = writ::core::Editor::open(std::path::Path::new("notes.md"));
//! writ::shell::run()?; // opens a window and edits the file from --file
//! ```

#[cfg(feature = "editor")]
pub use core::Editor;
pub use editor::{Direction, EditorTheme};
#[cfg(feature = "remote-images")]
pub use image_load::RepaintSignal;
pub use markdown_view::MarkdownView;
pub use raster::rasterize_scene_to_png;

pub mod buffer;
pub mod chrome;
pub mod config;
pub mod consts;
#[cfg(feature = "editor")]
pub mod core;
pub mod cursor;
pub mod diff;
pub mod doc_layout;
pub mod editor;
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
pub mod validation;
