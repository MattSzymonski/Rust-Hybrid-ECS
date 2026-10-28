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
//! naming a graphics type, while the renderer module turns its wgpu errors into
//! these. The `engine::renderer` namespace is shared with the other renderer
//! backends, so a failure's diagnostic code names the same condition whichever
//! backend a build links.

// External crates
use pill_core_macros::engine_error;

/// Failures the renderer reports to its host.
///
/// Only the failures a host or frontend acts on are variants of their own; the
/// backend's own startup and swapchain failures arrive as [`Self::Backend`],
/// with the GPU module's text, so a new way for a device to fail needs no edit
/// here.
#[engine_error(namespace = engine::renderer, runtime = ::pill_core::error)]
pub enum RendererError {
    /// The GPU surface could not be created for the supplied window.
    #[message("failed to create the GPU surface: ", value(detail))]
    SurfaceCreation { detail: String },
    /// The GPU backend failed: no adapter or no device, no format or alpha mode
    /// to present with, a configuration the driver refused, or a swapchain that
    /// stayed lost or gave no texture.
    ///
    /// The renderer module keeps its own error type for these - the wgpu status
    /// and its finer-grained codes stay there - and reports them here as the one
    /// thing its host acts on: the backend did not start, or did not hand this
    /// frame a surface.
    #[message("the GPU backend failed: ", value(detail))]
    Backend { detail: String },
    /// The renderer cannot draw into another surface, or not into this one.
    #[message("the renderer cannot draw into this surface: ", value(detail))]
    UnsupportedSurface { detail: String },

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
/// for `Option<T>`, so a call site names its own operation - loading this
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
