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
//! writes. That name is declared here rather than there because the passes that
//! consume it live here, and the geometry pass only writes it.
//!
//! # Where the shaders are
//!
//! In `shader_sources/post_processing/shaders/`: the fullscreen vertex stage each
//! pass here draws through, and the four fragment stages. None of the five
//! `#include` anything, so unlike the geometry half's stages these need no
//! `include/` directory.

// External crates
use pill_engine::{AssetGuid, AssetManager, Handle};

// Current crate
use crate::assets::{MaterialParameter, PassKind, PassTarget, RenderPass, Texture, TextureType};
use crate::config::ShaderSourceRecord;

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

/// The cooked fullscreen vertex stage every pass here draws through.
const FULLSCREEN_VERTEX: &str = "post_processing/shaders/fullscreen_vertex.wgsl";

/// The bloom prefilter's fragment stage: the path its `.hlsl` source is
/// imported from, and the cooked file beside it.
const PREFILTER_SHADER: &str = "post_processing/shaders/bloom_prefilter_fragment.hlsl";
const PREFILTER_FRAGMENT: &str = "post_processing/shaders/bloom_prefilter_fragment.wgsl";
/// The bloom prefilter shader's guid, which its meta repeats.
const PREFILTER_GUID: AssetGuid = AssetGuid::from_name("pill.pbr.shader.bloom.prefilter");

/// The bloom composite's fragment stage.
const COMPOSITE_SHADER: &str = "post_processing/shaders/bloom_composite_fragment.hlsl";
const COMPOSITE_FRAGMENT: &str = "post_processing/shaders/bloom_composite_fragment.wgsl";
/// The bloom composite shader's guid, which its meta repeats.
const COMPOSITE_GUID: AssetGuid = AssetGuid::from_name("pill.pbr.shader.bloom.composite");

/// The tonemap's fragment stage.
const TONEMAP_SHADER: &str = "post_processing/shaders/tonemap_fragment.hlsl";
const TONEMAP_FRAGMENT: &str = "post_processing/shaders/tonemap_fragment.wgsl";
/// The tonemap shader's guid, which its meta repeats.
const TONEMAP_GUID: AssetGuid = AssetGuid::from_name("pill.pbr.shader.tonemap");

/// The lens pass's fragment stage.
const LENS_SHADER: &str = "post_processing/shaders/lens_fragment.hlsl";
const LENS_FRAGMENT: &str = "post_processing/shaders/lens_fragment.wgsl";
/// The lens shader's guid, which its meta repeats.
const LENS_GUID: AssetGuid = AssetGuid::from_name("pill.pbr.shader.lens");

