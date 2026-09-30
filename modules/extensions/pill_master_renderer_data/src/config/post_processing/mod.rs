//! The post-processing half of the PBR frame: bloom, its composite, the tonemap
//! and the lens pass that writes the surface.
//!
//! # Responsibilities
//!
//! - Declare the four fullscreen passes the frame ends with, and the shaders
//!   they draw through ([`install`]).
//! - Own the offscreen target names the passes hand between each other, and the
//!   generated grain tile the last one samples.
//!
//! # Design
//!
//! Four passes in a row, each reading a target an earlier pass wrote:
//!
//! ```text
//! bloom      reads hdr         → bloom    what is brighter than the threshold, at half size
//! composite  reads hdr, bloom  → lit      bloom added back over the lit image
//! tonemap    reads lit         → ldr      the range brought down for the display
//! lens       reads ldr         → surface  grain, grade and vignette
//! ```
//!
//! The first of them reads [`HDR_TARGET`], which the frame's geometry pass
//! writes. That name is declared here rather than there because this is where
//! every pass that *consumes* it lives: the geometry pass only writes it.
//!
//! # Where the shaders are
//!
//! In `shaders/` beside this file: the fullscreen vertex stage each pass here
//! draws through, and the four fragment stages. That directory is a shader
//! cooker root of its own, because the rule matches `<root>/shaders/*.hlsl` one
//! level down and does not walk to a nested one - which is why a pipeline that
//! keeps its stages beside itself costs one line in `build.rs` and nothing else.
//! None of the five `#include` anything, so unlike the geometry half's root this
//! one has no `include/` beside it.

// External crates
use pill_engine::{AssetLoadResult, AssetManager, Handle};

// Current crate
use crate::assets::{
    MaterialParameter, PassKind, PassTarget, RenderPass, Shader, ShaderParameterSlot,
    ShaderParameterType, ShaderTextureSlot, Texture, TextureType,
};
use crate::config::{ConfigShaderFile, ShaderSourceRecord};

/// Name of the target the frame's geometry pass writes, and the first pass here
/// reads.
pub(super) const HDR_TARGET: &str = "hdr";
/// Target the prefilter writes and the composite reads.
const BLOOM_TARGET: &str = "bloom";
/// Target the composite writes and the tonemap reads.
const LIT_TARGET: &str = "lit";
/// Target the tonemap writes and the lens pass reads.
const LDR_TARGET: &str = "ldr";

/// Asset name of the generated grain tile.
pub const GRAIN_TEXTURE_NAME: &str = "pill.pbr.grain";

/// Asset name of the bloom prefilter's shader.
const PREFILTER_SHADER: &str = "pill.pbr.shader.bloom.prefilter";
/// Asset name of the bloom composite's shader.
const COMPOSITE_SHADER: &str = "pill.pbr.shader.bloom.composite";
/// Asset name of the tonemap's shader.
const TONEMAP_SHADER: &str = "pill.pbr.shader.tonemap";
/// Asset name of the lens pass's shader.
const LENS_SHADER: &str = "pill.pbr.shader.lens";

config_shader_file!(
    /// The fullscreen vertex stage every pass here draws through.
    FULLSCREEN_VERTEX, "post_processing/shaders/fullscreen_vertex.wgsl"
);
config_shader_file!(
    /// The bloom prefilter's fragment stage.
    PREFILTER_FRAGMENT, "post_processing/shaders/bloom_prefilter_fragment.wgsl"
);
config_shader_file!(
    /// The bloom composite's fragment stage.
    COMPOSITE_FRAGMENT, "post_processing/shaders/bloom_composite_fragment.wgsl"
);
config_shader_file!(
    /// The tonemap's fragment stage.
    TONEMAP_FRAGMENT, "post_processing/shaders/tonemap_fragment.wgsl"
);
config_shader_file!(
    /// The lens pass's fragment stage.
    LENS_FRAGMENT, "post_processing/shaders/lens_fragment.wgsl"
);

