//! Text: fonts, shaping, line breaking, and rasterization into the atlas.
//!
//! # Responsibilities
//!
//! - [`TextSystem`]: owns the font database (the bundled font first, system
//!   fonts after), shapes text through `cosmic-text`, and rasterizes glyphs
//!   into the shared [`Atlas`] on demand, caching one sprite per glyph.
//! - [`TextStyle`], [`ShapedText`], [`ShapedGlyph`]: the plain data a caller
//!   hands in and gets back.
//!
//! # Design
//!
//! Font resolution starts from the bundled Geist (SIL OFL 1.1; the
//! licence ships beside it and in `THIRD_PARTY_NOTICES.md`), loaded from the
//! binary, so a machine without system fonts - a container, the web - still
//! draws text; system fonts are added on top for a caller asking for a
//! family by name. Shaping positions glyphs at whole pixels
//! (`LayoutGlyph::physical`), which keeps the atlas key compact and the text
//! crisp. Rasterization goes through swash's uncached path into our own
//! atlas: one region per (font, glyph, pixel size, subpixel bin), written
//! once and reused for every later frame.

// Standard library
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

// External crates
use cosmic_text::{
    Attrs, Buffer, CacheKey, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent,
};
use fontdb::Source;
use pill_core::math::Vector2f;

// Current crate
use crate::atlas::{Atlas, Region};
use crate::draw::{Color, DrawList, Rect, TextureId};

// =============================================================================
// Constants
// =============================================================================

/// The bundled default font, embedded so every build draws text without one.
const BUNDLED_FONT: &[u8] = include_bytes!("../assets/Geist-Variable.ttf");

/// Family name used when the bundled font reports no usable one.
const FALLBACK_FAMILY: &str = "Geist";

// =============================================================================
// Style and shaped output
// =============================================================================

/// How one run of text is shaped and drawn.
#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    /// Font size in logical pixels.
    pub font_size: f32,
    /// Line height in logical pixels; must be at least the font size for
    /// sane spacing.
    pub line_height: f32,
    /// Font family name, or `None` for the bundled default font.
    pub family: Option<String>,
}

impl Default for TextStyle {
    fn default() -> TextStyle {
        TextStyle {
            font_size: 16.0,
            line_height: 20.0,
            family: None,
        }
    }
}

/// One shaped glyph: where its pen sits and how to rasterize it.
#[derive(Clone, Copy, Debug)]
pub struct ShapedGlyph {
    /// The pen position in logical pixels, relative to the text's top-left
    /// corner, y down; whole pixels by construction.
    pub origin: Vector2f,
    /// The rasterization key: font, glyph, pixel size and subpixel bin.
    pub cache_key: CacheKey,
}

/// Text after shaping and line breaking.
#[derive(Clone, Debug, Default)]
pub struct ShapedText {
    /// The block's size in logical pixels: the longest line's width, and the
    /// stacked line heights.
    pub size: Vector2f,
    /// Every visible glyph of every line, in reading order.
    pub glyphs: Vec<ShapedGlyph>,
}

// =============================================================================
// TextSystem
// =============================================================================

/// A glyph's home in the atlas and how to place it.
#[derive(Clone, Copy, Debug)]
struct GlyphSprite {
    /// The atlas entry the glyph lives in. The region is looked up by key
    /// on every draw, so an entry the atlas evicted - or a page growth moved
    /// - never leaves a stale place behind here.
    key: u64,
    /// The rasterization scale the glyph belongs to, for
    /// [`TextSystem::evict_scale`].
    scale: u64,
    /// Offset from the glyph's pen origin to the image's top-left corner, in
    /// logical pixels.
    bearing: Vector2f,
    /// The image's size in logical pixels.
    size: Vector2f,
}

/// The atlas key of one rasterization: the cache key's bits, hashed.
fn entry_key(cache_key: &CacheKey) -> u64 {
    let mut hasher = DefaultHasher::new();
    cache_key.hash(&mut hasher);
    hasher.finish()
}

