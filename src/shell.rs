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
//! `WRIT_SHELL_SNAPSHOT=out.png` (+ optional `WRIT_SHELL_{W,H,SCROLL,CURSOR,SEL_A,SEL_B}`)
//! to render one frame headlessly instead.

use std::ops::Range;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use unicode_segmentation::UnicodeSegmentation;
use vello::kurbo::{Affine, BezPath, Point, Rect, Stroke};
use vello::peniko::Fill;
use vello::util::{RenderContext, RenderSurface};
use vello::wgpu;
use vello::wgpu::CurrentSurfaceTexture;
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{Theme, Window, WindowId};

use winit::event_loop::ControlFlow;

use crate::buffer::Buffer;
use crate::chrome::{BarRect, FindButtonRects, StatusInfo, draw_find_bar, draw_status_bar};
use crate::config::Config;
use crate::consts::{
    CARET_WIDTH, FIND_ROW_H, FONT_SIZE, LINE_HEIGHT, OUTLINE_WIDTH, PADDING, STATUS_BAR_H,
    WHEEL_LINE_STEP,
};
use crate::core::{AutocompleteSuggestion, AutocompleteTrigger, Editor, FieldFocus, FindMode};
use crate::doc_layout::{
    DocLayout, GithubRenderData, HeightCache, LayoutParams, LineCache, PreeditView, RenderCache,
    ScreenRect, TableCache,
};
use crate::editor::{Direction, EditorTheme};
use crate::fold;
use crate::git::{detect_github_context, parse_github_repo_string};
use crate::github::{GitHubClient, ValidationResult};
use crate::image_cache::ImageCache;
use crate::image_load::{RepaintSignal, load_local_images_blocking, spawn_image_loads};
use crate::inline::{GitHubContext, GitHubRef};
#[cfg(feature = "math")]
use crate::math;
#[cfg(feature = "mermaid")]
use crate::mermaid;
use crate::outline::{current_heading_index, draw_outline};
use crate::overlay::{
    HoverTarget, draw_autocomplete, draw_hover_popover, find_hover_target, hover_target_at_offset,
};
use crate::raster::rasterize_scene_to_png;
use crate::text_engine::{TextEngine, peniko_color, peniko_color_alpha};
use crate::text_input;
use crate::validation::{GitHubValidationCache, IssueOrPr, MentionableUser, ValidatedRefData};

/// Chrome layout in device px: y where editor content begins, and its height. The
/// bottom is inset by the status bar plus the find bar (`find_h`, 0 when closed), so
/// the document viewport reflows above both.
fn chrome_metrics(scale: f32, height_dev: f32, find_h: f32) -> (f32, f32) {
    // The title bar is the native window decoration now, so editor content starts at
    // the surface top; only the bottom strips are inset.
    let content_top = 0.0;
    let editor_h = (height_dev - STATUS_BAR_H * scale - find_h).max(1.0);
    (content_top, editor_h)
}

/// Device-px width of the right-docked outline panel (0 when closed). Mirrors
/// `find_bar_height`: the document region insets by this so it never draws under the panel.
fn outline_width(editor: &Editor, scale: f32) -> f32 {
    if editor.outline_open() {
        OUTLINE_WIDTH * scale
    } else {
        0.0
    }
}

