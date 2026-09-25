//! Window-surface renderer owned by the host's optional rendering feature.
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

// External crates
use crate::component::{RenderViewport, VirtualResolution};
use pill_engine::engine::Engine;

// Current crate
use crate::sprite::SpriteRenderer;

// =============================================================================
// Re-exports
// =============================================================================

/// Rendering initialization or presentation failure without exposed wgpu types.
///
/// The semantic error enum is declared in [`crate::error::RendererError`] and
/// re-exported here for the pre-existing module path.
pub use crate::error::RendererError;

// =============================================================================
// RendererWindow
// =============================================================================

/// Window-handle capability accepted by the engine renderer.
///
/// The blanket implementation lets frontends pass compatible window values
/// such as `Arc<winit::window::Window>` without importing wgpu themselves.
pub trait RendererWindow: wgpu::WindowHandle {}

impl<T> RendererWindow for T where T: wgpu::WindowHandle {}

// =============================================================================
// Renderer
// =============================================================================

/// Engine-owned GPU state associated with one frontend window surface.
///
/// Holds every wgpu resource the engine needs to draw one frame — the surface,
/// device, queue, and sprite renderer — plus the optional viewport and
/// logical-resolution overrides installed by frontends.
pub struct Renderer {
    /// The GPU surface bound to the frontend's window handle.
    surface: wgpu::Surface<'static>,
    /// Logical GPU device used for all rendering commands.
    device: wgpu::Device,
    /// Command queue that submits rendered frames to the device.
    queue: wgpu::Queue,
    /// Surface configuration reapplied after creation, resize, or loss.
    surface_config: wgpu::SurfaceConfiguration,
    /// Draws the sprite entities into a texture view each frame.
    sprite_renderer: SpriteRenderer,
    /// Physical-pixel crop rectangle, or `None` for full-surface rendering.
    viewport: Option<RenderViewport>,
    /// Logical scene size filling the viewport, or `None` for one-to-one pixels.
    virtual_resolution: Option<VirtualResolution>,
}

impl Renderer {
    /// Create the GPU surface and all resources needed to draw an engine world.
    ///
    /// The supplied handle is retained by wgpu for the surface lifetime. An
    /// `Arc<winit::window::Window>` satisfies this API without making the engine
    /// depend on winit.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::SurfaceCreation`] when the window handle cannot
    /// be bound to a wgpu surface, [`RendererError::AdapterRequest`] when no
    /// compatible GPU adapter exists, and [`RendererError::DeviceCreation`]
    /// when the device cannot be created from the adapter. A surface exposing
    /// no texture formats or no alpha modes yields
    /// [`RendererError::NoTextureFormats`] or [`RendererError::NoAlphaModes`].
    pub fn new<W>(window: W, width: u32, height: u32) -> Result<Self, RendererError>
    where
        W: RendererWindow + 'static,
    {
        // Step 1: create the wgpu instance and bind it to the frontend window.
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window)
            .map_err(|error| RendererError::SurfaceCreation { source: error })?;

        // Step 2: acquire an adapter compatible with the surface.
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .map_err(|error| RendererError::AdapterRequest { source: error })?;

        // Which adapter and backend won is not visible from the surface
        // capabilities, and an adapter that cannot present to this window looks
        // identical to one that can until the first frame fails.
        let adapter_info = adapter.get_info();
        println!(
            "[render] Adapter: {} ({:?}, {:?}) driver={} {}",
            adapter_info.name,
            adapter_info.backend,
            adapter_info.device_type,
            adapter_info.driver,
            adapter_info.driver_info
        );

