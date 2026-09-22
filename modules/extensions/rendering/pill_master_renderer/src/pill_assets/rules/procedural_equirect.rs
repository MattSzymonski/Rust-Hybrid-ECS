//! Procedural studio panorama generation and shared RTEX helpers.
//!
//! # Responsibilities
//!
//! - Generates a linear HDR sky gradient with warm key and cool rim softboxes.
//! - Writes float panoramas and float mip chains for environment lighting.
//! - Provides the legacy byte texture writer and basic color helpers.
//!
//! # Design
//!
//! An empty marker file requests a deterministic panorama; its contents are
//! ignored. Longitude wraps at the image seam, latitude runs from top to bottom,
//! and values above one retain light intensity for later IBL convolution.

// Standard library
use std::f32::consts::PI;
use std::path::{Path, PathBuf};

// External crates
use anyhow::{Context, Result};

// Current crate
use crate::pill_assets::Rule;

// =============================================================================
// Studio Lighting Constants
// =============================================================================

/// Linear HDR sky color at the upper pole.
const TOP: [f32; 3] = [0.90, 0.90, 1.20];
/// Linear HDR sky color along the horizon.
const HORIZON: [f32; 3] = [0.43, 0.43, 0.46];
/// Linear HDR floor color at the lower pole.
const BOTTOM: [f32; 3] = [0.040, 0.040, 0.050];

// Key light: warm tungsten, upper-left, ~40° above horizon.
/// Wrapped longitude of the warm key softbox, in normalized image coordinates.
const KEY_U: f32 = 0.10;
/// Top-to-bottom latitude of the warm key softbox.
const KEY_V: f32 = 0.28;
/// Peak linear RGB radiance of the key softbox.
const KEY_INTENSITY: [f32; 3] = [6.0, 4.8, 2.5];
/// Gaussian width of the key softbox in normalized UV space.
const KEY_SIGMA: f32 = 0.0875;

// Rim light: cool blue, upper-right, ~45° above horizon.
/// Wrapped longitude of the cool rim softbox.
const RIM_U: f32 = 0.60;
/// Top-to-bottom latitude of the cool rim softbox.
const RIM_V: f32 = 0.25;
/// Peak linear RGB radiance of the rim softbox.
const RIM_INTENSITY: [f32; 3] = [1.5, 2.5, 5.0];
/// Gaussian width of the rim softbox in normalized UV space.
const RIM_SIGMA: f32 = 0.100;

// =============================================================================
// Conversion Rule
// =============================================================================

/// Procedurally generates a studio-lit equirectangular HDR panorama.
///
/// The `.procedural_equirect` input is a 0-byte trigger marker — its contents are
/// ignored; the panorama (gradient sky + two Gaussian softbox lights) is generated
/// entirely in code. Output: `{stem}_equirect.cooked_tex` (RTEX v2, Rgba32Float HDR).
pub struct ProceduralEquirect;

impl Rule for ProceduralEquirect {
    fn name(&self) -> &'static str {
        "procedural_equirect"
    }

    fn input_glob(&self) -> &'static str {
        "**/*.procedural_equirect"
    }

    fn output_for(&self, input: &Path) -> PathBuf {
        let stem = input.file_stem().unwrap().to_str().unwrap();
        input.with_file_name(format!("{stem}_equirect.cooked_tex"))
    }

    fn build(&self, _input: &Path, output: &Path) -> Result<()> {
        let (width, height) = (512u32, 256u32);
        let pixels = generate_equirect(width, height);
        write_rtex_hdr(output, width, height, &pixels)
    }
}

// =============================================================================
// Panorama Generation
// =============================================================================

