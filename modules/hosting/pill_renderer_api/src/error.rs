//! Public renderer failures at the host boundary.
//!
//! # Responsibilities
//!
//! - Declare [`RendererError`], the failures the renderer reports while it is
//!   built and while frames are presented.
//! - Provide the [`Result`] alias and [`ErrorContext`], so a call site can
//!   attach a description of what it was doing to any failure beneath it.
//!
//! # Design
//!
//! The variants carry plain strings rather than `wgpu` error values: the host
//! links this crate in every posture and reports renderer failures without
//! naming a graphics type; the renderer module turns its wgpu errors into these. The `engine::renderer` namespace
//! is shared with the sibling renderer backends, so a diagnostic code keeps
//! its meaning whichever one a build links.

// External crates
use pill_core_macros::engine_error;

/// Failures the renderer reports to its host.
///
/// Covers the startup path - surface, adapter, device - and frame
/// acquisition, so the host handles one enum instead of the wgpu error types
/// behind it.
#[engine_error(namespace = engine::renderer, runtime = ::pill_core::error)]
pub enum RendererError {
    /// The GPU surface could not be created for the supplied window.
    #[message("failed to create the GPU surface: ", value(detail))]
    SurfaceCreation { detail: String },
    /// No compatible GPU adapter was found for the surface.
    #[message("failed to find a compatible GPU adapter: ", value(detail))]
    AdapterRequest { detail: String },
    /// The GPU device could not be created from the adapter.
    #[message("failed to create the GPU device: ", value(detail))]
    DeviceCreation { detail: String },
    /// The surface advertises no texture formats to render into.
    #[message("GPU surface exposes no texture formats")]
    NoTextureFormats,
    /// The surface advertises no alpha modes to present with.
    #[message("GPU surface exposes no alpha modes")]
    NoAlphaModes,
    /// The surface stayed lost or outdated even after one reconfiguration.
    #[message("GPU surface was lost")]
    SurfaceLost,
    /// The surface ran out of memory while providing a frame.
    #[message("GPU surface is out of memory")]
    SurfaceOutOfMemory,
    /// Acquiring the surface texture failed for a reason other than loss or
    /// out-of-memory, such as a timeout.
    #[message("failed to acquire the GPU surface texture: ", value(detail))]
    SurfaceTextureFailed { detail: String },
    /// Every candidate surface configuration was refused by the driver, with
    /// each refusal collected into `detail`.
    #[message(
        "no surface configuration was accepted by the GPU driver: ",
        value(detail)
    )]
    SurfaceConfigurationRefused { detail: String },
    /// A resource the call referenced - a camera, a shader - is not in
    /// storage.
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

/// The renderer's result alias: [`RendererError`] on the failure side.
pub type Result<T> = std::result::Result<T, RendererError>;

/// Turns any lower-level failure into a [`RendererError::Other`] carrying a
/// description of what the caller was doing.
///
/// Implemented for every `Result<T, E>` whose error implements `Display`, and
/// for `Option<T>`, so a call site can name its own operation - loading this
/// texture, building this pass - instead of the enum growing a variant per
/// operation.
pub trait ErrorContext<T> {
    /// Returns the value, or [`RendererError::Other`] holding `message` and,
    /// for a `Result`, the original error's text.
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