/// Device-px height of the bottom find bar for the current find state (0 when closed):
/// one `FIND_ROW_H` row in Find mode, two in Replace mode, plus a small vertical pad.
fn find_bar_height(editor: &Editor, scale: f32) -> f32 {
    match editor.find_state() {
        None => 0.0,
        Some(find) => {
            let rows = if find.mode == FindMode::Replace {
                2.0
            } else {
                1.0
            };
            rows * FIND_ROW_H * scale + 8.0 * scale
        }
    }
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
    let headings = buffer.headings();
    current_heading_index(headings, line).map(|i| headings[i].level)
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
/// The `--demo` (and no-args) showcase document — exercises headings, emphasis,
/// inline code, task lists, a colored code fence, a blockquote, and links.
const DEMO_DOC: &str = r##"# writ — a live Markdown diff viewer

**writ** renders Markdown as you type, and shows a live *inline diff* against
git `HEAD`: added lines glow green, deletions appear as ghost rows, and
word-level changes highlight in place. Point it at a file an agent is editing and
watch the changes land in real time.

## Features

- [x] GPU-rendered Markdown (winit + Vello + Parley)
- [x] Live inline `git HEAD` diff with word-level changes
- [x] GitHub `#123` / `@user` refs — validated, with hover cards + autocomplete
- [ ] GFM tables
- [ ] Multi-cursor

## A code block

```rust
fn main() {
    let editor = Editor::new("# hello");
    println!("{}", editor.text());
}
```

> Ctrl-click any link or `#ref` to open it in your browser.

Try editing this file — every keystroke re-renders and re-diffs against HEAD.
"##;

/// A synthetic "git HEAD" version of `DEMO_DOC` so `--demo` shows the live inline
/// diff (added/deleted/changed lines) without needing a real git repo.
const DEMO_BASE: &str = r##"# writ — a Markdown viewer

**writ** renders Markdown as you type. Point it at a file and read it nicely.

## Features

- GPU-rendered Markdown
- This line is removed in the working copy, so it renders as a deleted ghost row.

## A code block

```rust
fn main() {
    let editor = Editor::new("# hello");
    println!("{}", editor.text());
}
```

Try editing this file.
"##;

/// The demo editor: the showcase doc with a synthetic HEAD base so the inline diff
/// (the headline feature) is visible on first open.
fn demo_editor() -> Editor {
    let mut editor = Editor::new(DEMO_DOC);
    editor.set_head_base(DEMO_BASE);
    editor
}

/// Wakeups sent from tokio worker tasks back into the winit loop. The work's
/// results are already written to the shared `Arc<Mutex>` caches; the event just
/// tells the loop to redraw (and, for autocomplete, drain the suggestion slot).
/// `(new file content, git HEAD blob text)` read off-thread for a file reload.
type ReloadData = (String, Option<String>);

/// Shared slot a debounced autocomplete fetch drops its results into for the main thread.
type AcSlot = Arc<Mutex<Option<FetchedSuggestions>>>;

#[derive(Debug, Clone)]
pub(crate) enum WritEvent {
    GithubUpdated,
    /// A standalone image finished loading (local or remote). A load changes a line's
    /// height, so the loop rebuilds (not just redraws) to reflow around it.
    ImageLoaded,
    /// The watched file changed on disk (forwarded from the file-watcher thread). The
    /// loop kicks off a blocking read off-thread rather than touching disk here.
    FileChanged,
    /// The off-thread file read finished; its `(content, base_text)` is in `reload_slot`,
    /// ready for the cheap main-thread apply (buffer swap + diff).
    FileReloaded,
}

/// Concrete `RepaintSignal` for the winit shell: wakes the loop with `ImageLoaded` so a
/// finished background image load reflows the doc around its now-known height.
#[derive(Clone)]
struct RedrawOnImageLoad(EventLoopProxy<WritEvent>);

impl RepaintSignal for RedrawOnImageLoad {
    fn notify(&self) {
        let _ = self.0.send_event(WritEvent::ImageLoaded);
    }
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
    /// Current find-bar height (device px), 0 when the bar is closed. Kept here so the
    /// shared `viewport()` accessor can shrink the editor height without threading the
    /// editor's find state through its ten call sites; synced by `sync_find_chrome`.
    find_bar_h: f32,
}

impl ActiveSurface {
    /// The editor viewport in device px: (surface width, editor content height, scale).
    /// The content height excludes the bottom status-bar and find-bar strips.
    fn viewport(&self) -> (f32, f32, f32) {
        let (_, editor_h) = chrome_metrics(
            self.scale,
            self.surface.config.height as f32,
            self.find_bar_h,
        );
        (self.surface.config.width as f32, editor_h, self.scale)
    }
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
    height_cache: HeightCache,
    table_cache: TableCache,
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
    /// Click-count tracking for double/triple-click select (winit doesn't provide it):
    /// the last press's time + position, and the running count (cycles 1→2→3).
    last_click: Option<(std::time::Instant, (f32, f32))>,
    click_count: usize,
    /// Non-zero while a drag is in a top/bottom edge hot zone: the auto-scroll velocity
    /// in physical px/second. Drives continuous auto-scroll from `about_to_wait` so the
    /// selection keeps growing even when the pointer is held still at the edge.
    drag_scroll_dy: f32,
    /// Timestamp of the last auto-scroll tick, so scrolling integrates against real
    /// elapsed time (frame-rate independent) rather than a fixed amount per tick.
    last_drag_tick: Option<std::time::Instant>,
    /// The pointer moved during a drag; the next redraw extends the selection once.
    /// Coalesces a flood of `CursorMoved` events (mice poll far faster than 60 Hz) into
    /// one relayout per frame instead of one per event — a drag was doing hundreds.
    drag_pending: bool,
    /// Set by async completions (validation/image); the next redraw does a single
    /// rebuild, coalescing many same-frame completions into one relayout.
    pending_rebuild: bool,
    /// Where the off-thread file read drops `(content, head_base_text)` for the
    /// main-thread `apply_reload` (keeps disk IO off the render thread).
    reload_slot: Arc<Mutex<Option<ReloadData>>>,
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
    /// Screen rects of the currently-drawn outline rows (for click-to-scroll routing).
    outline_row_rects: Vec<ScreenRect>,
    /// Outline row under the pointer, if any (drives the hover tint).
    outline_hover: Option<usize>,
    /// Fold chevron hit rects (heading byte offset → gutter rect) from the last paint.
    fold_chevron_rects: Vec<(usize, ScreenRect)>,
    /// Heading (by byte offset) whose gutter chevron is under the pointer, if any. Drives
    /// hover-reveal of the expanded (▾) chevron on non-folded headings.
    gutter_hover: Option<usize>,
    /// Screen rects of the find bar's Replace/All buttons (for click routing), when the
    /// bar is open in Replace mode.
    find_btn_rects: Option<FindButtonRects>,
    /// Monotonic generation for autocomplete-fetch debounce (latest wins).
    ac_gen: Arc<AtomicU64>,
    /// Slot a finished fetch task drops its results into for the main thread.
    ac_slot: AcSlot,
    /// Wakeup channel into the winit loop for finished tokio work.
    proxy: EventLoopProxy<WritEvent>,
    /// Handle to the process tokio runtime for spawning GitHub work.
    runtime: tokio::runtime::Handle,
}

impl App {
    fn new(
        mut editor: Editor,
        proxy: EventLoopProxy<WritEvent>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let title = window_title(&editor);
        // Forward file-watch notifications into the loop so it can park on `Wait`
        // rather than polling on a timer. The thread ends when the watcher (and its
        // sender) drop at app exit, making `recv()` return `Err`.
        if let Some(rx) = editor.take_file_watch_rx() {
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                while rx.recv().is_ok() {
                    if proxy.send_event(WritEvent::FileChanged).is_err() {
                        break;
                    }
                }
            });
        }
        Self {
            context: RenderContext::new(),
            renderer: None,
            state: None,
            scene: Scene::new(),
            doc_engine: DocEngine {
                text_engine: TextEngine::new(),
                line_cache: LineCache::new(),
                render_cache: RenderCache::new(),
                height_cache: HeightCache::new(),
                table_cache: TableCache::new(),
                theme: EditorTheme::dracula(),
                editor,
                doc: None,
                images: ImageCache::new(),
                preedit: None,
            },
            modifiers: ModifiersState::empty(),
            mouse_pos: (0.0, 0.0),
            mouse_down: false,
            last_click: None,
            click_count: 0,
            drag_scroll_dy: 0.0,
            last_drag_tick: None,
            drag_pending: false,
            pending_rebuild: false,
            reload_slot: Arc::new(Mutex::new(None)),
            clipboard: arboard::Clipboard::new().ok(),
            title,
            hovered: None,
            ac_row_rects: Vec::new(),
            outline_row_rects: Vec::new(),
            outline_hover: None,
            fold_chevron_rects: Vec::new(),
            gutter_hover: None,
            find_btn_rects: None,
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
            let (_, vh, _) = s.viewport();
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
    // Mermaid diagrams render off-thread into the same image cache; kick off any new ones
    // (does nothing for sources already loading/loaded/failed).
    #[cfg(feature = "mermaid")]
    {
        let sources = doc.mermaid_sources();
        if !sources.is_empty() {
            let proxy = proxy.clone();
            mermaid::spawn_mermaid_renders(sources, &doc_engine.images, move || {
                let _ = proxy.send_event(WritEvent::ImageLoaded);
            });
        }
    }
    // Math renders (block + inline) share the same off-thread → image-cache path.
    #[cfg(feature = "math")]
    {
        let jobs = doc.math_sources();
        if !jobs.is_empty() {
            let proxy = proxy.clone();
            math::spawn_math_renders(jobs, &doc_engine.images, move || {
                let _ = proxy.send_event(WritEvent::ImageLoaded);
            });
        }
    }
    let urls = doc.image_urls();
    if urls.is_empty() {
        return;
    }
    let dir = doc_engine
        .editor
        .file_path()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    spawn_image_loads(
        dir,
        urls,
        &doc_engine.images,
        runtime,
        RedrawOnImageLoad(proxy.clone()),
    );
}

/// The async side effects every buffer edit needs, in one place so an edit entry point
/// can't silently skip one: rebuild+re-detect, spawn validation for newly-visible refs,
/// and load newly-appeared images. Callers still own the surrounding UI (clearing the
/// hover, autocomplete sync, and `request_redraw`) since those vary per site.
fn after_edit(
    doc_engine: &mut DocEngine,
    runtime: &tokio::runtime::Handle,
    proxy: &EventLoopProxy<WritEvent>,
    w: f32,
    scale: f32,
    editor_h: f32,
) {
    doc_engine.refresh(w, scale, editor_h);
    let visible = doc_engine.doc.as_ref().map(|d| {
        let (a, b) = d.visible_range(editor_h);
        a..b
    });
    spawn_ref_validations(&doc_engine.editor, visible, runtime, proxy);
    sync_image_loads(doc_engine, runtime, proxy);
}

/// The full flow every edit site must run: drop the stale hover target, apply the async
/// side effects ([`after_edit`]), optionally refresh the autocomplete popup (pass `ac`
/// for keyboard edits, `None` for mouse/reload), then request a redraw. Defined once so a
/// new edit site can't silently skip a step. Takes the doc-engine's sibling fields
/// individually so it composes while `state` is borrowed at the call site.
#[allow(clippy::too_many_arguments)]
fn apply_edit_effects(
    doc_engine: &mut DocEngine,
    hovered: &mut Option<HoverTarget>,
    runtime: &tokio::runtime::Handle,
    proxy: &EventLoopProxy<WritEvent>,
    ac: Option<(&Arc<AtomicU64>, &AcSlot)>,
    window: &Window,
    w: f32,
    scale: f32,
    editor_h: f32,
) {
    // A buffer edit while the find bar is open leaves the highlighted match set stale
    // (fall-through typing, undo/redo, replace). Rescan so highlights track the document;
    // it no-ops when (version, query, case, regex) is unchanged. Runs before `after_edit`
    // so the cursor reveal below lands on the refreshed active match.
    if doc_engine.editor.find_state().is_some() {
        doc_engine.editor.find_rescan();
    }
    *hovered = None;
    after_edit(doc_engine, runtime, proxy, w, scale, editor_h);
    if let Some((ac_gen, ac_slot)) = ac {
        sync_autocomplete(&mut doc_engine.editor, runtime, proxy, ac_gen, ac_slot);
    }
    window.request_redraw();
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
        if cache.contains(&reference) {
            continue; // already pending/valid/invalid
        }
        cache.mark_pending(reference.clone());
        let client = client.clone();
        let cache = cache.clone();
        let proxy = proxy.clone();
        runtime.spawn(async move {
            validate_ref_into_cache(&client, &cache, reference).await;
            let _ = proxy.send_event(WritEvent::GithubUpdated);
        });
    }
}

