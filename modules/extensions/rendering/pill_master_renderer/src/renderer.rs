//! Window-surface renderer owned by the host's optional rendering feature.
//!
//! # Responsibilities
//!
//! - Creates the wgpu instance, surface, adapter, device, and queue.
//! - Selects an uncapped presentation mode when the platform supports one.
//! - Reconfigures the surface after frontend resize notifications.
//! - Acquires, draws, and presents one owned ECS frame packet.
//!
//! # Design
//!
//! Frontends retain ownership of their event loop and window. They pass a
//! cloneable window handle into [`Renderer::new`], then interact only through
//! [`Renderer::resize`] and [`Renderer::render`]. No frontend needs a direct
//! dependency on wgpu or an async executor.

// Current crate
use crate::{FrameOutcome, RenderFrame, RenderViewport};
use crate::graphics::{GpuPassContext, Pass};
use crate::pbr::PbrPipeline;

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

/// Candidate shader pass retained until its asynchronous validation scope resolves.
///
/// The active pass remains usable while this candidate is pending or rejected.
struct PendingShaders {
    /// Complete candidate pass, ready to replace the active pass on success.
    pipeline: PbrPipeline,
    /// Backend validation result, polled once per frame without blocking the event loop.
    validation: std::pin::Pin<Box<dyn std::future::Future<Output = Option<wgpu::Error>>>>,
}

