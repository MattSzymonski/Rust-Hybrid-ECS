//! Window-surface renderer owned by the engine's optional rendering feature.
//!
//! # Responsibilities
//!
//! - Creates the wgpu instance, surface, adapter, device, and queue.
//! - Selects an uncapped presentation mode when the platform supports one.
//! - Reconfigures the surface after frontend resize notifications.
//! - Acquires, draws, and presents one frame from the current [`Engine`].
//!
//! # Design
//!
//! Frontends retain ownership of their event loop and window. They pass a
//! cloneable window handle into [`Renderer::new`], then interact only through
//! [`Renderer::resize`] and [`Renderer::render`]. No frontend needs a direct
//! dependency on wgpu or an async executor.

use std::error::Error;
use std::fmt::{Display, Formatter};

use crate::engine::Engine;
use crate::render::{RenderViewport, SpriteRenderer};

/// Window-handle capability accepted by the engine renderer.
///
/// The blanket implementation lets frontends pass compatible window values
/// such as `Arc<winit::window::Window>` without importing wgpu themselves.
pub trait RendererWindow: wgpu::WindowHandle {}

impl<T> RendererWindow for T where T: wgpu::WindowHandle {}

/// Engine-owned GPU state associated with one frontend window surface.
pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
    sprite_renderer: SpriteRenderer,
    viewport: Option<RenderViewport>,
}

impl Renderer {
    /// Create the GPU surface and all resources needed to draw an engine world.
    ///
    /// The supplied handle is retained by wgpu for the surface lifetime. An
    /// `Arc<winit::window::Window>` satisfies this API without making the engine
    /// depend on winit.
    pub fn new<W>(window: W, width: u32, height: u32) -> Result<Self, RendererError>
    where
        W: RendererWindow + 'static,
    {
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window)
            .map_err(|error| RendererError::initialization("create GPU surface", error))?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .map_err(|error| RendererError::initialization("find a compatible GPU adapter", error))?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ECS renderer device"),
            required_features: wgpu::Features::empty(),
            required_limits:
                wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::default(),
            ..Default::default()
        }))
        .map_err(|error| RendererError::initialization("create GPU device", error))?;

        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .first()
            .copied()
            .ok_or_else(|| RendererError::message("GPU surface exposes no texture formats"))?;
        let alpha_mode = select_alpha_mode(&capabilities.alpha_modes)
            .ok_or_else(|| RendererError::message("GPU surface exposes no alpha modes"))?;
        let present_mode = select_present_mode(&capabilities.present_modes);
        println!("[render] Present mode: {present_mode:?}");

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);

        let sprite_renderer = SpriteRenderer::new(&device, format);
        Ok(Self {
            surface,
            device,
            queue,
            surface_config,
            sprite_renderer,
            viewport: None,
        })
    }

    /// Reconfigure the presentation surface for a new physical window size.
    ///
    /// Zero-sized notifications occur while a window is minimized and are
    /// ignored because wgpu surfaces cannot be configured with zero dimensions.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.configure_surface();
    }

    /// Return the physical dimensions of the currently configured surface.
    pub fn surface_size(&self) -> (u32, u32) {
        (self.surface_config.width, self.surface_config.height)
    }

    /// Restrict rendering to a physical-pixel rectangle within the surface.
    ///
    /// Passing `None` restores full-surface rendering. Frontends embedding the
    /// surface behind UI should update this rectangle whenever their layout or
    /// window scale changes.
    pub fn set_viewport(&mut self, viewport: Option<RenderViewport>) {
        self.viewport = viewport;
    }

    /// Draw and present every `(Position, Sprite)` entity in the engine world.
    ///
    /// Lost or outdated surfaces are reconfigured and skipped for one frame.
    /// Timeouts are transient and also skip the frame. Fatal allocation and
    /// generic surface failures are returned to the frontend for reporting.
    pub fn render(&mut self, engine: &mut Engine) -> Result<(), RendererError> {
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.configure_surface();
                return Ok(());
            }
            Err(wgpu::SurfaceError::Timeout) => return Ok(()),
            Err(error @ (wgpu::SurfaceError::OutOfMemory | wgpu::SurfaceError::Other)) => {
                return Err(RendererError::message(format!(
                    "failed to acquire GPU surface texture: {error}"
                )));
            }
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let viewport = self
            .viewport
            .unwrap_or_else(|| {
                RenderViewport::full(self.surface_config.width, self.surface_config.height)
            })
            .clamped_to(self.surface_config.width, self.surface_config.height)
            .unwrap_or_default();

        self.sprite_renderer.render_in_viewport(
            engine.world_mut(),
            &self.device,
            &self.queue,
            &view,
            viewport,
        );
        frame.present();
        Ok(())
    }

    /// Apply the current surface configuration after creation, resize, or loss.
    fn configure_surface(&self) {
        self.surface.configure(&self.device, &self.surface_config);
    }
}

