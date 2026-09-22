//! PNG decoding for runtime-ready RGBA8 textures.
//!
//! # Responsibilities
//!
//! - Expands supported PNG color types into tightly packed RGBA8.
//! - Writes tagged RTEX v5 output with color-correct mipmaps.
//!
//! # Design
//!
//! Palette and low-bit-depth inputs are expanded; 16-bit channels are reduced
//! to eight bits. Filename role selects the default color-space tag. The runtime
//! therefore needs only the RTEX reader, with no PNG decoder on the render path.

// Standard library
use std::path::{Path, PathBuf};

// External crates
use anyhow::{bail, Context, Result};

// Current crate
use crate::pill_assets::Rule;

// =============================================================================
// Conversion Rule
// =============================================================================

/// Convert a PNG image to RTEX v5 with tagged RGBA8 mip levels.
///
/// Header: `RTEX | version | width | height | mip_count | srgb_flag`, followed
/// by mip payloads from largest to smallest. Header words are little-endian.
pub struct PngToCookedTex;

impl Rule for PngToCookedTex {
    fn name(&self) -> &'static str {
        "png_to_cooked_tex"
    }

    fn input_glob(&self) -> &'static str {
        "**/*.png"
    }

    fn output_for(&self, input: &Path) -> PathBuf {
        input.with_extension("cooked_tex")
    }

    /// Decode one PNG, expand its channels, and write the complete texture payload.
    ///
    /// Decoder, unsupported color-type, and filesystem errors abort this rule.
    fn build(&self, input: &Path, output: &Path) -> Result<()> {
        // Step 1: decode to eight-bit channels, expanding palette/low-bit-depth inputs.
        let bytes = std::fs::read(input).with_context(|| format!("read {input:?}"))?;
        let mut decoder = png::Decoder::new(bytes.as_slice());
        // Expand palette/indexed images to RGB and low-bit depths to 8-bit.
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().context("png read_info")?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).context("png next_frame")?;
        let width = info.width;
        let height = info.height;
        let raw = &buf[..info.buffer_size()];

        // Step 2: provide all four channels, filling absent alpha with full opacity.
        let rgba: Vec<u8> = match info.color_type {
            png::ColorType::Rgba => raw.to_vec(),
            png::ColorType::Rgb => raw
                .chunks(3)
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect(),
            png::ColorType::Grayscale => raw.iter().flat_map(|&g| [g, g, g, 255]).collect(),
            png::ColorType::GrayscaleAlpha => raw
                .chunks(2)
                .flat_map(|p| [p[0], p[0], p[0], p[1]])
                .collect(),
            _ => bail!("unsupported PNG color type: {:?}", info.color_type),
        };

        // Step 3: choose the color interpretation and generate the cooked mip chain.
        crate::pill_assets::formats::write_rgba8(
            output,
            width,
            height,
            &rgba,
            crate::pill_assets::formats::is_color(input),
        )?;
        Ok(())
    }
}
