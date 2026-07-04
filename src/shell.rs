//! winit + wgpu + Vello application shell — the gpui replacement (see MIGRATION-PLAN.md).
//!
//! Phase 0–5: a working editor with inline git diff — renders a full markdown
//! document (variable-height lines, tree-sitter highlighting, browser-grade
//! wrapping, cursor-aware marker hiding) via Vello, with typing/arrows/backspace/
//! enter/tab, click-to-place and drag selection (Parley hit-testing through the
//! segment map), caret + selection painting, scroll (wheel + scroll-to-cursor),
//! minimal IME commit, inline diff vs HEAD (green added lines/words, red ghost
//! deleted lines interleaved above), and chrome (a title bar + a status bar with
//! nesting context and cursor position; the editor is inset + clipped between
//! them). CSD (custom window frame) and the async-blocked overlays are deferred.
//! Run with
//! `WGPU_BACKEND=vulkan cargo run --bin writ-next` on Asahi; set
//! `WRIT_SHELL_SNAPSHOT=out.ppm` (+ optional `WRIT_SHELL_{W,H,SCROLL,CURSOR,SEL_A,SEL_B}`)
//! to render one frame headlessly instead.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use parley::{Affinity, Cursor, Layout, Selection};
use unicode_segmentation::UnicodeSegmentation;
use vello::kurbo::{Affine, Rect};
use vello::peniko::{Brush, Fill};
use vello::util::{RenderContext, RenderSurface};
use vello::wgpu;
use vello::wgpu::CurrentSurfaceTexture;
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Theme, Window, WindowId};

use winit::event_loop::ControlFlow;

use crate::buffer::Buffer;
use crate::chrome::{BarRect, StatusInfo, draw_panel, draw_status_bar};
use crate::config::Config;
use crate::consts::{CARET_WIDTH, FONT_SIZE, LINE_HEIGHT, PADDING, STATUS_BAR_H, WHEEL_LINE_STEP};
use crate::core::{AutocompleteState, AutocompleteSuggestion, AutocompleteTrigger, Editor};
use crate::doc_layout::{
    DocLayout, GithubRenderData, LayoutParams, LineCache, PreeditView, RenderCache, ScreenRect,
};
use crate::editor::{Direction, EditorTheme};
use crate::git::{detect_github_context, parse_github_repo_string};
use crate::github::{
    GitHubClient, IssueOrPr, IssueStatus, MentionableUser, ValidatedRefData, ValidationResult,
    ValidationState,
};
use crate::image_cache::{ImageCache, decode};
use crate::inline::{GitHubContext, GitHubRef};
use crate::marker::MarkerKind;
use crate::text_engine::{StyleRun, TextEngine, peniko_color, peniko_color_alpha};

/// Chrome layout in device px: y where editor content begins, and its height.
fn chrome_metrics(scale: f32, height_dev: f32) -> (f32, f32) {
    // The title bar is the native window decoration now, so editor content starts at
    // the surface top; only the bottom status bar is inset.
    let content_top = 0.0;
    let editor_h = (height_dev - STATUS_BAR_H * scale).max(1.0);
    (content_top, editor_h)
}

/// The native window title: the file name with a `●` dirty marker, shown in the OS
/// decoration (we no longer draw our own title bar).
fn window_title(editor: &Editor) -> String {
    let name = editor
        .file_path()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "untitled".to_string());
    if editor.is_dirty() {
        format!("● {name}")
    } else {
        name
    }
}

/// Nearest heading level at or above `line`, if any.
fn heading_at(buffer: &Buffer, line: usize) -> Option<u8> {
    for i in (0..=line).rev() {
        for m in &buffer.line_markers(i).markers {
            if let MarkerKind::Heading(level) = m.kind {
                return Some(level);
            }
        }
    }
    None
}

/// Gather the status-bar inputs from the editor + viewport.
fn build_status_info(editor: &Editor, doc: &DocLayout, editor_h: f32) -> StatusInfo {
    let buffer = &editor.state.buffer;
    let cursor = editor.cursor_position();
    let line = buffer.byte_to_line(cursor);
    let line_start = buffer.line_to_byte(line);
    // Column counts grapheme clusters, not codepoints, so a multi-codepoint character
    // (emoji ZWJ sequence, combining accent) advances the column by one, matching what
    // the user sees on screen.
    let col = buffer.slice_cow(line_start..cursor).graphemes(true).count() + 1;
    let (first, last) = doc.visible_range(editor_h);
    StatusInfo {
        context: editor.state.build_nested_context(cursor),
        heading_level: heading_at(buffer, line),
        cursor_line: line + 1,
        cursor_col: col,
        total_lines: buffer.line_count(),
        first_visible: first,
        last_visible: last.saturating_sub(1),
    }
}

/// Sample document shown until the shell loads real files (Phase 4+ wires input).
const SAMPLE_DOC: &str = "\
# writ on Vello

The editor now renders a **whole document** through Parley and Vello, laid out
line by line with browser-grade wrapping so a closing paren never gets orphaned
onto the next line (see: writ). Headings are larger, giving *variable* line
heights that the prefix-sum viewport stacks correctly.

## Scrolling

This paragraph exists to push content past the bottom of the window so the
mouse wheel has something to do. Resize the window and the prose re-wraps to the
new width, exactly like a browser would reflow it.

```rust
fn main() {
    let editor = Editor::new(\"# hello\");
    println!(\"{}\", editor.text());
}
```

- a bulleted list item
- another item with `inline code`
- a third, slightly longer item so it wraps around when the window is narrow

### Still to come

Marker hiding (the real segment map), the cursor, selection, and inline git diff
land in the next phases. For now this proves the render + scroll skeleton.
";

/// Wakeups sent from tokio worker tasks back into the winit loop. The work's
/// results are already written to the shared `Arc<Mutex>` caches; the event just
/// tells the loop to redraw (and, for autocomplete, drain the suggestion slot).
#[derive(Debug, Clone)]
enum WritEvent {
    GithubUpdated,
    /// A standalone image finished loading (local or remote). A load changes a line's
    /// height, so the loop rebuilds (not just redraws) to reflow around it.
    ImageLoaded,
}

/// Autocomplete results a tokio task fetched, handed back to the main thread (via a
/// shared slot) where they're turned into `AutocompleteSuggestion`s and installed.
enum FetchedSuggestions {
    Issues {
        prefix: String,
        issues: Vec<IssueOrPr>,
    },
    Users {
        prefix: String,
        users: Vec<MentionableUser>,
    },
}

struct ActiveSurface {
    surface: RenderSurface<'static>,
    window: Arc<Window>,
    scale: f32,
}

/// A GitHub ref currently under the pointer, plus its on-screen caret rect (used to
/// anchor the hover popover above/below it).
struct HoverTarget {
    reference: GitHubRef,
    anchor: ScreenRect,
}

/// The document engine: the editor/buffer plus the caches and Parley/Vello text
/// engine that lay it out into `doc`. Held as one field kept SEPARATE from `state`
/// (the GPU surface) so `&mut self.doc_engine` and `&self.state` are disjoint
/// borrows — the split that lets a rebuild run while the surface stays borrowed at
/// the call site. The per-line caches + `Rc`-wrapped layouts/renders avoid deep
/// clones on the typing hot path.
/// In-progress IME composition (owned form of [`PreeditView`]): the composing `text`
/// and its caret/selection `cursor` (byte offsets within `text`). Held on `DocEngine`
/// so `rebuild` can splice it into the caret line without threading a param everywhere.
struct Preedit {
    text: String,
    cursor: Option<(usize, usize)>,
}

struct DocEngine {
    text_engine: TextEngine,
    line_cache: LineCache,
    render_cache: RenderCache,
    theme: EditorTheme,
    editor: Editor,
    doc: Option<DocLayout>,
    /// Shared cache of decoded standalone images, threaded into `DocLayout::build`.
    images: ImageCache,
    /// Active IME composition, spliced into the caret line at render time. `None` when
    /// not composing.
    preedit: Option<Preedit>,
}

struct App {
    context: RenderContext,
    // One renderer suffices; keyed to the surface's device.
    renderer: Option<Renderer>,
    state: Option<ActiveSurface>,
    scene: Scene,
    doc_engine: DocEngine,
    modifiers: ModifiersState,
    mouse_pos: (f32, f32),
    mouse_down: bool,
    /// System clipboard for copy/cut/paste. Held for the app's lifetime (on Wayland the
    /// instance keeps serving the copied data). `None` if the platform init failed.
    clipboard: Option<arboard::Clipboard>,
    /// Last title pushed to the native window (filename + dirty marker); re-set only
    /// when it changes, so the OS decoration carries the document name.
    title: String,
    /// GitHub ref under the pointer, if any (drives the hover popover).
    hovered: Option<HoverTarget>,
    /// Screen rects of the currently-drawn autocomplete rows (for click routing).
    ac_row_rects: Vec<ScreenRect>,
    /// Monotonic generation for autocomplete-fetch debounce (latest wins).
    ac_gen: Arc<AtomicU64>,
    /// Slot a finished fetch task drops its results into for the main thread.
    ac_slot: Arc<Mutex<Option<FetchedSuggestions>>>,
    /// Wakeup channel into the winit loop for finished tokio work.
    proxy: EventLoopProxy<WritEvent>,
    /// Handle to the process tokio runtime for spawning GitHub work.
    runtime: tokio::runtime::Handle,
}

impl App {
    fn new(
        editor: Editor,
        proxy: EventLoopProxy<WritEvent>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let title = window_title(&editor);
        Self {
            context: RenderContext::new(),
            renderer: None,
            state: None,
            scene: Scene::new(),
            doc_engine: DocEngine {
                text_engine: TextEngine::new(),
                line_cache: LineCache::new(),
                render_cache: RenderCache::new(),
                theme: EditorTheme::dracula(),
                editor,
                doc: None,
                images: ImageCache::new(),
                preedit: None,
            },
            modifiers: ModifiersState::empty(),
            mouse_pos: (0.0, 0.0),
            mouse_down: false,
            clipboard: arboard::Clipboard::new().ok(),
            title,
            hovered: None,
            ac_row_rects: Vec::new(),
            ac_gen: Arc::new(AtomicU64::new(0)),
            ac_slot: Arc::new(Mutex::new(None)),
            proxy,
            runtime,
        }
    }