/// Select the lowest-latency non-vsync mode supported by the current surface.
fn select_present_mode(supported: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    if supported.contains(&wgpu::PresentMode::Immediate) {
        wgpu::PresentMode::Immediate
    } else if supported.contains(&wgpu::PresentMode::Mailbox) {
        wgpu::PresentMode::Mailbox
    } else {
        wgpu::PresentMode::AutoNoVsync
    }
}

/// Prefer an alpha-composited surface for transparent UI overlays.
///
/// Standalone windows remain opaque at the platform window level, while
/// Dioxus can opt its window into transparency and reveal this same surface
/// beneath the webview layer.
fn select_alpha_mode(supported: &[wgpu::CompositeAlphaMode]) -> Option<wgpu::CompositeAlphaMode> {
    supported
        .iter()
        .copied()
        .find(|mode| *mode == wgpu::CompositeAlphaMode::PostMultiplied)
        .or_else(|| {
            supported
                .iter()
                .copied()
                .find(|mode| *mode == wgpu::CompositeAlphaMode::PreMultiplied)
        })
        .or_else(|| supported.first().copied())
}

/// Rendering initialization or presentation failure without exposed wgpu types.
#[derive(Debug)]
pub struct RendererError {
    message: String,
}

impl RendererError {
    /// Add operation context to an error returned by the GPU backend.
    fn initialization(operation: &str, error: impl Display) -> Self {
        Self::message(format!("failed to {operation}: {error}"))
    }

    /// Construct an opaque renderer error suitable for frontend diagnostics.
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for RendererError {
    /// Format the backend-independent renderer diagnostic.
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RendererError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prefer immediate presentation when the surface exposes it.
    #[test]
    fn present_mode_prefers_immediate() {
        let supported = [wgpu::PresentMode::Fifo, wgpu::PresentMode::Immediate];
        assert_eq!(
            select_present_mode(&supported),
            wgpu::PresentMode::Immediate
        );
    }

    /// Prefer mailbox over the automatic fallback when immediate is absent.
    #[test]
    fn present_mode_falls_back_to_mailbox() {
        let supported = [wgpu::PresentMode::Fifo, wgpu::PresentMode::Mailbox];
        assert_eq!(select_present_mode(&supported), wgpu::PresentMode::Mailbox);
    }

    /// Request automatic no-vsync selection when no explicit fast mode exists.
    #[test]
    fn present_mode_uses_auto_no_vsync_as_last_choice() {
        assert_eq!(
            select_present_mode(&[wgpu::PresentMode::Fifo]),
            wgpu::PresentMode::AutoNoVsync
        );
    }

    /// Transparent UI hosts prefer an explicitly composited alpha mode.
    #[test]
    fn alpha_mode_prefers_composited_surface() {
        let supported = [
            wgpu::CompositeAlphaMode::Opaque,
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::PostMultiplied,
        ];
        assert_eq!(
            select_alpha_mode(&supported),
            Some(wgpu::CompositeAlphaMode::PostMultiplied)
        );
    }

    /// Platforms without composited modes retain their first supported mode.
    #[test]
    fn alpha_mode_falls_back_to_first_supported_mode() {
        assert_eq!(
            select_alpha_mode(&[wgpu::CompositeAlphaMode::Opaque]),
            Some(wgpu::CompositeAlphaMode::Opaque)
        );
    }

    /// Embedded viewports cannot extend beyond their native surface.
    #[test]
    fn render_viewport_clamps_to_surface_bounds() {
        assert_eq!(
            RenderViewport::new(80, 40, 50, 70).clamped_to(100, 90),
            Some(RenderViewport::new(80, 40, 20, 50))
        );
        assert_eq!(
            RenderViewport::new(100, 0, 20, 20).clamped_to(100, 90),
            None
        );
    }
}
