//! Loads the committed helmet: mesh, PBR maps, shader, material, and the pass
//! and pipeline the renderer is handed.
//!
//! The chain is two passes: the helmet is drawn into an offscreen target that
//! keeps the range the lit pixels produce, and a fullscreen pass reads it back
//! and tonemaps it onto the surface.

use pill_engine::{AssetManager, Handle, World};
use pill_master_renderer::{
    AssetLoader, Material, MaterialParameter, Mesh, PassKind, PassTarget, RenderPass,
    RenderingPipeline, Shader, ShaderParameterSlot, ShaderParameterType, ShaderTextureSlot,
    Texture, TextureType,
};
use std::{collections::HashMap, path::PathBuf};

/// Asset names, so a reload that runs this twice is refused by the manager with
/// the offending name rather than quietly adding the helmet a second time.
const HELMET_MESH: &str = "helmet.mesh";
const HELMET_SHADER: &str = "helmet.shader.pbr";
const HELMET_MATERIAL: &str = "helmet.material";
const HELMET_PASS: &str = "helmet.pass.opaque";
const TONEMAP_SHADER: &str = "helmet.shader.tonemap";
const TONEMAP_PASS: &str = "helmet.pass.tonemap";
const BLOOM_SHADER: &str = "helmet.shader.bloom.prefilter";
const BLOOM_PASS: &str = "helmet.pass.bloom.prefilter";
const COMPOSITE_SHADER: &str = "helmet.shader.bloom.composite";
const COMPOSITE_PASS: &str = "helmet.pass.bloom.composite";
const LENS_SHADER: &str = "helmet.shader.lens";
const LENS_PASS: &str = "helmet.pass.lens";
const GRAIN_TEXTURE: &str = "helmet.grain";
const HELMET_PIPELINE: &str = "helmet.pipeline";

/// Names of the offscreen targets the chain passes between each other, in the
/// order they are written. A pass names one to write it and names it again to
/// read it, so this list is the shape of the chain.
const HDR_TARGET: &str = "hdr";
const BLOOM_TARGET: &str = "bloom";
const LIT_TARGET: &str = "lit";
const LDR_TARGET: &str = "ldr";

/// What the scene needs to draw the helmet.
pub(crate) struct SceneAssets {
    pub mesh: Handle<Mesh>,
    pub material: Handle<Material>,
    pub pipeline: Handle<RenderingPipeline>,
}

/// The texture slots `pbr_fragment.hlsl` declares, with the binding pairs it
/// declares them at: base colour, normal, metallic-roughness, emissive.
fn pbr_texture_slots() -> HashMap<String, ShaderTextureSlot> {
    HashMap::from([
        (
            "base_color".to_owned(),
            ShaderTextureSlot::new(TextureType::Color, (0, 1)),
        ),
        (
            "normal".to_owned(),
            ShaderTextureSlot::new(TextureType::Normal, (2, 3)),
        ),
        (
            "metallic_roughness".to_owned(),
            ShaderTextureSlot::new(TextureType::Color, (4, 5)),
        ),
        (
            "emissive".to_owned(),
            ShaderTextureSlot::new(TextureType::Color, (6, 7)),
        ),
    ])
}

/// The uniform slots `pbr_fragment.hlsl` declares, in slot order: the two
/// `Color` slots carry a tint, the two `Scalar` slots a factor.
fn pbr_parameter_slots() -> Vec<(String, ShaderParameterSlot)> {
    vec![
        (
            "pbr_base".to_owned(),
            ShaderParameterSlot::new(ShaderParameterType::Color),
        ),
        (
            "pbr_roughness".to_owned(),
            ShaderParameterSlot::new(ShaderParameterType::Scalar),
        ),
        (
            "pbr_metallic".to_owned(),
            ShaderParameterSlot::new(ShaderParameterType::Scalar),
        ),
        (
            "pbr_emissive".to_owned(),
            ShaderParameterSlot::new(ShaderParameterType::Color),
        ),
    ]
}

