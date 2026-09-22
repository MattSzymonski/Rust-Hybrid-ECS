//! Renderer assets now live directly in the engine world's `AssetManager`.

use pill_master_renderer::RendererError;
use std::path::Path;

pub(crate) struct NativeAssets;

impl NativeAssets {
    #[cfg(feature = "hot_reload")]
    pub fn prepare(_project: Option<&Path>, _workspace: &Path) -> Result<Self, RendererError> {
        Ok(Self)
    }

    #[cfg(not(feature = "hot_reload"))]
    pub fn prepare(_project: Option<&Path>) -> Result<Self, RendererError> {
        Ok(Self)
    }

    pub fn update(&mut self, _engine: &mut pill_engine::Engine) -> Result<(), RendererError> {
        Ok(())
    }
}