/// The shader assets [`install`] creates, and the files each is built from.
pub const SHADER_SOURCES: &[ShaderSourceRecord] = &[
    ShaderSourceRecord {
        shader_asset_name: PREFILTER_SHADER,
        vertex: FULLSCREEN_VERTEX,
        fragment: PREFILTER_FRAGMENT,
    },
    ShaderSourceRecord {
        shader_asset_name: COMPOSITE_SHADER,
        vertex: FULLSCREEN_VERTEX,
        fragment: COMPOSITE_FRAGMENT,
    },
    ShaderSourceRecord {
        shader_asset_name: TONEMAP_SHADER,
        vertex: FULLSCREEN_VERTEX,
        fragment: TONEMAP_FRAGMENT,
    },
    ShaderSourceRecord {
        shader_asset_name: LENS_SHADER,
        vertex: FULLSCREEN_VERTEX,
        fragment: LENS_FRAGMENT,
    },
];

/// Asset name of the bloom prefilter pass.
const BLOOM_PASS: &str = "pill.pbr.pass.bloom";
/// Asset name of the bloom composite pass.
const COMPOSITE_PASS: &str = "pill.pbr.pass.bloom.composite";
/// Asset name of the tonemap pass.
const TONEMAP_PASS: &str = "pill.pbr.pass.tonemap";
/// Asset name of the lens pass.
const LENS_PASS: &str = "pill.pbr.pass.lens";

