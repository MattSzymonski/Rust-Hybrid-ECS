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
//! The lit fragment stage is in `shader_sources/simple_pipeline/shaders/`, and
//! is imported from there like a project's own shader: its slots and its guid
//! live in the `.hlsl.meta` beside it. The vertex stage it pairs with lives in
//! `shader_sources/common_shaders/`, because it is not this pipeline's alone: the
//! renderer's fallback material is built from the same pair - a material written
//! for the fallback draws here without being rebuilt - and the PBR geometry pass
//! reuses the vertex stage, since every lit pass in the crate starts from the
//! same instance layout. Three readers, one copy of the vertex stage.
//!
//! The build script cooks the whole `shader_sources/` tree in one call: every
//! directory that directly holds `.hlsl` files is a source directory, and every
//! `include/` in the tree is on every source's include path.
//!
//! The one text this crate embeds is the fallback pair itself
//! ([`DEFAULT_VERTEX`] and [`DEFAULT_LIT_FRAGMENT`]): the GPU module builds its
//! stand-in shader from it before any store exists, so it cannot be read from
//! one.

// External crates
use pill_engine::{AssetManager, Handle};

// Current crate
use crate::{
    assets::{PassKind, PassTarget, RenderPass, RenderingPipeline},
    config::{ShaderSourceRecord, DEFAULT_VERTEX},
};

config_shader_file!(
    /// The pass's fragment stage, cooked from `default_lit_fragment.hlsl`; also
    /// the renderer's built-in fallback shader.
    pub DEFAULT_LIT_FRAGMENT, "simple_pipeline/shaders/default_lit_fragment.wgsl"
);

/// The shader assets [`install`] creates, and the files each is built from.
pub const SHADER_SOURCES: &[ShaderSourceRecord] = &[ShaderSourceRecord {
    shader_asset_name: SHADER_PATH,
    vertex_path: DEFAULT_VERTEX.relative_path,
    fragment_path: DEFAULT_LIT_FRAGMENT.relative_path,
}];

/// Asset name the installed pipeline is stored under.
pub const PIPELINE_NAME: &str = "pill.simple.pipeline";

/// The shader the pass draws through: the path its `.hlsl` source is imported
/// under, because an imported asset is named by the path it came from.
pub const SHADER_PATH: &str = "simple_pipeline/shaders/default_lit_fragment.hlsl";

/// The guid the shader is stored under.
///
/// Derived from a name rather than drawn at random, so every checkout and every
/// run gives it the same one; the `.hlsl.meta` beside the source carries this
/// value and [`install`] asserts the two agree.
pub const SHADER_GUID: pill_engine::AssetGuid =
    pill_engine::AssetGuid::from_name("pill.simple.shader");

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

    let shader = crate::config::import_shader(assets, SHADER_PATH, SHADER_GUID)?;

    let pass = RenderPass::new(PASS_NAME)
        .with_shader(shader)
        .with_kind(PassKind::Geometry)
        .with_target(PassTarget::Surface);
    let pass = assets.add_named_with_guid(
        PASS_NAME,
        pill_engine::AssetGuid::from_name(PASS_NAME),
        pass,
    )?;

    let pipeline = assets.add_named(PIPELINE_NAME, RenderingPipeline::new().with_pass(pass))?;

    Ok(pipeline)
}
