//! The atlas update data: what changed in a glyph and icon atlas since the
//! last take, as owned plain data a backend uploads.
//!
//! # Responsibilities
//!
//! - [`Region`]: a rectangle of an atlas image, in texels.
//! - [`AtlasRegionUpdate`]: one dirty region with its tightly packed RGBA8
//!   rows.
//! - [`AtlasUpdate`]: the image's size plus every changed region, applied by
//!   a consuming backend to its own texture.
//!
//! # Design
//!
//! The value crosses an artifact boundary - a UI built in one DLL, a
//! renderer in another - so it never carries a pointer into the producer's
//! atlas: the pixels are copies. The producing side keeps its atlas and
//! drains it; the consumer keeps its own texture and applies what arrives.

// =============================================================================
// Region
// =============================================================================

/// A rectangle of the atlas, in texels.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Region {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
}

impl Region {
    /// How many texels the region covers.
    pub fn texels(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

// =============================================================================
// AtlasUpdate
// =============================================================================

/// One dirty atlas region with its pixels, ready for a backend to upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtlasRegionUpdate {
    /// Where the pixels belong.
    pub region: Region,
    /// Tightly packed RGBA8 rows, `region.width * region.height * 4` bytes.
    pub pixels: Vec<u8>,
}

/// What changed in an atlas since the last take: owned plain data a backend
/// applies to its texture, per region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtlasUpdate {
    /// Image width in texels.
    pub width: u32,
    /// Image height in texels.
    pub height: u32,
    /// The changed regions with their pixels, in the order they were written.
    pub regions: Vec<AtlasRegionUpdate>,
}
