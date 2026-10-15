//! SVG icons: rasterized on the CPU into the shared atlas.
//!
//! # Responsibilities
//!
//! - [`IconCache`]: rasterizes SVG icon sources through resvg into the shared
//!   [`Atlas`], one region per (icon, revision, pixel length), and
//!   re-rasterizes when the caller's revision moves - the asset reloaded.
//! - [`EDITOR_ICONS`] / [`bundled_svg`]: the bundled editor icon set, the
//!   curated Phosphor subset (MIT; see `THIRD_PARTY_NOTICES.md`), white so a
//!   draw's vertex colour tints it.
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
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

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
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct IconKey {
    /// The caller's icon identity.
    id: IconId,
    /// The caller's revision; a change re-rasterizes.
    revision: u64,
    /// The rasterized side length in physical pixels.
    length: u32,
}

/// The icon half of the shared atlas: rasterize once, draw many times.
///
/// The atlas holds the registry: an icon asks for its entry by key and
/// rasterizes again when the lookup misses, so a full atlas evicting an
/// icon costs one re-render, never a missing picture.
#[derive(Clone, Debug, Default)]
pub struct IconCache {
    /// Whether the one allowed "the atlas is full" warning was logged.
    atlas_full_warned: bool,
}

/// The tag icons carry in the atlas; scale eviction never collects them
/// (a glyph's tag is its small rasterization scale, far below this).
const ICON_TAG: u64 = u64::MAX;

/// The atlas key of one icon rasterization; the leading zero byte keeps it
/// apart from a glyph's key, which hashes its cache key directly.
fn entry_key(key: &IconKey) -> u64 {
    let mut hasher = DefaultHasher::new();
    0u8.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
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
        let entry = entry_key(&key);
        if let Some(region) = atlas.touch(entry) {
            return Some(region);
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

        let Some(region) = atlas.reserve(entry, length, length, ICON_TAG) else {
            if !self.atlas_full_warned {
                self.atlas_full_warned = true;
                pill_core::warn!(
                    target: telemetry_target::UI,
                    "[pill_ui] the icon atlas is full; icons are not rasterized"
                );
            }
            return None;
        };
        atlas.write_region(region, &rgba);
        Some(region)
    }
}

// =============================================================================
// The bundled editor icon set
// =============================================================================

/// The bundled editor icons: the curated Phosphor subset (regular weight,
/// MIT), white so a draw tints them with its vertex colour. Names are stable;
/// consumers choose their own [`IconId`] for caching.
pub static EDITOR_ICONS: &[(&str, &[u8])] = &[
    (
        "arrow-counter-clockwise",
        include_bytes!("../assets/icons/arrow-counter-clockwise.svg"),
    ),
    (
        "arrows-clockwise",
        include_bytes!("../assets/icons/arrows-clockwise.svg"),
    ),
    (
        "arrows-out",
        include_bytes!("../assets/icons/arrows-out.svg"),
    ),
    (
        "arrows-out-cardinal",
        include_bytes!("../assets/icons/arrows-out-cardinal.svg"),
    ),
    ("bug", include_bytes!("../assets/icons/bug.svg")),
    (
        "caret-down",
        include_bytes!("../assets/icons/caret-down.svg"),
    ),
    (
        "caret-left",
        include_bytes!("../assets/icons/caret-left.svg"),
    ),
    (
        "caret-right",
        include_bytes!("../assets/icons/caret-right.svg"),
    ),
    ("caret-up", include_bytes!("../assets/icons/caret-up.svg")),
    ("check", include_bytes!("../assets/icons/check.svg")),
    (
        "check-circle",
        include_bytes!("../assets/icons/check-circle.svg"),
    ),
    ("copy", include_bytes!("../assets/icons/copy.svg")),
    ("cube", include_bytes!("../assets/icons/cube.svg")),
    ("cursor", include_bytes!("../assets/icons/cursor.svg")),
    (
        "dots-three",
        include_bytes!("../assets/icons/dots-three.svg"),
    ),
    (
        "dots-three-outline",
        include_bytes!("../assets/icons/dots-three-outline.svg"),
    ),
    (
        "download-simple",
        include_bytes!("../assets/icons/download-simple.svg"),
    ),
    ("export", include_bytes!("../assets/icons/export.svg")),
    ("eye", include_bytes!("../assets/icons/eye.svg")),
    ("eye-slash", include_bytes!("../assets/icons/eye-slash.svg")),
    ("file", include_bytes!("../assets/icons/file.svg")),
    ("file-code", include_bytes!("../assets/icons/file-code.svg")),
    ("file-text", include_bytes!("../assets/icons/file-text.svg")),
    (
        "floppy-disk",
        include_bytes!("../assets/icons/floppy-disk.svg"),
    ),
    ("folder", include_bytes!("../assets/icons/folder.svg")),
    (
        "folder-open",
        include_bytes!("../assets/icons/folder-open.svg"),
    ),
    ("gear", include_bytes!("../assets/icons/gear.svg")),
    ("grid-four", include_bytes!("../assets/icons/grid-four.svg")),
    ("image", include_bytes!("../assets/icons/image.svg")),
    ("info", include_bytes!("../assets/icons/info.svg")),
    ("lightbulb", include_bytes!("../assets/icons/lightbulb.svg")),
    ("list", include_bytes!("../assets/icons/list.svg")),
    ("lock", include_bytes!("../assets/icons/lock.svg")),
    ("lock-open", include_bytes!("../assets/icons/lock-open.svg")),
    ("magnet", include_bytes!("../assets/icons/magnet.svg")),
    (
        "magnifying-glass",
        include_bytes!("../assets/icons/magnifying-glass.svg"),
    ),
    ("minus", include_bytes!("../assets/icons/minus.svg")),
    (
        "paint-brush",
        include_bytes!("../assets/icons/paint-brush.svg"),
    ),
    ("palette", include_bytes!("../assets/icons/palette.svg")),
    ("pause", include_bytes!("../assets/icons/pause.svg")),
    ("play", include_bytes!("../assets/icons/play.svg")),
    ("plus", include_bytes!("../assets/icons/plus.svg")),
    ("question", include_bytes!("../assets/icons/question.svg")),
    (
        "sliders-horizontal",
        include_bytes!("../assets/icons/sliders-horizontal.svg"),
    ),
    ("sparkle", include_bytes!("../assets/icons/sparkle.svg")),
    ("stop", include_bytes!("../assets/icons/stop.svg")),
    ("sun", include_bytes!("../assets/icons/sun.svg")),
    (
        "terminal-window",
        include_bytes!("../assets/icons/terminal-window.svg"),
    ),
    ("trash", include_bytes!("../assets/icons/trash.svg")),
    (
        "tree-structure",
        include_bytes!("../assets/icons/tree-structure.svg"),
    ),
    (
        "upload-simple",
        include_bytes!("../assets/icons/upload-simple.svg"),
    ),
    ("warning", include_bytes!("../assets/icons/warning.svg")),
    (
        "warning-circle",
        include_bytes!("../assets/icons/warning-circle.svg"),
    ),
    ("x", include_bytes!("../assets/icons/x.svg")),
    ("x-circle", include_bytes!("../assets/icons/x-circle.svg")),
];