    /// Post-edit async side effects: validate freshly-detected GitHub refs and start
    /// loading any newly-appeared standalone images. Both are idempotent (cache-guarded).
    fn spawn_validations(&self) {
        let visible = self.state.as_ref().and_then(|s| {
            let (_, vh) = chrome_metrics(s.scale, s.surface.config.height as f32);
            self.doc_engine.doc.as_ref().map(|d| {
                let (a, b) = d.visible_range(vh);
                a..b
            })
        });
        spawn_ref_validations(&self.doc_engine.editor, visible, &self.runtime, &self.proxy);
        self.sync_images();
    }

    /// Kick off loads for every standalone image in the current layout that isn't cached
    /// yet. Idempotent (already-cached URLs are skipped), so it's safe to call after any
    /// rebuild/scroll.
    fn sync_images(&self) {
        sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
    }
}

/// Free-function form of `App::sync_images` that borrows only the doc-engine (+ runtime
/// / proxy) — usable from `window_event` where `self.state` is already borrowed mutably.
fn sync_image_loads(
    doc_engine: &DocEngine,
    runtime: &tokio::runtime::Handle,
    proxy: &EventLoopProxy<WritEvent>,
) {
    let Some(doc) = doc_engine.doc.as_ref() else {
        return;
    };
    let urls = doc.image_urls();
    if urls.is_empty() {
        return;
    }
    let dir = doc_engine
        .editor
        .file_path()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    spawn_image_loads(dir, &urls, &doc_engine.images, runtime, proxy);
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

/// GET + decode a remote image. Mirrors the reqwest/rustls client pattern in github.rs.
/// Logs (rather than silently drops) the failure reason — a non-2xx status, a transport
/// error, or an undecodable body (e.g. SVG, which the `image` crate can't rasterize).
async fn load_remote_image(url: &str) -> Option<crate::image_cache::LoadedImage> {
    let resp = match reqwest::Client::new()
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
    let bytes = resp.bytes().await.ok()?;
    match decode(&bytes) {
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

/// For each standalone-image URL with no cache entry, mark it loading and spawn a
/// worker: remote `http(s)` via reqwest on the async runtime, else a local file read +
/// decode on a blocking task (so IO/decoding doesn't stall the runtime). Each finish
/// wakes the loop with `ImageLoaded` so the doc rebuilds around the now-known height.
fn spawn_image_loads(
    doc_dir: Option<PathBuf>,
    urls: &[String],
    cache: &ImageCache,
    runtime: &tokio::runtime::Handle,
    proxy: &EventLoopProxy<WritEvent>,
) {
    for url in urls {
        if cache.get(url).is_some() {
            continue; // already loading/loaded/failed
        }
        cache.mark_loading(url);
        let cache = cache.clone();
        let proxy = proxy.clone();
        let url = url.clone();
        if url.starts_with("http://") || url.starts_with("https://") {
            runtime.spawn(async move {
                match load_remote_image(&url).await {
                    Some(img) => cache.set_loaded(&url, img),
                    None => cache.set_failed(&url),
                }
                let _ = proxy.send_event(WritEvent::ImageLoaded);
            });
        } else {
            let path = resolve_local_image(doc_dir.as_deref(), &url);
            runtime.spawn_blocking(move || {
                let loaded = path
                    .and_then(|p| std::fs::read(p).ok())
                    .and_then(|bytes| decode(&bytes));
                match loaded {
                    Some(img) => cache.set_loaded(&url, img),
                    None => cache.set_failed(&url),
                }
                let _ = proxy.send_event(WritEvent::ImageLoaded);
            });
        }
    }
}

/// For each detected GitHub ref (from refs + naked URLs) with no cache entry yet,
/// mark it pending and spawn a tokio task to validate it. Results land in the shared
/// `GitHubValidationCache`; a `GithubUpdated` wakeup triggers a redraw.
fn spawn_ref_validations(
    editor: &Editor,
    visible: Option<Range<usize>>,
    runtime: &tokio::runtime::Handle,
    proxy: &EventLoopProxy<WritEvent>,
) {
    let Some(client) = editor.github_client() else {
        return;
    };
    let cache = editor.github_validation_cache();

    // No layout yet → validate all; otherwise only the refs on visible lines.
    let refs = match visible {
        Some(r) => editor.detected_refs_in_lines(r),
        None => editor.detected_refs(),
    };
    for reference in refs {
        if cache.get(&reference).is_some() {
            continue; // already pending/valid/invalid
        }
        cache.mark_pending(reference.clone());
        let client = client.clone();
        let cache = cache.clone();
        let proxy = proxy.clone();
        runtime.spawn(async move {
            match client.validate_ref(&reference).await {
                ValidationResult::ValidWithData(d) => cache.set_valid(reference, Some(d)),
                ValidationResult::ValidNoData => cache.set_valid(reference, None),
                ValidationResult::Invalid => cache.set_invalid(reference),
            }
            let _ = proxy.send_event(WritEvent::GithubUpdated);
        });
    }
}

/// Translate a key press into an editor edit/move. Returns true if the editor
/// changed (so the caller rebuilds + reveals the cursor).
/// Lines the cursor jumps per PageUp/PageDown (fixed; viewport-aware is a refinement).
const PAGE_LINES: usize = 20;

/// Extra lines above/below the viewport that ref/URL detection covers — generous so it
/// spans every line the layout materializes and draws (viewport + measure overscan),
/// keeping scrolled-in refs colored while bounding the per-keystroke scan.
const DETECT_OVERSCAN_LINES: usize = 200;

fn apply_key(
    editor: &mut Editor,
    modifiers: ModifiersState,
    event: &winit::event::KeyEvent,
) -> bool {
    let shift = modifiers.shift_key();
    let alt = modifiers.alt_key();
    let ctrl = modifiers.control_key() || modifiers.super_key();
    match &event.logical_key {
        Key::Named(NamedKey::Enter) => {
            if shift && alt {
                editor.shift_alt_enter();
            } else if shift {
                editor.shift_enter();
            } else {
                editor.enter();
            }
            true
        }
        Key::Named(NamedKey::Backspace) => {
            editor.backspace();
            true
        }
        Key::Named(NamedKey::Delete) => {
            editor.delete_forward();
            true
        }
        Key::Named(NamedKey::Tab) => {
            // In a code block Tab indents (4 spaces) instead of cycling list nesting.
            if !shift && editor.cursor_in_code_block() {
                editor.insert_str("    ");
            } else if shift {
                editor.shift_tab();
            } else {
                editor.tab();
            }
            true
        }
        Key::Named(NamedKey::Space) => {
            // Smart space: suppressed at line/blockquote-content start.
            if !editor.try_insert_space() {
                editor.insert_str(" ");
            }
            true
        }
        Key::Named(NamedKey::ArrowLeft) => {
            editor.move_in_direction(Direction::Left, shift);
            true
        }
        Key::Named(NamedKey::ArrowRight) => {
            editor.move_in_direction(Direction::Right, shift);
            true
        }
        Key::Named(NamedKey::ArrowUp) => {
            editor.move_in_direction(Direction::Up, shift);
            true
        }
        Key::Named(NamedKey::ArrowDown) => {
            editor.move_in_direction(Direction::Down, shift);
            true
        }
        // Home/End: line boundary, or document boundary with Ctrl/Super; Shift extends.
        Key::Named(NamedKey::Home) => {
            let dir = if ctrl { Direction::DocStart } else { Direction::LineStart };
            editor.move_in_direction(dir, shift);
            true
        }
        Key::Named(NamedKey::End) => {
            let dir = if ctrl { Direction::DocEnd } else { Direction::LineEnd };
            editor.move_in_direction(dir, shift);
            true
        }
        // Page up/down: a fixed page of lines (viewport-aware sizing is a later refinement).
        Key::Named(NamedKey::PageUp) => {
            for _ in 0..PAGE_LINES {
                editor.move_in_direction(Direction::Up, shift);
            }
            true
        }
        Key::Named(NamedKey::PageDown) => {
            for _ in 0..PAGE_LINES {
                editor.move_in_direction(Direction::Down, shift);
            }
            true
        }
        _ => {
            // Command shortcuts (Ctrl or Super held). Any other Ctrl+key is swallowed
            // so it doesn't type a character.
            if ctrl {
                if let Key::Character(c) = &event.logical_key {
                    let c = c.as_str();
                    if c.eq_ignore_ascii_case("s") {
                        if let Err(e) = editor.save() {
                            eprintln!("[writ] save failed: {e}");
                        }
                        return true; // redraw so the title's dirty marker clears
                    }
                    if c.eq_ignore_ascii_case("z") {
                        // Ctrl+Shift+Z = redo, Ctrl+Z = undo.
                        if shift {
                            editor.redo();
                        } else {
                            editor.undo();
                        }
                        return true;
                    }
                    if c.eq_ignore_ascii_case("y") {
                        editor.redo();
                        return true;
                    }
                    if c.eq_ignore_ascii_case("a") {
                        editor.select_all();
                        return true;
                    }
                }
                return false;
            }
            // Printable text (respects shift for capitals/symbols).
            if let Some(text) = &event.text
                && !text.is_empty()
                && !text.chars().any(|c| c.is_control())
            {
                editor.insert_str(text);
                // Auto-complete markdown structure just typed: `> `→blockquote space,
                // ` ``` `/`~~~`→closing fence.
                match text.as_str() {
                    ">" => editor.maybe_complete_blockquote_marker(),
                    "`" | "~" => editor.maybe_complete_code_fence(),
                    _ => {}
                }
                return true;
            }
            false
        }
    }
}

impl DocEngine {
    /// Rebuild after an edit or cursor move (marker reveal is cursor-dependent),
    /// preserving scroll then revealing the cursor. Re-detects GitHub refs / naked
    /// URLs first so coloring + validation spawning see the current buffer.
    ///
    /// A method on `DocEngine` (not `App`) so it borrows only the doc-engine fields,
    /// leaving the caller's `&self.state` (GPU surface) borrow disjoint and intact.
    /// Line window to run GitHub-ref / URL detection over: the visible range widened by
    /// overscan (covering the lines the rebuild materializes + draws) and always
    /// including the cursor line. Whole buffer until the first layout exists. Scoping it
    /// keeps per-keystroke detection bounded instead of O(total lines) on large docs.
    fn detection_range(&self, editor_h: f32) -> Range<usize> {
        let n = self.editor.state.buffer.line_count();
        let cursor_line = self.editor.line_of(self.editor.cursor_position());
        match &self.doc {
            Some(d) => {
                let (first, last) = d.visible_range(editor_h);
                let lo = first.saturating_sub(DETECT_OVERSCAN_LINES).min(cursor_line);
                let hi = (last + DETECT_OVERSCAN_LINES).max(cursor_line + 1).min(n);
                lo..hi
            }
            None => 0..n,
        }
    }

    fn refresh(&mut self, device_width: f32, scale: f32, editor_h: f32) {
        self.editor.refresh_detection(self.detection_range(editor_h));
        let prev_scroll = self.doc.as_ref().map(|d| d.scroll_y).unwrap_or(0.0);
        let mut new_doc = self.rebuild(device_width, scale, prev_scroll + editor_h);
        new_doc.scroll_y = prev_scroll;
        new_doc.scroll_to(self.editor.cursor_position(), editor_h);
        self.doc = Some(new_doc);
    }

    /// Rebuild preserving the current scroll (no cursor reveal): the freshly-validated-
    /// refs recolor and wheel-remeasure paths, where the viewport must not jump. Re-runs
    /// detection over the (possibly scrolled) viewport — gated, so a same-window recolor
    /// wakeup doesn't rescan, but scrolling into new lines detects their refs.
    fn rebuild_preserving_scroll(&mut self, device_width: f32, scale: f32, editor_h: f32) {
        self.editor.refresh_detection(self.detection_range(editor_h));
        let prev_scroll = self.doc.as_ref().map(|d| d.scroll_y).unwrap_or(0.0);
        let mut new_doc = self.rebuild(device_width, scale, prev_scroll + editor_h);
        new_doc.scroll_y = prev_scroll;
        new_doc.clamp_scroll(editor_h);
        self.doc = Some(new_doc);
    }

    /// Lay out the whole document at `device_width`, returning the layout for the
    /// caller to store. `measure_to_y` is the device-px depth (scroll_y + viewport_h)
    /// that must be fully laid out; deeper lines are height-estimated. `f32::INFINITY`
    /// lays out the whole document. Borrows disjoint doc-engine fields so the caller's
    /// surface borrow stays intact.
    fn rebuild(&mut self, device_width: f32, scale: f32, measure_to_y: f32) -> DocLayout {
        let cursor_offset = self.editor.cursor_position();
        let version = self.editor.state.buffer.version();
        // Clone the diff before borrowing the buffer mutably for the snapshot.
        let diff = self.editor.diff_state().cloned();
        let snapshot = self.editor.state.buffer.render_snapshot();
        // The snapshot is owned, so the mutable borrow above is released — now gather
        // the GitHub autolink data (immutable borrows) to color validated refs.
        let github = GithubRenderData {
            refs_by_line: self.editor.github_refs_by_line(),
            urls_by_line: self.editor.naked_urls_by_line(),
            cache: self.editor.github_validation_cache(),
            context: self.editor.github_context(),
        };
        let params = LayoutParams {
            device_width,
            scale,
            pad_x: PADDING,
            pad_top: PADDING,
            pad_bottom: PADDING * 2.0,
            base_font_size: FONT_SIZE,
            line_height: LINE_HEIGHT,
            fg: peniko_color(self.theme.foreground),
        };
        let preedit = self.preedit.as_ref().map(|p| PreeditView {
            text: &p.text,
            cursor: p.cursor,
        });
        DocLayout::build(
            &mut self.text_engine,
            &mut self.line_cache,
            &mut self.render_cache,
            version,
            &snapshot,
            &self.theme,
            diff.as_ref(),
            Some(&github),
            &self.images,
            cursor_offset,
            &params,
            preedit.as_ref(),
            measure_to_y,
        )
    }
}

/// Re-evaluate the autocomplete popup against the cursor and, if a fetch is needed,
/// spawn a debounced (150ms) tokio task that fetches suggestions into the shared slot.
fn sync_autocomplete(
    editor: &mut Editor,
    runtime: &tokio::runtime::Handle,
    proxy: &EventLoopProxy<WritEvent>,
    fetch_gen: &Arc<AtomicU64>,
    slot: &Arc<Mutex<Option<FetchedSuggestions>>>,
) {
    if !editor.update_autocomplete_from_cursor() {
        return;
    }
    let Some((trigger, prefix)) = editor.begin_autocomplete_fetch() else {
        return;
    };
    let Some(client) = editor.github_client().cloned() else {
        return;
    };
    let Some(context) = editor.github_context().cloned() else {
        return;
    };

    let my_gen = fetch_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let fetch_gen = fetch_gen.clone();
    let proxy = proxy.clone();
    let slot = slot.clone();
    runtime.spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if fetch_gen.load(Ordering::SeqCst) != my_gen {
            return; // a newer keystroke superseded this fetch
        }
        let fetched = match trigger {
            AutocompleteTrigger::Issue => {
                let issues = client
                    .issues_matching_prefix(&context.owner, &context.repo, &prefix, 8)
                    .await;
                FetchedSuggestions::Issues { prefix, issues }
            }
            AutocompleteTrigger::User => {
                let users = client
                    .users_matching_prefix(&context.owner, &context.repo, &prefix, 8)
                    .await;
                FetchedSuggestions::Users { prefix, users }
            }
        };
        *slot.lock().unwrap() = Some(fetched);
        let _ = proxy.send_event(WritEvent::GithubUpdated);
    });
}

/// Turn fetched GitHub data into suggestion rows (and prime the validation cache with
/// issue data so hovers are instant), then install them if the popup still matches.
fn apply_fetched(editor: &mut Editor, fetched: FetchedSuggestions) {
    match fetched {
        FetchedSuggestions::Issues { prefix, issues } => {
            let context = editor.github_context().cloned();
            let cache = editor.github_validation_cache().clone();
            let mut suggestions = Vec::with_capacity(issues.len());
            for issue in issues {
                if let Some(ctx) = &context {
                    cache.set_valid(
                        GitHubRef::Issue {
                            owner: ctx.owner.clone(),
                            repo: ctx.repo.clone(),
                            number: issue.number,
                        },
                        Some(ValidatedRefData::Issue(issue.clone())),
                    );
                }
                suggestions.push(AutocompleteSuggestion::IssueOrPr {
                    number: issue.number,
                    symbol: issue.symbol().to_string(),
                    status: issue.status(),
                    title: issue.title,
                });
            }
            editor.apply_autocomplete_suggestions(AutocompleteTrigger::Issue, &prefix, suggestions);
        }
        FetchedSuggestions::Users { prefix, users } => {
            let suggestions = users
                .into_iter()
                .map(|u| AutocompleteSuggestion::User {
                    login: u.login,
                    name: u.name,
                })
                .collect();
            editor.apply_autocomplete_suggestions(AutocompleteTrigger::User, &prefix, suggestions);
        }
    }
}

/// Find the GitHub ref (regular or naked-URL) under a screen point, with its
/// on-screen anchor rect. Returns None when the pointer isn't over a detected ref.
fn find_hover_target(editor: &Editor, doc: &DocLayout, x: f32, y: f32) -> Option<HoverTarget> {
    let off = doc.hit_test(x, y)?;
    hover_target_at_offset(editor, doc, off)
}

/// Find the GitHub ref whose byte range contains `off`, with its anchor rect.
fn hover_target_at_offset(editor: &Editor, doc: &DocLayout, off: usize) -> Option<HoverTarget> {
    let line = editor.state.buffer.byte_to_line(off);
    if let Some(refs) = editor.github_refs_by_line().get(&line) {
        for m in refs {
            if m.byte_range.contains(&off) {
                let anchor = doc.caret_rect(m.byte_range.start, 2.0)?;
                return Some(HoverTarget {
                    reference: m.reference.clone(),
                    anchor,
                });
            }
        }
    }
    if let Some(urls) = editor.naked_urls_by_line().get(&line) {
        for u in urls {
            if u.byte_range.contains(&off)
                && let Some(r) = &u.github_ref
            {
                let anchor = doc.caret_rect(u.byte_range.start, 2.0)?;
                return Some(HoverTarget {
                    reference: r.clone(),
                    anchor,
                });
            }
        }
    }
    None
}

/// Text color for an issue/PR status badge.
fn status_color(status: IssueStatus, theme: &EditorTheme) -> vello::peniko::Color {
    match status {
        IssueStatus::Open => theme.green,
        IssueStatus::Draft => theme.comment,
        IssueStatus::Merged | IssueStatus::Closed => theme.purple,
        IssueStatus::ClosedNotPlanned => theme.red,
    }
}

/// A styled span in a hover/autocomplete panel line: text plus color and inline
/// markdown attributes. Titles carry `bold`/`italic`/`mono`; all other segments
/// are `PanelSeg::plain`.
struct PanelSeg {
    text: String,
    color: vello::peniko::Color,
    bold: bool,
    italic: bool,
    mono: bool,
}

impl PanelSeg {
    fn plain(text: String, color: vello::peniko::Color) -> Self {
        Self {
            text,
            color,
            bold: false,
            italic: false,
            mono: false,
        }
    }
}

/// Scan an issue/PR `title` and emit styled spans, translating a small subset of
/// inline markdown (`` `code` ``, `**bold**`, `*italic*`/`_italic_`) into
/// attributes with the delimiters stripped. Non-nested, left-to-right; an
/// unterminated delimiter is emitted as literal text. All spans use `color`.
fn parse_title_markdown(title: &str, color: vello::peniko::Color) -> Vec<PanelSeg> {
    let mut out: Vec<PanelSeg> = Vec::new();
    let mut plain = String::new();
    let chars: Vec<char> = title.chars().collect();
    let mut i = 0;

    let flush_plain = |plain: &mut String, out: &mut Vec<PanelSeg>| {
        if !plain.is_empty() {
            out.push(PanelSeg::plain(std::mem::take(plain), color));
        }
    };

    while i < chars.len() {
        let c = chars[i];
        // `**bold**` — must check the double marker before the single `*`.
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_close_double(&chars, i + 2, '*') {
                flush_plain(&mut plain, &mut out);
                out.push(PanelSeg {
                    text: chars[i + 2..end].iter().collect(),
                    color,
                    bold: true,
                    italic: false,
                    mono: false,
                });
                i = end + 2;
                continue;
            }
        } else if c == '*' || c == '_' {
            if let Some(end) = find_close_single(&chars, i + 1, c) {
                flush_plain(&mut plain, &mut out);
                out.push(PanelSeg {
                    text: chars[i + 1..end].iter().collect(),
                    color,
                    bold: false,
                    italic: true,
                    mono: false,
                });
                i = end + 1;
                continue;
            }
        } else if c == '`'
            && let Some(end) = find_close_single(&chars, i + 1, '`')
        {
            flush_plain(&mut plain, &mut out);
            out.push(PanelSeg {
                text: chars[i + 1..end].iter().collect(),
                color,
                bold: false,
                italic: false,
                mono: true,
            });
            i = end + 1;
            continue;
        }
        plain.push(c);
        i += 1;
    }
    flush_plain(&mut plain, &mut out);
    if out.is_empty() {
        out.push(PanelSeg::plain(String::new(), color));
    }
    out
}

