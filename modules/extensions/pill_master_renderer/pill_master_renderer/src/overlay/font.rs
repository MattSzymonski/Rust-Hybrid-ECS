//! The bitmap font debug labels are drawn in: 5 by 7 pixel glyphs, drawn into
//! an atlas once, and the quads a line of text becomes.
//!
//! # Responsibilities
//!
//! - Hold the glyphs as readable pixel art ([`GLYPHS`]) and lay them out as
//!   one single-channel atlas ([`build_atlas`]).
//! - Hand that atlas to the UI painter as a texture under its own reserved
//!   id ([`texture_set`], [`FONT_TEXTURE`]).
//! - Turn a label into glyph quads at a pixel position ([`layout_text`]).
//!
//! # Design
//!
//! Debug labels need to be legible, not pretty, and must not depend on a font
//! file being found at runtime: the glyphs live in the source and the atlas is
//! built from them when the renderer starts. Lowercase letters draw as
//! uppercase, and a character the font lacks draws as `?`.

// Standard library
use std::sync::Arc;

// External crates
use pill_draw::{TextureId, UiTextureFilter, UiTextureSet};

/// Glyph width in pixels.
pub(super) const GLYPH_WIDTH: u32 = 5;
/// Glyph height in pixels.
pub(super) const GLYPH_HEIGHT: u32 = 7;
/// An atlas cell: the glyph plus one pixel of space right and below, so
/// sampling one glyph never bleeds into the next.
const CELL_WIDTH: u32 = GLYPH_WIDTH + 1;
const CELL_HEIGHT: u32 = GLYPH_HEIGHT + 1;
/// Cells per atlas row.
const ATLAS_COLUMNS: u32 = 16;
/// How many screen pixels one glyph pixel covers.
pub(super) const GLYPH_SCALE: f32 = 2.0;

/// Every glyph: its character and seven rows of five pixels, `#` lit, rows
/// separated by `/`.
pub(super) const GLYPHS: &[(char, &str)] = &[
    (' ', "...../...../...../...../...../...../....."),
    ('!', "..#../..#../..#../..#../..#../...../..#.."),
    ('"', ".#.#./.#.#./...../...../...../...../....."),
    ('#', ".#.#./.#.#./#####/.#.#./#####/.#.#./.#.#."),
    ('%', "##..#/##..#/...#./..#../.#.../#..##/#..##"),
    ('\'', "..#../..#../...../...../...../...../....."),
    ('(', "...#./..#../.#.../.#.../.#.../..#../...#."),
    (')', ".#.../..#../...#./...#./...#./..#../.#..."),
    ('*', "...../..#../#.#.#/.###./#.#.#/..#../....."),
    ('+', "...../..#../..#../#####/..#../..#../....."),
    (',', "...../...../...../...../.##../..#../.#..."),
    ('-', "...../...../...../#####/...../...../....."),
    ('.', "...../...../...../...../...../.##../.##.."),
    ('/', "...../....#/...#./..#../.#.../#..../....."),
    ('0', ".###./#...#/#..##/#.#.#/##..#/#...#/.###."),
    ('1', "..#../.##../..#../..#../..#../..#../.###."),
    ('2', ".###./#...#/....#/...#./..#../.#.../#####"),
    ('3', "#####/...#./..#../...#./....#/#...#/.###."),
    ('4', "...#./..##./.#.#./#..#./#####/...#./...#."),
    ('5', "#####/#..../####./....#/....#/#...#/.###."),
    ('6', "..##./.#.../#..../####./#...#/#...#/.###."),
    ('7', "#####/....#/...#./..#../.#.../.#.../.#..."),
    ('8', ".###./#...#/#...#/.###./#...#/#...#/.###."),
    ('9', ".###./#...#/#...#/.####/....#/...#./.##.."),
    (':', "...../.##../.##../...../.##../.##../....."),
    (';', "...../.##../.##../...../.##../..#../.#..."),
    ('<', "...#./..#../.#.../#..../.#.../..#../...#."),
    ('=', "...../...../#####/...../#####/...../....."),
    ('>', ".#.../..#../...#./....#/...#./..#../.#..."),
    ('?', ".###./#...#/....#/...#./..#../...../..#.."),
    ('A', ".###./#...#/#...#/#####/#...#/#...#/#...#"),
    ('B', "####./#...#/#...#/####./#...#/#...#/####."),
    ('C', ".###./#...#/#..../#..../#..../#...#/.###."),
    ('D', "####./#...#/#...#/#...#/#...#/#...#/####."),
    ('E', "#####/#..../#..../####./#..../#..../#####"),
    ('F', "#####/#..../#..../####./#..../#..../#...."),
    ('G', ".###./#...#/#..../#.###/#...#/#...#/.####"),
    ('H', "#...#/#...#/#...#/#####/#...#/#...#/#...#"),
    ('I', ".###./..#../..#../..#../..#../..#../.###."),
    ('J', "..###/...#./...#./...#./...#./#..#./.##.."),
    ('K', "#...#/#..#./#.#../##.../#.#../#..#./#...#"),
    ('L', "#..../#..../#..../#..../#..../#..../#####"),
    ('M', "#...#/##.##/#.#.#/#.#.#/#...#/#...#/#...#"),
    ('N', "#...#/#...#/##..#/#.#.#/#..##/#...#/#...#"),
    ('O', ".###./#...#/#...#/#...#/#...#/#...#/.###."),
    ('P', "####./#...#/#...#/####./#..../#..../#...."),
    ('Q', ".###./#...#/#...#/#...#/#.#.#/#..#./.##.#"),
    ('R', "####./#...#/#...#/####./#.#../#..#./#...#"),
    ('S', ".####/#..../#..../.###./....#/....#/####."),
    ('T', "#####/..#../..#../..#../..#../..#../..#.."),
    ('U', "#...#/#...#/#...#/#...#/#...#/#...#/.###."),
    ('V', "#...#/#...#/#...#/#...#/#...#/.#.#./..#.."),
    ('W', "#...#/#...#/#...#/#.#.#/#.#.#/#.#.#/.#.#."),
    ('X', "#...#/#...#/.#.#./..#../.#.#./#...#/#...#"),
    ('Y', "#...#/#...#/.#.#./..#../..#../..#../..#.."),
    ('Z', "#####/....#/...#./..#../.#.../#..../#####"),
    ('[', ".###./.#.../.#.../.#.../.#.../.#.../.###."),
    ('\\', "...../#..../.#.../..#../...#./....#/....."),
    (']', ".###./...#./...#./...#./...#./...#./.###."),
    ('_', "...../...../...../...../...../...../#####"),
    ('|', "..#../..#../..#../..#../..#../..#../..#.."),
];

