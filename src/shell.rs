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
use winit::window::{Window, WindowId};

use winit::event_loop::ControlFlow;

use crate::buffer::Buffer;
use crate::chrome::{BarRect, StatusInfo, draw_status_bar, draw_title_bar};
use crate::config::Config;
use crate::core::Editor;
use crate::doc_layout::{DocLayout, GithubRenderData, LineCache};
use crate::editor::{Direction, EditorTheme};
use crate::git::{detect_github_context, parse_github_repo_string};
use crate::github::{GitHubClient, ValidationResult};
use crate::inline::GitHubRef;
use crate::marker::MarkerKind;
use crate::text_engine::{TextEngine, peniko_color};

const PADDING: f32 = 24.0;
const FONT_SIZE: f32 = 18.0;
const LINE_HEIGHT: f32 = 1.5;
/// Device px scrolled per mouse-wheel line notch.
const WHEEL_LINE_STEP: f32 = 48.0;
/// Caret width in logical px (scaled per display).
const CARET_WIDTH: f32 = 2.0;
/// Title/status bar heights in logical px.
const TITLE_BAR_H: f32 = 34.0;
const STATUS_BAR_H: f32 = 24.0;

/// Chrome layout in device px: y where editor content begins, and its height.
fn chrome_metrics(scale: f32, height_dev: f32) -> (f32, f32) {
    let content_top = TITLE_BAR_H * scale;
    let editor_h = (height_dev - (TITLE_BAR_H + STATUS_BAR_H) * scale).max(1.0);
    (content_top, editor_h)
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

struct ActiveSurface {
    surface: RenderSurface<'static>,
    window: Arc<Window>,
    scale: f32,
}

struct App {
    context: RenderContext,
    // One renderer suffices; keyed to the surface's device.
    renderer: Option<Renderer>,
    state: Option<ActiveSurface>,
    scene: Scene,
    text_engine: TextEngine,
    line_cache: LineCache,
    theme: EditorTheme,
    editor: Editor,
    doc: Option<DocLayout>,
    modifiers: ModifiersState,
    mouse_pos: (f32, f32),
    mouse_down: bool,
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
        Self {
            context: RenderContext::new(),
            renderer: None,
            state: None,
            scene: Scene::new(),
            text_engine: TextEngine::new(),
            line_cache: LineCache::new(),
            theme: EditorTheme::dracula(),
            editor,
            doc: None,
            modifiers: ModifiersState::empty(),
            mouse_pos: (0.0, 0.0),
            mouse_down: false,
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
            // Printable text (respects shift for capitals/symbols). Skip when a
            // command modifier is held so shortcuts don't type characters.
            if ctrl {
                return false;
            }
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
    let mut new_doc = rebuild_doc(engine, cache, editor, theme, device_width, scale);
    new_doc.scroll_y = prev_scroll;
    new_doc.scroll_to(editor.cursor_position(), editor_h);
    *doc = Some(new_doc);
}

/// Lay out the whole document at `device_width`. Free function so it borrows only
/// the fields it needs, leaving the caller's `&mut self.state` borrow intact.
fn rebuild_doc(
    engine: &mut TextEngine,
    cache: &mut LineCache,
    editor: &mut Editor,
    theme: &EditorTheme,
    device_width: f32,
    scale: f32,
) -> DocLayout {
    let cursor_offset = editor.cursor_position();
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
    );
    doc.set_content_top(TITLE_BAR_H * scale);
    doc
}

impl ApplicationHandler<WritEvent> for App {
    /// A tokio task finished (validation/suggestion). Results are already in the
    /// shared caches; rebuild the doc (ref colors may have changed) and redraw.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: WritEvent) {
        match event {
            WritEvent::GithubUpdated => {
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
                        &mut self.editor,
                        &self.theme,
                        w,
                        scale,
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
        let attrs = Window::default_attributes().with_title("writ");
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
        let doc = rebuild_doc(
            &mut self.text_engine,
            &mut self.line_cache,
            &mut self.editor,
            &self.theme,
            size.width as f32,
            scale,
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
                let doc = rebuild_doc(
                    &mut self.text_engine,
                    &mut self.line_cache,
                    &mut self.editor,
                    &self.theme,
                    size.width as f32,
                    state.scale,
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
                if let Some(doc) = self.doc.as_mut() {
                    let (_, editor_h) =
                        chrome_metrics(state.scale, state.surface.config.height as f32);
                    doc.scroll_by(dy, editor_h);
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
                    let w = state.surface.config.width as f32;
                    let (_, vh) = chrome_metrics(state.scale, state.surface.config.height as f32);
                    refresh_doc(
                        &mut self.text_engine,
                        &mut self.line_cache,
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
                if self.mouse_down
                    && let Some(off) = self
                        .doc
                        .as_ref()
                        .and_then(|d| d.hit_test(self.mouse_pos.0, self.mouse_pos.1))
                {
                    self.editor.drag(off);
                    let w = state.surface.config.width as f32;
                    let (_, vh) = chrome_metrics(state.scale, state.surface.config.height as f32);
                    refresh_doc(
                        &mut self.text_engine,
                        &mut self.line_cache,
                        &mut self.editor,
                        &self.theme,
                        &mut self.doc,
                        w,
                        state.scale,
                        vh,
                    );
                    state.window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                self.mouse_down = true;
                if let Some(off) = self
                    .doc
                    .as_ref()
                    .and_then(|d| d.hit_test(self.mouse_pos.0, self.mouse_pos.1))
                {
                    self.editor.click(off, self.modifiers.shift_key(), 1);
                    let w = state.surface.config.width as f32;
                    let (_, vh) = chrome_metrics(state.scale, state.surface.config.height as f32);
                    refresh_doc(
                        &mut self.text_engine,
                        &mut self.line_cache,
                        &mut self.editor,
                        &self.theme,
                        &mut self.doc,
                        w,
                        state.scale,
                        vh,
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
                if apply_key(&mut self.editor, self.modifiers, &event) {
                    let w = state.surface.config.width as f32;
                    let (_, vh) = chrome_metrics(state.scale, state.surface.config.height as f32);
                    refresh_doc(
                        &mut self.text_engine,
                        &mut self.line_cache,
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
            WindowEvent::RedrawRequested => {
                self.scene.reset();

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

                    // Chrome: title bar (top) + status bar (bottom).
                    let info = build_status_info(&self.editor, doc, editor_h);
                    let filename = self
                        .editor
                        .file_path()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "untitled".to_string());
                    draw_title_bar(
                        &mut self.text_engine,
                        &mut self.scene,
                        &self.theme,
                        &BarRect {
                            x0: 0.0,
                            y0: 0.0,
                            x1: width as f64,
                            y1: content_top as f64,
                        },
                        &filename,
                        self.editor.is_dirty(),
                        state.scale,
                    );
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
    }
    let (content_top, editor_h) = chrome_metrics(1.0, height as f32);
    let mut cache = LineCache::new();
    let mut doc = rebuild_doc(
        &mut engine,
        &mut cache,
        &mut editor,
        &theme,
        width as f32,
        1.0,
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
    let filename = editor
        .file_path()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "untitled.md".to_string());
    draw_title_bar(
        &mut engine,
        &mut scene,
        &theme,
        &BarRect {
            x0: 0.0,
            y0: 0.0,
            x1: width as f64,
            y1: content_top as f64,
        },
        &filename,
        editor.is_dirty(),
        1.0,
    );
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
