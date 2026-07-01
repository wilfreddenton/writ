# writ

[![CI](https://github.com/wilfreddenton/writ/actions/workflows/ci.yml/badge.svg)](https://github.com/wilfreddenton/writ/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/writ.svg)](https://crates.io/crates/writ)
[![docs.rs](https://docs.rs/writ/badge.svg)](https://docs.rs/writ)

A hybrid markdown editor combining raw text editing with live inline rendering.

## Install

```bash
cargo install writ --locked
```

## Usage

```bash
writ --file path/to/document.md
```

Run without arguments to open a built-in sample document.

writ is rendered on [winit](https://github.com/rust-windowing/winit) + [wgpu](https://wgpu.rs) + [Vello](https://github.com/linebender/vello) + [Parley](https://github.com/linebender/parley) (GPU 2D rendering and text layout). On Linux/Mesa (including Asahi) select the Vulkan backend with `WGPU_BACKEND=vulkan`. Fonts are resolved via [Fontique](https://github.com/linebender/parley) with fontconfig loaded at runtime — no `-dev` headers needed.

### Inline git diff

When the open file lives in a git repository, writ renders a live inline diff against `HEAD`: added lines and words are tinted green, deleted lines appear as red "ghost" rows above their position, all with the same markdown rendering as the rest of the document. writ watches the file, so edits made by an external tool (e.g. an AI agent) reload and re-diff live.

### GhostText Integration

writ can edit browser textareas via the [GhostText](https://ghosttext.fregante.com/) protocol. Install the GhostText browser extension, then run the `writd` daemon:

```bash
writd
```

The daemon listens on port 4001. When you activate GhostText on a textarea, `writd` spawns a writ instance with `--autosave` enabled. Edits in writ sync back to the browser in real-time. This is useful for writing GitHub comments, issues, and PRs with writ's markdown editing features.

The `--autosave` flag can also be used standalone to save on every edit:

```bash
writ --file doc.md --autosave
```

## Development

```bash
git clone https://github.com/wilfreddenton/writ
cd writ
cargo run -- --file path/to/document.md
```

On Linux, using a faster linker significantly improves build times. See [Zed's linker documentation](https://github.com/zed-industries/zed/blob/main/docs/src/development/linux.md#linkers-linker) for setup instructions.

### Build Profiles

Debug builds are significantly slower, especially for image loading and text rendering. For day-to-day development with better performance, use the `release-fast` profile:

```bash
cargo run --profile release-fast -- --file path/to/document.md
```

For maximum runtime performance (slower compile times), use a full release build:

```bash
cargo run --release -- --file path/to/document.md
```

The release profile enables thin LTO and single codegen unit for best optimization. The `release-fast` profile trades some runtime performance for faster compilation by disabling LTO and using parallel codegen units.

## Features

### Inline Rendering

Markdown syntax is hidden when your cursor is elsewhere, revealing clean formatted text. Move your cursor to any formatted element and the raw syntax appears for editing. Headings hide their `#` markers and display at the appropriate size. Bold and italic text hides the `*` markers. Inline code hides the backticks and renders in a monospace font. Links hide the URL syntax entirely and can be opened with Ctrl+click (Cmd+click on macOS).

### Images

Images render inline, supporting both URLs and local file paths (absolute or relative to the markdown file). When an image is on its own line, only the rendered image is shown. Move your cursor to the line to reveal the markdown syntax above the image.

### Lists and Blockquotes

Unordered list markers (`-`) are replaced with bullet symbols when the cursor is away. Ordered lists are automatically renumbered as you edit. Task lists render interactive checkboxes that you can click to toggle. Blockquotes hide their `>` markers and show a left border instead.

Nesting is fully supported. A task item inside a blockquote is represented internally as a stack of layers, and each layer contributes its visual treatment independently.

### Smart Enter and Tab

Enter inserts a raw newline—no magic. Shift+Enter continues the current container by copying markers from the current line (e.g., on `- item|`, Shift+Enter creates `\n- `). Shift+Alt+Enter creates an indented continuation for nested paragraphs within list items.

Tab cycles through nesting states based on tree-sitter context. On a blank line after a list item, Tab cycles through: sibling marker → paragraph indent → nested marker → empty. Shift+Tab cycles backwards. This replaces traditional indent/dedent with context-aware structure cycling.

### Code Blocks

Fenced code blocks render with syntax highlighting (currently Rust). The fence lines are hidden when the cursor is outside the block, showing only the highlighted code. Move your cursor into the block to reveal the fences for editing.

### Selection and Editing

Full selection support with click, drag, shift+arrow keys, double-click to select word, and triple-click to select line. Copy, cut, and paste work as expected. Undo and redo are supported with full cursor position restoration.

## Library Usage

The editing engine is a headless, renderer-free `core::Editor`: a rope buffer,
cursor/selection, tree-sitter markdown parsing, and an inline git-diff model, with
no window or GPU dependency. It's the core the Vello shell drives, and it can be
used directly.

```rust
use std::path::Path;
use writ::core::Editor;

// Open a file (loads content and the git-HEAD diff base)…
let mut editor = Editor::open(Path::new("notes.md"));
// …or start from a string.
let mut editor = Editor::new("# Hello, world!");

// Edit.
editor.type_char('x');
editor.enter();
editor.backspace();

// Query.
editor.text();
editor.cursor_position();     // cursor byte offset
editor.selection_range();     // None if collapsed
editor.is_dirty();
editor.diff_state();          // inline diff vs HEAD, if any

// Persist.
editor.save().unwrap();
```

Rendering that model to a window — Parley layout with browser-grade line breaking,
cursor-aware marker hiding, and the inline diff — lives in the `render`,
`doc_layout`, `text_engine`, and `shell` modules.

## Architecture

The buffer stores raw markdown text using ropey, a rope data structure that provides O(log n) insertions and deletions. On every edit, tree-sitter incrementally reparses the document. Tree-sitter-md produces two parse trees: a block tree representing document structure (paragraphs, headings, lists, code blocks) and separate inline trees for each paragraph's inline content (bold, italic, links). The parser maintains both trees and provides a unified cursor that transparently switches between them when traversing.

Line information is derived from the parse tree. A preorder traversal collects all nodes in document order, then for each line, binary search finds the relevant nodes and extracts markers. Each line has a list of markers representing block-level syntax elements—a task item inside a blockquote has two markers: `[Checkbox, BlockQuote]` (innermost to outermost). Each marker knows its byte range (the bytes to hide when the cursor is away), its visual substitution (e.g., `-` becomes `•`), and its continuation text for smart enter.

The renderer lays out each line independently with Parley. It determines whether to show or hide markers based on cursor position: if the cursor is on the line, raw markdown syntax is visible for editing; otherwise, markers are hidden and substitutions are shown. For inline styles like bold or italic, the same logic applies per-span. A per-line display↔buffer segment map translates between the laid-out *display* string (markers hidden) and *buffer* byte offsets, so cursor placement, click hit-testing, and diff highlights all stay aligned.

### Incremental Parsing

Tree-sitter's incremental parsing is central to writ's responsiveness. When you type a character, tree-sitter doesn't reparse the entire document. Instead, the buffer tells tree-sitter what changed (the byte range and new content), and tree-sitter reuses unchanged portions of the previous syntax tree. The complexity is O(log n + k) where n is the document size and k is the size of the change, rather than O(n) for a full reparse. This means editing a 10,000-line document feels the same as editing a 100-line document.

### Code Block Syntax Highlighting

Code blocks are highlighted using tree-sitter-highlight with language-specific grammars. The editor walks the markdown AST to find fenced code blocks, extracts their content along with the language identifier from the fence line, and highlights each block separately using the appropriate grammar.

This manual extraction approach was chosen over tree-sitter's built-in injection support, which proved unreliable for our use case. Editors like Zed and Helix build their own injection handling for similar reasons. The manual approach is simpler: we find code blocks, highlight them independently, and merge the results back with buffer-relative offsets.

Currently only Rust is supported, but adding new languages requires just the grammar crate and a highlights.scm query file. Highlights are cached and only recomputed after edits.

## Known Issues

### Short Headings Not Styled While Typing

When typing `# Hello`, tree-sitter doesn't recognize it as a heading until enough content is present or a newline is added. This is a quirk of the tree-sitter-md grammar. The heading styling appears once you press Enter or type enough characters.

### Ordered List Continuation Shows Wrong Number

Pressing Shift+Enter on an ordered list item inserts `1. ` as a placeholder. The correct number appears after you start typing, when tree-sitter recognizes the list structure and auto-numbering corrects it.