/// The atlas: its size and one coverage byte per texel, row by row.
pub(super) struct Atlas {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// `255` where a glyph pixel is lit, `0` elsewhere.
    pub texels: Vec<u8>,
}

/// The texture the label list samples the font from.
///
/// Reserved out of the game UI's external range: `pill_game_ui` numbers
/// egui's managed textures `1 << 63 | id`, and egui never reaches the id that
/// would make this one.
pub(crate) const FONT_TEXTURE: TextureId = TextureId::External(u64::MAX);

/// The atlas's width and height in texels: every glyph's cell, in rows of
/// [`ATLAS_COLUMNS`].
pub(super) fn atlas_size() -> (u32, u32) {
    let rows = (GLYPHS.len() as u32).div_ceil(ATLAS_COLUMNS);
    (ATLAS_COLUMNS * CELL_WIDTH, rows * CELL_HEIGHT)
}

/// Draw every glyph of [`GLYPHS`] into its cell, in table order.
pub(super) fn build_atlas() -> Atlas {
    let (width, height) = atlas_size();
    let mut texels = vec![0u8; (width * height) as usize];
    for (index, (_, art)) in GLYPHS.iter().enumerate() {
        let (cell_x, cell_y) = cell_origin(index);
        for (row, pixels) in art.split('/').enumerate() {
            for (column, pixel) in pixels.chars().enumerate() {
                if pixel == '#' {
                    let x = cell_x + column as u32;
                    let y = cell_y + row as u32;
                    texels[(y * width + x) as usize] = 255;
                }
            }
        }
    }
    Atlas {
        width,
        height,
        texels,
    }
}

/// The atlas as the one-texture set the UI painter registers once: white
/// texels whose alpha is the glyph coverage, sampled nearest so glyphs stay
/// crisp at [`GLYPH_SCALE`].
pub(crate) fn texture_set() -> UiTextureSet {
    let atlas = build_atlas();
    let pixels: Vec<u8> = atlas
        .texels
        .iter()
        .flat_map(|&coverage| [255, 255, 255, coverage])
        .collect();
    let mut set = UiTextureSet::new();
    set.push_full(
        FONT_TEXTURE,
        atlas.width,
        atlas.height,
        1,
        UiTextureFilter::Nearest,
        Arc::from(pixels),
    );
    set
}

/// The top-left texel of glyph `index`'s cell.
fn cell_origin(index: usize) -> (u32, u32) {
    let index = index as u32;
    (
        (index % ATLAS_COLUMNS) * CELL_WIDTH,
        (index / ATLAS_COLUMNS) * CELL_HEIGHT,
    )
}

