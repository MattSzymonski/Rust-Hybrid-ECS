//! An ordered list of passes, as one asset.
//!
//! # Responsibilities
//!
//! - Hold the passes a frame runs, in the order the game wrote them, each with
//!   the chain's own switch for it ([`PipelinePass`]).
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
//! order they run. Each entry carries an optional `enabled` key, so one chain can
//! leave a pass out while another that names the same pass runs it - which is
//! what the pass's own `enabled` key cannot express, since it is the same for
//! every chain.
//!
//! Nothing else belongs here. A pass already carries the shader it draws with
//! and the values it reads, so a pipeline that also listed its shaders and
//! textures would be saying the same thing twice - and would go stale the moment
//! a pass was edited. What an installer put in the store is the store's
//! business: `AssetManager::handle_by_name` is the index for it.

// External crates
use pill_engine::{
    pill_mirror_object, AssetLoadResult, AssetManager, AssetReference, Handle, PillMirror,
    StandaloneAsset,
};
use serde::{Deserialize, Serialize};

// Current crate
use crate::RenderPass;

/// One pass in a pipeline, with this chain's switch for it.
///
/// The switch is per entry rather than per pass asset: the same `.render_pass`
/// can appear in two chains, and a project may want bloom in one and not the
/// other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PillMirror)]
pub struct PipelinePass {
    /// The pass this entry runs.
    pub pass: Handle<RenderPass>,
    /// Whether the chain runs it. A `.pipeline` file writes it per entry; a
    /// chain built in code runs every pass it adds.
    pub enabled: bool,
}

impl PipelinePass {
    /// `pass`, run by the chain that holds it.
    pub fn new(pass: Handle<RenderPass>) -> Self {
        Self {
            pass,
            enabled: true,
        }
    }
}

/// The passes a frame runs, in the order they were added.
#[derive(Clone, Debug)]
#[pill_mirror_object(asset, shared = "pill_master_renderer::assets::RenderingPipeline")]
pub struct RenderingPipeline {
    /// The entries, in the order they run. A pass's own `order` key refines this
    /// when the renderer sorts the chain.
    pub passes: Vec<PipelinePass>,
}

impl Default for RenderingPipeline {
    /// An empty chain; both derived defaults agree.
    fn default() -> Self {
        Self { passes: Vec::new() }
    }
}

impl RenderingPipeline {
    /// An empty pipeline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one pass, run by this chain.
    pub fn add(&mut self, pass: Handle<RenderPass>) {
        self.passes.push(PipelinePass::new(pass));
    }

    /// Append one pass, for chaining.
    pub fn with_pass(mut self, pass: Handle<RenderPass>) -> Self {
        self.passes.push(PipelinePass::new(pass));
        self
    }
}

/// One entry as a `.pipeline` file holds it: the pass it names, and whether
/// this chain runs it.
///
/// The pass is a reference, so a chain outlives a pass being moved or renamed
/// and one pass can appear in several chains. Its fields sit beside `enabled`
/// rather than under a key of their own, which is what makes an entry read
/// `{ "guid": …, "enabled": false }`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelinePassDocument {
    /// The pass this entry runs, by guid.
    #[serde(flatten)]
    pub pass: AssetReference<RenderPass>,
    /// Whether the chain runs it. A file that leaves the key out runs the pass,
    /// so an older file keeps drawing what it always did.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

impl Default for PipelinePassDocument {
    fn default() -> Self {
        Self {
            pass: AssetReference::unset(),
            enabled: enabled_by_default(),
        }
    }
}

/// `enabled`'s default, named so the serde attribute and [`Default`] agree.
fn enabled_by_default() -> bool {
    true
}

/// The pipeline as its file holds it: the passes, in the order they run.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderingPipelineDocument {
    /// The passes, in the order they run.
    pub passes: Vec<PipelinePassDocument>,
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
                .map(|entry| PipelinePass {
                    pass: entry.pass.resolve(assets),
                    enabled: entry.enabled,
                })
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

        assert_eq!(
            pipeline
                .passes
                .iter()
                .map(|entry| entry.pass)
                .collect::<Vec<_>>(),
            vec![first, second]
        );
    }

    #[test]
    fn an_empty_pipeline_has_no_passes() {
        assert!(RenderingPipeline::new().passes.is_empty());
    }

    /// A chain built in code runs every pass it adds; only a file switches an
    /// entry off.
    #[test]
    fn an_added_pass_is_on_unless_the_file_says_otherwise() {
        let pass = Handle::from_raw(0, 1);

        let pipeline = RenderingPipeline::new().with_pass(pass);

        assert!(pipeline.passes[0].enabled);
    }

    /// The entry's key is optional, and leaving it out runs the pass so an older
    /// pipeline file draws what it always did.
    #[test]
    fn a_document_entry_reads_the_enabled_key_and_defaults_it_on() {
        let omitted: PipelinePassDocument =
            serde_json::from_str(r#"{ "guid": "0f5e2c7a9b1d34c8a6e40b52d7c19f83" }"#)
                .expect("an entry without the key parses");
        let off: PipelinePassDocument = serde_json::from_str(
            r#"{ "guid": "0f5e2c7a9b1d34c8a6e40b52d7c19f83", "enabled": false }"#,
        )
        .expect("an entry with the key parses");

        assert!(omitted.enabled);
        assert!(!off.enabled);
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
                PipelinePassDocument {
                    pass: AssetReference::from_handle(&assets, second)
                        .expect("a guid to name it by"),
                    enabled: true,
                },
                PipelinePassDocument {
                    pass: AssetReference::from_handle(&assets, first)
                        .expect("a guid to name it by"),
                    enabled: false,
                },
            ],
        };
        let pipeline =
            RenderingPipeline::from_document("pipelines/main.pipeline", document, &assets)
                .expect("the passes resolve");

        assert_eq!(
            pipeline
                .passes
                .iter()
                .map(|entry| entry.pass)
                .collect::<Vec<_>>(),
            vec![second, first]
        );
        assert!(pipeline.passes[0].enabled);
        assert!(
            !pipeline.passes[1].enabled,
            "the entry's own switch reaches the loaded chain"
        );
    }
}
