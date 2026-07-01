//! A Typora-style markdown editor component for GPUI.
//!
//! Writ provides an embeddable markdown editor with live inline rendering—markers
//! like `**`, `#`, and `-` are hidden when the cursor is elsewhere, showing only
//! the styled result.
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
//! use writ::{Editor, EditorConfig};
//!
//! // Create with default config
//! let editor = cx.new(|cx| Editor::new("# Hello", cx));
//!
//! // Or with custom config
//! let config = EditorConfig::default();
//! let editor = cx.new(|cx| Editor::with_config("# Hello", config, cx));
//! ```

pub use editor::{Direction, Editor, EditorAction, EditorConfig, EditorTheme};

pub mod buffer;
pub mod config;
pub mod core;
pub mod cursor;
pub mod demo;
pub mod diff;
pub mod doc_layout;
pub mod editor;
pub mod git;
pub mod github;
pub mod highlight;
pub mod http;
pub mod inline;
pub mod line;
pub mod marker;
pub mod parser;
pub mod paste;
pub mod render;
pub mod shell;
pub mod status_bar;
pub mod text_engine;
pub mod title_bar;
pub mod window;
