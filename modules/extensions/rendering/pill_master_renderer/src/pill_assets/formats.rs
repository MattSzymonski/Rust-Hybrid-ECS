//! Shared RGBA8 texture output for native conversion rules.
//!
//! # Responsibilities
//!
//! - Writes RTEX v5 textures with an explicit color-space tag and mip chain.
//! - Infers the default interpretation of loose textures from their filename suffix.
//!
//! # Design
//!
//! All header words are little-endian. The v5 header stores width, height,
//! mip count, and an sRGB flag before tightly packed RGBA8 levels. Mips are
//! filtered in linear light for color maps; data maps and alpha are averaged
//! directly. Runtime readers also accept the older v1/v2/v4 formats.

// External crates
use anyhow::Result;

// Standard library
use std::path::Path;

// =============================================================================
// Texture Encoding
// =============================================================================

/// Write a valid tightly packed RGBA8 base image and its generated mip chain.
///
/// Callers supply nonzero dimensions and exactly width times height times four
/// bytes. Filesystem errors propagate; image decoding/validation belongs to the
/// calling conversion rule.
pub fn write_rgba8(path: &Path, width: u32, height: u32, pixels: &[u8], srgb: bool) -> Result<()> {
    let base = crate::assets::TextureData {
        width,
        height,
        float: false,
        srgb: Some(srgb),
        mips: vec![pixels.to_vec()],
    };
    let texture = crate::assets::generate_mips(&base, srgb);
    let mut bytes = b"RTEX".to_vec();
    for n in [
        5u32,
        width,
        height,
        texture.mips.len() as u32,
        u32::from(srgb),
    ] {
        bytes.extend(n.to_le_bytes());
    }
    for mip in texture.mips {
        bytes.extend(mip);
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

// =============================================================================
// Default Color-Space Selection
// =============================================================================

/// Defaults for loose images; a material role can explicitly reinterpret a texture.
pub fn is_color(path: &Path) -> bool {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    !["normal", "metallic_roughness", "occlusion", "brdf_lut"]
        .iter()
        .any(|s| stem.ends_with(s))
}