/// Sample a sky/floor gradient and two seam-wrapped Gaussian lights as RGBA f32.
fn generate_equirect(width: u32, height: u32) -> Vec<f32> {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let longitude_u = (x as f32 + 0.5) / width as f32;
            let latitude_v = (y as f32 + 0.5) / height as f32;
            let elevation = (0.5 - latitude_v) * PI;
            let sin_elevation = elevation.sin();
            let gradient_factor = (sin_elevation + 1.0) * 0.5;
            let mut color = if gradient_factor < 0.5 {
                lerp3(BOTTOM, HORIZON, gradient_factor * 2.0)
            } else {
                lerp3(HORIZON, TOP, (gradient_factor - 0.5) * 2.0)
            };
            let key_delta_u = longitude_u - KEY_U;
            let key_delta_u = (key_delta_u - key_delta_u.round()).abs(); // wrap U to [-0.5, 0.5]
            let key_delta_v = latitude_v - KEY_V;
            let key_weight = (-(key_delta_u * key_delta_u + key_delta_v * key_delta_v)
                / (2.0 * KEY_SIGMA * KEY_SIGMA))
                .exp();
            color[0] = color[0].max(KEY_INTENSITY[0] * key_weight);
            color[1] = color[1].max(KEY_INTENSITY[1] * key_weight);
            color[2] = color[2].max(KEY_INTENSITY[2] * key_weight);
            let rim_delta_u = longitude_u - RIM_U;
            let rim_delta_u = (rim_delta_u - rim_delta_u.round()).abs();
            let rim_delta_v = latitude_v - RIM_V;
            let rim_weight = (-(rim_delta_u * rim_delta_u + rim_delta_v * rim_delta_v)
                / (2.0 * RIM_SIGMA * RIM_SIGMA))
                .exp();
            color[0] = color[0].max(RIM_INTENSITY[0] * rim_weight);
            color[1] = color[1].max(RIM_INTENSITY[1] * rim_weight);
            color[2] = color[2].max(RIM_INTENSITY[2] * rim_weight);
            pixels.push(color[0]);
            pixels.push(color[1]);
            pixels.push(color[2]);
            pixels.push(1.0);
        }
    }
    pixels
}

// =============================================================================
// Texture Writers and Color Helpers
// =============================================================================

/// Write a single linear RGBA32Float image using the RTEX v2 layout.
pub(crate) fn write_rtex_hdr(path: &Path, width: u32, height: u32, rgba_f32: &[f32]) -> Result<()> {
    let mut output = Vec::with_capacity(16 + rgba_f32.len() * 4);
    output.extend_from_slice(b"RTEX");
    output.extend_from_slice(&2u32.to_le_bytes()); // version 2 = Rgba32Float HDR
    output.extend_from_slice(&width.to_le_bytes());
    output.extend_from_slice(&height.to_le_bytes());
    for &value in rgba_f32 {
        output.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(path, &output).with_context(|| format!("write_rtex_hdr {path:?}"))?;
    Ok(())
}

/// Write linear RGBA32Float mip levels using the RTEX v4 layout.
///
/// Callers provide correctly sized levels, largest first. Each channel is
/// serialized explicitly in little-endian order.
pub(crate) fn write_rtex_hdr_mip(
    path: &Path,
    width: u32,
    height: u32,
    mips: &[Vec<f32>],
) -> Result<()> {
    let mut output = Vec::new();
    output.extend_from_slice(b"RTEX");
    output.extend_from_slice(&4u32.to_le_bytes());
    output.extend_from_slice(&width.to_le_bytes());
    output.extend_from_slice(&height.to_le_bytes());
    output.extend_from_slice(&(mips.len() as u32).to_le_bytes());
    for mip in mips {
        for &value in mip {
            output.extend_from_slice(&value.to_le_bytes());
        }
    }
    std::fs::write(path, &output).with_context(|| format!("write_rtex_hdr_mip {path:?}"))?;
    Ok(())
}

/// Write one untagged RGBA8 image using the legacy RTEX v1 layout.
///
/// The material role determines color interpretation; the BRDF LUT uses linear
/// channels even though ordinary color textures may be sampled as sRGB.
pub(crate) fn write_rtex(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    let mut output = Vec::with_capacity(16 + rgba.len());
    output.extend_from_slice(b"RTEX");
    output.extend_from_slice(&1u32.to_le_bytes());
    output.extend_from_slice(&width.to_le_bytes());
    output.extend_from_slice(&height.to_le_bytes());
    output.extend_from_slice(rgba);
    std::fs::write(path, &output).with_context(|| format!("write_rtex {path:?}"))?;
    Ok(())
}

/// Decode one normalized sRGB channel using the standard piecewise transfer curve.
pub(crate) fn srgb_to_linear(srgb: f32) -> f32 {
    if srgb <= 0.040_45 {
        srgb / 12.92
    } else {
        ((srgb + 0.055) / 1.055).powf(2.4)
    }
}

/// Interpolate three channels without clamping the interpolation factor.
pub(crate) fn lerp3(start: [f32; 3], end: [f32; 3], factor: f32) -> [f32; 3] {
    [
        start[0] + (end[0] - start[0]) * factor,
        start[1] + (end[1] - start[1]) * factor,
        start[2] + (end[2] - start[2]) * factor,
    ]
}
