//! A pass, described as data rather than as code.
//!
//! # Responsibilities
//!
//! - Carry everything one pass needs: the shader it draws with, its uniform
//!   parameters, its texture slots, where it reads and where it writes, and when
//!   it runs relative to the others.
//! - Stay the *only* pass type. Every pass in the renderer - geometry, lighting,
//!   post-processing - is one of these values, so adding a pass is data the game
//!   supplies, not a struct someone has to write into the renderer.
//! - Load a pass from a `.render_pass` file in `res` ([`StandaloneAsset`] with
//!   [`RenderPassDocument`]), naming its shader, material and textures by guid.
//!
//! # Design
//!
//! The fields mirror [`Material`](crate::Material) on purpose: a shader handle,
//! a parameter map and a texture map, packed by the same rules. A pass is
//! therefore what a material is to a mesh - the shader plus the values it reads -
//! and the renderer can drive any pass through the code path it already has for
//! drawing with a material.
//!
//! A pass can also name a [`Material`](crate::Material) outright
//! ([`RenderPass::material`]): the material supplies the shader, parameters and
//! textures, and whatever the pass sets itself overrides it. That is how a
//! skybox pass draws a sky: the sky is a `.material` file an artist edits, and
//! the pass only says when and where it is drawn.

use std::collections::BTreeMap;

use pill_engine::{Asset, AssetLoadResult, AssetManager, AssetReference, Handle, StandaloneAsset};
use serde::{Deserialize, Serialize};

// The pass vocabulary is part of the frame contract, so it lives there.
use crate::{Material, Shader, Texture};
use pill_renderer_api::frame::{CullMode, MaterialParameter, PassKind, PassTarget};

/// One pass in a [`RenderingPipeline`](crate::RenderingPipeline).
///
/// Its maps are `BTreeMap`s, never `HashMap`s. An empty `HashMap` does not
/// allocate: it points at a static inside the binary that created it. This
/// value lives in the world and outlives that binary - a project or module
/// that built it is reloaded and its retired image eventually unmapped - and
/// reading such a map afterwards faults. A `BTreeMap` holds no such pointer.
#[derive(Clone, Debug)]
pub struct RenderPass {
    /// Label used in logs, profiling and error messages.
    pub name: String,
    /// Shader the pass draws with. [`Handle::INVALID`] selects the renderer's
    /// built-in pass shader.
    pub shader: Handle<Shader>,
    /// Material the pass draws with: its shader, parameters and textures, under
    /// whatever this pass sets itself. [`Handle::INVALID`] names none.
    ///
    /// A [`PassKind::Skybox`] pass takes its sky from here.
    pub material: Handle<Material>,
    /// Uniform parameters, packed exactly as a material packs its own: one
    /// 16-byte slot each, in the order the shader declares them.
    pub parameters: BTreeMap<String, MaterialParameter>,
    /// Textures bound to the slots the shader declares, by slot name. A slot
    /// the shader declares and neither this map nor [`Self::inputs`] fills falls
    /// back to the renderer's default texture for its type.
    ///
    /// A slot named in both takes the input: that is the frame an earlier pass
    /// of the same chain produced, and the more specific thing to have asked
    /// for.
    pub textures: BTreeMap<String, Handle<Texture>>,
    /// Offscreen targets the pass samples, by the texture slot they bind to.
    ///
    /// Separate from [`Self::textures`] because a target is not an asset: it
    /// lives for one frame, is named by whichever pass writes it, and has no
    /// handle to hold. A name no earlier pass writes fails when the chain is
    /// built, naming the pass and the target rather than showing a flat frame.
    pub inputs: BTreeMap<String, String>,
    /// What the pass draws.
    pub kind: PassKind,
    /// Where the pass reads and writes.
    pub target: PassTarget,
    /// Further targets the pass writes in the same draw, in the order the
    /// shader's `SV_TARGET1`, `SV_TARGET2` ... name them.
    ///
    /// This is how a geometry pass leaves a normal buffer behind for a later
    /// pass to read: one draw, several pictures.
    pub extra_targets: Vec<PassTarget>,
    /// Divisor for the size of the targets this pass writes: 2 is half the
    /// surface, 1 (the default) is all of it.
    ///
    /// The first pass to name a target decides its size, because the target
    /// belongs to the chain rather than to any one pass. A pass at half size is
    /// how the expensive, blurry steps - ambient occlusion, depth of field, a
    /// bloom level - cost a quarter of the pixels.
    pub target_scale: u32,
    /// Whether the pass blends its output over what the target already holds.
    pub blend: bool,
    /// Whether the pass writes depth. A translucent pass reads depth and does
    /// not write it, so what is behind it still passes the test.
    pub depth_write: bool,
    /// Which faces the pass drops.
    pub cull: CullMode,
    /// Ordering key: lower runs first. Equal orders keep the pipeline's order.
    pub order: u8,
    /// Whether the pass runs at all. A disabled pass stays in the pipeline, so
    /// toggling one is a one-field change rather than an edit to the chain.
    pub enabled: bool,
}

