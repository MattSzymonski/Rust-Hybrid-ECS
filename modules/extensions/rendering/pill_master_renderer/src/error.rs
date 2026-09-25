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
    #[message("no surface configuration was accepted by the GPU driver: ", value(detail))]
    SurfaceConfigurationRefused { detail: String },
    #[message("renderer resource was not found")]
    RendererResourceNotFound,
    #[message("renderer operation failed: ", value(detail))]
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