/// The tag grouping glyphs rasterized at `font_size` (quantized to quarter
/// pixels, the step a monitor change produces; plan decision D30).
fn scale_tag(font_size: f32) -> u64 {
    (font_size.max(0.0) * 4.0).round() as u64
}

/// Fonts, shaping and the glyph atlas, one instance per process (shared
/// `dylib`, plan decision D17).
pub struct TextSystem {
    /// cosmic-text's font system: the database plus the shaping caches.
    font_system: FontSystem,
    /// swash's rasterization context.
    swash_cache: SwashCache,
    /// The bundled font's family name, the default for unset styles.
    bundled_family: String,
    /// One atlas sprite per rasterized glyph.
    sprites: BTreeMap<CacheKey, GlyphSprite>,
    /// Memoized shapings: panels redraw the same labels (and most console
    /// rows) every frame, and shaping is the single most expensive step in
    /// a paint on a debug build (the D32 measurement: 222 ms/frame for the
    /// shell's panels before this cache).
    shape_cache: HashMap<ShapeKey, CachedShape>,
    /// Monotonic tick so the cache can evict the least recently used
    /// entry without a linked list.
    shape_tick: u64,
    /// Whether the one allowed "the atlas is full" warning was logged.
    atlas_full_warned: bool,
}

/// One memoized shaping: everything [`TextSystem::shape`] reads, with the
/// floats folded to bits so equal inputs compare equal.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ShapeKey {
    text: String,
    font_size: u32,
    line_height: u32,
    family: Option<String>,
    max_width: Option<u32>,
}

/// A memoized shaping and when it was last asked for.
struct CachedShape {
    shaped: ShapedText,
    used_at: u64,
}

/// How many distinct shapings stay memoized. The steady state is hits; the
/// bound only exists so a pathological caller cannot grow the map without
/// limit.
const SHAPE_CACHE_CAPACITY: usize = 1024;

impl TextSystem {
    /// A text system with the bundled font guaranteed and every system font
    /// the platform reports.
    pub fn new() -> TextSystem {
        let mut database = fontdb::Database::new();
        let bundled_id = database
            .load_font_source(Source::Binary(Arc::new(BUNDLED_FONT)))
            .first()
            .copied();
        let bundled_family = bundled_id
            .and_then(|id| database.face(id))
            .and_then(|face| face.families.first())
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| FALLBACK_FAMILY.to_string());

        // System fonts wherever a filesystem exists; the bundled font is the
        // guaranteed fallback (headless CI, minimal containers, the web).
        #[cfg(not(target_arch = "wasm32"))]
        database.load_system_fonts();

