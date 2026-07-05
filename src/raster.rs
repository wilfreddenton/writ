//! Headless rasterization: turn a Vello [`Scene`] into a PNG on disk via wgpu, with no
//! window or surface. Shared by the golden-image snapshot harness ([`crate::shell`]) and
//! by library consumers/examples so they don't reimplement the device/renderer/readback
//! dance. On Apple Silicon Fedora (Asahi) set `WGPU_BACKEND=vulkan` so wgpu picks a
//! working adapter.

use anyhow::{Result, anyhow};
use image::{ImageBuffer, ImageFormat, Rgba};
use vello::peniko::Color;
use vello::util::RenderContext;
use vello::wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Extent3d, MapMode, Origin3d,
    PollType, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureAspect,
    TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, TextureViewDescriptor,
};
use vello::{AaConfig, AaSupport, RenderParams, Renderer, RendererOptions, Scene};

/// Render `scene` onto a `base_color` background of `width`×`height` device px to an
/// offscreen texture, read the pixels back, and write them to `path` as a PNG. Creates
/// its own wgpu device (no surface/window), so it runs headlessly; on Asahi set
/// `WGPU_BACKEND=vulkan`. The output is always PNG regardless of `path`'s extension.
pub fn rasterize_scene_to_png(
    scene: &Scene,
    width: u32,
    height: u32,
    base_color: Color,
    path: &str,
) -> Result<()> {
    let mut context = RenderContext::new();
    let dev_id = pollster::block_on(context.device(None))
        .ok_or_else(|| anyhow!("no wgpu device (set WGPU_BACKEND=vulkan on Asahi)"))?;
    let device = &context.devices[dev_id].device;
    let queue = &context.devices[dev_id].queue;

    let target = device.create_texture(&TextureDescriptor {
        label: Some("raster_target"),
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
            antialiasing_support: AaSupport::area_only(),
            num_init_threads: None,
            pipeline_cache: None,
        },
    )
    .map_err(|e| anyhow!("Renderer::new failed: {e:?}"))?;
    renderer
        .render_to_texture(
            device,
            queue,
            scene,
            &target_view,
            &RenderParams {
                base_color,
                width,
                height,
                antialiasing_method: AaConfig::Area,
            },
        )
        .map_err(|e| anyhow!("render_to_texture failed: {e:?}"))?;

    // Copy the texture into a mappable buffer (rows padded to 256-byte alignment).
    let bytes_per_pixel = 4u32;
    let unpadded = width * bytes_per_pixel;
    let align = 256u32;
    let padded = unpadded.div_ceil(align) * align;
    let buffer = device.create_buffer(&BufferDescriptor {
        label: Some("raster_readback"),
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

    // Strip the per-row padding into a tight RGBA buffer for the PNG encoder.
    let mut pixels = Vec::with_capacity((unpadded * height) as usize);
    for y in 0..height {
        let row = &data[(y * padded) as usize..(y * padded + unpadded) as usize];
        pixels.extend_from_slice(row);
    }
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_raw(width, height, pixels)
        .ok_or_else(|| anyhow!("pixel buffer size mismatch"))?;
    img.save_with_format(path, ImageFormat::Png)?;
    Ok(())
}