/// The glyph a character draws as: itself, its uppercase form, or `?`.
fn glyph_index(character: char) -> usize {
    let wanted = character.to_ascii_uppercase();
    let lookup = |target: char| GLYPHS.iter().position(|(glyph, _)| *glyph == target);
    lookup(wanted).or_else(|| lookup('?')).unwrap_or(0)
}

/// One glyph quad: its pixel rectangle and its atlas rectangle in texels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct GlyphQuad {
    /// Left and top screen pixel.
    pub origin: [f32; 2],
    /// Width and height in screen pixels.
    pub size: [f32; 2],
    /// Left and top atlas texel.
    pub atlas_origin: [u32; 2],
}

/// The quads `text` becomes, starting at pixel (`x`, `y`) as its top-left
/// corner; a newline starts the next line under the first character.
pub(super) fn layout_text(text: &str, x: f32, y: f32) -> Vec<GlyphQuad> {
    let advance = CELL_WIDTH as f32 * GLYPH_SCALE;
    let line_height = (CELL_HEIGHT + 2) as f32 * GLYPH_SCALE;
    let size = [
        GLYPH_WIDTH as f32 * GLYPH_SCALE,
        GLYPH_HEIGHT as f32 * GLYPH_SCALE,
    ];
    let mut quads = Vec::with_capacity(text.len());
    let (mut pen_x, mut pen_y) = (x, y);
    for character in text.chars() {
        if character == '\n' {
            pen_x = x;
            pen_y += line_height;
            continue;
        }
        // A space advances without drawing anything.
        if character != ' ' {
            let (atlas_x, atlas_y) = cell_origin(glyph_index(character));
            quads.push(GlyphQuad {
                origin: [pen_x, pen_y],
                size,
                atlas_origin: [atlas_x, atlas_y],
            });
        }
        pen_x += advance;
    }
    quads
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The painter's texture is the atlas widened to RGBA: coverage in alpha.
    #[test]
    fn the_texture_set_carries_the_atlas_as_white_coverage() {
        let atlas = build_atlas();
        let set = texture_set();
        let texture = &set.textures[0];
        assert_eq!((texture.width, texture.height), atlas_size());
        let pixels = texture.full_image().expect("the whole image");
        let lit = atlas
            .texels
            .iter()
            .position(|&texel| texel == 255)
            .expect("a lit texel");
        assert_eq!(&pixels[lit * 4..lit * 4 + 4], &[255, 255, 255, 255]);
        assert_eq!(
            &pixels[..4],
            &[255, 255, 255, 0],
            "the first texel is blank"
        );
    }

    /// Every glyph is seven rows of five pixels, so none draws garbage.
    #[test]
    fn every_glyph_is_five_by_seven() {
        for (character, art) in GLYPHS {
            let rows: Vec<&str> = art.split('/').collect();
            assert_eq!(rows.len(), GLYPH_HEIGHT as usize, "{character:?}");
            for row in rows {
                assert_eq!(row.len(), GLYPH_WIDTH as usize, "{character:?}");
                assert!(row.chars().all(|pixel| pixel == '#' || pixel == '.'));
            }
        }
    }

    /// The atlas holds every glyph, and a lit glyph pixel is lit in it.
    #[test]
    fn the_atlas_holds_every_glyph() {
        let atlas = build_atlas();
        assert_eq!(atlas.texels.len(), (atlas.width * atlas.height) as usize);
        // The top-left pixel of `#` is off, the second one is on.
        let index = GLYPHS.iter().position(|(glyph, _)| *glyph == '#').unwrap();
        let (x, y) = cell_origin(index);
        assert_eq!(atlas.texels[(y * atlas.width + x) as usize], 0);
        assert_eq!(atlas.texels[(y * atlas.width + x + 1) as usize], 255);
    }

    /// Lowercase draws as uppercase, an unknown character as `?`, and a
    /// space draws nothing but still advances.
    #[test]
    fn text_lays_out_left_to_right() {
        let quads = layout_text("a b", 10.0, 20.0);
        assert_eq!(quads.len(), 2);
        assert_eq!(quads[0].origin, [10.0, 20.0]);
        assert_eq!(
            quads[1].origin[0],
            10.0 + 2.0 * CELL_WIDTH as f32 * GLYPH_SCALE
        );
        assert_eq!(glyph_index('a'), glyph_index('A'));
        assert_eq!(glyph_index('~'), glyph_index('?'));
    }

    /// A newline returns to the first column on the next line.
    #[test]
    fn a_newline_starts_the_next_line() {
        let quads = layout_text("A\nB", 0.0, 0.0);
        assert_eq!(quads[1].origin[0], 0.0);
        assert!(quads[1].origin[1] > quads[0].origin[1]);
    }
}
