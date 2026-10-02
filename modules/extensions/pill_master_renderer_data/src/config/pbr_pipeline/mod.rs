//! The PBR frame: a lit geometry pass into a half-float target, and the
//! post-processing chain that brings it to the surface.
//!
//! # Responsibilities
//!
//! - Install the half of the frame that draws geometry ([`install`]): the lit
//!   pass, the shader it draws through, the offscreen target it writes, and a
//!   neutral material for geometry nobody gave one.
//! - Compose that pass with the post-processing half
//!   ([`super::post_processing`]) into one pipeline asset.
//!
//! # Design
//!
//! A frame written as data, exactly as a project would write it: the lit pass is
//! a `RenderPass` asset and the chain is a `RenderingPipeline` asset, so nothing
//! here needs an API a project cannot reach. What the crate adds is only that
//! these exist without anyone declaring them, and that the renderer points its
//! manager at the result.
//!
//! ```text
//! pill.pbr.opaque   geometry → hdr    the lit surface, in the range lighting produces
//! ─── the four post-processing passes, declared in `super::post_processing` ───
//! ```
//!
//! The shaders are cooked from HLSL into this crate at build time and embedded,
//! so installing the chain reads no files: the fragment stage from `shaders/`
//! beside this file, which `#include`s the crate's shared header, and the
//! post-processing stages from `../post_processing/shaders/`. The vertex stage is
//! the shared one - `../common_shaders/default_vertex.wgsl` - because every lit
//! pass in the crate starts from the same instance layout.

// External crates
use std::collections::BTreeMap;

use pill_engine::{AssetGuid, AssetManager, Handle};
use pill_renderer_api::frame::MaterialParameter;

// Current crate
use crate::{
    assets::{
        Material, PassKind, PassTarget, RenderPass, RenderingPipeline, Shader, ShaderParameterSlot,
        ShaderParameterType, ShaderTextureSlot, TextureType,
    },
    config::post_processing::{self, HDR_TARGET},
    config::{ShaderSourceRecord, DEFAULT_VERTEX},
};

config_shader_file!(
    /// The geometry pass's fragment stage, cooked from `pbr_fragment.hlsl`.
    PBR_FRAGMENT, "pbr_pipeline/shaders/pbr_fragment.wgsl"
);

/// The shader assets [`install`] creates, and the files each is built from.
pub const SHADER_SOURCES: &[ShaderSourceRecord] = &[ShaderSourceRecord {
    shader_asset_name: SHADER_NAME,
    vertex: DEFAULT_VERTEX,
    fragment: PBR_FRAGMENT,
}];

/// Asset name the installed chain is stored under.
pub const PIPELINE_NAME: &str = "pill.pbr.pipeline";

/// Asset name of the shader the geometry pass draws through.
pub const SHADER_NAME: &str = "pill.pbr.shader";

/// The guid the PBR shader is stored under, so a material file can name it.
///
/// Derived from [`SHADER_NAME`] rather than drawn at random: the shader is
/// built in code, not imported from a file with a `.meta` to keep a random
/// guid in, and every run must give it the same one.
pub const SHADER_GUID: AssetGuid = AssetGuid::from_name(SHADER_NAME);

/// Asset name of the material a mesh with none of its own draws with.
pub const MATERIAL_NAME: &str = "pill.pbr.material";

/// The PBR shader's parameters at their neutral values, by slot name.
///
/// The factors multiply the maps, so at these values what a mesh shows is its
/// own albedo, roughness and metalness - and with no map bound, the
/// renderer's default texture for each slot. Used by the chain's own default
/// material and by every newly created material file, so both start out the
/// same.
pub fn neutral_parameters() -> BTreeMap<String, MaterialParameter> {
    BTreeMap::from([
        (
            "pbr_base".to_owned(),
            MaterialParameter::Color([1.0, 1.0, 1.0]),
        ),
        ("pbr_roughness".to_owned(), MaterialParameter::Scalar(1.0)),
        ("pbr_metallic".to_owned(), MaterialParameter::Scalar(1.0)),
        (
            "pbr_emissive".to_owned(),
            MaterialParameter::Color([1.0, 1.0, 1.0]),
        ),
    ])
}

