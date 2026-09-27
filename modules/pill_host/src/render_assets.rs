//! Renderer assets now live directly in the engine world's `AssetManager`.
//!
//! # Responsibilities
//!
//! - Keep the `pill_host` side of renderer asset preparation compiling in both
//!   postures: the reloading build resolves project paths, the shipping build
//!   ignores them.
//! - Hold the native-assets handle the host stores beside the engine.

use pill_master_renderer::RendererError;
use std::path::Path;

/// Handle for native renderer assets owned by the host.
///
/// Empty by design: the assets themselves are registered into the engine's
/// `AssetManager`, and this type keeps the host's field layout independent of
/// which posture prepared them.
pub(crate) struct NativeAssets;

impl NativeAssets {
    /// Prepare native assets for a reloading build.
    ///
    /// Takes the project and workspace paths so the signature stays stable if
    /// preparation starts needing them; the reloading posture is the one that
    /// knows both.
    #[cfg(feature = "hot_reload")]
    pub fn prepare(_project: Option<&Path>, _workspace: &Path) -> Result<Self, RendererError> {
        Ok(Self)
    }

    /// Prepare native assets for a shipping build.
    ///
    /// The shipping posture has no reloading workspace, so only the project
    /// path is accepted.
    #[cfg(not(feature = "hot_reload"))]
    pub fn prepare(_project: Option<&Path>) -> Result<Self, RendererError> {
        Ok(Self)
    }

    /// Per-frame update hook; a no-op until native assets need one.
    pub fn update(&mut self, _engine: &mut pill_engine::Engine) -> Result<(), RendererError> {
        Ok(())
    }
}