/// The uniform slots `tonemap_fragment.hlsl` declares, in slot order. `b` and
/// `c` are not tuned by hand - see [`lottes_bc`] - but they are still slots,
/// because the shader has no other way to be told them.
fn tonemap_parameter_slots() -> Vec<(String, ShaderParameterSlot)> {
    ["contrast", "shoulder", "b", "c"]
        .into_iter()
        .map(|slot| {
            (
                slot.to_owned(),
                ShaderParameterSlot::new(ShaderParameterType::Scalar),
            )
        })
        .collect()
}

/// One `Color` slot under the given name. Post passes group their numbers into
/// slots this way because that is how the engine packs them.
fn color_parameter(slot: &str) -> (String, ShaderParameterSlot) {
    (
        slot.to_owned(),
        ShaderParameterSlot::new(ShaderParameterType::Color),
    )
}

/// The single frame `bloom_prefilter_fragment.hlsl` reads.
fn single_input_slots(slot: &str) -> HashMap<String, ShaderTextureSlot> {
    HashMap::from([(
        slot.to_owned(),
        ShaderTextureSlot::new(TextureType::Color, (0, 1)),
    )])
}

/// The curve shape and the two constants Lottes' tonemap is defined by.
///
/// `b` and `c` are solved rather than authored: an artist says where mid grey
/// should land and how much highlight the display has to fit, and these two
/// follow. See Tim Lottes, "Advanced Techniques and Optimization of VDR Color
/// Pipelines", GDC 2016, and `lottes_bc` in the reference renderer, which this
/// mirrors.
fn lottes_bc(contrast: f32, shoulder: f32, hdr_max: f32, mid_in: f32, mid_out: f32) -> (f32, f32) {
    let hdr_a = hdr_max.powf(contrast);
    let mid_a = mid_in.powf(contrast);
    let hdr_ad = hdr_a.powf(shoulder);
    let mid_ad = mid_a.powf(shoulder);
    let denom = (hdr_ad - mid_ad) * mid_out;
    let b = (-mid_a + hdr_a * mid_out) / denom;
    let c = (hdr_ad * mid_a - hdr_a * mid_ad * mid_out) / denom;
    (b, c)
}

/// A 256x256 grain tile, generated rather than shipped.
///
/// The reference samples a tile it commits as an asset. The pattern is value
/// noise - what matters is that neighbouring texels differ, not that it is
/// random - so this makes one at load time and the shader gets the same kind of
/// input without a binary in the repository.
fn grain_texture() -> Texture {
    const SIZE: u32 = 256;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);

    for y in 0..SIZE {
        for x in 0..SIZE {
            // A cheap integer hash standing in for noise.
            let mut hash = x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B);
            hash ^= hash >> 15;
            hash = hash.wrapping_mul(0x2545_F491);
            hash ^= hash >> 13;
            let value = (hash & 0xFF) as u8;
            rgba.extend_from_slice(&[value, value, value, 255]);
        }
    }

    Texture::from_rgba("helmet.grain", TextureType::Color, rgba, SIZE, SIZE)
}

/// Decode one committed texture under `name`.
fn load_texture(
    assets: &mut AssetManager,
    name: &str,
    file: &str,
    texture_type: TextureType,
) -> Result<Handle<Texture>, Box<dyn std::error::Error>> {
    let texture = Texture::new(name, texture_type, AssetLoader::Path(file.into()))?;
    Ok(assets.add_named(name, texture)?)
}

