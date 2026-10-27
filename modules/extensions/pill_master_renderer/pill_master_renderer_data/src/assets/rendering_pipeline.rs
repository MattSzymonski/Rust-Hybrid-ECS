//! An ordered list of passes, as one asset.
//!
//! # Responsibilities
//!
//! - Hold the passes a frame runs, in the order the game wrote them.
//! - Stay a handle list rather than a copy of the passes, so one pass can appear
//!   in several pipelines and editing a pass does not duplicate it.
//!
//! # Design
//!
//! This asset is only the declaration. Building the chain - ordering,
//! resolving target names, creating the pipelines the GPU runs - happens in the
//! renderer when the game hands this asset over through `RenderingManager`,
//! because that is the side that owns the device.
//!
//! It is also a file of its own (`.pipeline`): a chain the editor can create and
//! a project can load by path, whose body lists the `.render_pass` files in the
//! order they run.
//!
//! Nothing else belongs here. A pass already carries the shader it draws with
//! and the values it reads, so a pipeline that also listed its shaders and
//! textures would be saying the same thing twice - and would go stale the moment
//! a pass was edited. What an installer put in the store is the store's
//! business: `AssetManager::handle_by_name` is the index for it.

// External crates
use pill_engine::{
    pill_mirror_object, Asset, AssetLoadResult, AssetManager, AssetReference, Handle,
    StandaloneAsset,
};
use serde::{Deserialize, Serialize};

// Current crate
use crate::RenderPass;

/// The passes a frame runs, in the order they were added.
#[derive(Clone, Debug, Default)]
#[pill_mirror_object(asset)]
pub struct RenderingPipeline {
    /// Pass handles, in the order they run. A pass's own `order` key refines
    /// this when the renderer sorts the chain.
    pub passes: Vec<Handle<RenderPass>>,
}

impl RenderingPipeline {
    /// An empty pipeline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one pass.
    pub fn add(&mut self, pass: Handle<RenderPass>) {
        self.passes.push(pass);
    }

    /// Append one pass, for chaining.
    pub fn with_pass(mut self, pass: Handle<RenderPass>) -> Self {
        self.passes.push(pass);
        self
    }
}

/// The pinned shared name of [`RenderingPipeline`]; see the comment on its `Asset` impl.
const RENDERING_PIPELINE_SHARED_NAME: &str = "pill_master_renderer::assets::RenderingPipeline";

// Shared across binaries: the data module, the GPU module and every project
// compile their own copy of this crate, each with its own `TypeId`. The pinned
// name makes them one asset column (see `Asset::shared_name`); keep it
// verbatim when moving the type.
impl Asset for RenderingPipeline {
    fn shared_name() -> Option<&'static str> {
        Some(RENDERING_PIPELINE_SHARED_NAME)
    }

    fn shared_identity() -> Option<u128> {
        // A `const`, so the name is hashed at compile time. The default hashes
        // it on every call, and every `AssetManager` lookup makes that call:
        // the renderer does it several times per drawn entity, every frame.
        const IDENTITY: u128 =
            pill_engine::component::shared_component_identity(RENDERING_PIPELINE_SHARED_NAME);
        Some(IDENTITY)
    }
}

/// The pipeline as its file holds it: the passes, in the order they run.
///
/// Each entry names a `.render_pass` file by guid, so a chain outlives a pass
/// being moved or renamed and one pass can appear in several chains.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderingPipelineDocument {
    /// The passes, in the order they run.
    pub passes: Vec<AssetReference<RenderPass>>,
}

// The metadata type name defaults to the shared name above, so every binary's
// copy of `RenderingPipeline` reads the same files.
impl StandaloneAsset for RenderingPipeline {
    type Document = RenderingPipelineDocument;
    const FILE_EXTENSION: &'static str = "pipeline";

    fn from_document(
        _name: &str,
        document: RenderingPipelineDocument,
        assets: &AssetManager,
    ) -> AssetLoadResult<Self> {
        // A reference that resolves to nothing stays an invalid handle, exactly
        // as a material's does: the chain the renderer builds skips a pass it
        // cannot reach rather than refusing the whole file.
        Ok(Self {
            passes: document
                .passes
                .iter()
                .map(|pass| pass.resolve(assets))
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_keep_the_order_they_were_added() {
        let first = Handle::from_raw(0, 1);
        let second = Handle::from_raw(1, 1);
        let mut pipeline = RenderingPipeline::new().with_pass(first);

        pipeline.add(second);

        assert_eq!(pipeline.passes, vec![first, second]);
    }

    #[test]
    fn an_empty_pipeline_has_no_passes() {
        assert!(RenderingPipeline::new().passes.is_empty());
    }

    /// A chain's file names its passes by guid, and the document resolves them
    /// back to the handles the renderer wants, in the order they are listed.
    #[test]
    fn a_document_resolves_its_passes_in_order() {
        let mut assets = AssetManager::new();
        let first = assets
            .add_named_with_guid(
                "first",
                pill_engine::AssetGuid::from_name("first"),
                RenderPass::new("first"),
            )
            .expect("a free name");
        let second = assets
            .add_named_with_guid(
                "second",
                pill_engine::AssetGuid::from_name("second"),
                RenderPass::new("second"),
            )
            .expect("a free name");

        let document = RenderingPipelineDocument {
            passes: vec![
                AssetReference::from_handle(&assets, second).expect("a guid to name it by"),
                AssetReference::from_handle(&assets, first).expect("a guid to name it by"),
            ],
        };
        let pipeline =
            RenderingPipeline::from_document("pipelines/main.pipeline", document, &assets)
                .expect("the passes resolve");

        assert_eq!(pipeline.passes, vec![second, first]);
    }
}