        // Step 3: request the device and queue from the adapter.
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ECS renderer device"),
            required_features: wgpu::Features::empty(),
            required_limits:
                wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::default(),
            ..Default::default()
        }))
        .map_err(|error| RendererError::DeviceCreation { source: error })?;

        // Step 4: derive the surface format, alpha mode, and presentation mode.
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .first()
            .copied()
            .ok_or(RendererError::NoTextureFormats)?;
        let alpha_mode =
            select_alpha_mode(&capabilities.alpha_modes).ok_or(RendererError::NoAlphaModes)?;
        let present_mode = select_present_mode(&capabilities.present_modes);
        println!("[render] Present mode: {present_mode:?}");

        let mut surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };

        // Step 5: configure the surface, falling back when the driver refuses.
        //
        // `Surface::configure` returns nothing: a refused configuration is
        // reported through the device's uncaptured-error path, which panics by
        // default and takes the whole host down before any frontend sees a
        // `RendererError`. A refusal is realistic here because an uncapped
        // present mode needs a flip-model swapchain and a compositing alpha
        // mode needs a compositing window, neither of which the surface
        // capability list guarantees. Each candidate therefore runs inside its
        // own error scopes, and the first configuration the driver accepts wins.
        let mut configured = false;
        for (candidate_present, candidate_alpha) in configure_candidates(present_mode, alpha_mode) {
            surface_config.present_mode = candidate_present;
            surface_config.alpha_mode = candidate_alpha;

            // One scope per filter class, so a refusal is captured rather than
            // reaching the uncaptured-error handler.
            device.push_error_scope(wgpu::ErrorFilter::Validation);
            device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
            device.push_error_scope(wgpu::ErrorFilter::Internal);
            surface.configure(&device, &surface_config);
            // Acquiring a frame is what materialises the swapchain, so it is
            // part of the probe: wgpu-core accepting the configuration does not
            // mean the swapchain was created, and the refusal only shows up
            // here. Without a scope around it that refusal is fatal.
            let probe = surface.get_current_texture();
            let internal = pollster::block_on(device.pop_error_scope());
            let out_of_memory = pollster::block_on(device.pop_error_scope());
            let validation = pollster::block_on(device.pop_error_scope());

            let mut failure = internal
                .or(out_of_memory)
                .or(validation)
                .map(|error| error.to_string());
            let probed = match probe {
                Ok(frame) => Some(frame),
                Err(error) => {
                    failure.get_or_insert_with(|| error.to_string());
                    None
                }
            };

            match (failure, probed) {
                (None, Some(frame)) => {
                    // The probe frame is discarded; the frontend acquires its
                    // own once the first real frame is drawn.
                    drop(frame);
                    println!(
                        "[render] Surface configured: {candidate_present:?} / {candidate_alpha:?}"
                    );
                    configured = true;
                    break;
                }
                (failure, _) => {
                    eprintln!(
                        "[render] Rejected {candidate_present:?} / {candidate_alpha:?}: {}",
                        failure.unwrap_or_else(|| "unknown failure".to_owned())
                    );
                }
            }
        }

        if !configured {
            // Every candidate was refused; the backend errors are on stderr above.
            return Err(RendererError::SurfaceConfigure);
        }

        // Step 6: build the sprite renderer and assemble the renderer state.
        let sprite_renderer = SpriteRenderer::new(&device, format);
        Ok(Self {
            surface,
            device,
            queue,
            surface_config,
            sprite_renderer,
            viewport: None,
            virtual_resolution: None,
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

    /// Select the logical scene size that should fill the physical viewport.
    ///
    /// `None` keeps the original one-logical-unit-per-surface-pixel behavior.
    /// Invalid dimensions are rejected by disabling the override.
    pub fn set_virtual_resolution(&mut self, resolution: Option<VirtualResolution>) {
        self.virtual_resolution = resolution.filter(|resolution| resolution.is_valid());
    }

    /// Draw and present every `(Position, Sprite)` entity in the engine world.
    ///
    /// Lost or outdated surfaces are reconfigured and skipped for one frame.
    /// Timeouts are transient and also skip the frame. Fatal allocation and
    /// generic surface failures are returned to the frontend for reporting.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::SurfaceTextureFailed`] when the frame texture
    /// cannot be acquired due to an out-of-memory condition or an unknown
    /// backend failure. Lost, outdated, and timed-out surfaces are recovered
    /// internally and never produce an error.
    pub fn render(&mut self, engine: &mut Engine) -> Result<(), RendererError> {
        // Step 1: acquire the next frame texture, recovering transient errors.
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.configure_surface();
                return Ok(());
            }
            Err(wgpu::SurfaceError::Timeout) => return Ok(()),
            Err(error @ (wgpu::SurfaceError::OutOfMemory | wgpu::SurfaceError::Other)) => {
                return Err(RendererError::SurfaceTextureFailed { source: error });
            }
        };

        // Step 2: build the texture view and resolve the viewport and projection.
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

        let virtual_resolution = resolve_virtual_resolution(self.virtual_resolution, viewport);

        // Step 3: draw the sprite world into the view and present the frame.
        self.sprite_renderer.render_in_viewport_with_resolution(
            engine.world_mut(),
            &self.device,
            &self.queue,
            &view,
            viewport,
            virtual_resolution,
        );
        frame.present();
        Ok(())
    }

    /// Apply the current surface configuration after creation, resize, or loss.
    fn configure_surface(&self) {
        self.surface.configure(&self.device, &self.surface_config);
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Resolve the logical projection without coupling it to surface dimensions.
fn resolve_virtual_resolution(
    configured: Option<VirtualResolution>,
    viewport: RenderViewport,
) -> VirtualResolution {
    configured
        .filter(|resolution| resolution.is_valid())
        .unwrap_or_else(|| {
            VirtualResolution::new(viewport.width.max(1) as f32, viewport.height.max(1) as f32)
        })
}

/// Presentation and alpha combinations to try, most preferred first.
///
/// The first candidate is always the pair the selection helpers chose, so a
/// driver that accepts it behaves exactly as before. The rest give up the two
/// preferences the surface capability list does not guarantee: a compositing
/// alpha mode needs a compositing window, and an uncapped present mode needs a
/// flip-model swapchain. `Fifo` with `Opaque` is the pair every surface is
/// required to support, so it is the last resort.
///
/// Never empty: the caller depends on at least one configuration being tried.
fn configure_candidates(
    present_mode: wgpu::PresentMode,
    alpha_mode: wgpu::CompositeAlphaMode,
) -> Vec<(wgpu::PresentMode, wgpu::CompositeAlphaMode)> {
    let mut candidates = vec![(present_mode, alpha_mode)];
    if alpha_mode != wgpu::CompositeAlphaMode::Opaque {
        candidates.push((present_mode, wgpu::CompositeAlphaMode::Opaque));
    }
    if present_mode != wgpu::PresentMode::Fifo {
        candidates.push((wgpu::PresentMode::Fifo, wgpu::CompositeAlphaMode::Opaque));
    }
    candidates
}

/// Select the presentation mode to configure the surface with.
///
/// `Fifo` is the only mode a Vulkan surface is required to support, and a mode
/// returned by `Surface::get_capabilities` is not thereby creatable: the NVIDIA
/// Vulkan driver on Windows advertises `Immediate` and then fails the swapchain
/// with an out-of-memory error, which wgpu reports through the device's
/// uncaptured-error path and which aborts the host before any `RendererError`
/// can be constructed. Preferring the guaranteed mode keeps that failure off the
/// startup path; an uncapped mode stays available as an opt-in for a driver that
/// has been verified to create one.
fn select_present_mode(_supported: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    wgpu::PresentMode::Fifo
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

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The guaranteed mode wins even when the surface advertises an uncapped one.
    ///
    /// Advertising is not a promise: the NVIDIA Vulkan driver on Windows lists
    /// `Immediate` and then fails to create the swapchain for it, so the
    /// selector must not prefer it.
    #[test]
    fn present_mode_always_selects_fifo() {
        let advertised = [
            wgpu::PresentMode::Immediate,
            wgpu::PresentMode::Mailbox,
            wgpu::PresentMode::Fifo,
        ];
        assert_eq!(select_present_mode(&advertised), wgpu::PresentMode::Fifo);
        assert_eq!(
            select_present_mode(&[wgpu::PresentMode::Fifo]),
            wgpu::PresentMode::Fifo
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
        assert_eq!(P
            RenderViewport::new(100, 0, 20, 20).clamped_to(100, 90),
            None
        );
    }

    /// A configured project coordinate space remains stable as the panel changes.
    #[test]
    fn virtual_resolution_is_independent_of_physical_viewport_size() {
        let configured = VirtualResolution::new(800.0, 600.0);

        assert_eq!(
            resolve_virtual_resolution(Some(configured), RenderViewport::new(240, 80, 517, 463)),
            configured
        );
        assert_eq!(
            resolve_virtual_resolution(None, RenderViewport::new(240, 80, 517, 463)),
            VirtualResolution::new(517.0, 463.0)
        );
    }
}
