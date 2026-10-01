//! Renderer assets now live directly in the engine world's `AssetManager`.
//!
//! # Responsibilities
//!
//! - Hold the native-assets handle a windowed host keeps beside the engine,
//!   and its per-frame update hook.

// Standard library
use std::path::Path;

// External crates
use pill_renderer_api::RendererError;

/// Handle for native renderer assets owned by a windowed host.
///
/// Empty by design: the assets themselves are registered into the engine's
/// `AssetManager`, and this type keeps the host's field layout independent of
/// how they were prepared.
pub struct NativeAssets;

impl NativeAssets {
    /// Prepare native assets, given the project directory when the host has
    /// one (a development host does; a shipping build has none).
    ///
    /// # Errors
    ///
    /// Never today; returns a [`RendererError`] once preparation can fail.
    pub fn prepare(_project: Option<&Path>) -> Result<Self, RendererError> {
        Ok(Self)
    }

    /// Per-frame update hook; a no-op until native assets need one.
    ///
    /// # Errors
    ///
    /// Never today; returns a [`RendererError`] once an update can fail.
    pub fn update(&mut self, _engine: &mut pill_engine::Engine) -> Result<(), RendererError> {
        Ok(())
    }
}