/// Host-owned GPU state associated with one frontend window surface.
///
/// Owns the device, queue, PBR pass, and optional viewport. The frontend retains
/// its event loop and supplies extracted frame packets through the host contract.
pub struct Renderer {
    /// Candidate pass awaiting asynchronous backend validation.
    pending_shaders: Option<PendingShaders>,
    /// Hash of the most recently attempted shader sources, including rejected sources.
    shader_key: u64,
    /// The GPU surface bound to the frontend's window handle.
    surface: wgpu::Surface<'static>,
    /// Logical GPU device used for all rendering commands.
    device: wgpu::Device,
    /// Command queue that submits rendered frames to the device.
    queue: wgpu::Queue,
    /// Surface configuration reapplied after creation, resize, or loss.
    surface_config: wgpu::SurfaceConfiguration,
    /// Draws the mesh entities into a texture view each frame.
    pbr: PbrPipeline,
    /// Physical-pixel crop rectangle, or `None` for full-surface rendering.
    viewport: Option<RenderViewport>,
    /// Suppresses surface acquisition while the window has zero extent.
    minimized: bool,
    /// Latest uncaptured GPU or device-loss error, reported on the next frame attempt.
    errors: std::sync::Arc<std::sync::Mutex<Option<String>>>,
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
        pollster::block_on(Self::new_async(window, width, height))
    }

    /// Async initialization core, independent of the native blocking adapter.
    pub async fn new_async<W: RendererWindow + 'static>(
        window: W,
        width: u32,
        height: u32,
    ) -> Result<Self, RendererError> {
        // Step 1: create the wgpu instance and bind it to the frontend window.
        let instance = wgpu::Instance::default();
        let surface =
            instance
                .create_surface(window)
                .map_err(|error| RendererError::SurfaceCreation {
                    detail: error.to_string(),
                })?;

        // Step 2: acquire an adapter compatible with the surface.
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .map_err(|error| RendererError::AdapterRequest {
                detail: error.to_string(),
            })?;

        let format_features = adapter.get_texture_format_features(wgpu::TextureFormat::Rgba16Float);
        if !format_features
            .allowed_usages
            .contains(wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING)
            || !format_features
                .flags
                .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE)
        {
            return Err(RendererError::DeviceCreation {
                detail: "PBR requires filterable RGBA16Float render targets".into(),
            });
        }
        // Step 3: request the device and queue from the adapter.
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("ECS renderer device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_webgl2_defaults()
                    .using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::default(),
                ..Default::default()
            })
            .await
            .map_err(|error| RendererError::DeviceCreation {
                detail: error.to_string(),
            })?;

        let errors = std::sync::Arc::new(std::sync::Mutex::new(None));
        let error_sink = errors.clone();
        device.on_uncaptured_error(Box::new(move |error| {
            *error_sink.lock().unwrap() = Some(error.to_string());
        }));
        let loss_sink = errors.clone();
        device.set_device_lost_callback(move |reason, message| {
            *loss_sink.lock().unwrap() = Some(format!("device lost: {reason:?}: {message}"));
        });
        // Step 4: derive the surface format, alpha mode, and presentation mode.
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .or_else(|| capabilities.formats.first())
            .copied()
            .ok_or(RendererError::NoTextureFormats)?;
        let alpha_mode =
            select_alpha_mode(&capabilities.alpha_modes).ok_or(RendererError::NoAlphaModes)?;
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

        // Step 5: build the mesh renderer and assemble the renderer state.
        let pbr = PbrPipeline::new(&device, &queue, format, width.max(1), height.max(1));
        Ok(Self {
            pending_shaders: None,
            shader_key: 0,
            surface,
            device,
            queue,
            surface_config,
            pbr,
            viewport: None,
            minimized: width == 0 || height == 0,
            errors,
        })
    }

    /// Reconfigure the presentation surface for a new physical window size.
    ///
    /// Zero-sized notifications occur while a window is minimized and are
    /// ignored because wgpu surfaces cannot be configured with zero dimensions.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.minimized = width == 0 || height == 0;
        if self.minimized {
            return;
        }
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.pbr.resize(&self.device, width, height);
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

    /// Draw and present the frame extracted by the ECS rendering system.
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
    pub fn render(&mut self, packet: &RenderFrame) -> Result<FrameOutcome, RendererError> {
        if let Some(detail) = self.errors.lock().unwrap().take() {
            return Err(RendererError::SurfaceTextureFailed { detail });
        }
        if self.minimized {
            return Ok(FrameOutcome::Skipped);
        }
        // Step 1: acquire the next frame texture, recovering transient errors.
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.configure_surface();
                return Ok(FrameOutcome::Skipped);
            }
            Err(wgpu::SurfaceError::Timeout) => return Ok(FrameOutcome::Skipped),
            Err(error @ (wgpu::SurfaceError::OutOfMemory | wgpu::SurfaceError::Other)) => {
                return Err(RendererError::SurfaceTextureFailed {
                    detail: error.to_string(),
                });
            }
        };

        // Step 2: build the texture view and clip the host viewport to the surface.
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

        // Step 3: poll shader replacement, submit the extracted scene, and present.
        self.reload_shaders(packet);
        self.pbr.draw(
            GpuPassContext {
                device: &self.device,
                queue: &self.queue,
                output: &view,
                viewport,
            },
            packet,
        );
        frame.present();
        Ok(FrameOutcome::Presented)
    }

    /// Poll validation without blocking the event loop; only publish a valid pipeline.
    fn reload_shaders(&mut self, frame: &RenderFrame) {
        if let Some(pending) = self.pending_shaders.as_mut() {
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            if let std::task::Poll::Ready(error) = pending.validation.as_mut().poll(&mut context) {
                let mut candidate = self.pending_shaders.take().unwrap();
                if let Some(error) = error {
                    eprintln!("[render] retaining previous shaders: {error}");
                } else {
                    candidate.pipeline.resize(
                        &self.device,
                        self.surface_config.width,
                        self.surface_config.height,
                    );
                    self.pbr = candidate.pipeline;
                }
            }
            return;
        }
        let pbr = frame.assets.shaders.get("shaders/pbr.wgsl");
        let tone = frame.assets.shaders.get("shaders/tonemap.wgsl");
        let key = if pbr.is_none() && tone.is_none() {
            0
        } else {
            crate::assets::asset_id(&format!(
                "{}|{}",
                pbr.map_or("", String::as_str),
                tone.map_or("", String::as_str)
            ))
        };
        if key == self.shader_key {
            return;
        }
        // Remember failed source versions too: retry only after the source changes,
        // rather than rebuilding the same invalid pipeline every frame.
        self.shader_key = key;
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let pipeline = PbrPipeline::with_shaders(
            &self.device,
            &self.queue,
            self.surface_config.format,
            self.surface_config.width,
            self.surface_config.height,
            pbr.map_or(include_str!("shaders/pbr.wgsl"), String::as_str),
            tone.map_or(include_str!("shaders/tonemap.wgsl"), String::as_str),
        );
        self.pending_shaders = Some(PendingShaders {
            pipeline,
            validation: Box::pin(self.device.pop_error_scope()),
        });
    }

    /// Apply the current surface configuration after creation, resize, or loss.
    fn configure_surface(&self) {
        self.surface.configure(&self.device, &self.surface_config);
    }
}

// =============================================================================
// Free Functions
// =============================================================================

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

// =============================================================================
// Host Interface
// =============================================================================

impl crate::PillRenderer for Renderer {
    fn capabilities(&self) -> crate::api::RenderCapabilities {
        let limits = self.device.limits();
        crate::api::RenderCapabilities {
            max_texture_size: limits.max_texture_dimension_2d,
            max_buffer_bytes: limits.max_buffer_size,
            hdr: true,
        }
    }
    fn metrics(&self) -> crate::api::RenderMetrics {
        self.pbr.metrics
    }
    fn resize(&mut self, w: u32, h: u32) {
        Renderer::resize(self, w, h);
    }
    fn set_viewport(&mut self, v: Option<RenderViewport>) {
        Renderer::set_viewport(self, v);
    }
    fn render(&mut self, f: &RenderFrame) -> Result<FrameOutcome, RendererError> {
        Renderer::render(self, f)
    }
    fn invalidate_assets(&mut self) {
        self.pbr.invalidate_assets();
    }
}