        let locale = sys_locale::get_locale().unwrap_or_else(|| String::from("en-US"));
        let font_system = FontSystem::new_with_locale_and_db(locale, database);
        TextSystem {
            font_system,
            swash_cache: SwashCache::new(),
            bundled_family,
            sprites: BTreeMap::new(),
            shape_cache: HashMap::new(),
            shape_tick: 0,
            atlas_full_warned: false,
        }
    }

    /// The bundled font's family name; the default of [`TextStyle::family`].
    pub fn bundled_family(&self) -> &str {
        &self.bundled_family
    }

    /// Shape `text` into positioned glyphs, wrapping at `max_width` when one
    /// is given. Repeated calls with the same inputs come out of the memo.
    pub fn shape(&mut self, text: &str, style: &TextStyle, max_width: Option<f32>) -> ShapedText {
        let key = ShapeKey {
            text: text.to_string(),
            font_size: style.font_size.to_bits(),
            line_height: style.line_height.to_bits(),
            family: style.family.clone(),
            max_width: max_width.map(f32::to_bits),
        };
        self.shape_tick += 1;
        if let Some(entry) = self.shape_cache.get_mut(&key) {
            entry.used_at = self.shape_tick;
            return entry.shaped.clone();
        }
        let shaped = self.shape_uncached(text, style, max_width);
        if self.shape_cache.len() >= SHAPE_CACHE_CAPACITY {
            let oldest = self
                .shape_cache
                .iter()
                .min_by_key(|(_, entry)| entry.used_at)
                .map(|(key, _)| (*key).clone());
            if let Some(oldest) = oldest {
                self.shape_cache.remove(&oldest);
            }
        }
        self.shape_cache.insert(
            key,
            CachedShape {
                shaped: shaped.clone(),
                used_at: self.shape_tick,
            },
        );
        shaped
    }

    /// The actual shaping, bypassing the memo.
    fn shape_uncached(
        &mut self,
        text: &str,
        style: &TextStyle,
        max_width: Option<f32>,
    ) -> ShapedText {
        let metrics = Metrics::new(style.font_size, style.line_height);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        buffer.set_size(max_width, None);
        let family = style
            .family
            .clone()
            .unwrap_or_else(|| self.bundled_family.clone());
        let attrs = Attrs::new().family(Family::Name(family.as_str()));
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.font_system, false);

        let mut shaped = ShapedText::default();
        for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                let physical = glyph.physical((0.0, run.line_y), 1.0);
                shaped.glyphs.push(ShapedGlyph {
                    origin: Vector2f::new(physical.x as f32, physical.y as f32),
                    cache_key: physical.cache_key,
                });
            }
            // A run with nothing in it (the sole run of an empty string) is
            // not a line; giving it height would make empty text measure as
            // one line tall.
            if run.glyphs.is_empty() {
                continue;
            }
            shaped.size.x = shaped.size.x.max(run.line_w);
            shaped.size.y = shaped.size.y.max(run.line_top + run.line_height);
        }
        shaped
    }

    /// The size [`Self::shape`] would report for `text`, shaping it.
    pub fn measure(&mut self, text: &str, style: &TextStyle, max_width: Option<f32>) -> Vector2f {
        self.shape(text, style, max_width).size
    }

    /// Draw `text` into `list`, with its top-left corner at `position`.
    ///
    /// Glyphs rasterize into `atlas` on first use and are reused afterwards;
    /// the returned size is what [`Self::measure`] would report.
    // The draw's inputs are all plain values a caller has anyway (the list,
    // the atlas, the text, its place, its style, its wrap and its colour);
    // naming them in a struct would invent a concept no other call shares.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text(
        &mut self,
        list: &mut DrawList,
        atlas: &mut Atlas,
        text: &str,
        position: Vector2f,
        style: &TextStyle,
        max_width: Option<f32>,
        color: Color,
    ) -> Vector2f {
        let shaped = self.shape(text, style, max_width);
        let scale = scale_tag(style.font_size);
        // Reserve every glyph before pushing anything: growing the atlas
        // re-scales every UV and eviction reuses slots, so no quad may go
        // out until all the placements this text needs have settled.
        let mut placed = Vec::with_capacity(shaped.glyphs.len());
        for glyph in &shaped.glyphs {
            if let Some((region, sprite)) = self.glyph(atlas, glyph.cache_key, scale) {
                placed.push((glyph.origin, region, sprite));
            }
        }
        for (origin, region, sprite) in placed {
            let rect = Rect::from_min_size(position + origin + sprite.bearing, sprite.size);
            let (uv_min, uv_max) = atlas.uv_rect(region);
            list.push_quad(TextureId::Atlas, rect, uv_min, uv_max, color);
        }
        shaped.size
    }

    /// Evict every glyph rasterized for `font_size`, returning how many
    /// atlas entries went; glyphs still on screen rasterize again next
    /// frame. Nodes use this when a window leaves the monitor whose scale
    /// factor produced that size (plan decision D30).
    pub fn evict_scale(&mut self, atlas: &mut Atlas, font_size: f32) -> usize {
        let tag = scale_tag(font_size);
        self.sprites.retain(|_, sprite| sprite.scale != tag);
        atlas.evict_tag(tag)
    }

    /// The live atlas region for `cache_key` and its sprite, rasterizing and
    /// caching the glyph on first use.
    ///
    /// The atlas may have evicted a cached sprite (a full atlas under its
    /// cap); the region lookup notices and the glyph rasterizes again, so a
    /// glyph that is still on screen never goes missing.
    fn glyph(
        &mut self,
        atlas: &mut Atlas,
        cache_key: CacheKey,
        scale: u64,
    ) -> Option<(Region, GlyphSprite)> {
        if let Some(sprite) = self.sprites.get(&cache_key).copied() {
            if let Some(region) = atlas.touch(sprite.key) {
                return Some((region, sprite));
            }
            self.sprites.remove(&cache_key);
        }
        let sprite = self.rasterize(atlas, cache_key, scale)?;
        let region = atlas.touch(sprite.key)?;
        Some((region, sprite))
    }

    /// Rasterize `key` into the atlas and cache its sprite.
    ///
    /// `None` for an empty image (a space) or an atlas that cannot fit the
    /// glyph even after growth and eviction; the caller then draws nothing
    /// for that glyph, and the atlas's refusal is logged once.
    fn rasterize(&mut self, atlas: &mut Atlas, key: CacheKey, scale: u64) -> Option<GlyphSprite> {
        let image = self
            .swash_cache
            .get_image_uncached(&mut self.font_system, key)?;
        let width = image.placement.width;
        let height = image.placement.height;
        if width == 0 || height == 0 {
            return None;
        }
        let entry = entry_key(&key);
        let Some(region) = atlas.reserve(entry, width, height, scale) else {
            if !self.atlas_full_warned {
                self.atlas_full_warned = true;
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::UI,
                    "[pill_ui] the glyph atlas is full; glyphs are not rasterized"
                );
            }
            return None;
        };

        // Every glyph becomes straight-alpha RGBA: white where the atlas
        // carries coverage, the bitmap's own colours where swash produced a
        // colour image. The draw's colour multiplies it at sampling time.
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        match image.content {
            SwashContent::Mask => {
                for (pixel, coverage) in rgba.chunks_exact_mut(4).zip(image.data.iter()) {
                    pixel.copy_from_slice(&[255, 255, 255, *coverage]);
                }
            }
            SwashContent::SubpixelMask => {
                // swash's RGB coverage; the atlas keeps one alpha channel.
                for (pixel, coverage) in rgba.chunks_exact_mut(4).zip(image.data.chunks_exact(4)) {
                    let average =
                        (coverage[0] as u16 + coverage[1] as u16 + coverage[2] as u16) / 3;
                    pixel.copy_from_slice(&[255, 255, 255, average as u8]);
                }
            }
            SwashContent::Color => {
                for (pixel, colour) in rgba.chunks_exact_mut(4).zip(image.data.chunks_exact(4)) {
                    pixel.copy_from_slice(colour);
                }
            }
        }
        atlas.write_region(region, &rgba);

        let sprite = GlyphSprite {
            key: entry,
            scale,
            bearing: Vector2f::new(image.placement.left as f32, -(image.placement.top as f32)),
            size: Vector2f::new(width as f32, height as f32),
        };
        self.sprites.insert(key, sprite);
        Some(sprite)
    }
}