/// The shader assets [`install`] creates, and the files each is built from.
pub const SHADER_SOURCES: &[ShaderSourceRecord] = &[
    ShaderSourceRecord {
        shader_asset_name: PREFILTER_SHADER,
        vertex_path: FULLSCREEN_VERTEX,
        fragment_path: PREFILTER_FRAGMENT,
    },
    ShaderSourceRecord {
        shader_asset_name: COMPOSITE_SHADER,
        vertex_path: FULLSCREEN_VERTEX,
        fragment_path: COMPOSITE_FRAGMENT,
    },
    ShaderSourceRecord {
        shader_asset_name: TONEMAP_SHADER,
        vertex_path: FULLSCREEN_VERTEX,
        fragment_path: TONEMAP_FRAGMENT,
    },
    ShaderSourceRecord {
        shader_asset_name: LENS_SHADER,
        vertex_path: FULLSCREEN_VERTEX,
        fragment_path: LENS_FRAGMENT,
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

// --- The chain's look ---
//
// Every value below is a look decision, named so this frame can be read and
// tuned from here. The engine packs one 16-byte slot per parameter, so a pass
// with unrelated scalars carries them in one `Color`, in the order its shader's
// `MaterialParams` declares them; each constant is named after the field it
// fills.

/// Brightness a pixel has to exceed before it blooms.
const BLOOM_THRESHOLD: f32 = 1.0;
/// How softly the bloom ramps in above [`BLOOM_THRESHOLD`].
const BLOOM_KNEE: f32 = 0.6;
/// How much of the bloom is added back over the lit image.
const BLOOM_STRENGTH: f32 = 0.6;

/// The tonemap curve's contrast exponent.
const TONEMAP_CONTRAST: f32 = 1.6;
/// Where the tonemap's shoulder starts to bend the highlights.
const TONEMAP_SHOULDER: f32 = 0.977;
/// The range the display has to fit, in scene units.
const HDR_MAX: f32 = 8.0;
/// Where mid grey enters the curve, in scene units.
const MID_GREY_IN: f32 = 0.18;
/// Where mid grey lands on the display.
const MID_GREY_OUT: f32 = 0.267;

/// Barrel distortion: negative barrels the image, positive pincushions it.
const LENS_BARREL_K1: f32 = -0.06;
/// How far the vignette pulls the corners down.
const LENS_VIGNETTE_STRENGTH: f32 = 1.0;
/// How far red and blue split at the edge, as a scale of the radius squared.
const LENS_CHROMATIC_ABERRATION: f32 = 0.004;
/// Per-channel exponent applied before the grade's ramp.
const LENS_GAMMA: [f32; 3] = [1.0, 1.0, 1.0];
/// The grade's ramp: where it starts, where it ends, and how much of the graded
/// image is mixed in. A descending ramp inverts the grade.
const LENS_GRADE: [f32; 3] = [1.0, 0.0, 0.0];
/// Grain amplitude.
const LENS_GRAIN_AMPLITUDE: f32 = 0.06;
/// Screen pixels per grain texel.
const GRAIN_PIXELS_PER_TEXEL: f32 = 1.0;

/// Edge of the grain tile, in texels.
///
/// One number for both sides: the texture is generated at this size, and the
/// lens pass receives it as a parameter so the shader divides screen pixels by
/// the same value rather than repeating a literal of its own.
const GRAIN_TILE_SIZE: u32 = 256;

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

    let prefilter = crate::config::import_shader(assets, PREFILTER_SHADER, PREFILTER_GUID)?;
    let bloom = RenderPass::new(BLOOM_PASS)
        .with_shader(prefilter)
        .with_kind(PassKind::Fullscreen)
        .with_input("hdr", HDR_TARGET)
        .with_parameter(
            "threshold",
            MaterialParameter::Color([BLOOM_THRESHOLD, BLOOM_KNEE, 0.0]),
        )
        .with_target(PassTarget::Offscreen(BLOOM_TARGET.to_owned()))
        // Half the surface: bloom is a blur, and a blur that costs a quarter of
        // the pixels is the reason a pass declares its own target size.
        .with_target_scale(2)
        .with_order(1);
    let bloom = assets.add_named_with_guid(
        BLOOM_PASS,
        pill_engine::AssetGuid::from_name(BLOOM_PASS),
        bloom,
    )?;

    let composite = crate::config::import_shader(assets, COMPOSITE_SHADER, COMPOSITE_GUID)?;
    let composite_pass = RenderPass::new(COMPOSITE_PASS)
        .with_shader(composite)
        .with_kind(PassKind::Fullscreen)
        .with_input("hdr", HDR_TARGET)
        .with_input("bloom", BLOOM_TARGET)
        .with_parameter(
            "bloom",
            MaterialParameter::Color([BLOOM_STRENGTH, 0.0, 0.0]),
        )
        .with_target(PassTarget::Offscreen(LIT_TARGET.to_owned()))
        .with_order(2);
    let composite_pass = assets.add_named_with_guid(
        COMPOSITE_PASS,
        pill_engine::AssetGuid::from_name(COMPOSITE_PASS),
        composite_pass,
    )?;

    // Solved from the art parameters, so an artist sets the look and these follow.
    let (b, c) = lottes_bc(
        TONEMAP_CONTRAST,
        TONEMAP_SHOULDER,
        HDR_MAX,
        MID_GREY_IN,
        MID_GREY_OUT,
    );
    let tonemap = crate::config::import_shader(assets, TONEMAP_SHADER, TONEMAP_GUID)?;
    let tonemap_pass = RenderPass::new(TONEMAP_PASS)
        .with_shader(tonemap)
        .with_kind(PassKind::Fullscreen)
        .with_input("hdr", LIT_TARGET)
        .with_parameter("contrast", MaterialParameter::Scalar(TONEMAP_CONTRAST))
        .with_parameter("shoulder", MaterialParameter::Scalar(TONEMAP_SHOULDER))
        .with_parameter("b", MaterialParameter::Scalar(b))
        .with_parameter("c", MaterialParameter::Scalar(c))
        .with_target(PassTarget::Offscreen(LDR_TARGET.to_owned()))
        .with_order(3);
    let tonemap_pass = assets.add_named_with_guid(
        TONEMAP_PASS,
        pill_engine::AssetGuid::from_name(TONEMAP_PASS),
        tonemap_pass,
    )?;

    // The last stage writes the swapchain, so the chain ends where it has to.
    let lens = crate::config::import_shader(assets, LENS_SHADER, LENS_GUID)?;
    let lens_pass = RenderPass::new(LENS_PASS)
        .with_shader(lens)
        .with_kind(PassKind::Fullscreen)
        .with_input("source", LDR_TARGET)
        .with_texture("grain", grain)
        .with_parameter(
            "shape",
            MaterialParameter::Color([
                LENS_BARREL_K1,
                LENS_VIGNETTE_STRENGTH,
                LENS_CHROMATIC_ABERRATION,
            ]),
        )
        .with_parameter(
            "gamma",
            MaterialParameter::Color([LENS_GAMMA[0], LENS_GAMMA[1], LENS_GAMMA[2]]),
        )
        .with_parameter(
            "grade",
            MaterialParameter::Color([LENS_GRADE[0], LENS_GRADE[1], LENS_GRADE[2]]),
        )
        .with_parameter(
            "grain",
            MaterialParameter::Color([
                LENS_GRAIN_AMPLITUDE,
                GRAIN_PIXELS_PER_TEXEL,
                GRAIN_TILE_SIZE as f32,
            ]),
        )
        .with_target(PassTarget::Surface)
        .with_order(4);
    let lens_pass = assets.add_named_with_guid(
        LENS_PASS,
        pill_engine::AssetGuid::from_name(LENS_PASS),
        lens_pass,
    )?;

    Ok(vec![bloom, composite_pass, tonemap_pass, lens_pass])
}

/// The curve shape and the two constants Lottes' tonemap is defined by.
///
/// `b` and `c` are solved rather than authored: an artist says where mid grey
/// should land and how much highlight the display has to fit, and these two
/// follow. See Tim Lottes' GDC 2016 talk on HDR color pipelines; the fragment
/// shader's own `lottes` function evaluates the curve these place.
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

/// A [`GRAIN_TILE_SIZE`] square grain tile, generated rather than shipped.
///
/// The pattern is value noise - what matters is that neighbouring texels differ,
/// not that it is random - so this makes one at install time and the shader gets
/// that input without a binary in the repository.
fn grain_texture() -> Texture {
    const SIZE: u32 = GRAIN_TILE_SIZE;
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
        let (b, c) = lottes_bc(
            TONEMAP_CONTRAST,
            TONEMAP_SHOULDER,
            HDR_MAX,
            MID_GREY_IN,
            MID_GREY_OUT,
        );

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