/// Decodes the helmet and everything the frame draws it with.
///
/// # Errors
///
/// Returns an error when the manager is missing, when one of the committed
/// files fails to decode, or when an asset name is already taken.
pub(crate) fn load(world: &mut World) -> Result<SceneAssets, Box<dyn std::error::Error>> {
    AssetLoader::set_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("res"));
    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or_else(|| "the engine AssetManager resource is missing".to_owned())?;

    let obj = AssetLoader::Path("models/helmet.obj".into()).load()?;
    let mesh = assets.add_named(HELMET_MESH, Mesh::from_obj_bytes("helmet", &obj)?)?;

    let base_color = load_texture(
        assets,
        "helmet.base_color",
        "textures/helmet_basecolor.jpg",
        TextureType::Color,
    )?;
    let normal = load_texture(
        assets,
        "helmet.normal",
        "textures/helmet_normal.jpg",
        TextureType::Normal,
    )?;
    let metallic_roughness = load_texture(
        assets,
        "helmet.metallic_roughness",
        "textures/helmet_metallic_roughness.jpg",
        TextureType::Color,
    )?;
    let emissive = load_texture(
        assets,
        "helmet.emissive",
        "textures/helmet_emissive.jpg",
        TextureType::Color,
    )?;

    // Both stages are cooked from HLSL by this project's `build.rs`.
    let shader = Shader::new(
        "helmet_pbr",
        AssetLoader::Path("shaders/default_vertex.wgsl".into()),
        AssetLoader::Path("shaders/pbr_fragment.wgsl".into()),
        pbr_parameter_slots(),
        pbr_texture_slots(),
        true,
        true,
    )?;
    let shader = assets.add_named(HELMET_SHADER, shader)?;

    let grain = assets.add_named(GRAIN_TEXTURE, grain_texture())?;

    // The maps carry most of the look; the factors are neutral, so what the
    // helmet shows is its own albedo, roughness and metalness.
    let material = Material::builder("helmet_pbr")
        .shader(&shader)
        .texture("base_color", &base_color)
        .texture("normal", &normal)
        .texture("metallic_roughness", &metallic_roughness)
        .texture("emissive", &emissive)
        .color_parameter("pbr_base", [1.0, 1.0, 1.0])
        .scalar_parameter("pbr_roughness", 1.0)
        .scalar_parameter("pbr_metallic", 1.0)
        .color_parameter("pbr_emissive", [1.0, 1.0, 1.0])
        .build();
    let material = assets.add_named(HELMET_MATERIAL, material)?;

    // The frame the project wants, as data: the helmet into an offscreen target
    // that can hold what the lighting produces, a bright pass and its composite
    // for bloom, the tonemap that brings the range down, and a lens pass that
    // writes the surface. Passes and pipelines are assets like any other, so a
    // game builds, stores and swaps them without touching the renderer.
    let helmet = RenderPass::new("helmet.opaque")
        .with_shader(shader)
        .with_kind(PassKind::Geometry)
        .with_target(PassTarget::Offscreen(HDR_TARGET.to_owned()))
        .with_order(0);
    let helmet = assets.add_named(HELMET_PASS, helmet)?;

    let bloom = Shader::new(
        "helmet_bloom",
        AssetLoader::Path("shaders/fullscreen_vertex.wgsl".into()),
        AssetLoader::Path("shaders/bloom_prefilter_fragment.wgsl".into()),
        vec![color_parameter("threshold")],
        single_input_slots("hdr"),
        true,
        true,
    )?;
    let bloom = assets.add_named(BLOOM_SHADER, bloom)?;
    let bloom_pass = RenderPass::new("helmet.bloom")
        .with_shader(bloom)
        .with_kind(PassKind::Fullscreen)
        .with_input("hdr", HDR_TARGET)
        .with_parameter("threshold", MaterialParameter::Color([1.0, 0.6, 0.0]))
        .with_target(PassTarget::Offscreen(BLOOM_TARGET.to_owned()))
        // Half the surface: bloom is a blur, and a blur that costs a quarter of
        // the pixels is the reason a pass declares its own target size.
        .with_target_scale(2)
        .with_order(1);
    let bloom_pass = assets.add_named(BLOOM_PASS, bloom_pass)?;

    let composite = Shader::new(
        "helmet_bloom_composite",
        AssetLoader::Path("shaders/fullscreen_vertex.wgsl".into()),
        AssetLoader::Path("shaders/bloom_composite_fragment.wgsl".into()),
        vec![color_parameter("bloom")],
        HashMap::from([
            (
                "hdr".to_owned(),
                ShaderTextureSlot::new(TextureType::Color, (0, 1)),
            ),
            (
                "bloom".to_owned(),
                ShaderTextureSlot::new(TextureType::Color, (2, 3)),
            ),
        ]),
        true,
        true,
    )?;
    let composite = assets.add_named(COMPOSITE_SHADER, composite)?;
    let composite_pass = RenderPass::new("helmet.bloom_composite")
        .with_shader(composite)
        .with_kind(PassKind::Fullscreen)
        .with_input("hdr", HDR_TARGET)
        .with_input("bloom", BLOOM_TARGET)
        .with_parameter("bloom", MaterialParameter::Color([0.6, 0.0, 0.0]))
        .with_target(PassTarget::Offscreen(LIT_TARGET.to_owned()))
        .with_order(2);
    let composite_pass = assets.add_named(COMPOSITE_PASS, composite_pass)?;

    let contrast = 1.6;
    let shoulder = 0.977;
    let (b, c) = lottes_bc(contrast, shoulder, 8.0, 0.18, 0.267);
    let tonemap = Shader::new(
        "helmet_tonemap",
        AssetLoader::Path("shaders/fullscreen_vertex.wgsl".into()),
        AssetLoader::Path("shaders/tonemap_fragment.wgsl".into()),
        tonemap_parameter_slots(),
        single_input_slots("hdr"),
        true,
        true,
    )?;
    let tonemap = assets.add_named(TONEMAP_SHADER, tonemap)?;
    let tonemap_pass = RenderPass::new("helmet.tonemap")
        .with_shader(tonemap)
        .with_kind(PassKind::Fullscreen)
        .with_input("hdr", LIT_TARGET)
        .with_parameter("contrast", MaterialParameter::Scalar(contrast))
        .with_parameter("shoulder", MaterialParameter::Scalar(shoulder))
        .with_parameter("b", MaterialParameter::Scalar(b))
        .with_parameter("c", MaterialParameter::Scalar(c))
        .with_target(PassTarget::Offscreen(LDR_TARGET.to_owned()))
        .with_order(3);
    let tonemap_pass = assets.add_named(TONEMAP_PASS, tonemap_pass)?;

    let lens = Shader::new(
        "helmet_lens",
        AssetLoader::Path("shaders/fullscreen_vertex.wgsl".into()),
        AssetLoader::Path("shaders/lens_fragment.wgsl".into()),
        vec![
            color_parameter("shape"),
            color_parameter("gamma"),
            color_parameter("grade"),
            color_parameter("grain"),
        ],
        HashMap::from([
            (
                "source".to_owned(),
                ShaderTextureSlot::new(TextureType::Color, (0, 1)),
            ),
            (
                "grain".to_owned(),
                ShaderTextureSlot::new(TextureType::Color, (2, 3)),
            ),
        ]),
        true,
        true,
    )?;
    let lens = assets.add_named(LENS_SHADER, lens)?;
    let lens_pass = RenderPass::new("helmet.lens")
        .with_shader(lens)
        .with_kind(PassKind::Fullscreen)
        .with_input("source", LDR_TARGET)
        .with_texture("grain", grain)
        .with_parameter("shape", MaterialParameter::Color([-0.06, 1.0, 0.004]))
        .with_parameter("gamma", MaterialParameter::Color([1.0, 1.0, 1.0]))
        .with_parameter("grade", MaterialParameter::Color([1.0, 0.0, 0.0]))
        .with_parameter("grain", MaterialParameter::Color([0.06, 1.0, 0.0]))
        .with_target(PassTarget::Surface)
        .with_order(4);
    let lens_pass = assets.add_named(LENS_PASS, lens_pass)?;

    let pipeline = assets.add_named(
        HELMET_PIPELINE,
        RenderingPipeline::new()
            .with_pass(helmet)
            .with_pass(bloom_pass)
            .with_pass(composite_pass)
            .with_pass(tonemap_pass)
            .with_pass(lens_pass),
    )?;

    Ok(SceneAssets {
        mesh,
        material,
        pipeline,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_solved_curve_constants_are_positive_and_are_the_reference_values() {
        let (b, c) = lottes_bc(1.6, 0.977, 8.0, 0.18, 0.267);

        assert!(
            b > 0.0 && c > 0.0,
            "the curve needs both constants positive"
        );
        // The reference's own values for these art parameters, to three places:
        // a change here means the solve moved, not that rounding did.
        assert!((b - 1.073).abs() < 0.002, "b came out {b}");
        assert!((c - 0.168).abs() < 0.002, "c came out {c}");
    }
}