impl Default for TextSystem {
    fn default() -> TextSystem {
        TextSystem::new()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shaping_produces_one_glyph_per_visible_character() {
        let mut text = TextSystem::new();
        let shaped = text.shape("AVA", &TextStyle::default(), None);
        assert_eq!(shaped.glyphs.len(), 3);
        assert!(shaped.size.x > 0.0);
        assert!(shaped.size.y > 0.0);
        // Every glyph of one line sits on the same baseline row.
        let y = shaped.glyphs[0].origin.y;
        assert!(shaped.glyphs.iter().all(|glyph| glyph.origin.y == y));

        let empty = text.shape("", &TextStyle::default(), None);
        assert!(empty.glyphs.is_empty());
        assert_eq!(empty.size, Vector2f::ZERO);
    }

    #[test]
    fn shaping_wraps_at_the_given_width() {
        let mut text = TextSystem::new();
        let style = TextStyle::default();
        let unwrapped = text.shape("alpha beta gamma delta", &style, None);
        assert_eq!(
            unwrapped.glyphs[0].origin.y,
            unwrapped.glyphs.last().unwrap().origin.y
        );

        let wrapped = text.shape("alpha beta gamma delta", &style, Some(40.0));
        let mut rows: Vec<f32> = wrapped.glyphs.iter().map(|glyph| glyph.origin.y).collect();
        rows.sort_by(f32::total_cmp);
        rows.dedup();
        assert!(rows.len() >= 2, "the text must wrap onto several lines");
        assert!(wrapped.size.y > unwrapped.size.y);
    }

    #[test]
    fn the_bundled_font_is_found() {
        let text = TextSystem::new();
        assert!(!text.bundled_family().is_empty());
    }

    #[test]
    fn drawing_text_lands_glyphs_in_the_atlas() {
        let mut text = TextSystem::new();
        let mut atlas = Atlas::new(256, 256);
        let mut list = DrawList::new();
        let size = text.draw_text(
            &mut list,
            &mut atlas,
            "hello",
            Vector2f::new(4.0, 4.0),
            &TextStyle::default(),
            None,
            Color::WHITE,
        );
        assert!(size.x > 0.0);
        assert!(!list.is_empty());
        assert!(!list.vertices.is_empty());

        let regions = atlas.take_updated_regions();
        assert!(!regions.is_empty(), "first draw rasterizes glyphs");
        assert!(atlas.take_updated_regions().is_empty(), "regions drain");
        for region in regions {
            assert!(region.x + region.width <= atlas.width());
            assert!(region.y + region.height <= atlas.height());
        }
    }

    #[test]
    fn a_text_larger_than_the_initial_atlas_grows_it_and_draws_every_glyph() {
        // Sixty-two distinct glyphs: more than a 64x64 image can hold, so
        // the first frame has to grow the atlas in pages. Every glyph must
        // still come out as a quad, with UVs valid for the grown image.
        let mut text = TextSystem::new();
        let mut atlas = Atlas::with_limit(64, 64, 512);
        let mut list = DrawList::new();
        let style = TextStyle::default();
        let sample = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let glyphs = text.shape(sample, &style, None).glyphs.len();
        assert!(glyphs >= 62);

        for frame in 0..2 {
            list.clear();
            text.draw_text(
                &mut list,
                &mut atlas,
                sample,
                Vector2f::ZERO,
                &style,
                None,
                Color::WHITE,
            );
            assert_eq!(
                list.vertices.len() / 4,
                glyphs,
                "every glyph draws, grown atlas or not"
            );
            // The second frame is all cache hits: nothing rasterized again.
            let dirty = atlas.take_updated_regions();
            if frame == 1 {
                assert!(dirty.is_empty());
            }
        }
        assert!(atlas.width() > 64, "the atlas grew a page");
    }

    #[test]
    fn a_glyph_survives_its_atlas_entry_being_evicted() {
        // Twenty-six glyphs against a 64-texel atlas: every frame evicts
        // and re-rasterizes, and every glyph still comes out as a quad.
        let mut text = TextSystem::new();
        let mut atlas = Atlas::with_limit(64, 64, 64);
        let mut list = DrawList::new();
        let style = TextStyle::default();
        let sample = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        for _frame in 0..2 {
            list.clear();
            text.draw_text(
                &mut list,
                &mut atlas,
                sample,
                Vector2f::ZERO,
                &style,
                None,
                Color::WHITE,
            );
            assert_eq!(list.vertices.len() / 4, 26, "no glyph is lost");
            atlas.take_updated_regions();
        }
    }

    #[test]
    fn evicting_a_scale_drops_exactly_its_glyphs() {
        let mut text = TextSystem::new();
        let mut atlas = Atlas::new(256, 256);
        let mut list = DrawList::new();
        let small = TextStyle {
            font_size: 13.0,
            line_height: 17.0,
            family: None,
        };
        let large = TextStyle {
            font_size: 26.0,
            line_height: 32.0,
            family: None,
        };
        text.draw_text(
            &mut list,
            &mut atlas,
            "scale",
            Vector2f::ZERO,
            &small,
            None,
            Color::WHITE,
        );
        text.draw_text(
            &mut list,
            &mut atlas,
            "scale",
            Vector2f::ZERO,
            &large,
            None,
            Color::WHITE,
        );
        assert!(atlas.entry_count() >= 10, "both scales are cached");

        let removed = text.evict_scale(&mut atlas, 13.0);
        assert!(
            removed >= 5,
            "the small scale's entries went, got {removed}"
        );

        // The other scale survives: drawing it rasterizes nothing again.
        atlas.take_updated_regions();
        list.clear();
        text.draw_text(
            &mut list,
            &mut atlas,
            "scale",
            Vector2f::ZERO,
            &large,
            None,
            Color::WHITE,
        );
        assert!(
            atlas.take_updated_regions().is_empty(),
            "the large scale stayed cached"
        );

        // The evicted scale comes back on demand.
        list.clear();
        text.draw_text(
            &mut list,
            &mut atlas,
            "scale",
            Vector2f::ZERO,
            &small,
            None,
            Color::WHITE,
        );
        assert!(
            !atlas.take_updated_regions().is_empty(),
            "the evicted scale rasterized again"
        );
    }

    #[test]
    fn a_second_draw_reuses_the_cached_glyphs() {
        let mut text = TextSystem::new();
        let mut atlas = Atlas::new(256, 256);
        let mut list = DrawList::new();
        let style = TextStyle::default();
        text.draw_text(
            &mut list,
            &mut atlas,
            "cache me",
            Vector2f::ZERO,
            &style,
            None,
            Color::WHITE,
        );
        atlas.take_updated_regions();

        list.clear();
        text.draw_text(
            &mut list,
            &mut atlas,
            "cache me",
            Vector2f::ZERO,
            &style,
            None,
            Color::WHITE,
        );
        assert!(
            !list.vertices.is_empty(),
            "the second draw still emits quads"
        );
        assert!(
            atlas.take_updated_regions().is_empty(),
            "a second draw of the same text must not rasterize again"
        );
    }

    #[test]
    fn repeated_shaping_is_memoized_and_bounded() {
        let mut system = TextSystem::new();
        let style = TextStyle::default();
        let first = system.shape("hello", &style, None);
        let second = system.shape("hello", &style, None);
        assert_eq!(first.glyphs.len(), second.glyphs.len());
        assert_eq!(
            system.shape_cache.len(),
            1,
            "the identical shaping must be one entry"
        );
        // A style or width difference is a different shaping, not a hit.
        system.shape("hello", &style, Some(100.0));
        assert_eq!(system.shape_cache.len(), 2);
        // The memo cannot grow past its capacity.
        for index in 0..(SHAPE_CACHE_CAPACITY + 16) {
            system.shape(&format!("label {index}"), &style, None);
        }
        assert!(system.shape_cache.len() <= SHAPE_CACHE_CAPACITY);
    }

    #[test]
    fn measurement_matches_a_shaped_draw() {
        let mut text = TextSystem::new();
        let mut atlas = Atlas::new(256, 256);
        let mut list = DrawList::new();
        let style = TextStyle::default();
        let measured = text.measure("measure", &style, None);
        let drawn = text.draw_text(
            &mut list,
            &mut atlas,
            "measure",
            Vector2f::ZERO,
            &style,
            None,
            Color::WHITE,
        );
        assert_eq!(measured, drawn);
    }
}