/// Install the four post-processing passes.
///
/// Returns the passes in the order the frame runs them. The grain tile the last
/// one samples is not returned: the lens pass already binds it, and a second
/// hand-out of that handle would be a copy free to disagree with the pass.
///
/// Idempotent: a store that already holds them gets the same handles back rather
/// than a second copy, so this is safe to call once per generation.
///
/// # Errors
///
/// Returns an error when a name this owns is already taken by an asset of a
/// different type, or when one of the shaders fails to build.
pub fn install(
    assets: &mut AssetManager,
) -> Result<Vec<Handle<RenderPass>>, Box<dyn std::error::Error>> {
    if let (Some(bloom), Some(composite), Some(tonemap), Some(lens)) = (
        assets.handle_by_name::<RenderPass>(BLOOM_PASS),
        assets.handle_by_name::<RenderPass>(COMPOSITE_PASS),
        assets.handle_by_name::<RenderPass>(TONEMAP_PASS),
        assets.handle_by_name::<RenderPass>(LENS_PASS),
    ) {
        return Ok(vec![bloom, composite, tonemap, lens]);
    }

    // The grain tile the lens pass samples, generated rather than shipped: the
    // pattern only has to make neighbouring texels differ, not be random, and
    // generating it keeps a binary out of the repository.
    let grain = assets.add_named(GRAIN_TEXTURE_NAME, grain_texture())?;

    let prefilter = fullscreen_shader(
        "pill_pbr_bloom_prefilter",
        PREFILTER_FRAGMENT,
        [ShaderParameterSlot::new(
            "threshold",
            ShaderParameterType::Color,
        )],
        [ShaderTextureSlot::new("hdr", TextureType::Color, (0, 1))],
    )?;
    let prefilter = assets.add_named(PREFILTER_SHADER, prefilter)?;
    let bloom = RenderPass::new("pill.pbr.bloom")
        .with_shader(prefilter)
        .with_kind(PassKind::Fullscreen)
        .with_input("hdr", HDR_TARGET)
        .with_parameter("threshold", MaterialParameter::Color([1.0, 0.6, 0.0]))
        .with_target(PassTarget::Offscreen(BLOOM_TARGET.to_owned()))
        // Half the surface: bloom is a blur, and a blur that costs a quarter of
        // the pixels is the reason a pass declares its own target size.
        .with_target_scale(2)
        .with_order(1);
    let bloom = assets.add_named(BLOOM_PASS, bloom)?;

    let composite = fullscreen_shader(
        "pill_pbr_bloom_composite",
        COMPOSITE_FRAGMENT,
        [ShaderParameterSlot::new(
            "bloom",
            ShaderParameterType::Color,
        )],
        [
            ShaderTextureSlot::new("hdr", TextureType::Color, (0, 1)),
            ShaderTextureSlot::new("bloom", TextureType::Color, (2, 3)),
        ],
    )?;
    let composite = assets.add_named(COMPOSITE_SHADER, composite)?;
    let composite_pass = RenderPass::new("pill.pbr.bloom_composite")
        .with_shader(composite)
        .with_kind(PassKind::Fullscreen)
        .with_input("hdr", HDR_TARGET)
        .with_input("bloom", BLOOM_TARGET)
        .with_parameter("bloom", MaterialParameter::Color([0.6, 0.0, 0.0]))
        .with_target(PassTarget::Offscreen(LIT_TARGET.to_owned()))
        .with_order(2);
    let composite_pass = assets.add_named(COMPOSITE_PASS, composite_pass)?;

    // The two curve constants are solved from the art parameters rather than
    // authored, so an artist says where mid grey lands and how much highlight has
    // to fit, and these follow.
    let contrast = 1.6;
    let shoulder = 0.977;
    let (b, c) = lottes_bc(contrast, shoulder, 8.0, 0.18, 0.267);
    let tonemap = fullscreen_shader(
        "pill_pbr_tonemap",
        TONEMAP_FRAGMENT,
        [
            ShaderParameterSlot::new("contrast", ShaderParameterType::Scalar),
            ShaderParameterSlot::new("shoulder", ShaderParameterType::Scalar),
            ShaderParameterSlot::new("b", ShaderParameterType::Scalar),
            ShaderParameterSlot::new("c", ShaderParameterType::Scalar),
        ],
        [ShaderTextureSlot::new("hdr", TextureType::Color, (0, 1))],
    )?;
    let tonemap = assets.add_named(TONEMAP_SHADER, tonemap)?;
    let tonemap_pass = RenderPass::new("pill.pbr.tonemap")
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

    // The last stage writes the swapchain, so the chain ends where it has to.
    let lens = fullscreen_shader(
        "pill_pbr_lens",
        LENS_FRAGMENT,
        [
            ShaderParameterSlot::new("shape", ShaderParameterType::Color),
            ShaderParameterSlot::new("gamma", ShaderParameterType::Color),
            ShaderParameterSlot::new("grade", ShaderParameterType::Color),
            ShaderParameterSlot::new("grain", ShaderParameterType::Color),
        ],
        [
            ShaderTextureSlot::new("source", TextureType::Color, (0, 1)),
            ShaderTextureSlot::new("grain", TextureType::Color, (2, 3)),
        ],
    )?;
    let lens = assets.add_named(LENS_SHADER, lens)?;
    let lens_pass = RenderPass::new("pill.pbr.lens")
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

    Ok(vec![bloom, composite_pass, tonemap_pass, lens_pass])
}

/// A fullscreen shader: this module's fullscreen vertex stage, the given
/// fragment stage, and the engine's and the camera's groups.
///
/// # Errors
///
/// Returns the engine's load error when the stages cannot be built.
fn fullscreen_shader(
    name: &str,
    fragment: ConfigShaderFile,
    parameters: impl IntoIterator<Item = ShaderParameterSlot>,
    textures: impl IntoIterator<Item = ShaderTextureSlot>,
) -> AssetLoadResult<Shader> {
    Shader::new(name)
        .with_wgsl(FULLSCREEN_VERTEX.embedded_source, fragment.embedded_source)
        .with_parameter_slots(parameters)
        .with_texture_slots(textures)
        .with_engine_parameters(true)
        .with_camera_parameters(true)
        .build()
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
/// random - so this makes one at install time and the shader gets the same kind
/// of input without a binary in the repository.
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

    Texture::from_rgba(GRAIN_TEXTURE_NAME, TextureType::Color, rgba, SIZE, SIZE)
        .expect("the tile is SIZE x SIZE RGBA")
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
