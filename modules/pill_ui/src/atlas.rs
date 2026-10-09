//! One RGBA image holding every rasterized glyph and icon.
//!
//! # Responsibilities
//!
//! - [`Atlas`]: a fixed-size RGBA8 image with a shelf allocator
//!   ([`Atlas::allocate`]), row writes ([`Atlas::write_region`]), and the
//!   bookkeeping a backend needs to upload only what changed
//!   ([`Atlas::take_updated_regions`]).
//! - [`Region`]: a rectangle of the image in texels.
//!
//! # Design
//!
//! Shelves, not a free-list: entries are packed left to right in the current
//! row, a full row opens the next one, and a single texel of padding keeps a
//! sampled neighbour from bleeding in. The image never moves or shrinks -
//! moving it would invalidate every UV already in a draw list - so a full
//! atlas refuses new entries and their callers fall back to drawing nothing.
//! Frontends hand the backend the image once and the dirty regions per frame.

// External crates
use pill_core::math::Vector2f;

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

// =============================================================================
// Atlas
// =============================================================================

/// Texels of padding between entries, against sampling bleed.
const PADDING: u32 = 1;

/// The side length of a default atlas: 2048 texels, 16 MiB of RGBA.
pub const DEFAULT_LENGTH: u32 = 2048;

/// The shared glyph and icon image.
#[derive(Clone, Debug)]
pub struct Atlas {
    /// Image width in texels.
    width: u32,
    /// Image height in texels.
    height: u32,
    /// RGBA8 pixels, row-major, row stride `width * 4`.
    pixels: Vec<u8>,
    /// Height of the shelf currently being filled, padding included.
    shelf_height: u32,
    /// Left edge of the free space in the current shelf.
    cursor_x: u32,
    /// Top edge of the current shelf.
    cursor_y: u32,
    /// Regions written since the last [`Self::take_updated_regions`].
    dirty: Vec<Region>,
}

impl Default for Atlas {
    fn default() -> Atlas {
        Atlas::new(DEFAULT_LENGTH, DEFAULT_LENGTH)
    }
}

impl Atlas {
    /// An empty atlas of the given size, every texel transparent.
    pub fn new(width: u32, height: u32) -> Atlas {
        Atlas {
            width,
            height,
            pixels: vec![0; (width as usize) * (height as usize) * 4],
            shelf_height: 0,
            cursor_x: 0,
            cursor_y: 0,
            dirty: Vec::new(),
        }
    }

    /// Image width in texels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Image height in texels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The RGBA8 image, row-major.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Reserve a `width` by `height` region, or `None` when the atlas is
    /// full or the entry does not fit at all.
    pub fn allocate(&mut self, width: u32, height: u32) -> Option<Region> {
        if width == 0 || height == 0 || width > self.width || height > self.height {
            return None;
        }
        // A row that cannot hold the entry starts the next shelf.
        if self.cursor_x + width > self.width {
            self.cursor_y += self.shelf_height + PADDING;
            self.cursor_x = 0;
            self.shelf_height = 0;
        }
        if self.cursor_y + height > self.height {
            return None;
        }
        let region = Region {
            x: self.cursor_x,
            y: self.cursor_y,
            width,
            height,
        };
        self.cursor_x += width + PADDING;
        self.shelf_height = self.shelf_height.max(height + PADDING);
        Some(region)
    }

    /// Copy one RGBA8 image (`width * height * 4` bytes, row-major, straight
    /// alpha) into `region`, and record it as changed.
    ///
    /// `region` must come from [`Self::allocate`]; a mismatch between the
    /// region and the data is a programming error, so debug builds panic.
    pub fn write_region(&mut self, region: Region, rgba: &[u8]) {
        debug_assert_eq!(
            rgba.len(),
            (region.width as usize) * (region.height as usize) * 4,
            "atlas write data does not match its region"
        );
        debug_assert!(
            region.x + region.width <= self.width && region.y + region.height <= self.height
        );
        let row_bytes = (region.width as usize) * 4;
        for row in 0..region.height as usize {
            let destination =
                ((region.y as usize) + row) * (self.width as usize) * 4 + (region.x as usize) * 4;
            let source = row * row_bytes;
            self.pixels[destination..destination + row_bytes]
                .copy_from_slice(&rgba[source..source + row_bytes]);
        }
        self.dirty.push(region);
    }

    /// The regions written since the previous call; drains the record.
    pub fn take_updated_regions(&mut self) -> Vec<Region> {
        std::mem::take(&mut self.dirty)
    }

    /// The corner-to-corner texture coordinates of `region`, normalized to
    /// `0.0..=1.0`.
    pub fn uv_rect(&self, region: Region) -> (Vector2f, Vector2f) {
        let min = Vector2f::new(
            region.x as f32 / self.width as f32,
            region.y as f32 / self.height as f32,
        );
        let max = Vector2f::new(
            (region.x + region.width) as f32 / self.width as f32,
            (region.y + region.height) as f32 / self.height as f32,
        );
        (min, max)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_pack_left_to_right_and_wrap_to_a_new_shelf() {
        let mut atlas = Atlas::new(16, 16);
        let first = atlas.allocate(4, 4).unwrap();
        let second = atlas.allocate(4, 4).unwrap();
        let third = atlas.allocate(4, 4).unwrap();
        let fourth = atlas.allocate(4, 4).unwrap();
        assert_eq!(
            first,
            Region {
                x: 0,
                y: 0,
                width: 4,
                height: 4
            }
        );
        assert_eq!(
            second,
            Region {
                x: 5,
                y: 0,
                width: 4,
                height: 4
            }
        );
        assert_eq!(
            third,
            Region {
                x: 10,
                y: 0,
                width: 4,
                height: 4
            }
        );
        // 15 + 4 does not fit the first shelf, so this entry opens the next
        // one: shelf height (4 + padding) plus the inter-shelf padding row.
        assert_eq!(
            fourth,
            Region {
                x: 0,
                y: 6,
                width: 4,
                height: 4
            }
        );
    }

    #[test]
    fn a_full_atlas_refuses_and_an_oversized_entry_is_refused() {
        let mut atlas = Atlas::new(8, 8);
        assert!(atlas.allocate(16, 4).is_none());
        assert!(atlas.allocate(4, 16).is_none());
        assert!(atlas.allocate(0, 4).is_none());
        assert!(atlas.allocate(8, 8).is_some());
        // The first shelf filled the whole image.
        assert!(atlas.allocate(1, 1).is_none());
    }

    #[test]
    fn a_write_records_one_region_once() {
        let mut atlas = Atlas::new(8, 8);
        let region = atlas.allocate(2, 1).unwrap();
        let pixels = [10u8, 20, 30, 40, 50, 60, 70, 80];
        atlas.write_region(region, &pixels);
        assert_eq!(atlas.take_updated_regions(), [region]);
        assert!(atlas.take_updated_regions().is_empty());
        // The pixels landed where the region says.
        let offset = (region.y * atlas.width() + region.x) as usize * 4;
        assert_eq!(&atlas.pixels()[offset..offset + 8], &pixels);
    }

    #[test]
    fn texture_coordinates_normalize_the_region() {
        let atlas = Atlas::new(100, 50);
        let (min, max) = atlas.uv_rect(Region {
            x: 25,
            y: 10,
            width: 25,
            height: 10,
        });
        assert_eq!(min, Vector2f::new(0.25, 0.2));
        assert_eq!(max, Vector2f::new(0.5, 0.4));
    }
}
