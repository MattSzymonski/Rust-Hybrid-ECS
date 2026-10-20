//! A windowless device, and rendering a draw list to pixels.
//!
//! # Responsibilities
//!
//! - [`request_device`]: a wgpu device for tests and the golden harness,
//!   optionally the platform's software fallback adapter (WARP on Windows,
//!   lavapipe on Linux) so rendering runs without a GPU.
//! - [`render_rgba`]: render a [`DrawList`] offscreen and copy the pixels
//!   back as tightly packed RGBA8 rows.
//!
//! # Design
//!
//! Both functions are async and never block; the caller supplies the
//! executor (the tests and the golden harness block on
//! `pill_core::platform::futures::block_on`). The offscreen target is
//! `Rgba8UnormSrgb`, the format the renderer's shader is written for, so the
//! pixels that come back are what a correctly configured surface shows.

// Standard library
use std::sync::mpsc;

// External crates
use pill_ui::{Color, DrawList};

// Current crate
use crate::renderer::UiRenderer;

/// wgpu's texture-to-buffer copies require rows aligned to this many bytes;
/// the readback strips the padding back off.
const COPY_ROW_ALIGNMENT: usize = 256;

// =============================================================================
// request_device
// =============================================================================

/// Ask for a device and queue without a surface.
///
/// With `prefer_software` the adapter request asks the backend for its
/// fallback adapter first - WARP (DX12) on Windows, lavapipe (Vulkan) on
/// Linux - so a machine without a usable GPU, and every CI runner, still
/// renders. Returns `None` when the backend offers no adapter at all.
pub async fn request_device(prefer_software: bool) -> Option<(wgpu::Device, wgpu::Queue)> {
    // The primary backends only, like the renderer: the GL backend is the
    // one backend the plan bans (D19), and it needs no fallback here.
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        flags: wgpu::InstanceFlags::from_build_config(),
        backend_options: wgpu::BackendOptions::default(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: None,
            force_fallback_adapter: prefer_software,
            apply_limit_buckets: false,
        })
        .await
        .ok()?;
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("pill_ui_wgpu offscreen device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        })
        .await
        .ok()?;
    Some((device, queue))
}

// =============================================================================
// render_rgba
// =============================================================================

/// Render `list` into an offscreen sRGB target of `target_size` physical
/// pixels and return its pixels as tightly packed RGBA8 rows.
///
/// `scale_factor` is the monitor scale the list's logical pixels are in;
/// `clear` fills the target first (see [`UiRenderer::render`]), `None` still
/// produces defined pixels because the target starts cleared to transparent
/// black.
pub async fn render_rgba(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &mut UiRenderer,
    target_size: (u32, u32),
    scale_factor: f32,
    list: &DrawList,
    clear: Option<Color>,
) -> Result<Vec<u8>, String> {
    let (width, height) = target_size;
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("pill_ui offscreen target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let row_bytes = width as usize * 4;
    let padded_row_bytes = row_bytes.div_ceil(COPY_ROW_ALIGNMENT) * COPY_ROW_ALIGNMENT;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pill_ui offscreen readback"),
        size: (padded_row_bytes * height as usize) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("pill_ui offscreen encoder"),
    });
    renderer.render(
        device,
        queue,
        &mut encoder,
        &view,
        target_size,
        scale_factor,
        list,
        clear,
    );
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row_bytes as u32),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let slice = readback.slice(..);
    let (sender, receiver) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .map_err(|error| error.to_string())?;
    receiver
        .recv()
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;

    let mapped = slice
        .get_mapped_range()
        .map_err(|error| error.to_string())?;
    let rows = tighten_rows(&mapped, padded_row_bytes, row_bytes, height as usize);
    drop(mapped);
    readback.unmap();
    Ok(rows)
}

// =============================================================================
// Free Functions
// =============================================================================

/// Strip the row padding a texture-to-buffer copy adds, returning tightly
/// packed rows.
fn tighten_rows(
    padded: &[u8],
    padded_row_bytes: usize,
    row_bytes: usize,
    height: usize,
) -> Vec<u8> {
    let mut rows = Vec::with_capacity(row_bytes * height);
    for row in 0..height {
        let start = row * padded_row_bytes;
        rows.extend_from_slice(&padded[start..start + row_bytes]);
    }
    rows
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tighten_rows_strips_the_alignment_padding() {
        // Two 3-pixel rows padded to 16 bytes: each row keeps its first 12
        // bytes and drops the tail.
        let padded: Vec<u8> = vec![
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 90, 90, 90, 90, //
            21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 90, 90, 90, 90,
        ];
        let rows = tighten_rows(&padded, 16, 12, 2);
        assert_eq!(
            rows,
            vec![
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
                32
            ]
        );
    }
}
