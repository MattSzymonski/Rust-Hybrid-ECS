//! The simple frame: one lit geometry pass onto the surface.
//!
//! # Responsibilities
//!
//! - Install the smallest chain that draws something - one geometry pass,
//!   through a shader built from the renderer's own lit WGSL, writing the
//!   swapchain - and hand back the pipeline asset ([`install`]).
//!
//! # Design
//!
//! The pass names a shader, so it draws every instance whose material was built
//! from that shader, and the shader it installs is the same lit one the renderer
//! falls back to with the same declared slots: a material written for the
//! fallback works here without being rebuilt.
//!
//! The renderer's built-in chain is a geometry pass that names *no* shader, and
//! draws every instance through its own material. This is what to install when a
//! project wants that frame named, stored and swappable instead of implicit.
//!
//! # Where the shaders are
//!
//! In `shaders/` beside this file, the lit fragment stage. The vertex stage it
//! pairs with lives in `config/common_shaders/`, because it is not this
//! pipeline's alone: the renderer's fallback material is built from the same
//! pair - a material written for the fallback draws here without being rebuilt -
//! and the PBR geometry pass reuses the vertex stage, since every lit pass in
//! the crate starts from the same instance layout. Three readers, one copy of
//! the vertex stage.
//!
//! Each of those directories is a shader cooker root of its own: the rule
//! matches `<root>/shaders/*.hlsl` one level down and does not walk to a nested
//! one.
//!
//! Nothing here reads a file at runtime: the WGSL is cooked into the crate at
//! build time and embedded, so the pipeline installs for a project whose assets
//! live somewhere the renderer has never heard of.

// External crates
use pill_engine::{AssetManager, Handle};

// Current crate
use crate::{
    assets::{
        PassKind, PassTarget, RenderPass, RenderingPipeline, Shader, ShaderParameterSlot,
        ShaderParameterType, ShaderTextureSlot, TextureType,
    },
};

/// Asset name the installed pipeline is stored under.
pub const PIPELINE_NAME: &str = "pill.simple.pipeline";

/// Asset name of the shader the pass draws through.
pub const SHADER_NAME: &str = "pill.simple.shader";

/// Asset name of the pass the pipeline holds.
pub const PASS_NAME: &str = "pill.simple.pass";

/// Install the simple frame, and return the pipeline asset.
///
/// Idempotent: a store that already holds the pipeline gets that handle back
/// rather than a second copy, so this is safe to call once per generation.
///
/// # Errors
///
/// Returns an error when a name this owns is already taken by an asset of a
/// different type, or when the shader's stages fail to build.
pub fn install(
    assets: &mut AssetManager,
) -> Result<Handle<RenderingPipeline>, Box<dyn std::error::Error>> {
    if let Some(pipeline) = assets.handle_by_name::<RenderingPipeline>(PIPELINE_NAME) {
        return Ok(pipeline);
    }

    let shader = Shader::new("pill_simple")
        .with_wgsl(
            include_str!("../common_shaders/default_vertex.wgsl"),
            include_str!("shaders/default_lit_fragment.wgsl"),
        )
        .with_parameter_slots(vec![
            ShaderParameterSlot::new("tint", ShaderParameterType::Color),
            ShaderParameterSlot::new("specularity", ShaderParameterType::Scalar),
        ])
        .with_texture_slots(vec![
            ShaderTextureSlot::new("color", TextureType::Color, (0, 1)),
            ShaderTextureSlot::new("normal", TextureType::Normal, (2, 3)),
        ])
        .with_engine_parameters(true)
        .with_camera_parameters(true)
        .build()?;
    let shader = assets.add_named(SHADER_NAME, shader)?;

    let pass = RenderPass::new("pill.simple.opaque")
        .with_shader(shader)
        .with_kind(PassKind::Geometry)
        .with_target(PassTarget::Surface);
    let pass = assets.add_named(PASS_NAME, pass)?;

    let pipeline = assets.add_named(PIPELINE_NAME, RenderingPipeline::new().with_pass(pass))?;

    Ok(pipeline)
}
