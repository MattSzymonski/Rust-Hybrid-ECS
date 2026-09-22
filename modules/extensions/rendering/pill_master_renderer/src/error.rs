//! Rendering failures reported by the wgpu backend.
//!
//! # Responsibilities
//!
//! - Declares [`RendererError`], covering surface, adapter, device and
//!   frame-acquisition failures.
//!
//! # Design
//!
//! Backend errors are converted to owned descriptions so the public host
//! contract compiles without GPU dependencies.
//!
//! The `engine::renderer` namespace is deliberately unchanged from when this
//! enum lived in the engine, so the diagnostic codes users may already have
//! seen keep meaning the same thing.

// External crates
use pill_core_macros::engine_error;

// =============================================================================
// Renderer Errors
// =============================================================================

/// Rendering initialization or presentation failures of the wgpu backend.
#[engine_error(namespace = engine::renderer, runtime = ::pill_core::error)]
pub enum RendererError {
    /// Cooked asset preparation or loading failed before a usable frame could be built.
    #[message("render assets failed: ", value(detail))]
    Assets { detail: String },
    /// The GPU surface could not be created for the supplied window.
    #[message("failed to create the GPU surface: ", value(detail))]
    SurfaceCreation { detail: String },

    /// No compatible GPU adapter could be found.
    #[message("failed to find a compatible GPU adapter: ", value(detail))]
    AdapterRequest { detail: String },

    /// The GPU device could not be created from the adapter.
    #[message("failed to create the GPU device: ", value(detail))]
    DeviceCreation { detail: String },

    /// The surface exposes no texture formats.
    #[message("GPU surface exposes no texture formats")]
    NoTextureFormats,

    /// The surface exposes no alpha modes.
    #[message("GPU surface exposes no alpha modes")]
    NoAlphaModes,

    /// The frame texture could not be acquired for a fatal reason.
    #[message("failed to acquire the GPU surface texture: ", value(detail))]
    SurfaceTextureFailed { detail: String },
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    // `code()` comes from the diagnostic trait `#[engine_error]` implements.
    use miette::Diagnostic as _;

    /// Renderer unit variants produce their diagnostic code.
    ///
    /// The code is derived from the namespace and the variant name, so moving
    /// this enum out of `pill_engine` must not change what a user sees.
    #[test]
    fn renderer_error_code_derives_from_namespace_and_variant() {
        assert_eq!(
            RendererError::NoTextureFormats
                .code()
                .map(|code| code.to_string())
                .as_deref(),
            Some("engine::renderer::no_texture_formats")
        );
    }
}
