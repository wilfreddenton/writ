//! winit + wgpu + Vello application shell — the gpui replacement (see MIGRATION-PLAN.md).
//!
//! Phase 0–1: opens a resizable window and renders one syntax-highlighted,
//! Chromium-wrapped markdown line via Vello on the GPU. Later phases layer full
//! document rendering, input, diff, and chrome onto this skeleton. Run with
//! `WGPU_BACKEND=vulkan cargo run --bin writ-next` on Asahi; set
//! `WRIT_SHELL_SNAPSHOT=out.ppm` to render one frame headlessly instead.

use std::sync::Arc;

use anyhow::Result;
use parley::Layout;
use vello::peniko::Brush;
use vello::util::{RenderContext, RenderSurface};
use vello::wgpu;
use vello::wgpu::CurrentSurfaceTexture;
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

use crate::editor::EditorTheme;
use crate::text_engine::{StyleRun, TextEngine, peniko_color};

const PADDING: f32 = 24.0;
const FONT_SIZE: f32 = 18.0;
const LINE_HEIGHT: f32 = 1.5;

struct ActiveSurface {
    surface: RenderSurface<'static>,
    window: Arc<Window>,
    scale: f32,
    /// The Phase 1 demo line, laid out to the current width. Rebuilt on resize.
    line: Option<Layout<Brush>>,
}

struct App {
    context: RenderContext,
    // One renderer suffices; keyed to the surface's device.
    renderer: Option<Renderer>,
    state: Option<ActiveSurface>,
    scene: Scene,
    text_engine: TextEngine,
    theme: EditorTheme,
}

impl App {
    fn new() -> Self {
        Self {
            context: RenderContext::new(),
            renderer: None,
            state: None,
            scene: Scene::new(),
            text_engine: TextEngine::new(),
            theme: EditorTheme::dracula(),
        }
    }
}

/// Build the Phase 1 hard-coded, syntax-highlighted markdown line, wrapped to
/// `device_width`. Stand-in for the real per-line pipeline (Phases 3+). Free
/// function (not a method) so it borrows only `engine`+`theme`, leaving the
/// caller free to keep its `&mut self.state` surface borrow alive.
fn build_demo_line(
    engine: &mut TextEngine,
    theme: &EditorTheme,
    device_width: f32,
    scale: f32,
) -> Layout<Brush> {
    let fg = peniko_color(theme.foreground);
    let text = "Heading — the quick (brown) fox jumps over `lazy_dog()`, \
                renders **bold** and *italic* prose, and wraps like a browser; \
                e.g. it will not orphan a closing paren [see: writ].";

    // Hand-built highlight spans (a real markdown+tree-sitter pass replaces this).
    let mut runs = Vec::new();
    let mut style = |needle: &str, f: &dyn Fn(&mut StyleRun)| {
        if let Some(start) = text.find(needle) {
            let mut run = StyleRun::new(start..start + needle.len(), fg);
            f(&mut run);
            runs.push(run);
        }
    };
    style("Heading", &|r| r.color = peniko_color(theme.purple));
    style("(brown)", &|r| r.color = peniko_color(theme.orange));
    style("lazy_dog()", &|r| {
        r.mono = true;
        r.color = peniko_color(theme.green);
    });
    style("**bold**", &|r| {
        r.bold = true;
        r.color = peniko_color(theme.pink);
    });
    style("*italic*", &|r| {
        r.italic = true;
        r.color = peniko_color(theme.cyan);
    });
    style("[see: writ]", &|r| {
        r.underline = true;
        r.color = peniko_color(theme.yellow);
    });

    let max_advance = (device_width - 2.0 * PADDING * scale).max(1.0);
    engine.build_line(
        text,
        scale,
        FONT_SIZE,
        LINE_HEIGHT,
        fg,
        Some(max_advance),
        &runs,
    )
}

impl ApplicationHandler for App {
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
        let line = build_demo_line(&mut self.text_engine, &self.theme, size.width as f32, scale);
        self.state = Some(ActiveSurface {
            surface,
            window,
            scale,
            line: Some(line),
        });
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
                let line = build_demo_line(
                    &mut self.text_engine,
                    &self.theme,
                    size.width as f32,
                    state.scale,
                );
                state.line = Some(line);
                state.window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                state.scale = scale_factor as f32;
                state.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                self.scene.reset();

                // Paint the demo line at (PADDING, PADDING) in device px.
                if let Some(line) = state.line.as_ref() {
                    let pad = PADDING * state.scale;
                    self.text_engine
                        .draw_line(&mut self.scene, line, (pad, pad));
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
        return snapshot(&path, 1000, 400);
    }

    let event_loop = EventLoop::new()?;
    let mut app = App::new();
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Render a single frame of the demo line to an offscreen texture and write it to
/// `path` as a PNG. Independent of any surface/window, so it runs headlessly and
/// doubles as a golden-image harness for later phases.
pub fn snapshot(path: &str, width: u32, height: u32) -> Result<()> {
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
    let line = build_demo_line(&mut engine, &theme, width as f32, 1.0);
    let mut scene = Scene::new();
    engine.draw_line(&mut scene, &line, (PADDING, PADDING));

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
