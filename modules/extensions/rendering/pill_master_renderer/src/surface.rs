//! The window surface and the swapchain lifecycle it carries.
//!
//! # Responsibilities
//!
//! - Build the GPU bootstrap in the only order it can happen: instance,
//!   surface, adapter, device, queue, configuration ([`Surface::create`]).
//! - Keep the surface configured - at startup, on a resize, and on the
//!   recovery path a lost or outdated swapchain needs.
//! - Hand out frames ([`Surface::acquire`]), and own the colour format every
//!   pipeline that renders to it declares.
//!
//! # Design
//!
//! The surface, the device and the queue cannot be built independently: adapter
//! selection needs a compatible surface, device creation needs the adapter, and
//! the surface cannot be configured until a device exists. [`Surface::create`]
//! therefore returns all three. It owns what is surface state - the swapchain,
//! its configuration, and the format chosen from the surface's capabilities -
//! and takes the device as an argument wherever it needs one, so a type named
//! for the surface does not quietly become the device's owner.
//!
//! Nothing here names a windowing crate: the surface is built from a
//! [`RendererWindow`], and the sizes that reach it are plain pixels.

// External crates
use pill_core::info;

// Current crate
use crate::error::{RendererError, Result};

/// A window the renderer can build its surface from.
///
/// Blanket-implemented for every wgpu window handle, so a frontend hands the
/// renderer its own window type without naming wgpu's.
pub trait RendererWindow: wgpu::WindowHandle {}
impl<T> RendererWindow for T where T: wgpu::WindowHandle {}

/// The colour format every pipeline rendering to this surface declares.
///
/// Preference order taken from the reference: an sRGB target where the surface
/// offers one, Rgba ahead of Bgra because it needs no channel swizzle, and
/// finally whatever the surface does advertise rather than refusing to start.
fn pick_color_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    [
        wgpu::TextureFormat::Rgba8UnormSrgb,
        wgpu::TextureFormat::Bgra8UnormSrgb,
        wgpu::TextureFormat::Bgra8Unorm,
    ]
    .into_iter()
    .find(|format| formats.contains(format))
    .or_else(|| formats.first().copied())
}

/// The alpha mode to ask a new surface for.
///
/// A compositing mode first, because the editor paints its panels over the same
/// window the scene viewport lives in: that needs the swapchain image to carry
/// alpha, and a window created transparent can be given one. The renderer's own
/// shaders write opaque pixels, so a window that has nothing to show through
/// stays opaque either way, and [`configure`] falls back to `Opaque` by itself
/// when the compositor refuses.
fn pick_alpha_mode(modes: &[wgpu::CompositeAlphaMode]) -> Option<wgpu::CompositeAlphaMode> {
    [
        wgpu::CompositeAlphaMode::PreMultiplied,
        wgpu::CompositeAlphaMode::PostMultiplied,
    ]
    .into_iter()
    .find(|mode| modes.contains(mode))
    .or_else(|| modes.first().copied())
}

/// The alpha modes to try, in order: the requested one, then `Opaque`.
///
/// A compositing alpha mode needs a compositing window, which a capability list
/// does not promise; `Opaque` is the mode every surface must support, so it is
/// the fallback rather than a second guess. `Opaque` is not retried against
/// itself.
fn alpha_mode_candidates(requested: wgpu::CompositeAlphaMode) -> Vec<wgpu::CompositeAlphaMode> {
    let mut modes = vec![requested];
    if requested != wgpu::CompositeAlphaMode::Opaque {
        modes.push(wgpu::CompositeAlphaMode::Opaque);
    }
    modes
}

/// The window's swapchain, its configuration, and the format it was chosen for.
///
/// Built once by [`Surface::create`] and reconfigured in place for the rest of
/// the renderer's life. The device it was configured against is passed in
/// rather than held, because the same device is what every other GPU object in
/// the renderer is created from and owning it here would say otherwise.
pub struct Surface {
    surface: wgpu::Surface<'static>,
    configuration: wgpu::SurfaceConfiguration,
    format: wgpu::TextureFormat,
}

