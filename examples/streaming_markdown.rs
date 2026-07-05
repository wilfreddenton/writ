//! Headless streaming demo: feed a markdown document into a [`MarkdownView`] in
//! successive chunks (as an LLM's output would arrive), render the result into a Vello
//! `Scene`, and rasterize it to a PNG — no window, no editor. Run with:
//!
//! ```sh
//! WGPU_BACKEND=vulkan cargo run --example streaming_markdown -- out.png
//! ```

use vello::Scene;

use writ::{MarkdownView, rasterize_scene_to_png};

/// The demo document, split into the chunks a streaming producer would emit.
const CHUNKS: &[&str] = &[
    "# Streaming Markdown\n\n",
    "This page was rendered **headlessly**: content arrived in pieces via `push_str`, \
     just like tokens from a language model.\n\n",
    "## What it exercises\n\n",
    "- Headings and paragraphs\n- *Emphasis* and `inline code`\n- Bulleted lists\n\n",
    "> Blockquotes render with a gutter bar, so nested context stays readable.\n\n",
    "```rust\nfn main() {\n    let mut view = MarkdownView::new();\n    \
     view.push_str(\"# Hello\\n\");\n}\n```\n",
];

fn main() -> anyhow::Result<()> {
    let width = 900u32;
    let height = 1200u32;
    let scale = 2.0f32;

    let mut view = MarkdownView::new();
    for chunk in CHUNKS {
        view.push_str(chunk);
    }

    let mut scene = Scene::new();
    view.render(&mut scene, width as f32, height as f32, scale);

    let bg = view.theme().background;
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "./streaming_markdown.png".to_string());
    rasterize_scene_to_png(&scene, width, height, bg, &path)?;
    eprintln!("wrote {path} ({width}x{height})");
    Ok(())
}