/// Validate one ref and record the outcome in the shared cache. Shared by the async
/// GUI path and the blocking snapshot path so the mapping lives in one place.
async fn validate_ref_into_cache(
    client: &GitHubClient,
    cache: &GitHubValidationCache,
    reference: GitHubRef,
) {
    match client.validate_ref(&reference).await {
        ValidationResult::ValidWithData(d) => cache.set_valid(reference, Some(d)),
        ValidationResult::ValidNoData => cache.set_valid(reference, None),
        ValidationResult::Invalid => cache.set_invalid(reference),
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

/// Max gap between presses (and max pointer travel, in physical px) still counted as a
/// repeated click for double/triple-click selection.
const MULTI_CLICK_MS: u128 = 400;
const MULTI_CLICK_DIST: f32 = 6.0;

// Drag-select autoscroll tuning. Follows the de-facto pattern (VS Code, CodeMirror 6,
// AppKit, GTK): a hot zone at the viewport edge, speed that eases in across the zone then
// grows with how far the pointer is past the edge, capped so a flick can't teleport — all
// integrated against real elapsed time (frame-rate independent) like VS Code's
// dragScrolling. Values are logical px; the caller multiplies by elapsed seconds.
/// Depth of the top/bottom hot zone; speed eases 0→`EDGE_SPEED` across it.
const DRAG_SCROLL_ZONE: f32 = 24.0;
/// Speed right at the viewport edge (~6 lines/s) — the calm baseline.
const DRAG_SCROLL_EDGE_SPEED: f32 = 140.0;
/// Extra px/s per px the pointer is *past* the edge (e.g. dragged outside the window).
const DRAG_SCROLL_GAIN: f32 = 10.0;
/// Hard cap so pushing far past the edge stays fast-but-controllable (~36 lines/s).
const DRAG_SCROLL_MAX_SPEED: f32 = 800.0;

/// Auto-scroll velocity (physical px/second) when a drag pointer sits at height `y` in a
/// `[0, editor_h]` viewport: 0 in the middle; eases in across the hot zone; grows
/// linearly once past the edge; clamped to `MAX_SPEED`. Negative = scroll up. The caller
/// multiplies by real elapsed seconds so speed is independent of tick/frame rate.
fn drag_edge_velocity(y: f32, editor_h: f32, scale: f32) -> f32 {
    let zone = DRAG_SCROLL_ZONE * scale;
    let edge = DRAG_SCROLL_EDGE_SPEED * scale;
    let cap = DRAG_SCROLL_MAX_SPEED * scale;
    // `depth` = physical px past the inner edge of the zone (== zone at the viewport edge,
    // > zone once outside the window). Sign picks scroll direction.
    let (depth, dir) = if y < zone {
        (zone - y, -1.0)
    } else if y > editor_h - zone {
        (y - (editor_h - zone), 1.0)
    } else {
        return 0.0;
    };
    let speed = edge * (depth / zone).min(1.0) + DRAG_SCROLL_GAIN * (depth - zone).max(0.0);
    dir * speed.min(cap)
}

/// One drag step: scroll by `dy` (if any), then extend the selection to whatever sits
/// under the viewport-clamped pointer (so dragging into the margin still grows it).
/// Takes `&mut DocEngine` (not `&mut self`) so it composes while `state` is borrowed.
fn drag_extend_step(
    doc_engine: &mut DocEngine,
    mouse: (f32, f32),
    dy: f32,
    w: f32,
    scale: f32,
    editor_h: f32,
) {
    if dy != 0.0
        && let Some(doc) = doc_engine.doc.as_mut()
    {
        doc.scroll_by(dy, editor_h);
    }
    let hy = mouse.1.clamp(0.0, editor_h);
    if let Some(off) = doc_engine
        .doc
        .as_ref()
        .and_then(|d| d.hit_test(mouse.0, hy))
    {
        doc_engine.editor.drag(off);
    }
    doc_engine.refresh(w, scale, editor_h);
}

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
            // Smart space: try_insert_space suppresses the space at a line's
            // start / blockquote-content start (returns false) — no fallback insert.
            editor.try_insert_space();
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
            let dir = if ctrl {
                Direction::DocStart
            } else {
                Direction::LineStart
            };
            editor.move_in_direction(dir, shift);
            true
        }
        Key::Named(NamedKey::End) => {
            let dir = if ctrl {
                Direction::DocEnd
            } else {
                Direction::LineEnd
            };
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
                    if c.eq_ignore_ascii_case("r") {
                        // Force GitHub refs to re-validate (bust a stale/invalid cache);
                        // returning true runs after_edit, which re-spawns validation.
                        editor.revalidate_github_refs();
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

    /// Rebuild at the current doc's scroll anchor and restore that scroll position
    /// (clamped). The shared core of `refresh` and `rebuild_preserving_scroll`; callers
    /// run detection and any cursor reveal around it.
    fn relayout_at_anchor(&mut self, device_width: f32, scale: f32, editor_h: f32) -> DocLayout {
        let (anchor_line, anchor_off) = self
            .doc
            .as_ref()
            .map(|d| d.scroll_anchor())
            .unwrap_or((0, 0.0));
        let mut new_doc = self.rebuild(device_width, scale, anchor_line, editor_h);
        new_doc.scroll_y = new_doc.anchor_scroll_y(anchor_line, anchor_off);
        new_doc.clamp_scroll(editor_h);
        new_doc
    }

    fn refresh(&mut self, device_width: f32, scale: f32, editor_h: f32) {
        // Auto-unfold if a cursor move (arrows, typing, find fall-through) entered a
        // folded region, before we lay out around the new position.
        self.editor.reveal_cursor();
        self.editor
            .refresh_detection(self.detection_range(editor_h));
        let mut new_doc = self.relayout_at_anchor(device_width, scale, editor_h);
        new_doc.scroll_to(self.editor.cursor_position(), editor_h);
        self.doc = Some(new_doc);
        // A cursor jump (Ctrl+End / PageDown) can reveal lines outside the band built
        // around the old anchor; rebuild once around the new position so it's laid out.
        if self
            .doc
            .as_ref()
            .is_some_and(|d| d.needs_remeasure(editor_h))
        {
            self.doc = Some(self.relayout_at_anchor(device_width, scale, editor_h));
        }
    }

    /// Rebuild preserving the current scroll (no cursor reveal): the freshly-validated-
    /// refs recolor and wheel-remeasure paths, where the viewport must not jump. Re-runs
    /// detection over the (possibly scrolled) viewport — gated, so a same-window recolor
    /// wakeup doesn't rescan, but scrolling into new lines detects their refs.
    fn rebuild_preserving_scroll(&mut self, device_width: f32, scale: f32, editor_h: f32) {
        self.editor
            .refresh_detection(self.detection_range(editor_h));
        self.doc = Some(self.relayout_at_anchor(device_width, scale, editor_h));
    }

    /// Lay out the document at `device_width`, materializing a band around `anchor_line`
    /// (the top visible line) that covers `viewport_h` + overscan; the head and tail are
    /// height-estimated. `anchor_line = 0` + `viewport_h = f32::INFINITY` lays out
    /// everything. Borrows disjoint doc-engine fields so the caller's surface borrow stays
    /// intact.
    fn rebuild(
        &mut self,
        device_width: f32,
        scale: f32,
        anchor_line: usize,
        viewport_h: f32,
    ) -> DocLayout {
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
        // The outline panel (when open) claims a right strip, so the doc region is
        // `[0, device_width - outline_w]`. `device_width` stays the full surface width;
        // the inset is applied here so every rebuild call site keeps passing it whole.
        let outline_w = outline_width(&self.editor, scale);
        let params = LayoutParams {
            content_x0: 0.0,
            content_w: (device_width - outline_w).max(1.0),
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
        let folds = self.editor.hidden_line_ranges();
        let selection = self.editor.selection_range().unwrap_or_else(|| {
            let c = self.editor.cursor_position();
            c..c
        });
        DocLayout::build(
            &mut self.text_engine,
            &mut self.line_cache,
            &mut self.render_cache,
            &mut self.height_cache,
            &mut self.table_cache,
            version,
            &snapshot,
            &self.theme,
            diff.as_ref(),
            Some(&github),
            &self.images,
            cursor_offset,
            &params,
            preedit.as_ref(),
            anchor_line,
            viewport_h,
            &folds,
            &selection,
            self.editor.math_spans_by_line(),
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
    slot: &AcSlot,
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
        let fetched = fetch_suggestions(&client, &context, trigger, prefix).await;
        *slot.lock().unwrap() = Some(fetched);
        let _ = proxy.send_event(WritEvent::GithubUpdated);
    });
}

/// Fetch issue/user autocomplete suggestions for `trigger`. Shared by the async GUI
/// path (debounced) and the blocking snapshot path.
async fn fetch_suggestions(
    client: &GitHubClient,
    context: &GitHubContext,
    trigger: AutocompleteTrigger,
    prefix: String,
) -> FetchedSuggestions {
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
/// Index of the autocomplete row whose rect contains `pos` (physical px), if any.
fn ac_row_at(rects: &[ScreenRect], pos: (f32, f32)) -> Option<usize> {
    let (px, py) = (pos.0 as f64, pos.1 as f64);
    rects
        .iter()
        .position(|&(x0, y0, x1, y1)| px >= x0 && px <= x1 && py >= y0 && py <= y1)
}

fn rect_contains(rect: &ScreenRect, pos: (f32, f32)) -> bool {
    let (px, py) = (pos.0 as f64, pos.1 as f64);
    let &(x0, y0, x1, y1) = rect;
    px >= x0 && px <= x1 && py >= y0 && py <= y1
}

/// Paint the clipped document body (diff backgrounds, quote gutters, rules, images,
/// selection, glyphs, caret) plus the bottom status bar into `scene`. Overlays
/// (hover popover / autocomplete) differ per caller and are drawn separately. Shared by
/// the live redraw and the headless snapshot so the golden frame can't drift from the
/// real one. Takes the doc-engine's fields individually so callers can pass a `doc`
/// borrowed from the same engine alongside `&mut` access to its text engine.
#[allow(clippy::too_many_arguments)]
fn paint_document(
    scene: &mut Scene,
    engine: &mut TextEngine,
    theme: &EditorTheme,
    editor: &Editor,
    doc: &DocLayout,
    content_top: f32,
    editor_h: f32,
    find_h: f32,
    width: f32,
    height: f32,
    scale: f32,
    outline_hover: Option<usize>,
    gutter_hover: Option<usize>,
) -> (
    Option<FindButtonRects>,
    Vec<ScreenRect>,
    Vec<(usize, ScreenRect)>,
) {
    // The outline panel (when open) reserves a right strip; clip the document body to the
    // reduced region so glyphs/backgrounds never draw under the panel.
    let outline_w = outline_width(editor, scale);
    let doc_w = (width - outline_w).max(0.0);
    let clip = Rect::new(
        0.0,
        content_top as f64,
        doc_w as f64,
        (content_top + editor_h) as f64,
    );
    scene.push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
    // Draw order (all before glyphs): diff row/word bg, quote gutters, rules, images,
    // then selection.
    doc.draw_added_backgrounds(scene, editor_h);
    doc.draw_blockquote_gutters(scene, editor_h);
    doc.draw_horizontal_rules(scene, editor_h);
    doc.draw_images(engine, scene, editor_h);
    doc.draw_tables(engine, scene, editor_h);
    // Faint yellow tint under every visible find match EXCEPT the active one, which is
    // drawn last (below) in a distinct warm orange so the current match reads clearly.
    if let Some(find) = editor.find_state()
        && !find.matches.is_empty()
    {
        let tint = peniko_color_alpha(theme.yellow, 0.16);
        let (first, last) = doc.visible_range(editor_h);
        for (i, m) in find.matches.iter().enumerate() {
            if find.active == Some(i) {
                continue;
            }
            if doc.line_of(m.end) < first || doc.line_of(m.start) >= last {
                continue;
            }
            for (x0, y0, x1, y1) in doc.selection_rects(m.clone()) {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    tint,
                    None,
                    &Rect::new(x0, y0, x1, y1),
                );
            }
        }
    }
    if let Some(sel) = editor.selection_range() {
        let color = peniko_color(theme.selection);
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
    // The active find match, drawn last (over the selection) in a distinct warm orange
    // with an outline, so the current match stands out from the faint-yellow others.
    if let Some(find) = editor.find_state()
        && let Some(active) = find.active
        && let Some(m) = find.matches.get(active)
    {
        let fill = peniko_color_alpha(theme.orange, 0.34);
        let border = peniko_color_alpha(theme.orange, 0.95);
        let stroke = Stroke::new(1.5 * scale as f64);
        for (x0, y0, x1, y1) in doc.selection_rects(m.clone()) {
            let r = Rect::new(x0, y0, x1, y1);
            scene.fill(Fill::NonZero, Affine::IDENTITY, fill, None, &r);
            scene.stroke(&stroke, Affine::IDENTITY, border, None, &r);
        }
    }
    doc.draw(engine, scene, editor_h);
    let cw = CARET_WIDTH * scale;
    let caret = doc
        .preedit_caret_rect(cw)
        .or_else(|| doc.caret_rect(editor.cursor_position(), cw));
    if let Some((x0, y0, x1, y1)) = caret {
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            peniko_color(theme.foreground),
            None,
            &Rect::new(x0, y0, x1.max(x0 + 1.0), y1),
        );
    }
    let fold_rects = draw_fold_chevrons(scene, theme, editor, doc, editor_h, gutter_hover, scale);
    scene.pop_layer();

    // The right-docked outline panel: the document's heading list with the current
    // section highlighted. Rects (one per row) route clicks back to a scroll.
    let outline_rects = if outline_w > 0.0 {
        let panel = BarRect {
            x0: (width - outline_w) as f64,
            y0: content_top as f64,
            x1: width as f64,
            y1: (content_top + editor_h) as f64,
        };
        let buffer = &editor.state.buffer;
        let headings = buffer.headings();
        let cursor_line = buffer.byte_to_line(editor.cursor_position());
        let current = current_heading_index(headings, cursor_line);
        let folded: Vec<bool> = headings
            .iter()
            .map(|h| editor.is_heading_folded(h.byte_offset))
            .collect();
        draw_outline(
            engine,
            scene,
            theme,
            headings,
            current,
            outline_hover,
            &folded,
            &panel,
            scale,
        )
    } else {
        Vec::new()
    };

    // Chrome (bottom, above the OS-native title bar): the find bar sits in the gap
    // between the document and the status bar, so the status bar starts below it.
    let bar_top = (content_top + editor_h) as f64;
    let status_top = bar_top + find_h as f64;
    let btn_rects = if find_h > 0.0
        && let Some(find) = editor.find_state()
    {
        draw_find_bar(
            engine,
            scene,
            theme,
            find,
            &BarRect {
                x0: 0.0,
                y0: bar_top,
                x1: width as f64,
                y1: status_top,
            },
            scale,
        )
    } else {
        None
    };
    let info = build_status_info(editor, doc, editor_h);
    draw_status_bar(
        engine,
        scene,
        theme,
        &BarRect {
            x0: 0.0,
            y0: status_top,
            x1: width as f64,
            y1: height as f64,
        },
        &info,
        scale,
    );
    (btn_rects, outline_rects, fold_rects)
}

/// Heading depth for a `Ctrl+Shift+<n>` fold-to-level shortcut, from the physical digit
/// key (1..6), or `None` for any other key.
fn fold_level_key(key: PhysicalKey) -> Option<u8> {
    match key {
        PhysicalKey::Code(KeyCode::Digit1) => Some(1),
        PhysicalKey::Code(KeyCode::Digit2) => Some(2),
        PhysicalKey::Code(KeyCode::Digit3) => Some(3),
        PhysicalKey::Code(KeyCode::Digit4) => Some(4),
        PhysicalKey::Code(KeyCode::Digit5) => Some(5),
        PhysicalKey::Code(KeyCode::Digit6) => Some(6),
        _ => None,
    }
}

/// Draw fold chevrons in the left gutter beside every visible foldable heading, and
/// return their hit rects keyed by heading byte offset. A folded heading shows a
/// persistent ▸; an expanded heading shows ▾ only while its gutter is hovered.
fn draw_fold_chevrons(
    scene: &mut Scene,
    theme: &EditorTheme,
    editor: &Editor,
    doc: &DocLayout,
    editor_h: f32,
    hover: Option<usize>,
    scale: f32,
) -> Vec<(usize, ScreenRect)> {
    let line_count = editor.line_count();
    let (first, last) = doc.visible_range(editor_h);
    // Anchors collapsed inside a folded ancestor are zero-height but still fall inside the
    // contiguous visible-index span — exclude them so no stray chevron paints at the
    // ancestor's y.
    let hidden = editor.hidden_line_ranges();
    let visible_foldable = |line: usize| -> bool {
        line >= first && line < last && !hidden.iter().any(|r| r.contains(&line))
    };
    // Every visible, non-collapsed, foldable anchor — headings AND list items alike; the
    // draw + hit-rect geometry below is identical for both.
    let headings = editor.state.buffer.headings();
    let list_items = editor.state.buffer.list_items();
    let mut anchors: Vec<(usize, usize)> = Vec::new(); // (line, byte_offset)
    for (idx, h) in headings.iter().enumerate() {
        if visible_foldable(h.line) && fold::heading_is_foldable(headings, idx, line_count) {
            anchors.push((h.line, h.byte_offset));
        }
    }
    for (idx, it) in list_items.iter().enumerate() {
        if visible_foldable(it.line) && fold::list_item_is_foldable(list_items, idx) {
            anchors.push((it.line, it.byte_offset));
        }
    }

    let body_left = doc.body_left();
    let s = 9.0 * scale; // chevron glyph size (device px)
    let mut rects: Vec<(usize, ScreenRect)> = Vec::new();
    for (line, byte_offset) in anchors {
        let Some(top) = doc.line_top_screen(line) else {
            continue;
        };
        // The fold set holds both heading and list-item offsets, so this reports either.
        let folded = editor.is_heading_folded(byte_offset);
        // Full-height gutter hit target for easy clicking; the glyph is centered in it.
        let row_h = doc.line_text_height(line);
        let hit: ScreenRect = (
            (body_left - 20.0 * scale) as f64,
            top as f64,
            (body_left - 2.0 * scale) as f64,
            (top + row_h) as f64,
        );
        rects.push((byte_offset, hit));
        // Expanded chevrons only appear on hover; folded ones are always shown.
        if !folded && hover != Some(byte_offset) {
            continue;
        }
        let cx = body_left - 13.0 * scale;
        let cy = top + row_h / 2.0;
        let color = if hover == Some(byte_offset) {
            peniko_color(theme.foreground)
        } else {
            peniko_color(theme.comment)
        };
        let mut tri = BezPath::new();
        if folded {
            // ▸ pointing right.
            tri.move_to(Point::new((cx - s * 0.3) as f64, (cy - s * 0.5) as f64));
            tri.line_to(Point::new((cx - s * 0.3) as f64, (cy + s * 0.5) as f64));
            tri.line_to(Point::new((cx + s * 0.45) as f64, cy as f64));
        } else {
            // ▾ pointing down.
            tri.move_to(Point::new((cx - s * 0.5) as f64, (cy - s * 0.3) as f64));
            tri.line_to(Point::new((cx + s * 0.5) as f64, (cy - s * 0.3) as f64));
            tri.line_to(Point::new(cx as f64, (cy + s * 0.45) as f64));
        }
        tri.close_path();
        scene.fill(Fill::NonZero, Affine::IDENTITY, color, None, &tri);
    }
    rects
}

impl ApplicationHandler<WritEvent> for App {
    /// A tokio task finished (validation/suggestion). Results are already in the
    /// shared caches; rebuild the doc (ref colors may have changed) and redraw.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: WritEvent) {
        // The watched file changed on disk: read it (fs + git HEAD) on a blocking worker
        // so the render thread never touches disk, then finish on FileReloaded.
        if matches!(event, WritEvent::FileChanged) {
            let Some(path) = self.doc_engine.editor.file_path().map(|p| p.to_path_buf()) else {
                return;
            };
            let last_mtime = self.doc_engine.editor.last_save_mtime();
            let slot = self.reload_slot.clone();
            let proxy = self.proxy.clone();
            self.runtime.spawn_blocking(move || {
                if let Some(data) = Editor::read_reload(&path, last_mtime) {
                    *slot.lock().unwrap() = Some(data);
                    let _ = proxy.send_event(WritEvent::FileReloaded);
                }
            });
            return;
        }
        // The off-thread read finished: apply it (cheap parse/diff) and treat it like an
        // edit (refresh detection / revalidate refs / reload images).
        if matches!(event, WritEvent::FileReloaded) {
            let data = self.reload_slot.lock().unwrap().take();
            if let Some((content, base_text)) = data
                && let Some(state) = self.state.as_ref()
            {
                let (w, vh, scale) = state.viewport();
                let window = state.window.clone();
                self.doc_engine.editor.apply_reload(content, base_text);
                apply_edit_effects(
                    &mut self.doc_engine,
                    &mut self.hovered,
                    &self.runtime,
                    &self.proxy,
                    None,
                    &window,
                    w,
                    scale,
                    vh,
                );
            }
            return;
        }
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
        // Defer the relayout to the next redraw so a burst of completions (many refs /
        // images finishing in the same frame) collapses into a single rebuild.
        if let Some(state) = self.state.as_ref() {
            self.pending_rebuild = true;
            state.window.request_redraw();
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
        // Find is closed at startup, so no find-bar inset yet.
        let (_, editor_h) = chrome_metrics(scale, size.height as f32, 0.0);
        let doc = self
            .doc_engine
            .rebuild(size.width as f32, scale, 0, editor_h);
        self.doc_engine.doc = Some(doc);
        self.state = Some(ActiveSurface {
            surface,
            window,
            scale,
            find_bar_h: 0.0,
        });
        // Validate refs already present in the loaded file, and start loading images
        // (spawn_validations already calls sync_images).
        self.spawn_validations();
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
                // A width change re-wraps, so re-run detection + relayout at the anchor —
                // the same preserve-scroll path used for recolor/remeasure rebuilds.
                let (w, editor_h, scale) = state.viewport();
                self.doc_engine
                    .rebuild_preserving_scroll(w, scale, editor_h);
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
                let (_, editor_h) = chrome_metrics(
                    state.scale,
                    state.surface.config.height as f32,
                    state.find_bar_h,
                );
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
                // While the find bar owns focus, never splice composition into the document
                // behind it. When the bar is open but unfocused, composition targets the
                // document as usual.
                if self
                    .doc_engine
                    .editor
                    .find_state()
                    .is_some_and(|f| f.focused)
                {
                    state.window.request_redraw();
                    return;
                }
                // Some IMEs (GNOME/IBus on Wayland) spam empty preedit events at idle. An
                // empty preedit with nothing to clear is a no-op — skip it, or every frame
                // would trigger a full relayout (murder on an expensive doc, e.g. mermaid).
                if text.is_empty() && self.doc_engine.preedit.is_none() {
                    return;
                }
                self.doc_engine.preedit = (!text.is_empty()).then_some(Preedit { text, cursor });
                let (w, vh, _) = state.viewport();
                self.doc_engine
                    .rebuild_preserving_scroll(w, state.scale, vh);
                if let Some(doc) = self.doc_engine.doc.as_ref() {
                    let cw = CARET_WIDTH * state.scale;
                    let rect = doc
                        .preedit_caret_rect(cw)
                        .or_else(|| doc.caret_rect(self.doc_engine.editor.cursor_position(), cw));
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
                // While the find bar owns focus, route committed IME text (CJK/dead-key/
                // accent) to the focused field instead of the document.
                if self
                    .doc_engine
                    .editor
                    .find_state()
                    .is_some_and(|f| f.focused)
                {
                    if !text.is_empty() {
                        let to_replace = self
                            .doc_engine
                            .editor
                            .find_state()
                            .is_some_and(|f| f.focus == FieldFocus::Replace);
                        if let Some(find) = self.doc_engine.editor.find_state_mut() {
                            if to_replace {
                                find.replace.insert(&text);
                            } else {
                                find.search.insert(&text);
                            }
                        }
                        if !to_replace {
                            let (_, vh, _) = state.viewport();
                            self.doc_engine.editor.find_rescan();
                            if let Some(doc) = self.doc_engine.doc.as_mut() {
                                doc.scroll_to(self.doc_engine.editor.cursor_position(), vh);
                            }
                        }
                    }
                    state.window.request_redraw();
                    return;
                }
                let had_preedit = self.doc_engine.preedit.take().is_some();
                let (w, vh, _) = state.viewport();
                if !text.is_empty() {
                    self.doc_engine.editor.insert_str(&text);
                    apply_edit_effects(
                        &mut self.doc_engine,
                        &mut self.hovered,
                        &self.runtime,
                        &self.proxy,
                        None,
                        &state.window,
                        w,
                        state.scale,
                        vh,
                    );
                } else if had_preedit {
                    // Composition cancelled (empty commit): drop the spliced preedit.
                    self.doc_engine
                        .rebuild_preserving_scroll(w, state.scale, vh);
                    state.window.request_redraw();
                }
            }
            WindowEvent::Ime(winit::event::Ime::Disabled) => {
                if self.doc_engine.preedit.take().is_some() {
                    let (w, vh, _) = state.viewport();
                    self.doc_engine
                        .rebuild_preserving_scroll(w, state.scale, vh);
                    state.window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x as f32, position.y as f32);
                // Outline panel hover: highlight the row under the pointer (independent of
                // the document/autocomplete hover machinery below).
                let panel_left = state.surface.config.width as f32 - OUTLINE_WIDTH * state.scale;
                let new_outline_hover =
                    if self.doc_engine.editor.outline_open() && self.mouse_pos.0 >= panel_left {
                        ac_row_at(&self.outline_row_rects, self.mouse_pos)
                    } else {
                        None
                    };
                if new_outline_hover != self.outline_hover {
                    self.outline_hover = new_outline_hover;
                    state.window.request_redraw();
                }
                // Fold-gutter hover: reveal the expanded (▾) chevron under the pointer.
                let new_gutter_hover = self
                    .fold_chevron_rects
                    .iter()
                    .find(|(_, r)| rect_contains(r, self.mouse_pos))
                    .map(|(off, _)| *off);
                if new_gutter_hover != self.gutter_hover {
                    self.gutter_hover = new_gutter_hover;
                    state.window.request_redraw();
                }
                if self.mouse_down {
                    let (_, vh, _) = state.viewport();
                    // Record the edge auto-scroll velocity for the timer tick; the move
                    // itself only extends the selection (dy=0), so scroll speed stays
                    // fixed to the tick cadence rather than the mouse-move rate.
                    self.drag_scroll_dy = drag_edge_velocity(self.mouse_pos.1, vh, state.scale);
                    // Don't relayout per event — mice fire far faster than the display
                    // refreshes. Mark it and let the next redraw extend the selection once.
                    self.drag_pending = true;
                    state.window.request_redraw();
                } else if self.doc_engine.editor.autocomplete().is_some() {
                    // Autocomplete popup open: the highlighted row follows the pointer.
                    if let Some(row) = ac_row_at(&self.ac_row_rects, self.mouse_pos) {
                        self.doc_engine.editor.autocomplete_select(row);
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
                let (w, vh, _) = state.viewport();

                // Find-bar focus routing. The bar is docked at the bottom, so the document
                // occupies y < vh, the find strip [vh, vh + find_bar_h), the status bar
                // below that. A click in the strip focuses the bar (and picks the field by
                // row); a click in the document unfocuses it (keys then type into the
                // buffer) and falls through to normal caret placement.
                let in_find_bar = if self.doc_engine.editor.find_state().is_some() {
                    let bar_top = vh;
                    let bar_bottom = vh + state.find_bar_h;
                    let click_y = self.mouse_pos.1;
                    if click_y < bar_top {
                        if let Some(find) = self.doc_engine.editor.find_state_mut() {
                            find.focused = false;
                        }
                        false
                    } else if click_y < bar_bottom {
                        // Coarse which-row hit test: in Replace mode the lower half is the
                        // replace field, otherwise it's the search field (precise field
                        // rects aren't threaded back here).
                        let field = if self
                            .doc_engine
                            .editor
                            .find_state()
                            .is_some_and(|f| f.mode == FindMode::Replace)
                            && click_y >= bar_top + state.find_bar_h / 2.0
                        {
                            FieldFocus::Replace
                        } else {
                            FieldFocus::Search
                        };
                        if let Some(find) = self.doc_engine.editor.find_state_mut() {
                            find.focused = true;
                            find.focus = field;
                        }
                        true
                    } else {
                        false // status bar: leave focus unchanged
                    }
                } else {
                    false
                };

                // Find-bar Replace/All button click (only present in Replace mode).
                if let Some(rects) = self.find_btn_rects.as_ref() {
                    let hit_all = rect_contains(&rects.all, self.mouse_pos);
                    let hit_replace = rect_contains(&rects.replace, self.mouse_pos);
                    if hit_all || hit_replace {
                        if hit_all {
                            self.doc_engine.editor.find_replace_all();
                        } else {
                            self.doc_engine.editor.find_replace_current();
                        }
                        apply_edit_effects(
                            &mut self.doc_engine,
                            &mut self.hovered,
                            &self.runtime,
                            &self.proxy,
                            None,
                            &state.window,
                            w,
                            state.scale,
                            vh,
                        );
                        return;
                    }
                }

                // A click inside the find strip that missed the buttons just (re)focuses
                // the bar — redraw the focus ring and don't fall through to doc caret
                // placement (the doc's hit-test would otherwise snap the caret to the
                // nearest line for a click below the content).
                if in_find_bar {
                    state.window.request_redraw();
                    return;
                }

                // Autocomplete row click: accept that suggestion, don't move the caret.
                if self.doc_engine.editor.autocomplete().is_some()
                    && let Some(row) = ac_row_at(&self.ac_row_rects, self.mouse_pos)
                {
                    self.doc_engine.editor.autocomplete_select(row);
                    if self.doc_engine.editor.accept_autocomplete_suggestion() {
                        apply_edit_effects(
                            &mut self.doc_engine,
                            &mut self.hovered,
                            &self.runtime,
                            &self.proxy,
                            None,
                            &state.window,
                            w,
                            state.scale,
                            vh,
                        );
                    }
                    return;
                }

                // Fold-gutter chevron click: toggle that heading's fold, don't place the
                // caret. Checked before the document hit-test since the chevron sits in
                // the left margin, outside the text body.
                if let Some(&(off, _)) = self
                    .fold_chevron_rects
                    .iter()
                    .find(|(_, r)| rect_contains(r, self.mouse_pos))
                {
                    // Modifier-clicks escalate scope on the same two axes for both kinds:
                    // Ctrl = breadth (all at this heading level / list depth), Shift = depth
                    // (recursive: this item + everything nested), Ctrl+Shift = breadth AND
                    // depth, plain = just this one.
                    let ctrl = self.modifiers.control_key() || self.modifiers.super_key();
                    let shift = self.modifiers.shift_key();
                    let editor = &mut self.doc_engine.editor;
                    match (editor.is_list_fold_offset(off), ctrl, shift) {
                        (true, true, true) => editor.toggle_fold_list_level_deep_at(off),
                        (true, true, false) => editor.toggle_fold_list_level_at(off),
                        (true, false, true) => editor.toggle_fold_recursive(off),
                        (true, false, false) => editor.toggle_fold(off),
                        (false, true, true) => editor.toggle_fold_level_deep_at(off),
                        (false, true, false) => editor.toggle_fold_level_at(off),
                        (false, false, true) => editor.toggle_fold_recursive(off),
                        (false, false, false) => editor.toggle_fold(off),
                    }
                    self.doc_engine
                        .rebuild_preserving_scroll(w, state.scale, vh);
                    state.window.request_redraw();
                    return;
                }

                // Outline panel click: jump to the clicked heading (pin it to the top).
                // A click anywhere in the strip returns without placing the doc caret,
                // even if it missed a row (the strip isn't part of the document).
                if self.doc_engine.editor.outline_open()
                    && self.mouse_pos.0 >= w - OUTLINE_WIDTH * state.scale
                {
                    if let Some(i) = ac_row_at(&self.outline_row_rects, self.mouse_pos) {
                        let off = self
                            .doc_engine
                            .editor
                            .state
                            .buffer
                            .headings()
                            .get(i)
                            .map(|h| h.byte_offset);
                        if let Some(off) = off {
                            self.doc_engine.editor.set_cursor(off);
                            // The clicked heading may sit inside a folded ancestor; reveal
                            // it and relayout so its geometry exists before we pin it.
                            if self.doc_engine.editor.reveal_cursor() {
                                self.doc_engine
                                    .rebuild_preserving_scroll(w, state.scale, vh);
                            }
                            let mut remeasure = false;
                            if let Some(doc) = self.doc_engine.doc.as_mut() {
                                doc.scroll_line_to_top(doc.line_of(off), vh);
                                remeasure = doc.needs_remeasure(vh);
                            }
                            // If the heading sits outside the materialized band, rebuild
                            // around the new scroll anchor so it lays out, then re-pin.
                            if remeasure {
                                self.doc_engine
                                    .rebuild_preserving_scroll(w, state.scale, vh);
                                if let Some(doc) = self.doc_engine.doc.as_mut() {
                                    doc.scroll_line_to_top(doc.line_of(off), vh);
                                }
                            }
                            sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                        }
                    }
                    state.window.request_redraw();
                    return;
                }

                if let Some(off) = self
                    .doc_engine
                    .doc
                    .as_ref()
                    .and_then(|d| d.hit_test(self.mouse_pos.0, self.mouse_pos.1))
                {
                    // Ctrl/Cmd-click on a link (markdown link, naked URL, or GitHub ref)
                    // opens it in the browser instead of placing the caret.
                    if (self.modifiers.control_key() || self.modifiers.super_key())
                        && let Some(url) = self.doc_engine.editor.link_at(off)
                    {
                        if let Err(e) = open::that_detached(&url) {
                            eprintln!("[writ] failed to open {url}: {e}");
                        }
                        return;
                    }
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
                    // Advance the click-count state machine (winit gives no click count):
                    // within the time+distance threshold it cycles 1→2→3→1 (double = word,
                    // triple = line select), else restarts at 1. Inlined (not a &mut self
                    // method) so it uses disjoint fields while `state` stays borrowed.
                    let now = std::time::Instant::now();
                    let repeat = self.last_click.is_some_and(|(t, p)| {
                        now.duration_since(t).as_millis() <= MULTI_CLICK_MS
                            && (p.0 - self.mouse_pos.0).hypot(p.1 - self.mouse_pos.1)
                                <= MULTI_CLICK_DIST
                    });
                    self.click_count = if repeat {
                        (self.click_count % 3) + 1
                    } else {
                        1
                    };
                    self.last_click = Some((now, self.mouse_pos));
                    self.doc_engine
                        .editor
                        .click(off, self.modifiers.shift_key(), self.click_count);
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
                self.drag_scroll_dy = 0.0;
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
                let (w, vh, _) = state.viewport();

                // Find / Replace bar (Ctrl+F / Ctrl+H). Opening or closing changes the
                // editor height, so re-sync the find-bar inset and reflow the document.
                // While open, this intercept sits ABOVE clipboard/autocomplete/apply_key
                // and captures EVERY keystroke, so nothing leaks into the document.
                let open_find = cmd
                    && matches!(&event.logical_key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("f"));
                let open_replace = cmd
                    && matches!(&event.logical_key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("h"));
                if open_find || open_replace {
                    self.doc_engine.editor.open_find(open_replace);
                    state.find_bar_h = find_bar_height(&self.doc_engine.editor, state.scale);
                    let (fw, feh, fscale) = state.viewport();
                    self.doc_engine.rebuild_preserving_scroll(fw, fscale, feh);
                    if let Some(doc) = self.doc_engine.doc.as_mut() {
                        doc.scroll_to(self.doc_engine.editor.cursor_position(), feh);
                    }
                    state.window.request_redraw();
                    return;
                }

                // Outline panel (Ctrl+Shift+O): the panel claims a right strip and the
                // document reflows into the remaining width (a pure inset — the window /
                // surface size is untouched, so no framebuffer scaling).
                let shift = self.modifiers.shift_key();
                if cmd
                    && shift
                    && matches!(&event.logical_key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("o"))
                {
                    self.doc_engine.editor.toggle_outline();
                    let (ow, oeh, oscale) = state.viewport();
                    self.doc_engine.rebuild_preserving_scroll(ow, oscale, oeh);
                    state.window.request_redraw();
                    return;
                }

                // Heading folding: Ctrl/Cmd+Shift+[ / ] fold / unfold the current section;
                // Ctrl/Cmd+Alt+[ / ] fold / unfold all headings; Ctrl/Cmd+Shift+1..6 fold
                // to that heading depth (Sublime's chord-free "fold by level"). Matched on
                // physical keys so Shift's `{`/`}`/`!` remaps don't matter.
                let alt = self.modifiers.alt_key();
                if cmd && (shift || alt) {
                    let bracket = match event.physical_key {
                        PhysicalKey::Code(KeyCode::BracketLeft) => Some(true),
                        PhysicalKey::Code(KeyCode::BracketRight) => Some(false),
                        _ => None,
                    };
                    let level = shift.then(|| fold_level_key(event.physical_key)).flatten();
                    let acted = if let Some(left) = bracket {
                        match (shift, left) {
                            (true, true) => self.doc_engine.editor.fold_at_cursor(),
                            (true, false) => self.doc_engine.editor.unfold_at_cursor(),
                            (false, true) => self.doc_engine.editor.fold_all_headings(),
                            (false, false) => self.doc_engine.editor.unfold_all(),
                        }
                        true
                    } else if let Some(level) = level {
                        self.doc_engine.editor.fold_to_level(level);
                        true
                    } else {
                        false
                    };
                    if acted {
                        let (fw, feh, fscale) = state.viewport();
                        self.doc_engine.rebuild_preserving_scroll(fw, fscale, feh);
                        if let Some(doc) = self.doc_engine.doc.as_mut() {
                            doc.scroll_to(self.doc_engine.editor.cursor_position(), feh);
                        }
                        state.window.request_redraw();
                        return;
                    }
                }

                // Escape closes the find bar regardless of which pane holds focus (so it
                // works even after clicking into the document unfocuses the bar).
                if self.doc_engine.editor.find_state().is_some()
                    && matches!(&event.logical_key, Key::Named(NamedKey::Escape))
                {
                    self.doc_engine.editor.close_find();
                    state.find_bar_h = find_bar_height(&self.doc_engine.editor, state.scale);
                    let (fw, feh, fscale) = state.viewport();
                    self.doc_engine.rebuild_preserving_scroll(fw, fscale, feh);
                    state.window.request_redraw();
                    return;
                }

                // The find intercept swallows keys ONLY while the bar owns focus. When it's
                // open but unfocused (the document was clicked), keys fall through to the
                // normal document handling below so typing/arrows/undo edit the buffer.
                if self
                    .doc_engine
                    .editor
                    .find_state()
                    .is_some_and(|f| f.focused)
                {
                    let alt = self.modifiers.alt_key();
                    let shift = self.modifiers.shift_key();

                    // Undo/redo always act on the DOCUMENT, even with the find field
                    // focused, so a replace can be undone without clicking away. The buffer
                    // changed, so `apply_edit_effects` rescans + reveals the active match.
                    let is_z = matches!(&event.logical_key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("z"));
                    let is_y = matches!(&event.logical_key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("y"));
                    if cmd && (is_z || is_y) {
                        if is_y || (is_z && shift) {
                            self.doc_engine.editor.redo();
                        } else {
                            self.doc_engine.editor.undo();
                        }
                        self.doc_engine.editor.find_rescan();
                        apply_edit_effects(
                            &mut self.doc_engine,
                            &mut self.hovered,
                            &self.runtime,
                            &self.proxy,
                            None,
                            &state.window,
                            w,
                            state.scale,
                            vh,
                        );
                        return;
                    }
                    match &event.logical_key {
                        Key::Named(NamedKey::Enter) => {
                            let replace_mode = self
                                .doc_engine
                                .editor
                                .find_state()
                                .is_some_and(|f| f.mode == FindMode::Replace);
                            let focus_replace = self
                                .doc_engine
                                .editor
                                .find_state()
                                .is_some_and(|f| f.focus == FieldFocus::Replace);
                            if replace_mode && (cmd || (!shift && focus_replace)) {
                                // Ctrl/Cmd+Enter replaces every match; a plain Enter in the
                                // replace field replaces the active match and advances. Both
                                // mutate the buffer, so run the full post-edit flow.
                                if cmd {
                                    self.doc_engine.editor.find_replace_all();
                                } else {
                                    self.doc_engine.editor.find_replace_current();
                                }
                                apply_edit_effects(
                                    &mut self.doc_engine,
                                    &mut self.hovered,
                                    &self.runtime,
                                    &self.proxy,
                                    None,
                                    &state.window,
                                    w,
                                    state.scale,
                                    vh,
                                );
                            } else {
                                let hit = if shift {
                                    self.doc_engine.editor.find_prev()
                                } else {
                                    self.doc_engine.editor.find_next()
                                };
                                if hit.is_some() {
                                    // Jumping to a match inside a folded section reveals it.
                                    if self.doc_engine.editor.reveal_cursor() {
                                        self.doc_engine.rebuild_preserving_scroll(
                                            w,
                                            state.scale,
                                            vh,
                                        );
                                    }
                                    if let Some(doc) = self.doc_engine.doc.as_mut() {
                                        doc.scroll_to(self.doc_engine.editor.cursor_position(), vh);
                                    }
                                }
                            }
                        }
                        Key::Named(NamedKey::Tab) => self.doc_engine.editor.find_toggle_field(),
                        Key::Character(c) if alt && c.as_str().eq_ignore_ascii_case("r") => {
                            self.doc_engine.editor.find_toggle_regex();
                            if let Some(doc) = self.doc_engine.doc.as_mut() {
                                doc.scroll_to(self.doc_engine.editor.cursor_position(), vh);
                            }
                        }
                        Key::Character(c) if alt && c.as_str().eq_ignore_ascii_case("c") => {
                            self.doc_engine.editor.find_toggle_case();
                            if let Some(doc) = self.doc_engine.doc.as_mut() {
                                doc.scroll_to(self.doc_engine.editor.cursor_position(), vh);
                            }
                        }
                        _ => {
                            let ctrl_v = cmd
                                && matches!(&event.logical_key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("v"));
                            let to_replace = self
                                .doc_engine
                                .editor
                                .find_state()
                                .is_some_and(|f| f.focus == FieldFocus::Replace);
                            if ctrl_v {
                                // Read the clipboard first, then insert — keeps the
                                // clipboard borrow disjoint from the editor's.
                                let text = self
                                    .clipboard
                                    .as_mut()
                                    .and_then(|cb| cb.get_text().ok())
                                    .filter(|t| !t.is_empty());
                                if let Some(text) = text
                                    && let Some(find) = self.doc_engine.editor.find_state_mut()
                                {
                                    if to_replace {
                                        find.replace.insert(&text);
                                    } else {
                                        find.search.insert(&text);
                                    }
                                }
                            } else if let Some(find) = self.doc_engine.editor.find_state_mut() {
                                let field = if to_replace {
                                    &mut find.replace
                                } else {
                                    &mut find.search
                                };
                                text_input::apply_key(field, &event, self.modifiers);
                            }
                            // A search-field edit changes the match set: rescan, then
                            // bring the (new) active match into view.
                            if !to_replace {
                                self.doc_engine.editor.find_rescan();
                                if let Some(doc) = self.doc_engine.doc.as_mut() {
                                    doc.scroll_to(self.doc_engine.editor.cursor_position(), vh);
                                }
                            }
                        }
                    }
                    // Swallow unconditionally: no key reaches the buffer while open.
                    state.window.request_redraw();
                    return;
                }

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
                                apply_edit_effects(
                                    &mut self.doc_engine,
                                    &mut self.hovered,
                                    &self.runtime,
                                    &self.proxy,
                                    None,
                                    &state.window,
                                    w,
                                    state.scale,
                                    vh,
                                );
                            }
                            return;
                        }
                        "v" => {
                            if let Some(cb) = self.clipboard.as_mut()
                                && let Ok(text) = cb.get_text()
                                && !text.is_empty()
                            {
                                self.doc_engine.editor.paste(&text);
                                apply_edit_effects(
                                    &mut self.doc_engine,
                                    &mut self.hovered,
                                    &self.runtime,
                                    &self.proxy,
                                    Some((&self.ac_gen, &self.ac_slot)),
                                    &state.window,
                                    w,
                                    state.scale,
                                    vh,
                                );
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
                            apply_edit_effects(
                                &mut self.doc_engine,
                                &mut self.hovered,
                                &self.runtime,
                                &self.proxy,
                                None,
                                &state.window,
                                w,
                                state.scale,
                                vh,
                            );
                            return;
                        }
                        _ => {}
                    }
                }

                if apply_key(&mut self.doc_engine.editor, self.modifiers, &event) {
                    apply_edit_effects(
                        &mut self.doc_engine,
                        &mut self.hovered,
                        &self.runtime,
                        &self.proxy,
                        Some((&self.ac_gen, &self.ac_slot)),
                        &state.window,
                        w,
                        state.scale,
                        vh,
                    );
                }
            }
            WindowEvent::RedrawRequested => {
                // Apply a coalesced drag move once per frame (see `drag_pending`).
                if self.drag_pending {
                    self.drag_pending = false;
                    let (w, vh, scale) = state.viewport();
                    drag_extend_step(&mut self.doc_engine, self.mouse_pos, 0.0, w, scale, vh);
                }
                self.scene.reset();

                // Keep the native title bar's text current (filename + dirty marker).
                let desired = window_title(&self.doc_engine.editor);
                if desired != self.title {
                    self.title = desired;
                    state.window.set_title(&self.title);
                }

                let width = state.surface.config.width as f32;
                let height = state.surface.config.height as f32;
                let (content_top, editor_h) = chrome_metrics(state.scale, height, state.find_bar_h);

                // Apply any coalesced async-completion rebuild once, before drawing.
                if self.pending_rebuild {
                    self.doc_engine
                        .rebuild_preserving_scroll(width, state.scale, editor_h);
                    sync_image_loads(&self.doc_engine, &self.runtime, &self.proxy);
                    self.pending_rebuild = false;
                }

                if let Some(doc) = self.doc_engine.doc.as_ref() {
                    let (btn_rects, outline_rects, fold_rects) = paint_document(
                        &mut self.scene,
                        &mut self.doc_engine.text_engine,
                        &self.doc_engine.theme,
                        &self.doc_engine.editor,
                        doc,
                        content_top,
                        editor_h,
                        state.find_bar_h,
                        width,
                        height,
                        state.scale,
                        self.outline_hover,
                        self.gutter_hover,
                    );
                    self.find_btn_rects = btn_rects;
                    self.outline_row_rects = outline_rects;
                    self.fold_chevron_rects = fold_rects;

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
                    // The find bar itself is drawn inside `paint_document` as a bottom
                    // chrome strip, so the document reflows above it.
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

    /// Park the loop until the next real event. External file edits arrive as
    /// `WritEvent::FileChanged` (forwarded from the watcher thread), so there's no
    /// need to poll on a timer — EXCEPT while a drag sits in an edge hot zone, when we
    /// tick a short timer to keep auto-scrolling the selection even if the pointer is
    /// held still.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.mouse_down && self.drag_scroll_dy != 0.0 {
            // Integrate velocity against real elapsed time: a first tick (no prior
            // timestamp) scrolls nothing and just starts the clock, so the amount can't
            // spike regardless of how the loop woke.
            let now = std::time::Instant::now();
            let dt = self
                .last_drag_tick
                .map_or(0.0, |t| now.duration_since(t).as_secs_f32());
            self.last_drag_tick = Some(now);
            let dy = self.drag_scroll_dy * dt;
            if let Some((w, editor_h, scale, window)) = self.state.as_ref().map(|s| {
                let (w, editor_h, scale) = s.viewport();
                (w, editor_h, scale, s.window.clone())
            }) {
                drag_extend_step(&mut self.doc_engine, self.mouse_pos, dy, w, scale, editor_h);
                window.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                now + std::time::Duration::from_millis(16),
            ));
        } else {
            self.last_drag_tick = None;
            event_loop.set_control_flow(ControlFlow::Wait);
        }
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

    // No args → the demo showcase.
    if std::env::args().len() <= 1 {
        return demo_editor();
    }

    let config = Config::parse();
    // `--demo`: showcase doc with a synthetic HEAD diff (no file needed).
    if config.demo {
        return demo_editor();
    }
    let Some(path) = config.file.clone() else {
        return demo_editor();
    };

    let mut editor = Editor::open(&path);
    editor.set_autosave(config.autosave);

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
            validate_ref_into_cache(&client, &cache, r).await;
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
    let fetched = tokio::runtime::Handle::current()
        .block_on(fetch_suggestions(&client, &context, trigger, prefix));
    apply_fetched(editor, fetched);
}

/// Render a single frame of the document to an offscreen texture and write it to
/// `path` as a PNG. Independent of any surface/window, so it runs headlessly and
/// doubles as a golden-image harness for later phases.
pub fn snapshot(path: &str, width: u32, height: u32, scroll_y: f32) -> Result<()> {
    // WRIT_SHELL_FILE opens a real file (with live HEAD diff); else the demo doc.
    let mut editor = match std::env::var("WRIT_SHELL_FILE") {
        Ok(p) => Editor::open(std::path::Path::new(&p)),
        Err(_) => Editor::new(DEMO_DOC),
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
        editor.set_head_base(DEMO_BASE);
    }
    // WRIT_SHELL_OUTLINE reserves the right outline strip so the golden frame shows the
    // reflowed document beside the placeholder panel.
    if std::env::var("WRIT_SHELL_OUTLINE").is_ok() {
        editor.set_outline_open(true);
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
    // Headless snapshots never open the find bar, so no find-bar inset.
    let (content_top, editor_h) = chrome_metrics(1.0, height as f32, 0.0);
    // Reuse the shared rebuild path (caches + Parley/Vello engine) headlessly.
    let mut de = DocEngine {
        text_engine: TextEngine::new(),
        line_cache: LineCache::new(),
        render_cache: RenderCache::new(),
        height_cache: HeightCache::new(),
        table_cache: TableCache::new(),
        theme: EditorTheme::dracula(),
        editor,
        doc: None,
        images: ImageCache::new(),
        preedit: None,
    };
    // Run viewport detection (naked URLs, GitHub refs, inline `$…$` math) over the whole
    // document so the golden frame matches the live render, which detects before layout.
    de.editor.refresh_detection(0..usize::MAX);
    // Headless: lay out the whole document (anchor 0, infinite viewport) so any scroll
    // position renders correctly for the golden frame.
    let mut doc = de.rebuild(width as f32, 1.0, 0, f32::INFINITY);
    // Synchronously decode local standalone images so they appear in the single frame
    // (remote images stay a placeholder headlessly). Then rebuild so their heights land.
    let img_dir = de
        .editor
        .file_path()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));
    load_local_images_blocking(img_dir.as_deref(), doc.image_urls(), &de.images);
    #[cfg(feature = "mermaid")]
    mermaid::render_mermaid_blocking(doc.mermaid_sources(), &de.images);
    #[cfg(feature = "math")]
    math::render_math_blocking(doc.math_sources(), &de.images);
    doc = de.rebuild(width as f32, 1.0, 0, f32::INFINITY);
    doc.scroll_by(scroll_y, editor_h);
    let mut scene = Scene::new();
    paint_document(
        &mut scene,
        &mut de.text_engine,
        &de.theme,
        &de.editor,
        &doc,
        content_top,
        editor_h,
        0.0,
        width as f32,
        height as f32,
        1.0,
        None,
        None,
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

    rasterize_scene_to_png(
        &scene,
        width,
        height,
        peniko_color(de.theme.background),
        path,
    )?;
    eprintln!("[writ] wrote snapshot: {path} ({width}x{height})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_edge_velocity_zones() {
        let h = 400.0;
        // Middle of the viewport: no auto-scroll.
        assert_eq!(drag_edge_velocity(200.0, h, 1.0), 0.0);
        // Top zone: negative (scroll up), deeper = faster.
        assert!(drag_edge_velocity(5.0, h, 1.0) < drag_edge_velocity(20.0, h, 1.0));
        assert!(drag_edge_velocity(5.0, h, 1.0) < 0.0);
        // Bottom zone: positive (scroll down).
        assert!(drag_edge_velocity(h - 2.0, h, 1.0) > 0.0);
        // At the exact edge the speed eases to EDGE_SPEED (not the cap).
        assert!((drag_edge_velocity(0.0, h, 1.0) + DRAG_SCROLL_EDGE_SPEED).abs() < 1e-3);
        // Past the edge (outside the window) it grows but never exceeds the cap.
        assert!(drag_edge_velocity(-10.0, h, 1.0).abs() > DRAG_SCROLL_EDGE_SPEED);
        assert!(drag_edge_velocity(-100_000.0, h, 1.0).abs() <= DRAG_SCROLL_MAX_SPEED + 1e-3);
        assert!(drag_edge_velocity(h + 100_000.0, h, 1.0) <= DRAG_SCROLL_MAX_SPEED + 1e-3);
        // Scale widens the zone: 30px from the top is inside the zone at 2x, not at 1x.
        assert!(drag_edge_velocity(30.0, h, 2.0) < 0.0);
        assert_eq!(drag_edge_velocity(30.0, h, 1.0), 0.0);
    }
}