impl Surface {
    /// Builds the GPU bootstrap: instance, surface, adapter, device and queue.
    ///
    /// Returns the surface beside the device and queue it was configured with,
    /// because the three cannot be created separately - adapter selection needs
    /// a compatible surface, and the surface cannot be configured before there
    /// is a device to configure it on.
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`] when a creation step fails: surface or
    /// adapter acquisition, device creation, a surface advertising no texture
    /// format or alpha mode, or a configuration every candidate was refused
    /// for.
    pub async fn create<W: RendererWindow + 'static>(
        window: W,
        width: u32,
        height: u32,
    ) -> Result<(Self, wgpu::Device, wgpu::Queue)> {
        // A zero-sized window still needs a surface it can configure; the first
        // resize replaces these.
        let width = width.max(1);
        let height = height.max(1);
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            flags: wgpu::InstanceFlags::from_build_config().with_env(),
            backend_options: wgpu::BackendOptions::default(),
        });
        let surface =
            instance
                .create_surface(window)
                .map_err(|error| RendererError::SurfaceCreation {
                    detail: error.to_string(),
                })?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|error| RendererError::AdapterRequest {
                detail: error.to_string(),
            })?;
        let info = adapter.get_info();
        info!(target: pill_core::telemetry::telemetry_target::RENDERING, "Using GPU: {} ({:?})", info.name, info.backend);
        let wanted = wgpu::Features::DEPTH_CLIP_CONTROL;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("pill renderer device"),
                required_features: wanted & adapter.features(),
                required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::default(),
            })
            .await
            .map_err(|error| RendererError::DeviceCreation {
                detail: error.to_string(),
            })?;
        let capabilities = surface.get_capabilities(&adapter);
        let color_format =
            pick_color_format(&capabilities.formats).ok_or(RendererError::NoTextureFormats)?;
        let alpha_mode =
            pick_alpha_mode(&capabilities.alpha_modes).ok_or(RendererError::NoAlphaModes)?;
        // `Fifo` is the one present mode a surface is required to support, and
        // a mode listed by `Surface::get_capabilities` is not thereby
        // creatable: the NVIDIA Vulkan driver on Windows advertises `Mailbox`
        // and then fails the flip-model swapchain with "Not enough memory
        // left", which reaches wgpu's uncaptured-error path and aborts the host
        // before a `RendererError` can be constructed. An uncapped mode stays
        // an opt-in for a driver that has been verified to create one.
        let present_mode = wgpu::PresentMode::Fifo;
        println!("[render] Present mode: {present_mode:?}");
        let mut configuration = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: color_format,
            width,
            height,
            desired_maximum_frame_latency: 2,
            present_mode,
            alpha_mode,
            view_formats: vec![color_format],
        };
        configure(&surface, &device, &mut configuration)?;
        Ok((
            Self {
                surface,
                configuration,
                format: color_format,
            },
            device,
            queue,
        ))
    }

    /// The colour format every pipeline rendering to this surface declares.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// The configured size, in physical pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.configuration.width, self.configuration.height)
    }

    /// Whether the swapchain image's alpha reaches the compositor.
    ///
    /// This is what decides whether a transparent pixel means "leave this pixel
    /// to whatever else paints this window" or is simply opaque black: only a
    /// compositing mode carries alpha out of the image. [`configure`] records
    /// the mode the surface actually accepted, so an `Opaque` fallback is
    /// visible here rather than assumed away.
    pub fn composites(&self) -> bool {
        matches!(
            self.configuration.alpha_mode,
            wgpu::CompositeAlphaMode::PreMultiplied | wgpu::CompositeAlphaMode::PostMultiplied
        )
    }

    /// The whole configuration, for resources sized alongside the surface.
    pub fn configuration(&self) -> &wgpu::SurfaceConfiguration {
        &self.configuration
    }

    /// Reconfigure for a new size.
    ///
    /// The new size is applied through [`configure`] - scoped, with its
    /// `Opaque` fallback - and only committed once the surface accepted it, so
    /// a refusal leaves the old, consistent configuration in place instead of
    /// one that disagrees with the swapchain.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::SurfaceConfigurationRefused`] when every
    /// candidate configuration was refused, with each refusal collected.
    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) -> Result<()> {
        let mut configuration = self.configuration.clone();
        configuration.width = width;
        configuration.height = height;
        configure(&self.surface, device, &mut configuration)?;
        self.configuration = configuration;
        Ok(())
    }

    /// Acquire the next frame, recovering a lost or outdated swapchain once.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::SurfaceOutOfMemory`] when the surface is out of
    /// memory, [`RendererError::SurfaceLost`] when it is still lost or outdated
    /// after one reconfiguration, and [`RendererError::SurfaceTextureFailed`]
    /// for anything else.
    pub fn acquire(&mut self, device: &wgpu::Device) -> Result<wgpu::SurfaceTexture> {
        match self.surface.get_current_texture() {
            Ok(frame) => Ok(frame),
            // A lost or outdated swapchain is recovered by reconfiguring and
            // retrying once; reporting it and returning left the renderer dead
            // until some later resize event happened to arrive.
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.reconfigure(device)?;
                self.surface
                    .get_current_texture()
                    .map_err(|error| match error {
                        wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                            RendererError::SurfaceLost
                        }
                        wgpu::SurfaceError::OutOfMemory => RendererError::SurfaceOutOfMemory,
                        other => RendererError::SurfaceTextureFailed {
                            detail: other.to_string(),
                        },
                    })
            }
            Err(wgpu::SurfaceError::OutOfMemory) => Err(RendererError::SurfaceOutOfMemory),
            Err(other) => Err(RendererError::SurfaceTextureFailed {
                detail: other.to_string(),
            }),
        }
    }

    /// Reconfigure the surface from its current settings.
    ///
    /// The recovery path for a lost or outdated swapchain: the configuration is
    /// still what the window wants, only the platform-side surface needs
    /// recreating.
    fn reconfigure(&mut self, device: &wgpu::Device) -> Result<()> {
        configure(&self.surface, device, &mut self.configuration)
    }
}

