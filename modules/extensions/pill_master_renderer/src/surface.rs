//! The window surface and the swapchain lifecycle it carries.
//!
//! # Responsibilities
//!
//! - Build the GPU bootstrap in the only order it can happen: instance,
//!   surface, adapter, device, queue, configuration ([`Surface::create`]).
//! - Keep the surface configured - at startup, on a resize, and on the
//!   recovery path a lost or outdated swapchain needs - in vsync unless
//!   `PILL_PRESENT_MODE` asks for another present mode.
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
//! [`RawWindowData`] - the window's platform handles as plain data - and the
//! sizes that reach it are plain pixels. That is what lets a frontend on any
//! windowing crate (winit in the standalone host, tao in the editor) attach
//! this renderer, and what keeps a windowing crate out of this one.

// External crates
use pill_core::{info, warn};

// External crates
use pill_renderer_api::RawWindowData;

// Current crate
use crate::error::{captured, captured_now, CapturedErrors, RendererError, Result};

/// Environment variable that asks for a present mode other than vsync.
const PRESENT_MODE_VARIABLE: &str = "PILL_PRESENT_MODE";

/// The present mode `PILL_PRESENT_MODE` asks for, when the surface lists it.
///
/// `immediate` presents without waiting (tearing possible), `mailbox` without
/// waiting but replaces a queued frame instead of tearing, and `fifo` is
/// vsync, the default. `None` when the variable is unset, names no mode, or
/// names one the surface does not list, which is logged.
fn requested_present_mode(available: &[wgpu::PresentMode]) -> Option<wgpu::PresentMode> {
    let requested = std::env::var(PRESENT_MODE_VARIABLE).ok()?;
    let present_mode = match requested.trim().to_ascii_lowercase().as_str() {
        "immediate" => wgpu::PresentMode::Immediate,
        "mailbox" => wgpu::PresentMode::Mailbox,
        "fifo" => wgpu::PresentMode::Fifo,
        _ => {
            warn!(
                target: pill_core::telemetry::telemetry_target::RENDERING,
                "{PRESENT_MODE_VARIABLE}={requested} is not a present mode (immediate, mailbox, fifo); using Fifo"
            );
            return None;
        }
    };
    if !available.contains(&present_mode) {
        warn!(
            target: pill_core::telemetry::telemetry_target::RENDERING,
            "{PRESENT_MODE_VARIABLE} asked for {present_mode:?}, which this surface does not offer ({available:?}); using Fifo"
        );
        return None;
    }
    Some(present_mode)
}

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