/// The SVG source of a bundled icon, by name.
pub fn bundled_svg(name: &str) -> Option<&'static [u8]> {
    EDITOR_ICONS
        .iter()
        .find(|(bundled_name, _)| *bundled_name == name)
        .map(|(_, source)| *source)
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
        assert_eq!(atlas.entry_count(), 1);
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
        assert_eq!(atlas.entry_count(), 2, "both revisions stay cached");
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
        assert_eq!(atlas.entry_count(), 0);
        assert!(atlas.take_updated_regions().is_empty());
    }

    #[test]
    fn an_icon_bigger_than_the_atlas_cap_is_refused() {
        let mut atlas = Atlas::with_limit(4, 4, 4);
        let mut cache = IconCache::new();
        assert_eq!(
            cache.icon(&mut atlas, IconId(1), 0, RED_SQUARE, 8.0, 1.0),
            None
        );
        assert_eq!(atlas.entry_count(), 0);
    }

    #[test]
    fn every_bundled_icon_is_named_once_and_not_empty() {
        let mut names: Vec<&str> = EDITOR_ICONS.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), EDITOR_ICONS.len(), "icon names are unique");
        assert!(EDITOR_ICONS.len() >= 40, "the editor set stays curated");
        for (name, svg) in EDITOR_ICONS {
            assert!(!name.is_empty());
            assert!(!svg.is_empty(), "{name} has bytes");
        }
        assert!(bundled_svg("caret-down").is_some());
        assert!(bundled_svg("not-an-icon").is_none());
    }

    #[test]
    fn every_bundled_icon_rasterizes_its_own_texels_at_every_scale() {
        // 100/125/150/200% monitor scales rasterize the 16 px icon to
        // exactly 16/20/24/32 physical pixels: each scale gets its own crisp
        // texels rather than an upscaled bitmap (the mixed-DPI contract).
        let mut atlas = Atlas::new(1024, 1024);
        let mut cache = IconCache::new();
        for (index, (name, svg)) in EDITOR_ICONS.iter().enumerate() {
            for scale in [1.0f32, 1.25, 1.5, 2.0] {
                let expected = (16.0 * scale).round() as u32;
                let region = cache
                    .icon(&mut atlas, IconId(index as u64), 0, svg, 16.0, scale)
                    .unwrap_or_else(|| panic!("{name} must rasterize at {scale}"));
                assert_eq!(region.width, expected, "{name} at {scale}");
                assert_eq!(region.height, expected, "{name} at {scale}");
                let mut has_ink = false;
                for y in 0..region.height {
                    let row = (region.y as usize + y as usize) * atlas.width() as usize;
                    for x in 0..region.width {
                        let alpha = atlas.pixels()[(row + region.x as usize + x as usize) * 4 + 3];
                        has_ink |= alpha > 0;
                    }
                }
                assert!(has_ink, "{name} at {scale} draws something");
            }
        }
    }
}
