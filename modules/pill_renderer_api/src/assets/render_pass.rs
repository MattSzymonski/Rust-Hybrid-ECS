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
//!
//! # Design
//!
//! The fields mirror [`Material`](crate::Material) on purpose: a shader handle,
//! a parameter map and a texture map, packed by the same rules. A pass is
//! therefore what a material is to a mesh - the shader plus the values it reads -
//! and the renderer can drive any pass through the code path it already has for
//! drawing with a material.

use std::collections::HashMap;

use pill_engine::{Asset, Handle};

use crate::{MaterialParameter, Shader, Texture};

/// What a pass draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PassKind {
    /// Draws the scene's meshes, one instance batch per material.
    #[default]
    Geometry,
    /// Draws one fullscreen triangle: the shape post-processing passes take.
    Fullscreen,
}

/// Where a pass reads from and writes to.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum PassTarget {
    /// The swapchain image. A pass writing here reads nothing and ends the
    /// frame, so a pipeline has at most one such pass and it runs last.
    #[default]
    Surface,
    /// An offscreen colour target, named so later passes can sample it.
    ///
    /// The renderer owns these: a name that no earlier pass declares is an
    /// error when the pipeline is built, not a silently blank frame.
    Offscreen(String),
}

/// Which faces a pass drops.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CullMode {
    /// Back faces: the default for solid geometry.
    #[default]
    Back,
    /// Front faces, for looking at the inside of a shape.
    Front,
    /// Nothing: a fullscreen triangle has no outside worth dropping, and
    /// two-sided geometry has no inside.
    None,
}

/// One pass in a [`RenderingPipeline`](crate::RenderingPipeline).
#[derive(Clone, Debug)]
pub struct RenderPass {
    /// Label used in logs, profiling and error messages.
    pub name: String,
    /// Shader the pass draws with. [`Handle::INVALID`] selects the renderer's
    /// built-in pass shader.
    pub shader: Handle<Shader>,
    /// Uniform parameters, packed exactly as a material packs its own: one
    /// 16-byte slot each, in the order the shader declares them.
    pub parameters: HashMap<String, MaterialParameter>,
    /// Textures bound to the slots the shader declares, by slot name. A slot
    /// the shader declares and neither this map nor [`Self::inputs`] fills falls
    /// back to the renderer's default texture for its type.
    ///
    /// A slot named in both takes the input: that is the frame an earlier pass
    /// of the same chain produced, and the more specific thing to have asked
    /// for.
    pub textures: HashMap<String, Handle<Texture>>,
    /// Offscreen targets the pass samples, by the texture slot they bind to.
    ///
    /// Separate from [`Self::textures`] because a target is not an asset: it
    /// lives for one frame, is named by whichever pass writes it, and has no
    /// handle to hold. A name no earlier pass writes fails when the chain is
    /// built, naming the pass and the target rather than showing a flat frame.
    pub inputs: HashMap<String, String>,
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
            parameters: HashMap::new(),
            textures: HashMap::new(),
            inputs: HashMap::new(),
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

impl Asset for RenderPass {}

#[cfg(test)]
mod tests {
    use super::*;

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