/// The window's swapchain, its configuration, and the format pipelines draw
/// to it in.
///
/// Built once by [`Surface::create`] and reconfigured in place for the rest of
/// the renderer's life. The device it was configured against is passed in
/// rather than held, because the same device is what every other GPU object in
/// the renderer is created from and owning it here would say otherwise.
pub struct Surface {
    surface: wgpu::Surface<'static>,
    configuration: wgpu::SurfaceConfiguration,
    /// The sRGB format of the views drawn into, which may differ from the
    /// configured format only by its sRGB-ness; see [`Surface::create`].
    format: wgpu::TextureFormat,
    /// The graphics API the device runs on, for labelling GPU profiles.
    backend: wgpu::Backend,
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
    ///
    /// # Safety
    ///
    /// `window` must name a live window, and that window must outlive the
    /// returned surface: nothing here keeps it alive.
    pub async unsafe fn create(
        window: RawWindowData,
        width: u32,
        height: u32,
    ) -> Result<(Self, wgpu::Device, wgpu::Queue)> {
        // A zero-sized window still needs a surface it can configure; the first
        // resize replaces these.
        let width = width.max(1);
        let height = height.max(1);
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            // Vulkan, DX12, Metal or WebGPU, never OpenGL unless `WGPU_BACKEND`
            // asks for it. On Windows wgpu's GL backend leaves a hidden helper
            // window behind whose message handler is code in this DLL; once the
            // host unloads the renderer (a reload, or shutdown), the next message
            // to that window calls freed code and the process dies with
            // 0xC000041D / 0xC0000005. Every machine this renders on has one of
            // the primary backends, so GL only ever added that hazard.
            backends: wgpu::Backends::from_env().unwrap_or(wgpu::Backends::PRIMARY),
            // Debug labels on, the backend's validation layer off unless
            // `WGPU_VALIDATION=1` asks for it: that layer ships with the Vulkan
            // SDK, and without it every start warns that it is missing. wgpu's
            // own API validation runs either way.
            flags: wgpu::InstanceFlags::from_build_config()
                .difference(wgpu::InstanceFlags::VALIDATION)
                .with_env(),
            backend_options: wgpu::BackendOptions::default(),
        });
        let (raw_window_handle, raw_display_handle) = window.to_raw()?;
        // SAFETY: the handles name a live window that outlives the surface,
        // which is this function's own contract with its caller.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle,
                raw_window_handle,
            })
        }
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
        let backend = info.backend;
        info!(target: pill_core::telemetry::telemetry_target::RENDERING, "Using GPU: {} ({:?})", info.name, info.backend);
        let mut wanted = wgpu::Features::DEPTH_CLIP_CONTROL;
        // Only on request: the query features change nothing until used, but a
        // device asks only for what it needs. Occlusion queries need no
        // feature (they are core in wgpu).
        if crate::profiler::gpu_profiling_requested() {
            wanted |= crate::profiler::GPU_PROFILE_FEATURES;
        }
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
        crate::error::report_uncaptured_errors(&device);
        let capabilities = surface.get_capabilities(&adapter);
        let color_format =
            pick_color_format(&capabilities.formats).ok_or(RendererError::NoTextureFormats)?;
        // Pipelines draw through an sRGB view, so the hardware encodes the
        // linear colours they write. A surface that offers no sRGB format - a
        // WebGPU canvas never does - is configured in its plain format with
        // the sRGB variant allowed as a view format; drawing into the plain
        // format directly would put linear values on screen, far too dark.
        let render_format = color_format.add_srgb_suffix();
        let alpha_mode = capabilities
            .alpha_modes
            .first()
            .copied()
            .ok_or(RendererError::NoAlphaModes)?;
        // `Fifo` is the one present mode a surface is required to support, and
        // a mode listed by `Surface::get_capabilities` is not thereby
        // creatable: the NVIDIA Vulkan driver on Windows advertises `Mailbox`
        // and then fails the flip-model swapchain with "Not enough memory
        // left", which reaches wgpu's uncaptured-error path and aborts the host
        // before a `RendererError` can be constructed. An uncapped mode stays
        // an opt-in for a driver that has been verified to create one
        // (`PILL_PRESENT_MODE`, see `requested_present_mode`).
        let requested = requested_present_mode(&capabilities.present_modes);
        let present_mode = requested.unwrap_or(wgpu::PresentMode::Fifo);
        let mut configuration = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: color_format,
            width,
            height,
            desired_maximum_frame_latency: 2,
            present_mode,
            alpha_mode,
            view_formats: vec![render_format],
        };
        // A requested mode the driver refuses falls back to `Fifo` instead of
        // failing the renderer: the request is a development knob, and vsync
        // is always creatable.
        if let Err(error) = configure_first(&surface, &device, &mut configuration).await {
            if configuration.present_mode == wgpu::PresentMode::Fifo {
                return Err(error);
            }
            warn!(
                target: pill_core::telemetry::telemetry_target::RENDERING,
                "present mode {:?} was refused ({error}); falling back to Fifo", configuration.present_mode
            );
            configuration.present_mode = wgpu::PresentMode::Fifo;
            configure_first(&surface, &device, &mut configuration).await?;
        }
        info!(target: pill_core::telemetry::telemetry_target::RENDERING, "Present mode: {:?}", configuration.present_mode);
        Ok((
            Self {
                surface,
                configuration,
                format: render_format,
                backend,
            },
            device,
            queue,
        ))
    }

    /// The graphics API the device behind this surface runs on.
    pub fn backend(&self) -> wgpu::Backend {
        self.backend
    }

    /// The colour format every pipeline rendering to this surface declares,
    /// and the format of the view each frame is drawn through: always sRGB.
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
    /// The new size is applied through [`configure`] - with its `Opaque`
    /// fallback - and only committed once the surface accepted it, so a
    /// refusal leaves the old, consistent configuration in place instead of
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

