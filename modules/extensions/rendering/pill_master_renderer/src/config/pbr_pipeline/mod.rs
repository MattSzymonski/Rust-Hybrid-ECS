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
//! - Build a material that draws through the shipped shader, given the four maps
//!   it reads ([`material`]).
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
//! the one the simple pipeline ships -
//! `../simple_pipeline/shaders/default_vertex.wgsl` - because every lit pass in
//! the crate starts from the same instance layout.

// External crates
use pill_engine::{AssetBindingResult, AssetManager, Handle};

// Current crate
use crate::{
    assets::{
        Material, PassKind, PassTarget, RenderPass, RenderingPipeline, Shader, ShaderParameterSlot,
        ShaderParameterType, ShaderTextureSlot, Texture, TextureType,
    },
    config::post_processing::{self, HDR_TARGET},
};

/// Asset name the installed chain is stored under.
pub const PIPELINE_NAME: &str = "pill.pbr.pipeline";

/// Asset name of the shader the geometry pass draws through.
pub const SHADER_NAME: &str = "pill.pbr.shader";

/// Asset name of the material a mesh with none of its own draws with.
pub const MATERIAL_NAME: &str = "pill.pbr.material";

/// Asset name of the lit pass.
const OPAQUE_PASS: &str = "pill.pbr.pass.opaque";

/// The handles installing the chain produced.
///
/// The shader and the material travel with the pipeline because a caller needs
/// them to draw into it: [`material`] builds a textured material from the
/// shader, and an instance whose material is anything else is not drawn by the
/// geometry pass at all.
pub struct PbrPipeline {
    /// The chain to hand the renderer.
    pub pipeline: Handle<RenderingPipeline>,
    /// The shader the geometry pass draws through.
    pub shader: Handle<Shader>,
    /// The material a mesh with none of its own draws with.
    pub material: Handle<Material>,
}

/// The four maps the chain's shader reads, in the slots it declares them under.
pub struct PbrMaps {
    /// Base colour, in the `base_color` slot.
    pub base_color: Handle<Texture>,
    /// Tangent-space normal, in the `normal` slot.
    pub normal: Handle<Texture>,
    /// Metalness in the blue channel and roughness in the green, packed the way
    /// the glTF convention packs them, in the `metallic_roughness` slot.
    pub metallic_roughness: Handle<Texture>,
    /// Emissive colour, in the `emissive` slot.
    pub emissive: Handle<Texture>,
}

/// Install the PBR chain, and return the handles a caller needs to draw into it.
///
/// Idempotent: a store that already holds the chain gets the same handles back
/// rather than a second copy of it, so this is safe to call once per generation
/// - which is what [`register`](crate::register) does.
///
/// # Errors
///
/// Returns the engine's asset error when a name this owns is already taken by an
/// asset of a different type.
pub fn install(assets: &mut AssetManager) -> AssetBindingResult<PbrPipeline> {
    if let (Some(pipeline), Some(shader), Some(material)) = (
        assets.handle_by_name::<RenderingPipeline>(PIPELINE_NAME),
        assets.handle_by_name::<Shader>(SHADER_NAME),
        assets.handle_by_name::<Material>(MATERIAL_NAME),
    ) {
        return Ok(PbrPipeline {
            pipeline,
            shader,
            material,
        });
    }

    // The geometry pass's shader: the fragment stage this crate cooks from HLSL,
    // with the four maps it reads declared under their slots and the two groups
    // every shader in the engine may bind.
    let pbr = Shader::from_wgsl(
        "pill_pbr",
        include_str!("../simple_pipeline/shaders/default_vertex.wgsl"),
        include_str!("shaders/pbr_fragment.wgsl"),
        [
            ShaderParameterSlot::new("pbr_base", ShaderParameterType::Color),
            ShaderParameterSlot::new("pbr_roughness", ShaderParameterType::Scalar),
            ShaderParameterSlot::new("pbr_metallic", ShaderParameterType::Scalar),
            ShaderParameterSlot::new("pbr_emissive", ShaderParameterType::Color),
        ],
        [
            ShaderTextureSlot::new("base_color", TextureType::Color, (0, 1)),
            ShaderTextureSlot::new("normal", TextureType::Normal, (2, 3)),
            ShaderTextureSlot::new("metallic_roughness", TextureType::Color, (4, 5)),
            ShaderTextureSlot::new("emissive", TextureType::Color, (6, 7)),
        ],
        true,
        true,
    );
    let pbr = assets.add_named(SHADER_NAME, pbr)?;

    // The maps carry the look; these factors are neutral, so what a mesh shows is
    // its own albedo, roughness and metalness. A slot left unbound falls back to
    // the renderer's own default texture, which is what lets this material exist
    // before anyone has supplied a map.
    let material = Material::builder(MATERIAL_NAME)
        .shader(&pbr)
        .color_parameter("pbr_base", [1.0, 1.0, 1.0])
        .scalar_parameter("pbr_roughness", 1.0)
        .scalar_parameter("pbr_metallic", 1.0)
        .color_parameter("pbr_emissive", [1.0, 1.0, 1.0])
        .build();
    let material = assets.add_named(MATERIAL_NAME, material)?;

    // The lit geometry, into a target that can hold what the lighting produces.
    let opaque = RenderPass::new("pill.pbr.opaque")
        .with_shader(pbr)
        .with_kind(PassKind::Geometry)
        .with_target(PassTarget::Offscreen(HDR_TARGET.to_owned()))
        .with_order(0);
    let opaque = assets.add_named(OPAQUE_PASS, opaque)?;

    // The other half of the frame, which reads the target this one just wrote.
    let post = post_processing::install(assets)?;

    let pipeline = RenderingPipeline::new()
        .with_pass(opaque)
        .with_pass(post.bloom)
        .with_pass(post.composite)
        .with_pass(post.tonemap)
        .with_pass(post.lens);
    let pipeline = assets.add_named(PIPELINE_NAME, pipeline)?;

    Ok(PbrPipeline {
        pipeline,
        shader: pbr,
        material,
    })
}

/// Build a material that draws through the shipped shader, and store it.
///
/// The slot names are the ones the installed shader declares, so a caller
/// supplies the maps and nothing else. The factors stay neutral: the maps carry
/// the look.
///
/// # Errors
///
/// Returns the engine's asset error when `name` is already taken.
pub fn material(
    assets: &mut AssetManager,
    name: &str,
    pbr: &PbrPipeline,
    maps: PbrMaps,
) -> AssetBindingResult<Handle<Material>> {
    let material = Material::builder(name)
        .shader(&pbr.shader)
        .texture("base_color", &maps.base_color)
        .texture("normal", &maps.normal)
        .texture("metallic_roughness", &maps.metallic_roughness)
        .texture("emissive", &maps.emissive)
        .color_parameter("pbr_base", [1.0, 1.0, 1.0])
        .scalar_parameter("pbr_roughness", 1.0)
        .scalar_parameter("pbr_metallic", 1.0)
        .color_parameter("pbr_emissive", [1.0, 1.0, 1.0])
        .build();
    assets.add_named(name, material)
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
        let pipeline = assets.get(pbr.pipeline).expect("the pipeline it just added");

        assert_eq!(pipeline.passes.len(), 5, "one lit pass and four post passes");
        assert_eq!(pipeline.passes[0], opaque, "the lit pass comes first");
    }
}