/// Index of the next `marker` char at or after `from`, or None if unterminated.
fn find_close_single(chars: &[char], from: usize, marker: char) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == marker)
}

/// Index of the first char of a `marker`×2 run at or after `from`, or None.
fn find_close_double(chars: &[char], from: usize, marker: char) -> Option<usize> {
    (from..chars.len().saturating_sub(1)).find(|&j| chars[j] == marker && chars[j + 1] == marker)
}

/// Colored segments for an issue/PR: `<symbol> #<number> <title>`, with the
/// title's inline markdown rendered.
fn issue_segments(
    symbol: &str,
    number: u64,
    title: &str,
    status: IssueStatus,
    theme: &EditorTheme,
) -> Vec<PanelSeg> {
    let mut v = vec![
        PanelSeg::plain(format!("{symbol} "), status_color(status, theme)),
        PanelSeg::plain(format!("#{number} "), theme.cyan),
    ];
    v.extend(parse_title_markdown(title, theme.foreground));
    v
}

/// Colored segments for a user: `@login` and, if present, its display name.
fn user_segments(login: &str, name: Option<&str>, theme: &EditorTheme) -> Vec<PanelSeg> {
    let mut v = vec![PanelSeg::plain(format!("@{login}"), theme.cyan)];
    if let Some(name) = name {
        v.push(PanelSeg::plain(format!("  {name}"), theme.comment));
    }
    v
}