/// Configure the surface for the first time, giving up the compositing alpha
/// mode if the driver refuses it.
///
/// `Surface::configure` returns nothing: a refused configuration is reported
/// through the device's uncaptured-error path, which panics by default and
/// takes the whole host down before any frontend sees a `RendererError`. A
/// refusal is realistic because a compositing alpha mode needs a compositing
/// window, which a capability list does not promise. The requested mode is
/// therefore tried inside its own error scopes, and `Opaque` - the mode every
/// surface must support - is the fallback. The scopes are awaited: the
/// renderer is being built asynchronously, so every target can wait for them.
async fn configure_first(
    surface: &wgpu::Surface<'static>,
    device: &wgpu::Device,
    surface_configuration: &mut wgpu::SurfaceConfiguration,
) -> Result<()> {
    let mut failures = Vec::new();
    for alpha_mode in alpha_mode_candidates(surface_configuration.alpha_mode) {
        surface_configuration.alpha_mode = alpha_mode;
        let (probe, errors) = captured(device, || {
            apply_and_probe(surface, device, surface_configuration)
        })
        .await;
        match attempt_failure(probe, errors) {
            None => {
                info!(target: pill_core::telemetry::telemetry_target::RENDERING, "Surface configured: {alpha_mode:?}");
                return Ok(());
            }
            Some(failure) => failures.push(format!("{alpha_mode:?} ({failure})")),
        }
    }
    Err(RendererError::SurfaceConfigurationRefused {
        detail: failures.join("; "),
    })
}

/// Reconfigure the surface during a frame (resize, a lost swapchain), with the
/// same `Opaque` fallback as [`configure_first`].
///
/// A frame cannot await, so the error scopes are blocked on only in a native
/// development build. Elsewhere a refusal reaches the device's error handler,
/// which logs it, and the frame probe still reports it here - the alpha mode
/// reused is the one the first configuration proved, so this is the rare
/// path.
fn configure(
    surface: &wgpu::Surface<'static>,
    device: &wgpu::Device,
    surface_configuration: &mut wgpu::SurfaceConfiguration,
) -> Result<()> {
    let mut failures = Vec::new();
    for alpha_mode in alpha_mode_candidates(surface_configuration.alpha_mode) {
        surface_configuration.alpha_mode = alpha_mode;
        let (probe, errors) = captured_now(device, || {
            apply_and_probe(surface, device, surface_configuration)
        });
        match attempt_failure(probe, errors) {
            None => return Ok(()),
            Some(failure) => failures.push(format!("{alpha_mode:?} ({failure})")),
        }
    }
    Err(RendererError::SurfaceConfigurationRefused {
        detail: failures.join("; "),
    })
}

/// Configure the surface and acquire a frame from it.
///
/// Acquiring a frame is what materialises the swapchain: wgpu-core accepting
/// the configuration does not mean the driver created one, and the refusal
/// only shows up here.
fn apply_and_probe(
    surface: &wgpu::Surface<'static>,
    device: &wgpu::Device,
    surface_configuration: &wgpu::SurfaceConfiguration,
) -> std::result::Result<wgpu::SurfaceTexture, wgpu::SurfaceError> {
    surface.configure(device, surface_configuration);
    surface.get_current_texture()
}

/// Why one configuration attempt failed, or `None` when it succeeded: a
/// captured error first, else the probe's own failure.
fn attempt_failure(
    probe: std::result::Result<wgpu::SurfaceTexture, wgpu::SurfaceError>,
    errors: CapturedErrors,
) -> Option<String> {
    let reported = errors
        .internal
        .or(errors.out_of_memory)
        .or(errors.validation)
        .map(|error| error.to_string());
    match probe {
        Ok(frame) => {
            // Dropped rather than presented: the frame loop acquires its own.
            drop(frame);
            reported
        }
        Err(error) => Some(reported.unwrap_or_else(|| error.to_string())),
    }
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