impl RenderPass {
    /// A surface-writing geometry pass with the built-in shader and no inputs.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            shader: Handle::INVALID,
            material: Handle::INVALID,
            parameters: BTreeMap::new(),
            textures: BTreeMap::new(),
            inputs: BTreeMap::new(),
            kind: PassKind::Geometry,
            target: PassTarget::Surface,
            extra_targets: Vec::new(),
            target_scale: 1,
            blend: false,
            depth_write: true,
            cull: CullMode::Back,
            order: 0,
            enabled: true,
        }
    }

    /// Set the shader the pass draws with.
    pub fn with_shader(mut self, shader: Handle<Shader>) -> Self {
        self.shader = shader;
        self
    }

    /// Draw with a material's shader, parameters and textures.
    pub fn with_material(mut self, material: Handle<Material>) -> Self {
        self.material = material;
        self
    }

    /// Set one uniform parameter.
    pub fn with_parameter(mut self, slot: impl Into<String>, value: MaterialParameter) -> Self {
        self.parameters.insert(slot.into(), value);
        self
    }

    /// Bind one texture slot.
    pub fn with_texture(mut self, slot: impl Into<String>, texture: Handle<Texture>) -> Self {
        self.textures.insert(slot.into(), texture);
        self
    }

    /// Sample one offscreen target, at the named texture slot.
    pub fn with_input(mut self, slot: impl Into<String>, target: impl Into<String>) -> Self {
        self.inputs.insert(slot.into(), target.into());
        self
    }

    /// Choose what the pass draws.
    pub fn with_kind(mut self, kind: PassKind) -> Self {
        self.kind = kind;
        self
    }

    /// Choose where the pass reads and writes.
    pub fn with_target(mut self, target: PassTarget) -> Self {
        self.target = target;
        self
    }

    /// Choose when the pass runs.
    pub fn with_order(mut self, order: u8) -> Self {
        self.order = order;
        self
    }

    /// Write a further target in the same draw.
    pub fn with_extra_target(mut self, target: PassTarget) -> Self {
        self.extra_targets.push(target);
        self
    }

    /// Write this pass's targets at `1 / scale` of the surface.
    pub fn with_target_scale(mut self, scale: u32) -> Self {
        self.target_scale = scale.max(1);
        self
    }

    /// Blend over what the target holds instead of replacing it.
    pub fn with_blend(mut self, blend: bool) -> Self {
        self.blend = blend;
        self
    }

    /// Write depth, or only read it.
    pub fn with_depth_write(mut self, depth_write: bool) -> Self {
        self.depth_write = depth_write;
        self
    }

    /// Choose which faces the pass drops.
    pub fn with_cull(mut self, cull: CullMode) -> Self {
        self.cull = cull;
        self
    }

    /// Include the pass in the frame, or leave it out.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

/// A pass as written in its `.render_pass` file.
///
/// The asset itself, under the standard asset header. Its shader and textures
/// are guids ([`AssetReference`]) resolved when the file loads; everything
/// else is the [`RenderPass`] field of the same name. The default document is
/// [`RenderPass::new`]'s pass - a geometry pass onto the surface with the
/// built-in shader - which the renderer runs as it is, so a newly created file
/// is valid before anyone edits it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderPassDocument {
    /// See [`RenderPass::shader`]; unset for the built-in shader.
    pub shader: AssetReference<Shader>,
    /// See [`RenderPass::material`]; unset for none.
    pub material: AssetReference<Material>,
    /// See [`RenderPass::parameters`].
    pub parameters: BTreeMap<String, MaterialParameter>,
    /// See [`RenderPass::textures`].
    pub textures: BTreeMap<String, AssetReference<Texture>>,
    /// See [`RenderPass::inputs`].
    pub inputs: BTreeMap<String, String>,
    /// See [`RenderPass::kind`].
    pub kind: PassKind,
    /// See [`RenderPass::target`].
    pub target: PassTarget,
    /// See [`RenderPass::extra_targets`].
    pub extra_targets: Vec<PassTarget>,
    /// See [`RenderPass::target_scale`].
    pub target_scale: u32,
    /// See [`RenderPass::blend`].
    pub blend: bool,
    /// See [`RenderPass::depth_write`].
    pub depth_write: bool,
    /// See [`RenderPass::cull`].
    pub cull: CullMode,
    /// See [`RenderPass::order`].
    pub order: u8,
    /// See [`RenderPass::enabled`].
    pub enabled: bool,
}

impl Default for RenderPassDocument {
    /// [`RenderPass::new`]'s pass.
    fn default() -> Self {
        let pass = RenderPass::new(String::new());
        Self {
            shader: AssetReference::unset(),
            material: AssetReference::unset(),
            parameters: pass.parameters,
            textures: BTreeMap::new(),
            inputs: pass.inputs,
            kind: pass.kind,
            target: pass.target,
            extra_targets: pass.extra_targets,
            target_scale: pass.target_scale,
            blend: pass.blend,
            depth_write: pass.depth_write,
            cull: pass.cull,
            order: pass.order,
            enabled: pass.enabled,
        }
    }
}