/// Lay out colored `segments` into a single styled line.
fn segments_to_line(
    engine: &mut TextEngine,
    segments: &[PanelSeg],
    scale: f32,
    font_size: f32,
    max_text: f32,
    theme: &EditorTheme,
) -> (Layout<Brush>, Vec<Range<usize>>) {
    let mut text = String::new();
    let mut runs = Vec::new();
    // Panel text is built directly (no segment map), so a mono segment's byte range in
    // `text` is already its display range — usable as-is for the code-chip background.
    let mut code_ranges = Vec::new();
    for seg in segments {
        let start = text.len();
        text.push_str(&seg.text);
        if seg.mono {
            code_ranges.push(start..text.len());
        }
        let mut run = StyleRun::new(start..text.len(), peniko_color(seg.color));
        run.bold = seg.bold;
        run.italic = seg.italic;
        run.mono = seg.mono;
        runs.push(run);
    }
    let layout = engine.build_line(
        &text,
        scale,
        font_size,
        1.3,
        peniko_color(theme.foreground),
        Some(max_text),
        &runs,
    );
    (layout, code_ranges)
}

/// The inline-code chip background color, matching the body's code spans.
fn code_chip_color(theme: &EditorTheme) -> vello::peniko::Color {
    peniko_color_alpha(theme.comment, 0.22)
}

/// Paint the inline-code background chip behind each display `range` of `layout`,
/// with the layout drawn at `origin` (device px). Mirrors the body's code chips for
/// hover/autocomplete titles.
fn fill_code_chips(
    scene: &mut Scene,
    layout: &Layout<Brush>,
    ranges: &[Range<usize>],
    origin: (f32, f32),
    color: vello::peniko::Color,
) {
    for r in ranges {
        let sel = Selection::new(
            Cursor::from_byte_index(layout, r.start, Affinity::Downstream),
            Cursor::from_byte_index(layout, r.end, Affinity::Upstream),
        );
        for (bb, _) in sel.geometry(layout) {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                color,
                None,
                &Rect::new(
                    bb.x0 + origin.0 as f64,
                    bb.y0 + origin.1 as f64,
                    bb.x1 + origin.0 as f64,
                    bb.y1 + origin.1 as f64,
                ),
            );
        }
    }
}

/// Position a `panel_w`×`panel_h` panel near `anchor`, preferring below it but
/// flipping above when it would overflow the viewport, and clamping horizontally.
/// Returns the panel's top-left (x, y).
fn place_panel(
    anchor: ScreenRect,
    panel_w: f32,
    panel_h: f32,
    viewport: (f32, f32),
    gap: f64,
) -> (f64, f64) {
    let (viewport_w, viewport_h) = viewport;
    let (ax0, ay0, _ax1, ay1) = anchor;
    let mut x = ax0;
    if x as f32 + panel_w > viewport_w {
        x = (viewport_w - panel_w) as f64;
    }
    x = x.max(0.0);
    let below_y = ay1 + gap;
    let y = if below_y as f32 + panel_h <= viewport_h {
        below_y
    } else {
        (ay0 - gap - panel_h as f64).max(0.0)
    };
    (x, y)
}

/// The colored text segments shown in a ref's hover popover, per validation state.
fn hover_segments(
    reference: &GitHubRef,
    state: &Option<ValidationState>,
    context: Option<&GitHubContext>,
    theme: &EditorTheme,
) -> Vec<PanelSeg> {
    match state {
        Some(ValidationState::Valid(Some(ValidatedRefData::Issue(issue)))) => issue_segments(
            issue.symbol(),
            issue.number,
            &issue.title,
            issue.status(),
            theme,
        ),
        Some(ValidationState::Valid(Some(ValidatedRefData::User(user)))) => {
            user_segments(&user.login, user.name.as_deref(), theme)
        }
        Some(ValidationState::Valid(None)) => vec![
            PanelSeg::plain("✓ ".to_string(), theme.green),
            PanelSeg::plain(reference.short_display(context), theme.cyan),
        ],
        Some(ValidationState::Invalid) => vec![
            PanelSeg::plain("✗ ".to_string(), theme.red),
            PanelSeg::plain(reference.short_display(context), theme.cyan),
        ],
        Some(ValidationState::Pending) | None => vec![
            PanelSeg::plain("… ".to_string(), theme.comment),
            PanelSeg::plain(reference.short_display(context), theme.cyan),
        ],
    }
}

/// Draw the GitHub ref hover popover: a bordered panel anchored below (or above,
/// if space is tight) the hovered ref, showing its validated title/status.
#[allow(clippy::too_many_arguments)]
fn draw_hover_popover(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    editor: &Editor,
    target: &HoverTarget,
    viewport_w: f32,
    viewport_h: f32,
    scale: f32,
) {
    let state = editor.github_validation_cache().get(&target.reference);
    let segs = hover_segments(&target.reference, &state, editor.github_context(), theme);

    let font_size = 14.0;
    // Cap the panel width (long issue titles) so it doesn't run off-screen.
    let max_text = (viewport_w - 2.0 * PADDING * scale).min(480.0 * scale);
    let (layout, code_ranges) = segments_to_line(engine, &segs, scale, font_size, max_text, theme);
    let pad = 8.0 * scale;
    let panel_w = layout.width() + pad * 2.0;
    let panel_h = layout.height() + pad * 2.0;

    let gap = 4.0 * scale as f64;
    let (x, y) = place_panel(
        target.anchor,
        panel_w,
        panel_h,
        (viewport_w, viewport_h),
        gap,
    );

    let rect = Rect::new(x, y, x + panel_w as f64, y + panel_h as f64);
    draw_panel(
        scene,
        peniko_color(theme.background),
        peniko_color(theme.comment),
        &rect,
        6.0 * scale as f64,
        scale as f64,
    );
    let origin = (x as f32 + pad, y as f32 + pad);
    fill_code_chips(scene, &layout, &code_ranges, origin, code_chip_color(theme));
    engine.draw_line(scene, &layout, origin);
}

/// The colored text segments for one autocomplete row.
fn suggestion_segments(s: &AutocompleteSuggestion, theme: &EditorTheme) -> Vec<PanelSeg> {
    match s {
        AutocompleteSuggestion::IssueOrPr {
            number,
            symbol,
            status,
            title,
        } => issue_segments(symbol, *number, title, *status, theme),
        AutocompleteSuggestion::User { login, name } => {
            user_segments(login, name.as_deref(), theme)
        }
    }
}

