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

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use vello::kurbo::{Affine, Rect};
use vello::peniko::Fill;
use vello::util::{RenderContext, RenderSurface};
use vello::wgpu;
use vello::wgpu::CurrentSurfaceTexture;
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Theme, Window, WindowId};

use winit::event_loop::ControlFlow;

use crate::buffer::Buffer;
use crate::chrome::{BarRect, StatusInfo, draw_panel, draw_status_bar};
use crate::config::Config;
use crate::core::{AutocompleteState, AutocompleteSuggestion, AutocompleteTrigger, Editor};
use crate::doc_layout::{DocLayout, GithubRenderData, LineCache, RenderCache, ScreenRect};
use crate::editor::{Direction, EditorTheme};
use crate::git::{detect_github_context, parse_github_repo_string};
use crate::github::{
    GitHubClient, IssueOrPr, IssueStatus, MentionableUser, ValidatedRefData, ValidationResult,
    ValidationState,
};
use crate::inline::{GitHubContext, GitHubRef};
use crate::marker::MarkerKind;
use crate::text_engine::{StyleRun, TextEngine, peniko_color};

const PADDING: f32 = 24.0;
const FONT_SIZE: f32 = 18.0;
const LINE_HEIGHT: f32 = 1.5;
/// Device px scrolled per mouse-wheel line notch.
const WHEEL_LINE_STEP: f32 = 48.0;
/// Caret width in logical px (scaled per display).
const CARET_WIDTH: f32 = 2.0;
/// Status bar height in logical px (the title bar is the native decoration).
const STATUS_BAR_H: f32 = 24.0;

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
    let rope = buffer.rope();
    let col = rope
        .byte_to_char(cursor)
        .saturating_sub(rope.byte_to_char(line_start))
        + 1;
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

struct App {
    context: RenderContext,
    // One renderer suffices; keyed to the surface's device.
    renderer: Option<Renderer>,
    state: Option<ActiveSurface>,
    scene: Scene,
    text_engine: TextEngine,
    line_cache: LineCache,
    render_cache: RenderCache,
    theme: EditorTheme,
    editor: Editor,
    doc: Option<DocLayout>,
    modifiers: ModifiersState,
    mouse_pos: (f32, f32),
    mouse_down: bool,
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
            text_engine: TextEngine::new(),
            line_cache: LineCache::new(),
            render_cache: RenderCache::new(),
            theme: EditorTheme::dracula(),
            editor,
            doc: None,
            modifiers: ModifiersState::empty(),
            mouse_pos: (0.0, 0.0),
            mouse_down: false,
            title,
            hovered: None,
            ac_row_rects: Vec::new(),
            ac_gen: Arc::new(AtomicU64::new(0)),
            ac_slot: Arc::new(Mutex::new(None)),
            proxy,
            runtime,
        }
    }

    /// Spawn async validation for every not-yet-cached GitHub ref currently detected.
    /// Each finished task writes its result into the shared cache and wakes the loop.
    fn spawn_validations(&self) {
        spawn_ref_validations(&self.editor, &self.runtime, &self.proxy);
    }
}

/// For each detected GitHub ref (from refs + naked URLs) with no cache entry yet,
/// mark it pending and spawn a tokio task to validate it. Results land in the shared
/// `GitHubValidationCache`; a `GithubUpdated` wakeup triggers a redraw.
fn spawn_ref_validations(
    editor: &Editor,
    runtime: &tokio::runtime::Handle,
    proxy: &EventLoopProxy<WritEvent>,
) {
    let Some(client) = editor.github_client() else {
        return;
    };
    let cache = editor.github_validation_cache();

    let mut refs: Vec<GitHubRef> = Vec::new();
    for m in editor.github_refs_by_line().values().flatten() {
        refs.push(m.reference.clone());
    }
    for u in editor.naked_urls_by_line().values().flatten() {
        if let Some(r) = &u.github_ref {
            refs.push(r.clone());
        }
    }

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
            if shift {
                editor.shift_tab();
            } else {
                editor.tab();
            }
            true
        }
        Key::Named(NamedKey::Space) => {
            editor.insert_str(" ");
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
                }
                return false;
            }
            // Printable text (respects shift for capitals/symbols).
            if let Some(text) = &event.text
                && !text.is_empty()
                && !text.chars().any(|c| c.is_control())
            {
                editor.insert_str(text);
                return true;
            }
            false
        }
    }
}

