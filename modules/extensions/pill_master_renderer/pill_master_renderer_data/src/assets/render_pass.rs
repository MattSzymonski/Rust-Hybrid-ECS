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

// Standard library
use std::collections::BTreeMap;

// External crates
use pill_engine::{
    pill_mirror_object, AssetLoadResult, AssetManager, AssetReference, Handle, StandaloneAsset,
};
// The pass vocabulary is part of the frame contract, so it lives there.
use pill_renderer_api::frame::{CullMode, MaterialParameter, PassKind, PassSettings, PassTarget};
use serde::{Deserialize, Serialize};

// Current crate
use crate::{Material, Shader, Texture};

/// One pass in a [`RenderingPipeline`](crate::RenderingPipeline).
///
/// Its maps are `BTreeMap`s, never `HashMap`s. An empty `HashMap` does not
/// allocate: it points at a static inside the binary that created it. This
/// value lives in the world and outlives that binary - a project or module
/// that built it is reloaded and its retired image eventually unmapped - and
/// reading such a map afterwards faults. A `BTreeMap` holds no such pointer.
#[derive(Clone, Debug)]
#[pill_mirror_object(asset, standalone, shared = "pill_master_renderer::assets::RenderPass")]
pub struct RenderPass {
    /// Label used in logs, profiling and error messages.
    pub name: String,
    /// Shader the pass draws with. [`Handle::INVALID`] leaves a geometry pass
    /// drawing through each instance's own material pipeline; a fullscreen or
    /// skybox pass without one is not drawn.
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
    /// What the pass draws, where it writes, and how the draw is set up.
    pub settings: PassSettings,
    /// Whether the pass runs at all. A disabled pass stays in the pipeline, so
    /// toggling one is a one-field change rather than an edit to the chain.
    pub enabled: bool,
}

impl RenderPass {
    /// A surface-writing geometry pass with no shader of its own, so it draws
    /// through each instance's material pipeline.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            shader: Handle::INVALID,
            material: Handle::INVALID,
            parameters: BTreeMap::new(),
            textures: BTreeMap::new(),
            inputs: BTreeMap::new(),
            settings: PassSettings::default(),
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
        self.settings.kind = kind;
        self
    }

    /// Choose where the pass reads and writes.
    pub fn with_target(mut self, target: PassTarget) -> Self {
        self.settings.target = target;
        self
    }

    /// Choose when the pass runs.
    pub fn with_order(mut self, order: u8) -> Self {
        self.settings.order = order;
        self
    }

    /// Write a further target in the same draw.
    pub fn with_extra_target(mut self, target: PassTarget) -> Self {
        self.settings.extra_targets.push(target);
        self
    }

    /// Write this pass's targets at `1 / scale` of the surface.
    pub fn with_target_scale(mut self, scale: u32) -> Self {
        self.settings.target_scale = scale.max(1);
        self
    }

    /// Blend over what the target holds instead of replacing it.
    pub fn with_blend(mut self, blend: bool) -> Self {
        self.settings.blend = blend;
        self
    }

    /// Write depth, or only read it.
    pub fn with_depth_write(mut self, depth_write: bool) -> Self {
        self.settings.depth_write = depth_write;
        self
    }

    /// Choose which faces the pass drops.
    pub fn with_cull(mut self, cull: CullMode) -> Self {
        self.settings.cull = cull;
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
/// [`RenderPass::new`]'s pass - a geometry pass onto the surface with no shader
/// of its own - which the renderer runs as it is, so a newly created file is
/// valid before anyone edits it.
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
    /// See [`RenderPass::settings`]; flattened, so a `.render_pass` file
    /// carries these fields at its top level like every other one.
    #[serde(flatten)]
    pub settings: PassSettings,
    /// See [`RenderPass::enabled`].
    pub enabled: bool,
}

impl Default for RenderPassDocument {
    /// [`RenderPass::new`]'s pass.
    fn default() -> Self {
        Self {
            shader: AssetReference::unset(),
            material: AssetReference::unset(),
            parameters: BTreeMap::new(),
            textures: BTreeMap::new(),
            inputs: BTreeMap::new(),
            settings: PassSettings::default(),
            enabled: true,
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
            // A hand-edited file can write a zero scale, which no target can
            // be divided by.
            settings: PassSettings {
                target_scale: document.settings.target_scale.max(1),
                ..document.settings
            },
            enabled: document.enabled,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new `.render_pass` file's document builds `RenderPass::new`'s pass:
    /// a geometry pass onto the surface with no shader of its own.
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
        assert_eq!(pass.settings, reference.settings);
        assert!(pass.enabled);
    }

    /// A document round-trips through its file JSON, including the pass
    /// vocabulary's enums. The settings block is flattened, so the file keeps
    /// the shape it had when the fields were written on the document itself.
    #[test]
    fn a_document_round_trips_through_json() {
        let document = RenderPassDocument {
            settings: PassSettings {
                kind: PassKind::Fullscreen,
                target: PassTarget::Offscreen("hdr".to_owned()),
                cull: CullMode::None,
                order: 7,
                ..PassSettings::default()
            },
            ..RenderPassDocument::default()
        };
        let json = serde_json::to_value(&document).unwrap();
        assert_eq!(json["target"], serde_json::json!({"Offscreen": "hdr"}));
        assert_eq!(json["kind"], serde_json::json!("Fullscreen"));
        assert_eq!(json["order"], serde_json::json!(7));
        let read: RenderPassDocument = serde_json::from_value(json).unwrap();
        assert_eq!(
            read.settings,
            PassSettings {
                kind: PassKind::Fullscreen,
                target: PassTarget::Offscreen("hdr".to_owned()),
                cull: CullMode::None,
                order: 7,
                ..PassSettings::default()
            }
        );
    }

    /// A `.render_pass` file written before the settings block existed still
    /// loads: the block is flattened, so its fields sit at the top level of
    /// the asset exactly where the document used to name them.
    #[test]
    fn a_flat_document_file_still_loads() {
        let json = serde_json::json!({
            "blend": false,
            "cull": "Back",
            "depth_write": true,
            "enabled": true,
            "extra_targets": [],
            "inputs": {},
            "kind": "Geometry",
            "material": null,
            "order": 0,
            "parameters": {},
            "shader": { "guid": "73cff470b05f0370f5e4fba32f4d9b09" },
            "target": { "Offscreen": "hdr" },
            "target_scale": 1,
            "textures": {}
        });
        let document: RenderPassDocument = serde_json::from_value(json).unwrap();
        assert_eq!(document.settings.kind, PassKind::Geometry);
        assert_eq!(
            document.settings.target,
            PassTarget::Offscreen("hdr".to_owned())
        );
        assert!(document.settings.depth_write && document.enabled);
    }

    #[test]
    fn a_new_pass_writes_the_surface_with_the_builtin_shader() {
        let pass = RenderPass::new("opaque");

        assert_eq!(pass.name, "opaque");
        assert_eq!(pass.shader, Handle::INVALID);
        assert_eq!(pass.settings.kind, PassKind::Geometry);
        assert_eq!(pass.settings.target, PassTarget::Surface);
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
        assert_eq!(pass.settings.kind, PassKind::Fullscreen);
        assert_eq!(
            pass.settings.target,
            PassTarget::Offscreen("hdr".to_owned())
        );
        assert_eq!(pass.settings.order, 9);
        assert_eq!(pass.parameters.len(), 1);
        assert!(!pass.enabled);
    }
}