/// Draw the autocomplete dropdown anchored at the caret (flipping above if it would
/// overflow the viewport bottom). Returns the screen rect of each row for click
/// routing. Rows are stacked top-down; the selected row gets a highlight fill.
#[allow(clippy::too_many_arguments)]
fn draw_autocomplete(
    engine: &mut TextEngine,
    scene: &mut Scene,
    theme: &EditorTheme,
    ac: &AutocompleteState,
    caret: ScreenRect,
    viewport_w: f32,
    viewport_h: f32,
    scale: f32,
) -> Vec<ScreenRect> {
    let font_size = 14.0;
    let pad_x = 10.0 * scale;
    let pad_y = 5.0 * scale;
    let panel_w = (viewport_w - 2.0 * PADDING * scale)
        .min(480.0 * scale)
        .max(140.0 * scale);
    let max_text = panel_w - 2.0 * pad_x;

    // Lay out every row first so the panel can be sized to their total height.
    let mut rows = Vec::with_capacity(ac.suggestions.len());
    for s in &ac.suggestions {
        let segs = suggestion_segments(s, theme);
        let (layout, code) = segments_to_line(engine, &segs, scale, font_size, max_text, theme);
        let h = layout.height() + 2.0 * pad_y;
        rows.push((layout, h, code));
    }
    let panel_h: f32 = rows.iter().map(|(_, h, _)| *h).sum();

    let gap = 4.0 * scale as f64;
    let (x, y) = place_panel(caret, panel_w, panel_h, (viewport_w, viewport_h), gap);

    let panel = Rect::new(x, y, x + panel_w as f64, y + panel_h as f64);
    draw_panel(
        scene,
        peniko_color(theme.background),
        peniko_color(theme.comment),
        &panel,
        6.0 * scale as f64,
        scale as f64,
    );

    let sel_color = peniko_color(theme.selection);
    let mut rects = Vec::with_capacity(rows.len());
    let mut row_top = y;
    for (i, (layout, h, code)) in rows.iter().enumerate() {
        let row_bottom = row_top + *h as f64;
        if i == ac.selected_index {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                sel_color,
                None,
                &Rect::new(x, row_top, x + panel_w as f64, row_bottom),
            );
        }
        let origin = (x as f32 + pad_x, row_top as f32 + pad_y);
        fill_code_chips(scene, layout, code, origin, code_chip_color(theme));
        engine.draw_line(scene, layout, origin);
        rects.push((x, row_top, x + panel_w as f64, row_bottom));
        row_top = row_bottom;
    }
    rects
}

impl ApplicationHandler<WritEvent> for App {
    /// A tokio task finished (validation/suggestion). Results are already in the
    /// shared caches; rebuild the doc (ref colors may have changed) and redraw.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: WritEvent) {
        // GithubUpdated may carry fetched autocomplete suggestions built on the worker;
        // install them before rebuilding. ImageLoaded only changed the shared image
        // cache. Both then rebuild-preserving-scroll (validated refs recolor / a loaded
        // image reflows its line's height) and redraw.
        if matches!(event, WritEvent::GithubUpdated) {
            let fetched = self.ac_slot.lock().unwrap().take();
            if let Some(fetched) = fetched {
                apply_fetched(&mut self.doc_engine.editor, fetched);
            }
        }
        if let Some(state) = self.state.as_ref() {
            let w = state.surface.config.width as f32;
            let (_, vh) = chrome_metrics(state.scale, state.surface.config.height as f32);
            let scale = state.scale;
            let window = state.window.clone();
            self.doc_engine.rebuild_preserving_scroll(w, scale, vh);
            self.sync_images();
            window.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        // Native decorations (shadow, rounded corners, resize, snap — all free), but
        // request a dark title bar to match the editor instead of a bright one over
        // dark content. On Wayland CSD this also honors WINIT_WAYLAND_CSD_THEME.
        let attrs = Window::default_attributes()
            .with_title(self.title.clone())
            .with_theme(Some(Theme::Dark));
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );
        window.set_ime_allowed(true);
        let size = window.inner_size();

        let surface = pollster::block_on(self.context.create_surface(
            window.clone(),
            size.width.max(1),
            size.height.max(1),
            wgpu::PresentMode::AutoVsync,
        ))
        .expect("failed to create surface");

        let dev = &self.context.devices[surface.dev_id];
        self.renderer = Some(
            Renderer::new(
                &dev.device,
                RendererOptions {
                    use_cpu: false,
                    antialiasing_support: vello::AaSupport::area_only(),
                    num_init_threads: None,
                    pipeline_cache: None,
                },
            )
            .expect("failed to create Vello renderer"),
        );