/// Asset name of the lit pass.
const OPAQUE_PASS: &str = "pill.pbr.pass.opaque";

/// Install the PBR chain, and return the pipeline asset.
///
/// The shader the geometry pass draws through is stored under [`SHADER_NAME`],
/// which is how a caller that wants it finds it - the pass carries the same
/// handle, and the store is the index for the name.
///
/// Idempotent: a store that already holds the chain gets the same handle back
/// rather than a second copy of it, so this is safe to call once per generation
/// - which is what [`register`](crate::register) does.
///
/// # Errors
///
/// Returns an error when a name this owns is already taken by an asset of a
/// different type, or when one of the chain's shaders fails to build.
pub fn install(
    assets: &mut AssetManager,
) -> Result<Handle<RenderingPipeline>, Box<dyn std::error::Error>> {
    if let Some(pipeline) = assets.handle_by_name::<RenderingPipeline>(PIPELINE_NAME) {
        return Ok(pipeline);
    }

    // The geometry pass's shader: the fragment stage this crate cooks from HLSL,
    // with the four maps it reads declared under their slots and the two groups
    // every shader in the engine may bind.
    let pbr = Shader::new("pill_pbr")
        .with_wgsl(DEFAULT_VERTEX.embedded_source, PBR_FRAGMENT.embedded_source)
        .with_parameter_slots(vec![
            ShaderParameterSlot::new("pbr_base", ShaderParameterType::Color),
            ShaderParameterSlot::new("pbr_roughness", ShaderParameterType::Scalar),
            ShaderParameterSlot::new("pbr_metallic", ShaderParameterType::Scalar),
            ShaderParameterSlot::new("pbr_emissive", ShaderParameterType::Color),
        ])
        .with_texture_slots(vec![
            ShaderTextureSlot::new("base_color", TextureType::Color, (0, 1)),
            ShaderTextureSlot::new("normal", TextureType::Normal, (2, 3)),
            ShaderTextureSlot::new("metallic_roughness", TextureType::Color, (4, 5)),
            ShaderTextureSlot::new("emissive", TextureType::Color, (6, 7)),
        ])
        .with_engine_parameters(true)
        .with_camera_parameters(true)
        .build()?;
    let pbr = assets.add_named_with_guid(SHADER_NAME, SHADER_GUID, pbr)?;

    // The maps carry the look; these factors are neutral, so what a mesh shows is
    // its own albedo, roughness and metalness. A slot left unbound falls back to
    // the renderer's own default texture, which is what lets this material exist
    // before anyone has supplied a map. Nothing here needs the handle: a project
    // that wants it asks the store for `pill.pbr.material` by name.
    let mut material = Material::builder(MATERIAL_NAME).shader(&pbr).build();
    material.parameters = neutral_parameters();
    assets.add_named(MATERIAL_NAME, material)?;

    // The lit geometry, into a target that can hold what the lighting produces.
    let opaque = RenderPass::new("pill.pbr.opaque")
        .with_shader(pbr)
        .with_kind(PassKind::Geometry)
        .with_target(PassTarget::Offscreen(HDR_TARGET.to_owned()))
        .with_order(0);
    let opaque = assets.add_named(OPAQUE_PASS, opaque)?;

    // The other half of the frame, which reads the target this one just wrote.
    let post = post_processing::install(assets)?;

    let mut pipeline = RenderingPipeline::new().with_pass(opaque);
    for pass in post {
        pipeline.add(pass);
    }
    let pipeline = assets.add_named(PIPELINE_NAME, pipeline)?;

    Ok(pipeline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::Engine;

    #[test]
    fn the_installed_chain_is_the_lit_pass_then_the_post_processing_four() {
        let mut engine = Engine::new();
        let assets = engine
            .world_mut()
            .get_resource_mut::<AssetManager>()
            .expect("the engine inserts the store");

        let pbr = install(assets).expect("a free name");
        let opaque = assets
            .handle_by_name::<RenderPass>(OPAQUE_PASS)
            .expect("the lit pass");
        let pipeline = assets.get(pbr).expect("the pipeline it just added");

        assert_eq!(
            pipeline.passes.len(),
            5,
            "one lit pass and four post passes"
        );
        assert_eq!(pipeline.passes[0], opaque, "the lit pass comes first");
    }
}