/// Rebuild the document layout after an edit or cursor move (marker reveal is
/// cursor-dependent), preserving scroll then revealing the cursor. Free function
/// so it borrows only these fields, not all of `self` (the surface stays borrowed).
fn refresh_doc(
    engine: &mut TextEngine,
    cache: &mut LineCache,
    render_cache: &mut RenderCache,
    editor: &mut Editor,
    theme: &EditorTheme,
    doc: &mut Option<DocLayout>,
    device_width: f32,
    scale: f32,
    editor_h: f32,
) {
    // Re-detect GitHub refs / naked URLs after the edit so coloring + validation
    // spawning see the current buffer. Whole-buffer scan; cheap enough for now.
    editor.refresh_detection();
    let prev_scroll = doc.as_ref().map(|d| d.scroll_y).unwrap_or(0.0);
    let mut new_doc = rebuild_doc(
        engine,
        cache,
        render_cache,
        editor,
        theme,
        device_width,
        scale,
        prev_scroll + editor_h,
    );
    new_doc.scroll_y = prev_scroll;
    new_doc.scroll_to(editor.cursor_position(), editor_h);
    *doc = Some(new_doc);
}

/// Lay out the whole document at `device_width`. Free function so it borrows only
/// the fields it needs, leaving the caller's `&mut self.state` borrow intact.
fn rebuild_doc(
    engine: &mut TextEngine,
    cache: &mut LineCache,
    render_cache: &mut RenderCache,
    editor: &mut Editor,
    theme: &EditorTheme,
    device_width: f32,
    scale: f32,
    // Device-px depth (scroll_y + viewport_h) that must be fully laid out; deeper
    // lines are height-estimated. `f32::INFINITY` lays out the whole document.
    measure_to_y: f32,
) -> DocLayout {
    let cursor_offset = editor.cursor_position();
    let version = editor.state.buffer.version();
    // Clone the diff before borrowing the buffer mutably for the snapshot.
    let diff = editor.diff_state().cloned();
    let snapshot = editor.state.buffer.render_snapshot();
    // The snapshot is owned, so the mutable borrow above is released — now gather the
    // GitHub autolink data (immutable borrows) to color validated refs.
    let github = GithubRenderData {
        refs_by_line: editor.github_refs_by_line(),
        urls_by_line: editor.naked_urls_by_line(),
        cache: editor.github_validation_cache(),
        context: editor.github_context(),
    };
    let mut doc = DocLayout::build(
        engine,
        cache,
        render_cache,
        version,
        &snapshot,
        theme,
        diff.as_ref(),
        Some(&github),
        cursor_offset,
        device_width,
        scale,
        PADDING,
        PADDING,
        PADDING * 2.0,
        FONT_SIZE,
        LINE_HEIGHT,
        measure_to_y,
    );
    // Editor content begins at the surface top (native title bar is outside the surface).
    doc.set_content_top(0.0);
    doc
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

/// The colored text segments shown in a ref's hover popover, per validation state.
fn hover_segments(
    reference: &GitHubRef,
    state: &Option<ValidationState>,
    context: Option<&GitHubContext>,
    theme: &EditorTheme,
) -> Vec<(String, vello::peniko::Color)> {
    match state {
        Some(ValidationState::Valid(Some(ValidatedRefData::Issue(issue)))) => {
            let status_color = match issue.status() {
                IssueStatus::Open => theme.green,
                IssueStatus::Draft => theme.comment,
                IssueStatus::Merged | IssueStatus::Closed => theme.purple,
                IssueStatus::ClosedNotPlanned => theme.red,
            };
            vec![
                (format!("{} ", issue.symbol()), status_color),
                (format!("#{} ", issue.number), theme.cyan),
                (issue.title.clone(), theme.foreground),
            ]
        }
        Some(ValidationState::Valid(Some(ValidatedRefData::User(user)))) => {
            let mut v = vec![(format!("@{}", user.login), theme.cyan)];
            if let Some(name) = &user.name {
                v.push((format!("  {name}"), theme.comment));
            }
            v
        }
        Some(ValidationState::Valid(None)) => vec![
            ("✓ ".to_string(), theme.green),
            (reference.short_display(context), theme.cyan),
        ],
        Some(ValidationState::Invalid) => vec![
            ("✗ ".to_string(), theme.red),
            (reference.short_display(context), theme.cyan),
        ],
        Some(ValidationState::Pending) | None => vec![
            ("… ".to_string(), theme.comment),
            (reference.short_display(context), theme.cyan),
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

    let mut text = String::new();
    let mut runs = Vec::new();
    for (s, color) in &segs {
        let start = text.len();
        text.push_str(s);
        runs.push(StyleRun::new(start..text.len(), peniko_color(*color)));
    }

    let font_size = 14.0;
    // Cap the panel width (long issue titles) so it doesn't run off-screen.
    let max_text = (viewport_w - 2.0 * PADDING * scale).min(480.0 * scale);
    let layout = engine.build_line(
        &text,
        scale,
        font_size,
        1.3,
        peniko_color(theme.foreground),
        Some(max_text),
        &runs,
    );
    let pad = 8.0 * scale;
    let panel_w = layout.width() + pad * 2.0;
    let panel_h = layout.height() + pad * 2.0;

    let (ax0, ay0, _ax1, ay1) = target.anchor;
    let gap = 4.0 * scale as f64;
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

    let rect = Rect::new(x, y, x + panel_w as f64, y + panel_h as f64);
    draw_panel(
        scene,
        peniko_color(theme.background),
        peniko_color(theme.comment),
        &rect,
        6.0 * scale as f64,
        scale as f64,
    );
    engine.draw_line(scene, &layout, (x as f32 + pad, y as f32 + pad));
}

/// The colored text segments for one autocomplete row.
fn suggestion_segments(
    s: &AutocompleteSuggestion,
    theme: &EditorTheme,
) -> Vec<(String, vello::peniko::Color)> {
    match s {
        AutocompleteSuggestion::IssueOrPr {
            number,
            symbol,
            status,
            title,
        } => {
            let status_color = match status {
                IssueStatus::Open => theme.green,
                IssueStatus::Draft => theme.comment,
                IssueStatus::Merged | IssueStatus::Closed => theme.purple,
                IssueStatus::ClosedNotPlanned => theme.red,
            };
            vec![
                (format!("{symbol} "), status_color),
                (format!("#{number} "), theme.cyan),
                (title.clone(), theme.foreground),
            ]
        }
        AutocompleteSuggestion::User { login, name } => {
            let mut v = vec![(format!("@{login}"), theme.cyan)];
            if let Some(n) = name {
                v.push((format!("  {n}"), theme.comment));
            }
            v
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
        let mut text = String::new();
        let mut runs = Vec::new();
        for (seg, color) in &segs {
            let start = text.len();
            text.push_str(seg);
            runs.push(StyleRun::new(start..text.len(), peniko_color(*color)));
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
        let h = layout.height() + 2.0 * pad_y;
        rows.push((layout, h));
    }
    let panel_h: f32 = rows.iter().map(|(_, h)| *h).sum();

    let gap = 4.0 * scale as f64;
    let mut x = caret.0;
    if x as f32 + panel_w > viewport_w {
        x = (viewport_w - panel_w) as f64;
    }
    x = x.max(0.0);
    let below_y = caret.3 + gap;
    let y = if below_y as f32 + panel_h <= viewport_h {
        below_y
    } else {
        (caret.1 - gap - panel_h as f64).max(0.0)
    };

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
    for (i, (layout, h)) in rows.iter().enumerate() {
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
        engine.draw_line(scene, layout, (x as f32 + pad_x, row_top as f32 + pad_y));
        rects.push((x, row_top, x + panel_w as f64, row_bottom));
        row_top = row_bottom;
    }
    rects
}

impl ApplicationHandler<WritEvent> for App {
    /// A tokio task finished (validation/suggestion). Results are already in the
    /// shared caches; rebuild the doc (ref colors may have changed) and redraw.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: WritEvent) {
        match event {
            WritEvent::GithubUpdated => {
                // Install any fetched autocomplete suggestions (built on this thread).
                let fetched = self.ac_slot.lock().unwrap().take();
                if let Some(fetched) = fetched {
                    apply_fetched(&mut self.editor, fetched);
                }
                if let Some(state) = self.state.as_ref() {
                    let w = state.surface.config.width as f32;
                    let (_, vh) = chrome_metrics(state.scale, state.surface.config.height as f32);
                    let scale = state.scale;
                    let window = state.window.clone();
                    // Rebuild so freshly-validated refs recolor; detection is unchanged,
                    // so skip it and preserve the current scroll (no viewport jump).
                    let prev_scroll = self.doc.as_ref().map(|d| d.scroll_y).unwrap_or(0.0);
                    let mut new_doc = rebuild_doc(
                        &mut self.text_engine,
                        &mut self.line_cache,
                        &mut self.render_cache,
                        &mut self.editor,
                        &self.theme,
                        w,
                        scale,
                        prev_scroll + vh,
                    );
                    new_doc.scroll_y = prev_scroll;
                    new_doc.clamp_scroll(vh);
                    self.doc = Some(new_doc);
                    window.request_redraw();
                }
            }
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
        let doc = rebuild_doc(
            &mut self.text_engine,
            &mut self.line_cache,
            &mut self.render_cache,
            &mut self.editor,
            &self.theme,
            size.width as f32,
            scale,
            editor_h,
        );
        self.doc = Some(doc);
        self.state = Some(ActiveSurface {
            surface,
            window,
            scale,
        });
        // Validate refs already present in the loaded file.
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
                let (_, editor_h) = chrome_metrics(state.scale, size.height as f32);
                let doc = rebuild_doc(
                    &mut self.text_engine,
                    &mut self.line_cache,
                    &mut self.render_cache,
                    &mut self.editor,
                    &self.theme,
                    size.width as f32,
                    state.scale,
                    editor_h,
                );
                self.doc = Some(doc);
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
                let remeasure = if let Some(doc) = self.doc.as_mut() {
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
                    let new_scroll = self.doc.as_ref().map(|d| d.scroll_y).unwrap_or(0.0);
                    let mut nd = rebuild_doc(
                        &mut self.text_engine,
                        &mut self.line_cache,
                        &mut self.render_cache,
                        &mut self.editor,
                        &self.theme,
                        w,
                        scale,
                        new_scroll + editor_h,
                    );
                    nd.scroll_y = new_scroll;
                    nd.clamp_scroll(editor_h);
                    self.doc = Some(nd);
                }
                if self.doc.is_some() {
                    state.window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            // Minimal IME: insert committed text. Preedit (composition) rendering
            // is a follow-up; committing already covers most Latin input methods.
            WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                if !text.is_empty() {
                    self.editor.insert_str(&text);
                    self.hovered = None;
                    let w = state.surface.config.width as f32;
                    let (_, vh) = chrome_metrics(state.scale, state.surface.config.height as f32);
                    refresh_doc(
                        &mut self.text_engine,
                        &mut self.line_cache,
                        &mut self.render_cache,
                        &mut self.editor,
                        &self.theme,
                        &mut self.doc,
                        w,
                        state.scale,
                        vh,
                    );
                    spawn_ref_validations(&self.editor, &self.runtime, &self.proxy);
                    state.window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x as f32, position.y as f32);
                if self.mouse_down {
                    if let Some(off) = self
                        .doc
                        .as_ref()
                        .and_then(|d| d.hit_test(self.mouse_pos.0, self.mouse_pos.1))
                    {
                        self.editor.drag(off);
                        let w = state.surface.config.width as f32;
                        let (_, vh) =
                            chrome_metrics(state.scale, state.surface.config.height as f32);
                        refresh_doc(
                            &mut self.text_engine,
                            &mut self.line_cache,
                            &mut self.render_cache,
                            &mut self.editor,
                            &self.theme,
                            &mut self.doc,
                            w,
                            state.scale,
                            vh,
                        );
                        state.window.request_redraw();
                    }
                } else {
                    // Not dragging: update the hovered GitHub ref (popover source).
                    let new = self.doc.as_ref().and_then(|d| {
                        find_hover_target(&self.editor, d, self.mouse_pos.0, self.mouse_pos.1)
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
                if self.editor.autocomplete().is_some()
                    && let Some(row) = self.ac_row_rects.iter().position(|r| {
                        let (x0, y0, x1, y1) = *r;
                        (self.mouse_pos.0 as f64) >= x0
                            && (self.mouse_pos.0 as f64) <= x1
                            && (self.mouse_pos.1 as f64) >= y0
                            && (self.mouse_pos.1 as f64) <= y1
                    })
                {
                    self.editor.autocomplete_select(row);
                    if self.editor.accept_autocomplete_suggestion() {
                        self.hovered = None;
                        refresh_doc(
                            &mut self.text_engine,
                            &mut self.line_cache,
                            &mut self.render_cache,
                            &mut self.editor,
                            &self.theme,
                            &mut self.doc,
                            w,
                            state.scale,
                            vh,
                        );
                        spawn_ref_validations(&self.editor, &self.runtime, &self.proxy);
                        state.window.request_redraw();
                    }
                    return;
                }

                if let Some(off) = self
                    .doc
                    .as_ref()
                    .and_then(|d| d.hit_test(self.mouse_pos.0, self.mouse_pos.1))
                {
                    self.editor.click(off, self.modifiers.shift_key(), 1);
                    refresh_doc(
                        &mut self.text_engine,
                        &mut self.line_cache,
                        &mut self.render_cache,
                        &mut self.editor,
                        &self.theme,
                        &mut self.doc,
                        w,
                        state.scale,
                        vh,
                    );
                    // Clicking into/out of a ref opens/closes the popup.
                    sync_autocomplete(
                        &mut self.editor,
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

                // When the autocomplete popup is open, route navigation keys to it.
                // `ac_has` is Some(has_suggestions) iff the popup is open.
                let ac_has = self.editor.autocomplete().map(|ac| !ac.suggestions.is_empty());
                if let Some(has) = ac_has {
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.editor.close_autocomplete();
                            state.window.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::ArrowUp) if has => {
                            self.editor.autocomplete_move(false);
                            state.window.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::ArrowDown) if has => {
                            self.editor.autocomplete_move(true);
                            state.window.request_redraw();
                            return;
                        }
                        // `has` guarantees suggestions exist, so accept() always
                        // succeeds here (it mutates as part of the guard).
                        Key::Named(NamedKey::Enter | NamedKey::Tab)
                            if has && self.editor.accept_autocomplete_suggestion() =>
                        {
                            self.hovered = None;
                            refresh_doc(
                                &mut self.text_engine,
                                &mut self.line_cache,
                                &mut self.render_cache,
                                &mut self.editor,
                                &self.theme,
                                &mut self.doc,
                                w,
                                state.scale,
                                vh,
                            );
                            spawn_ref_validations(&self.editor, &self.runtime, &self.proxy);
                            state.window.request_redraw();
                            return;
                        }
                        _ => {}
                    }
                }

                if apply_key(&mut self.editor, self.modifiers, &event) {
                    self.hovered = None;
                    refresh_doc(
                        &mut self.text_engine,
                        &mut self.line_cache,
                        &mut self.render_cache,
                        &mut self.editor,
                        &self.theme,
                        &mut self.doc,
                        w,
                        state.scale,
                        vh,
                    );
                    spawn_ref_validations(&self.editor, &self.runtime, &self.proxy);
                    sync_autocomplete(
                        &mut self.editor,
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
                let desired = window_title(&self.editor);
                if desired != self.title {
                    self.title = desired;
                    state.window.set_title(&self.title);
                }

                let width = state.surface.config.width as f32;
                let height = state.surface.config.height as f32;
                let (content_top, editor_h) = chrome_metrics(state.scale, height);

                if let Some(doc) = self.doc.as_ref() {
                    // Editor content, clipped to the region between the chrome bars.
                    let clip = Rect::new(
                        0.0,
                        content_top as f64,
                        width as f64,
                        (content_top + editor_h) as f64,
                    );
                    self.scene
                        .push_clip_layer(Fill::NonZero, Affine::IDENTITY, &clip);
                    // Draw order (all before glyphs): diff row/word bg, then selection.
                    doc.draw_added_backgrounds(&mut self.scene, editor_h);
                    if let Some(sel) = self.editor.selection_range() {
                        let color = peniko_color(self.theme.selection);
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
                    doc.draw(&self.text_engine, &mut self.scene, editor_h);
                    if let Some((x0, y0, x1, y1)) =
                        doc.caret_rect(self.editor.cursor_position(), CARET_WIDTH * state.scale)
                    {
                        self.scene.fill(
                            Fill::NonZero,
                            Affine::IDENTITY,
                            peniko_color(self.theme.foreground),
                            None,
                            &Rect::new(x0, y0, x1.max(x0 + 1.0), y1),
                        );
                    }
                    self.scene.pop_layer();

                    // Chrome: status bar (bottom). The title bar is the OS/compositor's
                    // native one (filename goes there via `set_title`); we don't draw our
                    // own — that would double up on the native decoration.
                    let info = build_status_info(&self.editor, doc, editor_h);
                    draw_status_bar(
                        &mut self.text_engine,
                        &mut self.scene,
                        &self.theme,
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
                        .editor
                        .autocomplete()
                        .is_some_and(|ac| !ac.suggestions.is_empty());
                    if ac_open {
                        if let Some(caret) =
                            doc.caret_rect(self.editor.cursor_position(), CARET_WIDTH * state.scale)
                        {
                            let ac = self.editor.autocomplete().expect("open");
                            self.ac_row_rects = draw_autocomplete(
                                &mut self.text_engine,
                                &mut self.scene,
                                &self.theme,
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
                                &mut self.text_engine,
                                &mut self.scene,
                                &self.theme,
                                &self.editor,
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
                    base_color: peniko_color(self.theme.background),
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
        if self.editor.poll_file_changes()
            && let Some(state) = self.state.as_ref()
        {
            let w = state.surface.config.width as f32;
            let (_, editor_h) = chrome_metrics(state.scale, state.surface.config.height as f32);
            let window = state.window.clone();
            let scale = state.scale;
            refresh_doc(
                &mut self.text_engine,
                &mut self.line_cache,
                &mut self.render_cache,
                &mut self.editor,
                &self.theme,
                &mut self.doc,
                w,
                scale,
                editor_h,
            );
            spawn_ref_validations(&self.editor, &self.runtime, &self.proxy);
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
    editor.refresh_detection();

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
    let mut refs: Vec<GitHubRef> = Vec::new();
    for m in editor.github_refs_by_line().values().flatten() {
        refs.push(m.reference.clone());
    }
    for u in editor.naked_urls_by_line().values().flatten() {
        if let Some(r) = &u.github_ref {
            refs.push(r.clone());
        }
    }
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

    let mut engine = TextEngine::new();
    let theme = EditorTheme::dracula();
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
        editor.refresh_detection();
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
    let mut cache = LineCache::new();
    let mut render_cache = RenderCache::new();
    let mut doc = rebuild_doc(
        &mut engine,
        &mut cache,
        &mut render_cache,
        &mut editor,
        &theme,
        width as f32,
        1.0,
        scroll_y + editor_h,
    );
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
    doc.draw(&engine, &mut scene, editor_h);
    if let Some((x0, y0, x1, y1)) = doc.caret_rect(editor.cursor_position(), 2.0) {
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            peniko_color(theme.foreground),
            None,
            &Rect::new(x0, y0, x1.max(x0 + 1.0), y1),
        );
    }
    scene.pop_layer();
    let info = build_status_info(&editor, &doc, editor_h);
    draw_status_bar(
        &mut engine,
        &mut scene,
        &theme,
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
        && let Some(t) = hover_target_at_offset(&editor, &doc, off)
    {
        draw_hover_popover(
            &mut engine,
            &mut scene,
            &theme,
            &editor,
            &t,
            width as f32,
            height as f32,
            1.0,
        );
    }

    // Optional autocomplete golden image (see WRIT_SHELL_AUTOCOMPLETE above).
    if let Some(ac) = editor.autocomplete().filter(|ac| !ac.suggestions.is_empty())
        && let Some(caret) = doc.caret_rect(editor.cursor_position(), 2.0)
    {
        draw_autocomplete(
            &mut engine,
            &mut scene,
            &theme,
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
                base_color: peniko_color(theme.background),
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