        let scale = window.scale_factor() as f32;
        let (_, editor_h) = chrome_metrics(scale, size.height as f32);
        let doc = self.doc_engine.rebuild(size.width as f32, scale, editor_h);
        self.doc_engine.doc = Some(doc);
        self.state = Some(ActiveSurface {
            surface,
            window,
            scale,
        });
        // Validate refs already present in the loaded file, and start loading images.
        self.spawn_validations();
        self.sync_images();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.context.resize_surface(
                    &mut state.surface,
                    size.width.max(1),
                    size.height.max(1),
                );
                let (_, editor_h) = chrome_metrics(state.scale, size.height as f32);
                let scale = state.scale;
                let doc = self.doc_engine.rebuild(size.width as f32, scale, editor_h);
                self.doc_engine.doc = Some(doc);
                state.window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                state.scale = scale_factor as f32;
                state.window.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y * WHEEL_LINE_STEP * state.scale,
                    MouseScrollDelta::PixelDelta(p) => -p.y as f32,
                };
                let (_, editor_h) = chrome_metrics(state.scale, state.surface.config.height as f32);
                let remeasure = if let Some(doc) = self.doc_engine.doc.as_mut() {
                    doc.scroll_by(dy, editor_h);
                    doc.needs_remeasure(editor_h)
                } else {
                    false
                };
                self.hovered = None; // stored anchor goes stale on scroll
                // Wheel-scrolled into height-estimated lines → rebuild to lay them out.
                if remeasure {
                    let w = state.surface.config.width as f32;
                    let scale = state.scale;
                    self.doc_engine
                        .rebuild_preserving_scroll(w, scale, editor_h);
                    // Image lines scrolled into the materialized range need loading.
                    sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                }
                // Visible refs change with scroll_y alone (no rebuild needed); the cache
                // dedups, so validating on every wheel tick is cheap and safe.
                spawn_ref_validations(
                    &self.doc_engine.editor,
                    self.doc_engine.doc.as_ref().map(|d| {
                        let (a, b) = d.visible_range(editor_h);
                        a..b
                    }),
                    &self.runtime,
                    &self.proxy,
                );
                if self.doc_engine.doc.is_some() {
                    state.window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::Ime(winit::event::Ime::Enabled) => {
                self.doc_engine.preedit = None;
            }
            // In-progress composition: splice it into the caret line (render-only, no
            // buffer mutation) and move the OS candidate popup to the composition caret.
            WindowEvent::Ime(winit::event::Ime::Preedit(text, cursor)) => {
                self.doc_engine.preedit =
                    (!text.is_empty()).then_some(Preedit { text, cursor });
                let w = state.surface.config.width as f32;
                let (_, vh) = chrome_metrics(state.scale, state.surface.config.height as f32);
                self.doc_engine.rebuild_preserving_scroll(w, state.scale, vh);
                if let Some(doc) = self.doc_engine.doc.as_ref() {
                    let cw = CARET_WIDTH * state.scale;
                    let rect = doc.preedit_caret_rect(cw).or_else(|| {
                        doc.caret_rect(self.doc_engine.editor.cursor_position(), cw)
                    });
                    if let Some((x0, y0, x1, y1)) = rect {
                        state.window.set_ime_cursor_area(
                            PhysicalPosition::new(x0, y0),
                            PhysicalSize::new((x1 - x0).max(1.0), (y1 - y0).max(1.0)),
                        );
                    }
                }
                state.window.request_redraw();
            }
            // Commit: clear the composition, then insert the finalized text.
            WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                let had_preedit = self.doc_engine.preedit.take().is_some();
                let w = state.surface.config.width as f32;
                let (_, vh) = chrome_metrics(state.scale, state.surface.config.height as f32);
                if !text.is_empty() {
                    self.doc_engine.editor.insert_str(&text);
                    self.hovered = None;
                    self.doc_engine.refresh(w, state.scale, vh);
                    spawn_ref_validations(
                        &self.doc_engine.editor,
                        self.doc_engine.doc.as_ref().map(|d| {
                            let (a, b) = d.visible_range(vh);
                            a..b
                        }),
                        &self.runtime,
                        &self.proxy,
                    );
                    sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                    state.window.request_redraw();
                } else if had_preedit {
                    // Composition cancelled (empty commit): drop the spliced preedit.
                    self.doc_engine.rebuild_preserving_scroll(w, state.scale, vh);
                    state.window.request_redraw();
                }
            }
            WindowEvent::Ime(winit::event::Ime::Disabled) => {
                if self.doc_engine.preedit.take().is_some() {
                    let w = state.surface.config.width as f32;
                    let (_, vh) =
                        chrome_metrics(state.scale, state.surface.config.height as f32);
                    self.doc_engine.rebuild_preserving_scroll(w, state.scale, vh);
                    state.window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x as f32, position.y as f32);
                if self.mouse_down {
                    if let Some(off) = self
                        .doc_engine
                        .doc
                        .as_ref()
                        .and_then(|d| d.hit_test(self.mouse_pos.0, self.mouse_pos.1))
                    {
                        self.doc_engine.editor.drag(off);
                        let w = state.surface.config.width as f32;
                        let (_, vh) =
                            chrome_metrics(state.scale, state.surface.config.height as f32);
                        self.doc_engine.refresh(w, state.scale, vh);
                        state.window.request_redraw();
                    }
                } else {
                    // Not dragging: update the hovered GitHub ref (popover source).
                    let new = self.doc_engine.doc.as_ref().and_then(|d| {
                        find_hover_target(
                            &self.doc_engine.editor,
                            d,
                            self.mouse_pos.0,
                            self.mouse_pos.1,
                        )
                    });
                    let changed = self.hovered.as_ref().map(|h| &h.reference)
                        != new.as_ref().map(|h| &h.reference);
                    self.hovered = new;
                    if changed {
                        state.window.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                self.mouse_down = true;
                let w = state.surface.config.width as f32;
                let (_, vh) = chrome_metrics(state.scale, state.surface.config.height as f32);

                // Autocomplete row click: accept that suggestion, don't move the caret.
                if self.doc_engine.editor.autocomplete().is_some()
                    && let Some(row) = self.ac_row_rects.iter().position(|r| {
                        let (x0, y0, x1, y1) = *r;
                        (self.mouse_pos.0 as f64) >= x0
                            && (self.mouse_pos.0 as f64) <= x1
                            && (self.mouse_pos.1 as f64) >= y0
                            && (self.mouse_pos.1 as f64) <= y1
                    })
                {
                    self.doc_engine.editor.autocomplete_select(row);
                    if self.doc_engine.editor.accept_autocomplete_suggestion() {
                        self.hovered = None;
                        self.doc_engine.refresh(w, state.scale, vh);
                        spawn_ref_validations(
                            &self.doc_engine.editor,
                            self.doc_engine.doc.as_ref().map(|d| {
                                let (a, b) = d.visible_range(vh);
                                a..b
                            }),
                            &self.runtime,
                            &self.proxy,
                        );
                        sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                        state.window.request_redraw();
                    }
                    return;
                }

                if let Some(off) = self
                    .doc_engine
                    .doc
                    .as_ref()
                    .and_then(|d| d.hit_test(self.mouse_pos.0, self.mouse_pos.1))
                {
                    // Clicking a checkbox toggles it (leaving the caret where it was)
                    // rather than placing the caret in the box.
                    if let Some(line) = self.doc_engine.editor.checkbox_at(off) {
                        self.doc_engine.editor.toggle_checkbox(line);
                        self.doc_engine.refresh(w, state.scale, vh);
                        state.window.request_redraw();
                        return;
                    }
                    // A click on an image block hit-tests to the line start (the image
                    // has no text). Note the line first; after the click reveals its raw
                    // markdown, re-place the caret at the clicked x on the revealed row
                    // (its short row no longer sits under the original click y).
                    let image_line = self.doc_engine.doc.as_ref().and_then(|d| {
                        let line = self.doc_engine.editor.line_of(off);
                        d.is_image_line(line).then_some(line)
                    });
                    self.doc_engine
                        .editor
                        .click(off, self.modifiers.shift_key(), 1);
                    self.doc_engine.refresh(w, state.scale, vh);
                    if let Some(line) = image_line
                        && let Some(off2) = self
                            .doc_engine
                            .doc
                            .as_ref()
                            .and_then(|d| d.offset_in_line_at_x(line, self.mouse_pos.0))
                    {
                        self.doc_engine.editor.set_cursor(off2);
                    }
                    // Moving the caret off an image line materializes it as an image
                    // block, whose URL now needs loading — so kick off fetches here too.
                    sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                    // Clicking into/out of a ref opens/closes the popup.
                    sync_autocomplete(
                        &mut self.doc_engine.editor,
                        &self.runtime,
                        &self.proxy,
                        &self.ac_gen,
                        &self.ac_slot,
                    );
                    state.window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                self.mouse_down = false;
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                // Ctrl-W / Super-W closes the window (does not auto-save; use Ctrl-S).
                let cmd = self.modifiers.control_key() || self.modifiers.super_key();
                if cmd
                    && matches!(&event.logical_key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("w"))
                {
                    event_loop.exit();
                    return;
                }
                let w = state.surface.config.width as f32;
                let (_, vh) = chrome_metrics(state.scale, state.surface.config.height as f32);

                // Clipboard: Ctrl/Super + C copy, X cut, V paste.
                if cmd && let Key::Character(c) = &event.logical_key {
                    match c.as_str().to_ascii_lowercase().as_str() {
                        "c" | "x" if self.doc_engine.editor.selected_text().is_some() => {
                            if let Some(sel) = self.doc_engine.editor.selected_text()
                                && let Some(cb) = self.clipboard.as_mut()
                            {
                                let _ = cb.set_text(sel);
                            }
                            if c.as_str().eq_ignore_ascii_case("x") {
                                self.doc_engine.editor.backspace(); // delete the selection
                                self.doc_engine.refresh(w, state.scale, vh);
                                spawn_ref_validations(
                                    &self.doc_engine.editor,
                                    self.doc_engine.doc.as_ref().map(|d| {
                                        let (a, b) = d.visible_range(vh);
                                        a..b
                                    }),
                                    &self.runtime,
                                    &self.proxy,
                                );
                                sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                                state.window.request_redraw();
                            }
                            return;
                        }
                        "v" => {
                            if let Some(cb) = self.clipboard.as_mut()
                                && let Ok(text) = cb.get_text()
                                && !text.is_empty()
                            {
                                self.doc_engine.editor.insert_str(&text);
                                self.doc_engine.refresh(w, state.scale, vh);
                                spawn_ref_validations(
                                    &self.doc_engine.editor,
                                    self.doc_engine.doc.as_ref().map(|d| {
                                        let (a, b) = d.visible_range(vh);
                                        a..b
                                    }),
                                    &self.runtime,
                                    &self.proxy,
                                );
                                sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                                sync_autocomplete(
                                    &mut self.doc_engine.editor,
                                    &self.runtime,
                                    &self.proxy,
                                    &self.ac_gen,
                                    &self.ac_slot,
                                );
                                state.window.request_redraw();
                            }
                            return;
                        }
                        _ => {}
                    }
                }

                // When the autocomplete popup is open, route navigation keys to it.
                // `ac_has` is Some(has_suggestions) iff the popup is open.
                let ac_has = self
                    .doc_engine
                    .editor
                    .autocomplete()
                    .map(|ac| !ac.suggestions.is_empty());
                if let Some(has) = ac_has {
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.doc_engine.editor.close_autocomplete();
                            state.window.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::ArrowUp) if has => {
                            self.doc_engine.editor.autocomplete_move(false);
                            state.window.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::ArrowDown) if has => {
                            self.doc_engine.editor.autocomplete_move(true);
                            state.window.request_redraw();
                            return;
                        }
                        // `has` guarantees suggestions exist, so accept() always
                        // succeeds here (it mutates as part of the guard).
                        Key::Named(NamedKey::Enter | NamedKey::Tab)
                            if has && self.doc_engine.editor.accept_autocomplete_suggestion() =>
                        {
                            self.hovered = None;
                            self.doc_engine.refresh(w, state.scale, vh);
                            spawn_ref_validations(
                                &self.doc_engine.editor,
                                self.doc_engine.doc.as_ref().map(|d| {
                                    let (a, b) = d.visible_range(vh);
                                    a..b
                                }),
                                &self.runtime,
                                &self.proxy,
                            );
                            sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                            state.window.request_redraw();
                            return;
                        }
                        _ => {}
                    }
                }

                if apply_key(&mut self.doc_engine.editor, self.modifiers, &event) {
                    self.hovered = None;
                    self.doc_engine.refresh(w, state.scale, vh);
                    spawn_ref_validations(
                        &self.doc_engine.editor,
                        self.doc_engine.doc.as_ref().map(|d| {
                            let (a, b) = d.visible_range(vh);
                            a..b
                        }),
                        &self.runtime,
                        &self.proxy,
                    );
                    sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                    sync_autocomplete(
                        &mut self.doc_engine.editor,
                        &self.runtime,
                        &self.proxy,
                        &self.ac_gen,
                        &self.ac_slot,
                    );
                    state.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                self.scene.reset();

                // Keep the native title bar's text current (filename + dirty marker).
                let desired = window_title(&self.doc_engine.editor);
                if desired != self.title {
                    self.title = desired;
                    state.window.set_title(&self.title);
                }

                let width = state.surface.config.width as f32;
                let height = state.surface.config.height as f32;
                let (content_top, editor_h) = chrome_metrics(state.scale, height);

                if let Some(doc) = self.doc_engine.doc.as_ref() {
                    // Editor content, clipped to the region between the chrome bars.
                    let clip = Rect::new(
                        0.0,
                        content_top as f64,
                        width as f64,
                        (content_top + editor_h) as f64,
                    );
                    self.scene
                        .push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
                    // Draw order (all before glyphs): diff row/word bg, quote gutters,
                    // then selection.
                    doc.draw_added_backgrounds(&mut self.scene, editor_h);
                    doc.draw_blockquote_gutters(&mut self.scene, editor_h);
                    doc.draw_horizontal_rules(&mut self.scene, editor_h);
                    doc.draw_images(&mut self.doc_engine.text_engine, &mut self.scene, editor_h);
                    if let Some(sel) = self.doc_engine.editor.selection_range() {
                        let color = peniko_color(self.doc_engine.theme.selection);
                        for (x0, y0, x1, y1) in doc.selection_rects(sel) {
                            self.scene.fill(
                                Fill::NonZero,
                                Affine::IDENTITY,
                                color,
                                None,
                                &Rect::new(x0, y0, x1, y1),
                            );
                        }
                    }
                    doc.draw(&self.doc_engine.text_engine, &mut self.scene, editor_h);
                    let cw = CARET_WIDTH * state.scale;
                    let caret = doc.preedit_caret_rect(cw).or_else(|| {
                        doc.caret_rect(self.doc_engine.editor.cursor_position(), cw)
                    });
                    if let Some((x0, y0, x1, y1)) = caret {
                        self.scene.fill(
                            Fill::NonZero,
                            Affine::IDENTITY,
                            peniko_color(self.doc_engine.theme.foreground),
                            None,
                            &Rect::new(x0, y0, x1.max(x0 + 1.0), y1),
                        );
                    }
                    self.scene.pop_layer();

                    // Chrome: status bar (bottom). The title bar is the OS/compositor's
                    // native one (filename goes there via `set_title`); we don't draw our
                    // own — that would double up on the native decoration.
                    let info = build_status_info(&self.doc_engine.editor, doc, editor_h);
                    draw_status_bar(
                        &mut self.doc_engine.text_engine,
                        &mut self.scene,
                        &self.doc_engine.theme,
                        &BarRect {
                            x0: 0.0,
                            y0: (content_top + editor_h) as f64,
                            x1: width as f64,
                            y1: height as f64,
                        },
                        &info,
                        state.scale,
                    );

                    // Overlays on top of everything (unclipped): the autocomplete
                    // dropdown takes priority over the hover popover.
                    let ac_open = self
                        .doc_engine
                        .editor
                        .autocomplete()
                        .is_some_and(|ac| !ac.suggestions.is_empty());
                    if ac_open {
                        if let Some(caret) = doc.caret_rect(
                            self.doc_engine.editor.cursor_position(),
                            CARET_WIDTH * state.scale,
                        ) {
                            let ac = self.doc_engine.editor.autocomplete().expect("open");
                            self.ac_row_rects = draw_autocomplete(
                                &mut self.doc_engine.text_engine,
                                &mut self.scene,
                                &self.doc_engine.theme,
                                ac,
                                caret,
                                width,
                                height,
                                state.scale,
                            );
                        }
                    } else {
                        self.ac_row_rects.clear();
                        if let Some(target) = self.hovered.as_ref() {
                            draw_hover_popover(
                                &mut self.doc_engine.text_engine,
                                &mut self.scene,
                                &self.doc_engine.theme,
                                &self.doc_engine.editor,
                                target,
                                width,
                                height,
                                state.scale,
                            );
                        }
                    }
                }

                let dev = &self.context.devices[state.surface.dev_id];
                let params = RenderParams {
                    base_color: peniko_color(self.doc_engine.theme.background),
                    width: state.surface.config.width,
                    height: state.surface.config.height,
                    antialiasing_method: AaConfig::Area,
                };

                // wgpu 29: get_current_texture returns an enum, not a Result.
                let surface_texture = match state.surface.surface.get_current_texture() {
                    CurrentSurfaceTexture::Success(t) | CurrentSurfaceTexture::Suboptimal(t) => t,
                    other => {
                        eprintln!("[writ] skip frame: {:?}", other);
                        return;
                    }
                };

                // Vello 0.9 has no render_to_surface: render into the intermediate
                // STORAGE texture, then blit that into the swapchain frame.
                self.renderer
                    .as_mut()
                    .expect("renderer missing")
                    .render_to_texture(
                        &dev.device,
                        &dev.queue,
                        &self.scene,
                        &state.surface.target_view,
                        &params,
                    )
                    .expect("render_to_texture failed");

                let mut encoder =
                    dev.device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("surface_blit"),
                        });
                state.surface.blitter.copy(
                    &dev.device,
                    &mut encoder,
                    &state.surface.target_view,
                    &surface_texture
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default()),
                );
                dev.queue.submit([encoder.finish()]);
                state.window.pre_present_notify();
                surface_texture.present();
            }
            _ => {}
        }
    }

    /// Poll for external file edits (e.g. an agent writing the file) between events,
    /// reloading + recomputing the diff live, and keep a slow poll timer running.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.doc_engine.editor.poll_file_changes()
            && let Some(state) = self.state.as_ref()
        {
            let w = state.surface.config.width as f32;
            let (_, editor_h) = chrome_metrics(state.scale, state.surface.config.height as f32);
            let window = state.window.clone();
            let scale = state.scale;
            self.doc_engine.refresh(w, scale, editor_h);
            spawn_ref_validations(
                &self.doc_engine.editor,
                self.doc_engine.doc.as_ref().map(|d| {
                    let (a, b) = d.visible_range(editor_h);
                    a..b
                }),
                &self.runtime,
                &self.proxy,
            );
            sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
            window.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(200),
        ));
    }
}