impl StandaloneAsset for RenderPass {
    type Document = RenderPassDocument;
    const FILE_EXTENSION: &'static str = "render_pass";

    fn from_document(
        name: &str,
        document: RenderPassDocument,
        assets: &AssetManager,
    ) -> AssetLoadResult<Self> {
        let textures = document
            .textures
            .into_iter()
            .map(|(slot, texture)| (slot, texture.resolve(assets)))
            .collect();
        Ok(Self {
            name: name.to_owned(),
            shader: document.shader.resolve(assets),
            material: document.material.resolve(assets),
            parameters: document.parameters,
            textures,
            inputs: document.inputs,
            kind: document.kind,
            target: document.target,
            extra_targets: document.extra_targets,
            target_scale: document.target_scale.max(1),
            blend: document.blend,
            depth_write: document.depth_write,
            cull: document.cull,
            order: document.order,
            enabled: document.enabled,
        })
    }
}

/// The pinned shared name of [`RenderPass`]; see the comment on its `Asset` impl.
const RENDER_PASS_SHARED_NAME: &str = "pill_master_renderer::assets::RenderPass";

// Shared across binaries: the data module, the GPU module and every project
// compile their own copy of this crate, each with its own `TypeId`. The pinned
// name makes them one asset column (see `Asset::shared_name`); keep it
// verbatim when moving the type.
impl Asset for RenderPass {
    fn shared_name() -> Option<&'static str> {
        Some(RENDER_PASS_SHARED_NAME)
    }

    fn shared_identity() -> Option<u128> {
        // A `const`, so the name is hashed at compile time. The default hashes
        // it on every call, and every `AssetManager` lookup makes that call:
        // the renderer does it several times per drawn entity, every frame.
        const IDENTITY: u128 =
            pill_engine::component::shared_component_identity(RENDER_PASS_SHARED_NAME);
        Some(IDENTITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new `.render_pass` file's document builds `RenderPass::new`'s pass:
    /// a geometry pass onto the surface with the built-in shader.
    #[test]
    fn the_default_document_is_the_minimal_pass() {
        let assets = AssetManager::new();
        let pass = RenderPass::from_document(
            "passes/a.render_pass",
            RenderPassDocument::default(),
            &assets,
        )
        .unwrap();
        let reference = RenderPass::new("passes/a.render_pass");
        assert_eq!(pass.name, reference.name);
        assert_eq!(pass.shader, Handle::INVALID);
        assert_eq!(pass.kind, reference.kind);
        assert_eq!(pass.target, reference.target);
        assert_eq!(pass.target_scale, 1);
        assert_eq!(pass.cull, reference.cull);
        assert!(pass.enabled && pass.depth_write && !pass.blend);
    }

    /// A document round-trips through its file JSON, including the pass
    /// vocabulary's enums.
    #[test]
    fn a_document_round_trips_through_json() {
        let document = RenderPassDocument {
            kind: PassKind::Fullscreen,
            target: PassTarget::Offscreen("hdr".to_owned()),
            cull: CullMode::None,
            order: 7,
            ..RenderPassDocument::default()
        };
        let json = serde_json::to_value(&document).unwrap();
        assert_eq!(json["target"], serde_json::json!({"Offscreen": "hdr"}));
        let read: RenderPassDocument = serde_json::from_value(json).unwrap();
        assert_eq!(read.target, PassTarget::Offscreen("hdr".to_owned()));
        assert_eq!(
            (read.kind, read.cull, read.order),
            (PassKind::Fullscreen, CullMode::None, 7)
        );
    }

    #[test]
    fn a_new_pass_writes_the_surface_with_the_builtin_shader() {
        let pass = RenderPass::new("opaque");

        assert_eq!(pass.name, "opaque");
        assert_eq!(pass.shader, Handle::INVALID);
        assert_eq!(pass.kind, PassKind::Geometry);
        assert_eq!(pass.target, PassTarget::Surface);
        assert!(pass.enabled);
    }

    #[test]
    fn the_builder_sets_one_field_at_a_time() {
        let pass = RenderPass::new("tonemap")
            .with_kind(PassKind::Fullscreen)
            .with_target(PassTarget::Offscreen("hdr".to_owned()))
            .with_order(9)
            .with_parameter("exposure", MaterialParameter::Scalar(1.0))
            .with_enabled(false);

        assert_eq!(pass.name, "tonemap");
        assert_eq!(pass.kind, PassKind::Fullscreen);
        assert_eq!(pass.target, PassTarget::Offscreen("hdr".to_owned()));
        assert_eq!(pass.order, 9);
        assert_eq!(pass.parameters.len(), 1);
        assert!(!pass.enabled);
    }
}