/// Configure the surface, giving up the compositing alpha mode if the driver
/// refuses it.
///
/// `Surface::configure` returns nothing: a refused configuration is reported
/// through the device's uncaptured-error path, which panics by default and
/// takes the whole host down before any frontend sees a `RendererError`. A
/// refusal is realistic because a compositing alpha mode needs a compositing
/// window, which a capability list does not promise. The requested mode is
/// therefore tried inside its own error scopes, and `Opaque` - the mode every
/// surface must support - is the fallback.
fn configure(
    surface: &wgpu::Surface<'static>,
    device: &wgpu::Device,
    surface_configuration: &mut wgpu::SurfaceConfiguration,
) -> Result<()> {
    let requested_alpha_mode = surface_configuration.alpha_mode;
    let mut failures = Vec::new();
    for alpha_mode in alpha_mode_candidates(requested_alpha_mode) {
        surface_configuration.alpha_mode = alpha_mode;

        // One scope per filter class, so a refusal is captured here rather than
        // reaching the uncaptured-error handler.
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        device.push_error_scope(wgpu::ErrorFilter::Internal);
        surface.configure(device, surface_configuration);
        // Acquiring a frame is what materialises the swapchain: wgpu-core
        // accepting the configuration does not mean the driver created one, and
        // the refusal only shows up here.
        let probe = surface.get_current_texture();
        let internal = pollster::block_on(device.pop_error_scope());
        let out_of_memory = pollster::block_on(device.pop_error_scope());
        let validation = pollster::block_on(device.pop_error_scope());

        let reported = internal
            .or(out_of_memory)
            .or(validation)
            .map(|error| error.to_string());
        let failure = match probe {
            Ok(frame) => {
                // Dropped rather than presented: the frame loop acquires its own.
                drop(frame);
                reported
            }
            Err(error) => Some(reported.unwrap_or_else(|| error.to_string())),
        };

        match failure {
            None => {
                println!("[render] Surface configured: {alpha_mode:?}");
                return Ok(());
            }
            Some(failure) => failures.push(format!("{alpha_mode:?} ({failure})")),
        }
    }

    Err(RendererError::SurfaceConfigurationRefused {
        detail: failures.join("; "),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_srgb_format_is_preferred_where_the_surface_offers_one() {
        assert_eq!(
            pick_color_format(&[
                wgpu::TextureFormat::Bgra8Unorm,
                wgpu::TextureFormat::Bgra8UnormSrgb,
            ]),
            Some(wgpu::TextureFormat::Bgra8UnormSrgb)
        );
    }

    #[test]
    fn rgba_is_preferred_over_bgra_because_it_needs_no_swizzle() {
        assert_eq!(
            pick_color_format(&[
                wgpu::TextureFormat::Bgra8UnormSrgb,
                wgpu::TextureFormat::Rgba8UnormSrgb,
            ]),
            Some(wgpu::TextureFormat::Rgba8UnormSrgb)
        );
    }

    #[test]
    fn a_surface_offering_none_of_the_preferred_formats_still_starts() {
        assert_eq!(
            pick_color_format(&[wgpu::TextureFormat::Rgb10a2Unorm]),
            Some(wgpu::TextureFormat::Rgb10a2Unorm),
            "the first advertised format beats refusing to start"
        );
        assert_eq!(pick_color_format(&[]), None, "no formats is an error");
    }

    #[test]
    fn a_compositing_alpha_mode_is_preferred_where_the_surface_offers_one() {
        assert_eq!(
            pick_alpha_mode(&[
                wgpu::CompositeAlphaMode::Opaque,
                wgpu::CompositeAlphaMode::Auto,
                wgpu::CompositeAlphaMode::PreMultiplied,
            ]),
            Some(wgpu::CompositeAlphaMode::PreMultiplied),
            "the scene shares its window with the editor's panels, so the image has to carry alpha"
        );
        assert_eq!(
            pick_alpha_mode(&[
                wgpu::CompositeAlphaMode::Auto,
                wgpu::CompositeAlphaMode::PostMultiplied,
            ]),
            Some(wgpu::CompositeAlphaMode::PostMultiplied),
            "the other compositing mode is taken when it is the only one on offer"
        );
        assert_eq!(
            pick_alpha_mode(&[wgpu::CompositeAlphaMode::Opaque]),
            Some(wgpu::CompositeAlphaMode::Opaque)
        );
        assert_eq!(
            pick_alpha_mode(&[]),
            None,
            "no alpha modes advertised is an error, not a mode"
        );
    }

    #[test]
    fn opaque_is_the_only_alpha_fallback_and_is_not_retried_against_itself() {
        assert_eq!(
            alpha_mode_candidates(wgpu::CompositeAlphaMode::Auto),
            [
                wgpu::CompositeAlphaMode::Auto,
                wgpu::CompositeAlphaMode::Opaque,
            ]
        );
        assert_eq!(
            alpha_mode_candidates(wgpu::CompositeAlphaMode::Opaque),
            [wgpu::CompositeAlphaMode::Opaque]
        );
    }
}
