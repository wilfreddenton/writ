# writ

[![CI](https://github.com/wilfreddenton/writ/actions/workflows/ci.yml/badge.svg)](https://github.com/wilfreddenton/writ/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/writ.svg)](https://crates.io/crates/writ)
[![docs.rs](https://docs.rs/writ/badge.svg)](https://docs.rs/writ)

A hybrid markdown editor combining raw text editing with live inline rendering.

![writ rendering Markdown inline — a clean heading, styled prose, and a syntax-highlighted Rust code block](https://raw.githubusercontent.com/wilfreddenton/writ/main/assets/hero.png)

## Install

```bash
cargo install writ --locked
```

## Usage

```bash
writ --file path/to/document.md
```

Run `writ --demo` (or with no arguments) to open a built-in showcase document with a synthetic git-`HEAD` diff, so you can see the inline diff, rendering, and code-fence highlighting without setting anything up.

Flags:

| Flag | Description |
|------|-------------|
| `--file <path>` | Open a markdown file (watched for live external edits) |
| `--demo` | Open the showcase document with a synthetic HEAD diff |
| `--autosave` | Save on every edit (used by the GhostText daemon) |
| `--github-repo <owner/repo>` | GitHub context for ref validation (else auto-detected from the git remote) |
| `--github-token <token>` | GitHub API token (or the `GITHUB_TOKEN` env var) for ref validation, hover cards, and autocomplete |
| `--config <path>` | Config file location (default `~/.config/writ/config.toml`) |

writ is rendered on [winit](https://github.com/rust-windowing/winit) + [wgpu](https://wgpu.rs) + [Vello](https://github.com/linebender/vello) + [Parley](https://github.com/linebender/parley) (GPU 2D rendering and text layout). On Linux/Mesa (including Asahi) select the Vulkan backend with `WGPU_BACKEND=vulkan`. Fonts are resolved via [Fontique](https://github.com/linebender/parley) with fontconfig loaded at runtime — no `-dev` headers needed.

### Configuration

writ reads an optional TOML config from `~/.config/writ/config.toml` (platform config dir, overridable with `--config <path>`). It sets the theme and the document font; a missing or malformed file just falls back to the built-in defaults. See [`config.example.toml`](config.example.toml) for a fully commented example.

```toml
[font]
family = "JetBrains Mono"   # must be monospace; verified at startup, falls back if not
size = 18.0

[theme]
name = "nord"               # a built-in preset (see below) or a [themes.*] you define

[theme.overrides]           # optional per-color hex tweaks on top of the selected theme
comment = "#7A88B8"
```

- **Themes.** Pick a built-in preset by `name` — dark: `dracula` (default), `nord`, `solarized-dark`, `gruvbox-dark`, `tokyo-night`, `catppuccin-mocha`; light: `solarized-light`, `catppuccin-latte` — or define your own under `[themes.<name>]` (any omitted color is inherited from dracula) and select it. `[theme.overrides]` applies hex tweaks on top. The 12 color slots are `background`, `surface`, `foreground`, `selection`, `comment`, `red`, `orange`, `yellow`, `green`, `cyan`, `purple`, `pink`.
- **Font.** The family **must be monospace** — writ verifies fixed-pitch at startup and falls back to the system monospace (with a warning) if the font is proportional or not installed, protecting its deterministic wrapping.
- Config is read once at startup; edits take effect on the next launch.

### Inline git diff

When the open file lives in a git repository, writ renders a live inline diff against `HEAD`: added lines and words are tinted green, deleted lines appear as red "ghost" rows above their position, all with the same markdown rendering as the rest of the document. writ watches both the file and the repository's `HEAD`, so edits made by an external tool (e.g. an AI agent) reload and re-diff live — and committing, amending, or switching branches re-bases the diff immediately, even when the working file itself is untouched.

![Inline git diff: red ghost rows for deletions above green added lines, with word-level highlighting, all rendered as Markdown](https://raw.githubusercontent.com/wilfreddenton/writ/main/assets/diff.png)

### GitHub integration

When the open file is inside a GitHub repository (auto-detected from the `origin` remote, or set with `--github-repo owner/repo`) and a token is available (`--github-token` or the `GITHUB_TOKEN` env var), writ makes GitHub references live:

- **Ref detection + validation.** `#123` (issues/PRs), `@user` mentions, and cross-repo `owner/repo#456` refs are detected and validated against the GitHub API — valid refs are colored, invalid ones stay plain.
- **Hover cards.** Hover a validated ref for a popover with the issue/PR title and open/closed/merged status.
- **Autocomplete.** Type `#` to search issues and PRs, or `@` to search users, in a dropdown scoped to the repo.
- **Ctrl+R** force-revalidates all refs (busts a stale cache).

Validation is scoped to the visible viewport and cached, so scrolling a large document doesn't re-hit the API.

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

The dev profile optimizes dependencies (`opt-level = 2` for all deps, via `[profile.dev.package."*"]`) while keeping writ's own code unoptimized and debuggable — so a plain `cargo run` is comfortably interactive (the graphics/parsing stack — wgpu, Vello, Parley, tree-sitter — runs at full speed). Dependencies compile once, so incremental writ builds stay fast.

For maximum runtime performance (slower compile times), use a full release build:

```bash
cargo run --release -- --file path/to/document.md
```

The release profile enables thin LTO and a single codegen unit for best optimization; the `release-fast` profile (`cargo run --profile release-fast`) trades some of that for faster compilation by disabling LTO and using parallel codegen units.

## Features

### Inline Rendering

Markdown syntax is hidden when your cursor is elsewhere, revealing clean formatted text. Move your cursor to any formatted element and the raw syntax appears for editing. Headings hide their `#` markers and display at the appropriate size. Bold and italic text hides the `*` markers. Inline code hides the backticks and renders in a monospace font. Links hide the URL syntax entirely and can be opened with Ctrl+click (Cmd+click on macOS).

### Images

Images render inline, supporting both URLs and local file paths (absolute or relative to the markdown file). When an image is on its own line, only the rendered image is shown. Move your cursor to the line to reveal the markdown syntax above the image.

### Lists and Blockquotes

Unordered list markers (`-`) are replaced with bullet symbols when the cursor is away. Ordered lists are automatically renumbered as you edit. Task lists render interactive checkboxes that you can click to toggle. Blockquotes hide their `>` markers and show a left border instead.

Nesting is fully supported. A task item inside a blockquote is represented internally as a stack of layers, and each layer contributes its visual treatment independently.

### Callouts

Obsidian-style callouts — a blockquote whose first line is `> [!note]` (or `tip`, `warning`, `danger`, `success`, `question`, `example`, `quote`, …) — render inline with a type icon and accent color. Twelve types with their aliases are recognized. Add `+`/`-` after the type (`> [!tip]-`) to make a callout foldable (and start it collapsed); callouts nest, and each nesting level folds independently with the same gutter-chevron gestures as headings and lists. A custom title after the type (`> [!warning] Be careful`) replaces the default. As with other markers, the raw `[!type]` syntax reveals while your cursor is on the header line.

![Obsidian-style callouts: note, foldable tip, warning, and danger, each with its own icon and accent color](https://raw.githubusercontent.com/wilfreddenton/writ/main/assets/callouts.png)

### Smart Enter and Tab

Enter inserts a raw newline—no magic. Shift+Enter continues the current container by copying markers from the current line (e.g., on `- item|`, Shift+Enter creates `\n- `). Shift+Alt+Enter creates an indented continuation for nested paragraphs within list items.

Tab cycles through nesting states based on tree-sitter context. On a blank line after a list item, Tab cycles through: sibling marker → paragraph indent → nested marker → empty. Shift+Tab cycles backwards. This replaces traditional indent/dedent with context-aware structure cycling.

### Code Blocks

Fenced code blocks render with syntax highlighting (currently Rust and Bash). The fence line's delimiter and language name are colored distinctly, and the code is highlighted per-grammar. Move your cursor into the block to edit.

### Selection and Editing

Full selection support with click, drag (with edge auto-scroll), shift+arrow keys, double-click to select word, and triple-click to select line. Vertical movement keeps a sticky goal column through short lines. Copy, cut, and paste (with normalization) work as expected, and paste is context-aware inside blockquotes and code blocks. Undo/redo coalesce runs of typing into word-granular steps, with full cursor restoration. Cursor movement, selection, and deletion operate on whole grapheme clusters (so emoji and combining marks never split).

### GFM Tables

Pipe tables render as a live grid — bold header, per-column alignment (`:--`/`:-:`/`--:`), and inline styles (bold, code, links) inside cells. Move your cursor into a table and it reveals the raw pipe source for editing; move out and it snaps back to the grid. Type a header row like `| Name | Age |` and press Shift+Enter to scaffold the delimiter and a body row; Tab moves between cells (and appends a row from the last cell). Type a new column into the header and the delimiter and every body row grow to match — in place, wherever you added it. Backspace on a freshly-added empty row removes it in one press.

![A GFM table rendered as a grid with a bold header and right-aligned numeric columns](https://raw.githubusercontent.com/wilfreddenton/writ/main/assets/tables.png)

### Find and Replace

Ctrl+F opens a find bar docked above the status bar; Ctrl+H adds the replace row. Matches highlight live with a count, and the current match is emphasized; Enter/Shift+Enter cycle through them. Toggle regular expressions (Alt+R, with `$1` capture groups in the replacement) and case sensitivity (Alt+C). Enter in the replace field swaps the current match, Ctrl+Enter replaces all in one undo step. Click into the document to keep the bar open while you edit.

### Folding

Headings, nested lists, and callouts all fold from the gutter through one unified model. Hover the left margin to reveal a chevron on any foldable line and click to collapse its section; the outline mirrors what's folded. Modifier-clicks scope the fold: Ctrl folds every section at that level, Shift folds recursively, Ctrl+Shift folds that level and everything deeper. Jumping to a hidden line auto-unfolds it. Task lists fold the same way, so a long checklist collapses to its parent item.

![A collapsed callout showing its gutter chevron, above a foldable heading section and a nested list](https://raw.githubusercontent.com/wilfreddenton/writ/main/assets/folding.png)

### Mermaid Diagrams

Fenced ` ```mermaid ` blocks render inline as diagrams. Rendering runs off the UI thread and is cached, so scrolling stays smooth. Drag a selection through a diagram (or move the cursor onto its block) and it de-sugars back to the raw source you're actually editing, then snaps back to the rendered diagram when you leave.

![A Mermaid flowchart rendered inline in writ](https://raw.githubusercontent.com/wilfreddenton/writ/main/assets/mermaid.png)

### Math

LaTeX math renders inline via RaTeX with the KaTeX fonts embedded. Block `$$…$$` renders as centered display math on its own line; inline `$…$` renders as a baseline-aligned box within the text (the GitHub/pandoc rule keeps prose like "$5 and $10" literal). Glyphs take the theme foreground color. As with mermaid, moving the cursor into a span reveals its raw source for editing.

![Inline and block LaTeX math rendered in writ](https://raw.githubusercontent.com/wilfreddenton/writ/main/assets/math.png)

## Library Usage

writ is also a library: its rendering and editing layers work independently of the
desktop app, and the network/GUI dependencies are feature-gated so a render-only
consumer stays lean.

### Headless markdown renderer

`MarkdownView` renders markdown into a [Vello](https://github.com/linebender/vello)
`Scene` with no window, GPU device, or editor. Feed it content up front or stream it
in with `push_str`, then paint — useful for embedding writ's renderer elsewhere, or
for rendering LLM output as it arrives.

```rust
use vello::Scene;
use writ::MarkdownView;

let mut view = MarkdownView::new();
view.push_str("# Streaming\n\n");           // append incrementally (e.g. LLM tokens)
view.push_str("More text arrives later.\n");

let mut scene = Scene::new();
view.render(&mut scene, 800.0, 600.0, 1.0);  // draw into the Scene
// …then hand `scene` to a vello::Renderer to rasterize onto your own surface.
```

`writ::rasterize_scene_to_png` renders a `Scene` to a PNG headlessly — see
`examples/streaming_markdown.rs` for an end-to-end streaming → PNG demo. The layout
and draw internals live in the `markdown_view`, `render`, `doc_layout`, and
`text_engine` modules.

### Editing engine

`core::Editor` (behind the `editor` feature) is a headless, renderer-free editor: a
rope buffer, cursor/selection, tree-sitter markdown parsing, and an inline git-diff
model, with no window or GPU dependency.

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

### Feature flags

`default = ["app"]` builds the full desktop editor. For library use, disable defaults
and opt into only what you need:

```toml
# render-only: pulls in none of tokio/reqwest/gix/github/winit
writ = { version = "0.16", default-features = false }
```

| Feature | Adds |
|---------|------|
| *(base)* | `MarkdownView`, `rasterize_scene_to_png`, and all markdown parse / layout / diff rendering |
| `git` | inline-diff base sourced from git `HEAD` (gix) |
| `github` | GitHub ref validation + `#`/`@` autocomplete |
| `watch` | live file-reload on external edits |
| `remote-images` | fetch `http(s)` image URLs (adds tokio/reqwest) |
| `mermaid` | render fenced `mermaid` blocks as diagrams |
| `math` | render `$…$` / `$$…$$` LaTeX math (RaTeX) |
| `editor` | the full `core::Editor` orchestration (implies `git`, `github`, `watch`) |
| `app` *(default)* | the winit + Vello desktop application |
| `ghosttext` | the `writd` GhostText daemon |

## Architecture

The buffer stores raw markdown text using ropey, a rope data structure that provides O(log n) insertions and deletions. On every edit, tree-sitter incrementally reparses the document. Tree-sitter-md produces two parse trees: a block tree representing document structure (paragraphs, headings, lists, code blocks) and separate inline trees for each paragraph's inline content (bold, italic, links). The parser maintains both trees and provides a unified cursor that transparently switches between them when traversing.

Line information is derived from the parse tree. A preorder traversal collects all nodes in document order, then for each line, binary search finds the relevant nodes and extracts markers. Each line has a list of markers representing block-level syntax elements—a task item inside a blockquote has two markers: `[Checkbox, BlockQuote]` (innermost to outermost). Each marker knows its byte range (the bytes to hide when the cursor is away), its visual substitution (e.g., `-` becomes `•`), and its continuation text for smart enter.

The renderer lays out each line independently with Parley. It determines whether to show or hide markers based on cursor position: if the cursor is on the line, raw markdown syntax is visible for editing; otherwise, markers are hidden and substitutions are shown. For inline styles like bold or italic, the same logic applies per-span. A per-line display↔buffer segment map translates between the laid-out *display* string (markers hidden) and *buffer* byte offsets, so cursor placement, click hit-testing, and diff highlights all stay aligned.

### Viewport Virtualization

Layout is virtualized: only the lines in (and just around) the viewport are fully laid out with Parley; the rest are height-estimated from a persistent per-line height cache, so building the layout is O(visible) regardless of document size or scroll depth. Scroll position is pinned to a `(line, offset)` anchor and re-pinned after every rebuild, so off-screen height corrections never shift what's on screen — first-open and deep-scroll of a large file stay fast and jitter-free.

### Incremental Parsing

Tree-sitter's incremental parsing is central to writ's responsiveness. When you type a character, tree-sitter doesn't reparse the entire document. Instead, the buffer tells tree-sitter what changed (the byte range and new content), and tree-sitter reuses unchanged portions of the previous syntax tree. The complexity is O(log n + k) where n is the document size and k is the size of the change, rather than O(n) for a full reparse. This means editing a 10,000-line document feels the same as editing a 100-line document.

### Code Block Syntax Highlighting

Code blocks are highlighted using tree-sitter-highlight with language-specific grammars. The editor walks the markdown AST to find fenced code blocks, extracts their content along with the language identifier from the fence line, and highlights each block separately using the appropriate grammar.

This manual extraction approach was chosen over tree-sitter's built-in injection support, which proved unreliable for our use case. Editors like Zed and Helix build their own injection handling for similar reasons. The manual approach is simpler: we find code blocks, highlight them independently, and merge the results back with buffer-relative offsets.

Currently Rust and Bash are supported; adding a language requires just the grammar crate and a highlights.scm query file. Highlights are cached and only recomputed after edits.

## Limitations

Only **ATX** headings (`# Heading`) are recognized — setext headings (a line underlined with `===` or `---`) render as plain text and don't appear in the outline or fold. This is deliberate: setext text and its underline live on separate lines, so full support means both collecting *and* rendering it across the pair, for a syntax `#` dominates in practice.
