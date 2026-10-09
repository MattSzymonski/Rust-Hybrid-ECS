//! SVG icons: rasterized on the CPU into the shared atlas.
//!
//! # Responsibilities
//!
//! - [`IconCache`]: rasterizes SVG icon sources through resvg into the shared
//!   [`Atlas`], one region per (icon, revision, pixel length), and
//!   re-rasterizes when the caller's revision moves - the asset reloaded.
//! - A parsing or rasterization failure is logged and draws nothing; it never
//!   panics.
//!
//! # Design
//!
//! The cache key folds the requested size and scale factor into the pixel
//! length they produce: two requests that rasterize to the same texels are
//! the same request. The caller supplies the revision, so the cache follows
//! the asset system without depending on it. Icons rasterize into a square of
//! the requested length, scaled from the document's larger side, and land in
//! the atlas as straight-alpha RGBA (converted from tiny-skia's premultiplied
//! output).

// Standard library
use std::collections::BTreeMap;

// External crates
use pill_core::telemetry::telemetry_target;
use resvg::tiny_skia;
use resvg::usvg;

// Current crate
use crate::atlas::{Atlas, Region};

// =============================================================================
// IconId
// =============================================================================

/// A caller-defined icon identity: a file index, an asset id, a hash - the
/// cache only compares it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IconId(pub u64);

// =============================================================================
// IconCache
// =============================================================================

/// One cached icon: which icon revision at which pixel length.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct IconKey {
    /// The caller's icon identity.
    id: IconId,
    /// The caller's revision; a change re-rasterizes.
    revision: u64,
    /// The rasterized side length in physical pixels.
    length: u32,
}

/// The icon half of the shared atlas: rasterize once, draw many times.
#[derive(Clone, Debug, Default)]
pub struct IconCache {
    /// Where every rasterized icon lives.
    entries: BTreeMap<IconKey, Region>,
}

impl IconCache {
    /// An empty cache.
    pub fn new() -> IconCache {
        IconCache::default()
    }

    /// The atlas region of `svg` at `size` logical pixels and `scale` (the
    /// monitor's scale factor), rasterizing and caching it on first use.
    ///
    /// `revision` is the caller's change marker for the source: keeping it
    /// stable caches, moving it re-rasterizes on the next request.
    pub fn icon(
        &mut self,
        atlas: &mut Atlas,
        id: IconId,
        revision: u64,
        svg: &[u8],
        size: f32,
        scale: f32,
    ) -> Option<Region> {
        let length = (size * scale).round().max(1.0) as u32;
        let key = IconKey {
            id,
            revision,
            length,
        };
        if let Some(region) = self.entries.get(&key) {
            return Some(*region);
        }

        let options = usvg::Options::default();
        let tree = match usvg::Tree::from_data(svg, &options) {
            Ok(tree) => tree,
            Err(error) => {
                pill_core::warn!(
                    target: telemetry_target::UI,
                    icon = id.0,
                    error = %error,
                    "an icon source could not be parsed; drawing nothing for it"
                );
                return None;
            }
        };

        let mut pixmap = tiny_skia::Pixmap::new(length, length)?;
        let source = tree.size();
        let extent = source.width().max(source.height()).max(1.0);
        let factor = length as f32 / extent;
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(factor, factor),
            &mut pixmap.as_mut(),
        );

        // tiny-skia renders premultiplied; the atlas and the draw list use
        // straight alpha, so undo the multiply channel by channel.
        let mut rgba = pixmap.data().to_vec();
        for pixel in rgba.chunks_exact_mut(4) {
            let alpha = pixel[3] as u32;
            if alpha == 0 {
                pixel[..3].fill(0);
                continue;
            }
            for channel in &mut pixel[..3] {
                *channel = ((*channel as u32 * 255) / alpha).min(255) as u8;
            }
        }

