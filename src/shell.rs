//! winit + wgpu + Vello application shell — the gpui replacement (see MIGRATION-PLAN.md).
//!
//! Phase 0: opens a resizable window and clears it via Vello on the GPU. Later
//! phases layer the text engine, document rendering, input, diff, and chrome onto
//! this skeleton. Run with `WGPU_BACKEND=vulkan cargo run --bin writ-next` on Asahi.

use std::sync::Arc;

use anyhow::Result;
use vello::util::{RenderContext, RenderSurface};
use vello::wgpu::CurrentSurfaceTexture;
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};
use vello::{peniko::color::palette, wgpu};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

struct ActiveSurface {
    surface: RenderSurface<'static>,
    window: Arc<Window>,
}

struct App {
    context: RenderContext,
    // One renderer suffices; keyed to the surface's device.
    renderer: Option<Renderer>,
    state: Option<ActiveSurface>,
    scene: Scene,
}

impl App {
    fn new() -> Self {
        Self {
            context: RenderContext::new(),
            renderer: None,
            state: None,
            scene: Scene::new(),
        }
    }
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

        self.state = Some(ActiveSurface { surface, window });
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
                state.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                self.scene.reset();

                let dev = &self.context.devices[state.surface.dev_id];
                let params = RenderParams {
                    base_color: palette::css::REBECCA_PURPLE,
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

    let event_loop = EventLoop::new()?;
    let mut app = App::new();
    event_loop.run_app(&mut app)?;
    Ok(())
}
