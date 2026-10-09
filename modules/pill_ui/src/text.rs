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
//! Font resolution starts from the bundled Open Sans (SIL OFL 1.1; the
//! licence ships beside it and in `THIRD_PARTY_NOTICES.md`), loaded from the
//! binary, so a machine without system fonts - a container, the web - still
//! draws text; system fonts are added on top for a caller asking for a
//! family by name. Shaping positions glyphs at whole pixels
//! (`LayoutGlyph::physical`), which keeps the atlas key compact and the text
//! crisp. Rasterization goes through swash's uncached path into our own
//! atlas: one region per (font, glyph, pixel size, subpixel bin), written
//! once and reused for every later frame.

// Standard library
use std::collections::BTreeMap;
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
const BUNDLED_FONT: &[u8] = include_bytes!("../assets/OpenSans-Variable.ttf");

/// Family name used when the bundled font reports no usable one.
const FALLBACK_FAMILY: &str = "Open Sans";

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
    /// The glyph image's region.
    region: Region,
    /// Offset from the glyph's pen origin to the image's top-left corner, in
    /// logical pixels.
    bearing: Vector2f,
    /// The image's size in logical pixels.
    size: Vector2f,
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
}

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
        }
    }

    /// The bundled font's family name; the default of [`TextStyle::family`].
    pub fn bundled_family(&self) -> &str {
        &self.bundled_family
    }

    /// Shape `text` into positioned glyphs, wrapping at `max_width` when one
    /// is given.
    pub fn shape(&mut self, text: &str, style: &TextStyle, max_width: Option<f32>) -> ShapedText {
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
        for glyph in &shaped.glyphs {
            let Some(sprite) = self.glyph_sprite(atlas, glyph.cache_key) else {
                continue;
            };
            let rect = Rect::from_min_size(position + glyph.origin + sprite.bearing, sprite.size);
            let (uv_min, uv_max) = atlas.uv_rect(sprite.region);
            list.push_quad(TextureId::Atlas, rect, uv_min, uv_max, color);
        }
        shaped.size
    }

    /// The atlas sprite for `key`, rasterizing and caching it on first use.
    ///
    /// `None` for an empty image (a space) or a full atlas; the caller then
    /// draws nothing for that glyph.
    fn glyph_sprite(&mut self, atlas: &mut Atlas, key: CacheKey) -> Option<GlyphSprite> {
        if let Some(sprite) = self.sprites.get(&key) {
            return Some(*sprite);
        }
        let image = self
            .swash_cache
            .get_image_uncached(&mut self.font_system, key)?;
        let width = image.placement.width;
        let height = image.placement.height;
        if width == 0 || height == 0 {
            return None;
        }
        let region = atlas.allocate(width, height)?;

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
            region,
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