        let region = atlas.allocate(length, length)?;
        atlas.write_region(region, &rgba);
        self.entries.insert(key, region);
        Some(region)
    }

    /// How many icons are cached.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no icon is cached.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A solid red square, 8 by 8.
    const RED_SQUARE: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8" viewBox="0 0 8 8"><rect width="8" height="8" fill="#ff0000"/></svg>"##;

    /// The RGBA pixel at the region's top-left corner.
    fn corner_pixel(atlas: &Atlas, region: Region) -> [u8; 4] {
        let offset = (region.y as usize * atlas.width() as usize + region.x as usize) * 4;
        let mut pixel = [0u8; 4];
        pixel.copy_from_slice(&atlas.pixels()[offset..offset + 4]);
        pixel
    }

    #[test]
    fn a_square_rasterizes_into_the_atlas() {
        let mut atlas = Atlas::new(64, 64);
        let mut cache = IconCache::new();
        let region = cache
            .icon(&mut atlas, IconId(1), 0, RED_SQUARE, 8.0, 1.0)
            .expect("the square must rasterize");
        assert_eq!(region.width, 8);
        assert_eq!(region.height, 8);
        assert_eq!(atlas.take_updated_regions(), [region]);

        let pixel = corner_pixel(&atlas, region);
        assert_eq!(pixel[3], 255, "opaque");
        assert_eq!(pixel[0], 255, "red");
        assert_eq!(pixel[1], 0);
        assert_eq!(pixel[2], 0);
    }

    #[test]
    fn a_second_request_uses_the_cache() {
        let mut atlas = Atlas::new(64, 64);
        let mut cache = IconCache::new();
        let first = cache.icon(&mut atlas, IconId(1), 0, RED_SQUARE, 8.0, 1.0);
        atlas.take_updated_regions();

        let second = cache.icon(&mut atlas, IconId(1), 0, RED_SQUARE, 8.0, 1.0);
        assert_eq!(first, second);
        assert!(
            atlas.take_updated_regions().is_empty(),
            "no second rasterization"
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn a_revision_change_rasterizes_again() {
        let mut atlas = Atlas::new(64, 64);
        let mut cache = IconCache::new();
        cache.icon(&mut atlas, IconId(1), 0, RED_SQUARE, 8.0, 1.0);
        atlas.take_updated_regions();

        let region = cache
            .icon(&mut atlas, IconId(1), 1, RED_SQUARE, 8.0, 1.0)
            .expect("the new revision rasterizes");
        assert!(!atlas.take_updated_regions().is_empty());
        assert_eq!(cache.len(), 2, "both revisions stay cached");
        assert_eq!(corner_pixel(&atlas, region)[3], 255);
    }

    #[test]
    fn size_and_scale_fold_into_the_rasterized_length() {
        let mut atlas = Atlas::new(64, 64);
        let mut cache = IconCache::new();
        let small = cache
            .icon(&mut atlas, IconId(1), 0, RED_SQUARE, 8.0, 1.0)
            .unwrap();
        let scaled = cache
            .icon(&mut atlas, IconId(1), 0, RED_SQUARE, 4.0, 2.0)
            .unwrap();
        assert_eq!(
            scaled, small,
            "the same pixel length is the same rasterization"
        );
        let large = cache
            .icon(&mut atlas, IconId(1), 0, RED_SQUARE, 16.0, 1.0)
            .unwrap();
        assert_eq!(large.width, 16);
        assert_ne!(large, small);
    }

    #[test]
    fn a_broken_source_draws_nothing() {
        let mut atlas = Atlas::new(64, 64);
        let mut cache = IconCache::new();
        assert_eq!(
            cache.icon(&mut atlas, IconId(2), 0, b"not an svg", 8.0, 1.0),
            None
        );
        assert!(cache.is_empty());
        assert!(atlas.take_updated_regions().is_empty());
    }

    #[test]
    fn an_atlas_too_small_for_the_icon_refuses_it() {
        let mut atlas = Atlas::new(4, 4);
        let mut cache = IconCache::new();
        assert_eq!(
            cache.icon(&mut atlas, IconId(1), 0, RED_SQUARE, 8.0, 1.0),
            None
        );
        assert!(cache.is_empty());
    }
}
