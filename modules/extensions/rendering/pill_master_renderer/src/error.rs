//! Public renderer failures at the host boundary.

use pill_core_macros::engine_error;

#[engine_error(namespace = engine::renderer, runtime = ::pill_core::error)]
pub enum RendererError {
    #[message("failed to create the GPU surface: ", value(detail))]
    SurfaceCreation { detail: String },
    #[message("failed to find a compatible GPU adapter: ", value(detail))]
    AdapterRequest { detail: String },
    #[message("failed to create the GPU device: ", value(detail))]
    DeviceCreation { detail: String },
    #[message("GPU surface exposes no texture formats")]
    NoTextureFormats,
    #[message("GPU surface exposes no alpha modes")]
    NoAlphaModes,
    #[message("GPU surface was lost")]
    SurfaceLost,
    #[message("GPU surface is out of memory")]
    SurfaceOutOfMemory,
    #[message("failed to acquire the GPU surface texture: ", value(detail))]
    SurfaceTextureFailed { detail: String },
    #[message(
        "no surface configuration was accepted by the GPU driver: ",
        value(detail)
    )]
    SurfaceConfigurationRefused { detail: String },
    #[message("renderer resource was not found")]
    RendererResourceNotFound,
    /// An operation the caller described in its own words: the pass that reads a
    /// target nobody writes, the texture that is not loaded, the shader wgpu
    /// refused.
    ///
    /// No generic prefix, because the caller already says what it was doing -
    /// `Pass <name> is not drawn: <this>` - and repeating it reads as a stutter.
    #[message(value(detail))]
    Other { detail: String },
}

pub type Result<T> = std::result::Result<T, RendererError>;

pub trait ErrorContext<T> {
    fn context(self, message: impl Into<String>) -> Result<T>;
}

impl<T, E: std::fmt::Display> ErrorContext<T> for std::result::Result<T, E> {
    fn context(self, message: impl Into<String>) -> Result<T> {
        let message = message.into();
        self.map_err(|error| RendererError::Other {
            detail: format!("{message}: {error}"),
        })
    }
}

impl<T> ErrorContext<T> for Option<T> {
    fn context(self, message: impl Into<String>) -> Result<T> {
        self.ok_or_else(|| RendererError::Other {
            detail: message.into(),
        })
    }
}

/// Run a block of GPU work with wgpu's errors captured rather than delivered to
/// its uncaptured-error handler.
///
/// wgpu reports a refused pipeline, bind group or texture through that handler,
/// which panics by default and takes the host down with it. There is no return
/// value to check, so a shader or a pass the driver will not accept arrives as a
/// crash rather than as a mistake to report. Capturing the errors here turns
/// them into a message the caller can attach to whatever asked for the work -
/// the pass, the shader, the material - which is the difference between "the
/// host died" and "this pass is not drawn, and here is why".
pub(crate) fn capturing_validation<T>(
    device: &wgpu::Device,
    make: impl FnOnce() -> T,
) -> std::result::Result<T, String> {
    device.push_error_scope(wgpu::ErrorFilter::Validation);
    device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let value = make();

    // Scopes are a stack: the one pushed last is the one popped first.
    let out_of_memory = pollster::block_on(device.pop_error_scope());
    let validation = pollster::block_on(device.pop_error_scope());

    match validation.or(out_of_memory) {
        Some(error) => Err(error.to_string()),
        None => Ok(value),
    }
}