/// Boot the shell: install the TLS provider, stand up a tokio runtime for HTTP/
/// GitHub work (kept alive for the process), and run the winit event loop.
pub fn run() -> Result<()> {
    // Default winit's Wayland client-side decoration (sctk-adwaita) to a dark title
    // bar to match the editor. Set before any threads spawn; user can override.
    if std::env::var_os("WINIT_WAYLAND_CSD_THEME").is_none() {
        // SAFETY: called at process start, before the tokio runtime spawns any threads.
        unsafe { std::env::set_var("WINIT_WAYLAND_CSD_THEME", "dark") };
    }
    let _ = rustls::crypto::ring::default_provider().install_default();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _guard = runtime.enter();

    // Headless golden-image path: render one frame to a PNG and exit. Used to
    // verify the render offscreen (no compositor screenshot needed).
    if let Ok(path) = std::env::var("WRIT_SHELL_SNAPSHOT") {
        let env_f32 = |k: &str, d: f32| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let w = env_f32("WRIT_SHELL_W", 1000.0) as u32;
        let h = env_f32("WRIT_SHELL_H", 400.0) as u32;
        let scroll = env_f32("WRIT_SHELL_SCROLL", 0.0);
        return snapshot(&path, w, h, scroll);
    }

    let editor = load_editor_from_cli();
    let event_loop = EventLoop::<WritEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(editor, proxy, runtime.handle().clone());
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Build the editor from CLI args: open the `--file`, set GitHub context, and start
/// watching the file for external edits (the pivot use case — an agent edits the
/// file, writ shows the live diff). With no args, falls back to the sample doc.
fn load_editor_from_cli() -> Editor {
    use clap::Parser;

    // No args → sample document (dev/demo).
    if std::env::args().len() <= 1 {
        return Editor::new(SAMPLE_DOC);
    }

    let config = Config::parse();
    let Some(path) = config.file.clone() else {
        return Editor::new(SAMPLE_DOC);
    };

    let mut editor = Editor::open(&path);

    // GitHub context for ref detection: explicit --github-repo, else auto-detect.
    let context = config
        .github_repo
        .as_deref()
        .and_then(parse_github_repo_string)
        .or_else(|| detect_github_context(&path));
    if let Some(ctx) = context {
        editor.set_github_context(ctx);
    }
    if let Some(token) = config.github_token.clone() {
        editor.set_github_client(GitHubClient::new(token));
    }
    // Populate github_refs_by_line / naked_urls_by_line so the first frame can color
    // (and the shell can spawn validation for) any refs already in the file.
    editor.refresh_detection(0..usize::MAX);

    if let Err(e) = editor.watch_file() {
        eprintln!("[writ] file watch failed: {e}");
    }
    editor
}

/// Synchronously decode standalone images into the cache for the headless snapshot
/// frame: local reads (relative to the doc's directory) plus remote `http(s)` fetched
/// by blocking on the current runtime, so the golden frame reflects the real result.
fn load_local_images_blocking(doc_dir: Option<&Path>, urls: &[String], cache: &ImageCache) {
    for url in urls {
        let loaded = if url.starts_with("http://") || url.starts_with("https://") {
            tokio::runtime::Handle::current().block_on(load_remote_image(url))
        } else {
            resolve_local_image(doc_dir, url)
                .and_then(|p| std::fs::read(p).ok())
                .and_then(|bytes| decode(&bytes))
        };
        match loaded {
            Some(img) => cache.set_loaded(url, img),
            None => cache.set_failed(url),
        }
    }
}

/// Synchronously validate every detected GitHub ref (blocking on the current tokio
/// runtime). Snapshot-only helper so a single headless frame shows final ref colors.
fn validate_all_blocking(editor: &mut Editor) {
    let Some(client) = editor.github_client().cloned() else {
        return;
    };
    let cache = editor.github_validation_cache().clone();
    let refs = editor.detected_refs();
    tokio::runtime::Handle::current().block_on(async {
        for r in refs {
            match client.validate_ref(&r).await {
                ValidationResult::ValidWithData(d) => cache.set_valid(r, Some(d)),
                ValidationResult::ValidNoData => cache.set_valid(r, None),
                ValidationResult::Invalid => cache.set_invalid(r),
            }
        }
    });
}

/// Synchronously fetch + install autocomplete suggestions for the open popup.
/// Snapshot-only helper (the GUI fetches asynchronously with a debounce).
fn fetch_autocomplete_blocking(editor: &mut Editor) {
    let Some((trigger, prefix)) = editor.begin_autocomplete_fetch() else {
        return;
    };
    let Some(client) = editor.github_client().cloned() else {
        return;
    };
    let Some(context) = editor.github_context().cloned() else {
        return;
    };
    let fetched = tokio::runtime::Handle::current().block_on(async {
        match trigger {
            AutocompleteTrigger::Issue => {
                let issues = client
                    .issues_matching_prefix(&context.owner, &context.repo, &prefix, 8)
                    .await;
                FetchedSuggestions::Issues { prefix, issues }
            }
            AutocompleteTrigger::User => {
                let users = client
                    .users_matching_prefix(&context.owner, &context.repo, &prefix, 8)
                    .await;
                FetchedSuggestions::Users { prefix, users }
            }
        }
    });
    apply_fetched(editor, fetched);
}

/// Render a single frame of the document to an offscreen texture and write it to
/// `path` as a PNG. Independent of any surface/window, so it runs headlessly and
/// doubles as a golden-image harness for later phases.
pub fn snapshot(path: &str, width: u32, height: u32, scroll_y: f32) -> Result<()> {
    use vello::wgpu::{
        BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Extent3d, MapMode, Origin3d,
        PollType, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureAspect,
        TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, TextureViewDescriptor,
    };

    let mut context = RenderContext::new();
    let dev_id = pollster::block_on(context.device(None))
        .ok_or_else(|| anyhow::anyhow!("no wgpu device (set WGPU_BACKEND=vulkan on Asahi)"))?;
    let device = &context.devices[dev_id].device;
    let queue = &context.devices[dev_id].queue;

    // WRIT_SHELL_FILE opens a real file (with live HEAD diff); else the sample doc.
    let mut editor = match std::env::var("WRIT_SHELL_FILE") {
        Ok(p) => Editor::open(std::path::Path::new(&p)),
        Err(_) => Editor::new(SAMPLE_DOC),
    };
    // Place the caret mid-document so the snapshot exercises caret geometry.
    let env_usize = |k: &str| std::env::var(k).ok().and_then(|v| v.parse().ok());
    editor.set_cursor(env_usize("WRIT_SHELL_CURSOR").unwrap_or(0));
    // Optional selection A..B (via click+drag) to exercise selection rendering.
    if let (Some(a), Some(b)) = (env_usize("WRIT_SHELL_SEL_A"), env_usize("WRIT_SHELL_SEL_B")) {
        editor.click(a, false, 1);
        editor.drag(b);
    }
    // Optional diff: set a HEAD base that differs from the current doc so additions
    // (green) + a word-level change render. Exercises the inline-diff path.
    if std::env::var("WRIT_SHELL_DIFF").is_ok() {
        let base = SAMPLE_DOC
            .replace("## Scrolling\n\n", "")
            .replace("**whole document**", "**the document**");
        editor.set_head_base(&base);
    }
    // Optional GitHub golden-image check: wire a client from GITHUB_TOKEN +
    // WRIT_SHELL_GITHUB_REPO, then *synchronously* validate every ref so the single
    // frame shows final ref colors (the GUI does this asynchronously).
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && std::env::var("WRIT_SHELL_GITHUB").is_ok()
    {
        if let Some(ctx) = std::env::var("WRIT_SHELL_GITHUB_REPO")
            .ok()
            .and_then(|s| parse_github_repo_string(&s))
        {
            editor.set_github_context(ctx);
        }
        editor.set_github_client(GitHubClient::new(token));
        editor.refresh_detection(0..usize::MAX);
        validate_all_blocking(&mut editor);
        // WRIT_SHELL_AUTOCOMPLETE opens the popup at the caret (WRIT_SHELL_CURSOR).
        if std::env::var("WRIT_SHELL_AUTOCOMPLETE").is_ok() {
            editor.update_autocomplete_from_cursor();
            if editor.autocomplete().is_some() {
                fetch_autocomplete_blocking(&mut editor);
            }
        }
    }
    let (content_top, editor_h) = chrome_metrics(1.0, height as f32);
    // Reuse the shared rebuild path (caches + Parley/Vello engine) headlessly.
    let mut de = DocEngine {
        text_engine: TextEngine::new(),
        line_cache: LineCache::new(),
        render_cache: RenderCache::new(),
        theme: EditorTheme::dracula(),
        editor,
        doc: None,
        images: ImageCache::new(),
        preedit: None,
    };
    let mut doc = de.rebuild(width as f32, 1.0, scroll_y + editor_h);
    // Synchronously decode local standalone images so they appear in the single frame
    // (remote images stay a placeholder headlessly). Then rebuild so their heights land.
    let img_dir = de
        .editor
        .file_path()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));
    load_local_images_blocking(img_dir.as_deref(), &doc.image_urls(), &de.images);
    doc = de.rebuild(width as f32, 1.0, scroll_y + editor_h);
    doc.scroll_by(scroll_y, editor_h);
    let mut scene = Scene::new();
    let clip = Rect::new(
        0.0,
        content_top as f64,
        width as f64,
        (content_top + editor_h) as f64,
    );
    scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
    doc.draw_added_backgrounds(&mut scene, editor_h);
    doc.draw_blockquote_gutters(&mut scene, editor_h);
    doc.draw_horizontal_rules(&mut scene, editor_h);
    doc.draw_images(&mut de.text_engine, &mut scene, editor_h);
    if let Some(sel) = de.editor.selection_range() {
        let color = peniko_color(de.theme.selection);
        for (x0, y0, x1, y1) in doc.selection_rects(sel) {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                color,
                None,
                &Rect::new(x0, y0, x1, y1),
            );
        }
    }
    doc.draw(&de.text_engine, &mut scene, editor_h);
    if let Some((x0, y0, x1, y1)) = doc.caret_rect(de.editor.cursor_position(), 2.0) {
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            peniko_color(de.theme.foreground),
            None,
            &Rect::new(x0, y0, x1.max(x0 + 1.0), y1),
        );
    }
    scene.pop_layer();
    let info = build_status_info(&de.editor, &doc, editor_h);
    draw_status_bar(
        &mut de.text_engine,
        &mut scene,
        &de.theme,
        &BarRect {
            x0: 0.0,
            y0: (content_top + editor_h) as f64,
            x1: width as f64,
            y1: height as f64,
        },
        &info,
        1.0,
    );

    // Optional hover-popover golden image: WRIT_SHELL_HOVER=<byte offset> anchors the
    // popover on the ref containing that offset.
    if let Some(off) = std::env::var("WRIT_SHELL_HOVER")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        && let Some(t) = hover_target_at_offset(&de.editor, &doc, off)
    {
        draw_hover_popover(
            &mut de.text_engine,
            &mut scene,
            &de.theme,
            &de.editor,
            &t,
            width as f32,
            height as f32,
            1.0,
        );
    }

    // Optional autocomplete golden image (see WRIT_SHELL_AUTOCOMPLETE above).
    if let Some(ac) = de
        .editor
        .autocomplete()
        .filter(|ac| !ac.suggestions.is_empty())
        && let Some(caret) = doc.caret_rect(de.editor.cursor_position(), 2.0)
    {
        draw_autocomplete(
            &mut de.text_engine,
            &mut scene,
            &de.theme,
            ac,
            caret,
            width as f32,
            height as f32,
            1.0,
        );
    }

    let target = device.create_texture(&TextureDescriptor {
        label: Some("snapshot_target"),
        size: Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8Unorm,
        usage: TextureUsages::STORAGE_BINDING | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target.create_view(&TextureViewDescriptor::default());

    let mut renderer = Renderer::new(
        device,
        RendererOptions {
            use_cpu: false,
            antialiasing_support: vello::AaSupport::area_only(),
            num_init_threads: None,
            pipeline_cache: None,
        },
    )
    .map_err(|e| anyhow::anyhow!("Renderer::new failed: {e:?}"))?;
    renderer
        .render_to_texture(
            device,
            queue,
            &scene,
            &target_view,
            &RenderParams {
                base_color: peniko_color(de.theme.background),
                width,
                height,
                antialiasing_method: AaConfig::Area,
            },
        )
        .map_err(|e| anyhow::anyhow!("render_to_texture failed: {e:?}"))?;

    // Copy the texture into a mappable buffer (rows padded to 256-byte alignment).
    let bytes_per_pixel = 4u32;
    let unpadded = width * bytes_per_pixel;
    let align = 256u32;
    let padded = unpadded.div_ceil(align) * align;
    let buffer = device.create_buffer(&BufferDescriptor {
        label: Some("snapshot_readback"),
        size: (padded * height) as u64,
        usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: Origin3d::ZERO,
            aspect: TextureAspect::All,
        },
        TexelCopyBufferInfo {
            buffer: &buffer,
            layout: TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let slice = buffer.slice(..);
    slice.map_async(MapMode::Read, |r| r.expect("map_async failed"));
    let _ = device.poll(PollType::wait_indefinitely());
    let data = slice.get_mapped_range();

    // Write a binary PPM (P6) — dependency-free; converted to PNG by the caller.
    let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
    for y in 0..height {
        let row = &data[(y * padded) as usize..(y * padded + unpadded) as usize];
        for px in row.chunks_exact(4) {
            ppm.extend_from_slice(&px[..3]); // drop alpha
        }
    }
    std::fs::write(path, &ppm)?;
    eprintln!("[writ] wrote snapshot: {path} ({width}x{height})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vello::peniko::Color;

    const C: Color = Color::new([0.1, 0.2, 0.3, 1.0]);

    fn texts(segs: &[PanelSeg]) -> Vec<&str> {
        segs.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn plain_title_is_one_plain_span() {
        let segs = parse_title_markdown("just a title", C);
        assert_eq!(texts(&segs), vec!["just a title"]);
        assert!(!segs[0].bold && !segs[0].italic && !segs[0].mono);
    }

    #[test]
    fn empty_title_yields_one_empty_span() {
        let segs = parse_title_markdown("", C);
        assert_eq!(texts(&segs), vec![""]);
    }

    #[test]
    fn code_span_strips_backticks_and_sets_mono() {
        let segs = parse_title_markdown("Fix `foo()` crash", C);
        assert_eq!(texts(&segs), vec!["Fix ", "foo()", " crash"]);
        assert!(segs[1].mono && !segs[1].bold && !segs[1].italic);
        assert!(!segs[0].mono && !segs[2].mono);
        for s in &segs {
            assert!(!s.text.contains('`'));
        }
    }

    #[test]
    fn bold_span_strips_delimiters_and_sets_bold() {
        let segs = parse_title_markdown("**bold**", C);
        assert_eq!(texts(&segs), vec!["bold"]);
        assert!(segs[0].bold && !segs[0].italic && !segs[0].mono);
        assert!(!segs[0].text.contains('*'));
    }

    #[test]
    fn star_and_underscore_italics() {
        let star = parse_title_markdown("*it*", C);
        assert_eq!(texts(&star), vec!["it"]);
        assert!(star[0].italic && !star[0].bold && !star[0].mono);

        let under = parse_title_markdown("_it_", C);
        assert_eq!(texts(&under), vec!["it"]);
        assert!(under[0].italic && !under[0].bold && !under[0].mono);

        for segs in [&star, &under] {
            assert!(!segs[0].text.contains('*') && !segs[0].text.contains('_'));
        }
    }

    #[test]
    fn unterminated_backtick_stays_literal() {
        let segs = parse_title_markdown("a ` b", C);
        assert_eq!(texts(&segs), vec!["a ` b"]);
        assert!(!segs[0].mono);
    }

    #[test]
    fn mixed_markdown_in_title() {
        let segs = parse_title_markdown("**b** and `c` and *i*", C);
        assert_eq!(texts(&segs), vec!["b", " and ", "c", " and ", "i"]);
        assert!(segs[0].bold);
        assert!(segs[2].mono);
        assert!(segs[4].italic);
    }

    #[test]
    fn issue_segments_prefixes_plain_then_title_spans() {
        let theme = EditorTheme::default();
        let segs = issue_segments("●", 42, "Fix `foo`", IssueStatus::Open, &theme);
        assert_eq!(texts(&segs), vec!["● ", "#42 ", "Fix ", "foo"]);
        assert!(!segs[0].mono && !segs[1].mono);
        assert!(segs[3].mono);
    }

    #[test]
    fn segments_to_line_mono_run_sets_mono_flag() {
        let mut engine = TextEngine::new();
        let theme = EditorTheme::default();
        let segs = vec![
            PanelSeg::plain("x ".to_string(), C),
            PanelSeg {
                text: "code".to_string(),
                color: C,
                bold: false,
                italic: false,
                mono: true,
            },
        ];
        // Exercises the draw path's run construction without panicking.
        let _ = segments_to_line(&mut engine, &segs, 1.0, 14.0, 400.0, &theme);
    }
}
