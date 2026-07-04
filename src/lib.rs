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

pub use core::Editor;
pub use editor::{Direction, EditorTheme};

pub mod buffer;
pub mod chrome;
pub mod config;
pub mod consts;
pub mod core;
pub mod cursor;
pub mod diff;
pub mod doc_layout;
pub mod editor;
pub mod git;
pub mod github;
pub mod highlight;
pub mod image_cache;
pub mod image_load;
pub mod inline;
pub mod marker;
pub mod overlay;
pub mod parser;
pub mod paste;
pub mod render;
pub mod segment_map;
pub mod shell;
pub mod status_bar;
pub mod text_engine;
