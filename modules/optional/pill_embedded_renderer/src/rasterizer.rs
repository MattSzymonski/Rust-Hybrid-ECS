//! Software rasterizer: turns sprite instances into framebuffer pixels.
//!
//! # Responsibilities
//!
//! - Draws every `(Position, Sprite)` entity as an axis-aligned rectangle
//!   ([`SpriteRasterizer::render_in_viewport_with_resolution`]).
//! - Maps the project coordinate space onto the physical viewport.
//! - Clips each rectangle to the viewport before touching any pixel.
//!
//! # Design
//!
//! This is the CPU counterpart of the wgpu backend's shader and render pass,
//! and it deliberately mirrors that structure: collect instances, resolve the
//! projection, clear, then draw. What the vertex shader did per-vertex -
//! scaling a unit quad by `size`, offsetting by `position`, and mapping the
//! result into the target - is done here once per sprite, in
//! [`project_span`], because an axis-aligned rectangle needs no per-pixel
//! interpolation at all.
//!
//! Sprites are opaque rectangles far more often than not, so the inner loop
//! splits: a fully opaque sprite becomes a `fill` of packed pixels, and only a
//! translucent one pays for unpack-blend-repack per pixel. On the class of CPU
//! this renderer targets that distinction is the difference between a
//! comfortable frame and a missed one.
//!
//! There is no depth buffer and no sorting. Sprites paint in the order the ECS
//! reports them, which is archetype order - the same order the wgpu backend
//! submits its instances in, so the two agree on overlap.

// External crates
use pill_engine::world::World;

// Current crate
use crate::component::{sprite_instances, RenderViewport, SpriteInstance, VirtualResolution};
use crate::framebuffer::{pack_rgb565, Framebuffer};

// =============================================================================
// Constants
// =============================================================================

/// Viewport background, as sRGB channel values in the 0.0-1.0 range.
///
/// A dark desaturated blue-grey (roughly `#23282F`), the same colour the wgpu
/// backend clears to, so a project looks the same on a display as on a window.
///
/// No sRGB-to-linear conversion happens here, unlike in the wgpu backend: this
/// renderer writes RGB565 straight to a display controller that expects sRGB
/// values, so converting would wash the background out.
const VIEWPORT_BACKGROUND_SRGB: [f32; 3] = [35.0 / 255.0, 40.0 / 255.0, 47.0 / 255.0];

/// Alpha at or below which a sprite contributes nothing and is skipped.
const ALPHA_TRANSPARENT: f32 = 0.0;

/// Alpha at or above which a sprite is treated as fully opaque.
///
/// Above this the blend result is indistinguishable after RGB565 quantization,
/// so taking the fill path costs nothing visually and saves the per-pixel
/// unpack and repack.
const ALPHA_OPAQUE: f32 = 0.998;

// =============================================================================
// SpriteRasterizer
// =============================================================================

/// Draws sprite instances into a [`Framebuffer`] on the CPU.
///
/// Holds only a reusable instance scratch buffer: the framebuffer is owned by
/// the caller, which is what lets one rasterizer serve several targets.
#[derive(Default)]
pub struct SpriteRasterizer {
    /// Instances collected from the world, reused across frames.
    ///
    /// Kept as a field purely to avoid reallocating a vector of a few hundred
    /// records every frame.
    instances: Vec<SpriteInstance>,
}

impl SpriteRasterizer {
    /// Create a rasterizer with an empty instance buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Draw every `(Position, Sprite)` entity into `framebuffer`.
    ///
    /// `viewport` is the physical-pixel rectangle to draw inside, and
    /// `virtual_resolution` is the project coordinate space mapped onto it.
    /// The whole framebuffer is cleared first - the wgpu backend clears its
    /// entire target too, so a viewport smaller than the display leaves the
    /// background colour around it rather than stale pixels.
    pub fn render_in_viewport_with_resolution(
        &mut self,
        world: &World,
        framebuffer: &mut Framebuffer,
        viewport: RenderViewport,
        virtual_resolution: VirtualResolution,
    ) {
        debug_assert!(virtual_resolution.is_valid());

        // Step 1: Clear the whole target to the background colour.
        let [background_red, background_green, background_blue] = VIEWPORT_BACKGROUND_SRGB;
        framebuffer.clear(pack_rgb565(
            background_red,
            background_green,
            background_blue,
        ));

        // Step 2: Clip the viewport to the framebuffer. An empty intersection
        // means there is nowhere to draw, and every sprite would be discarded
        // individually anyway.
        let Some(viewport) = viewport.clamped_to(framebuffer.width(), framebuffer.height()) else {
            return;
        };
        if !virtual_resolution.is_valid() {
            return;
        }

        // Step 3: Collect this frame's drawable entities.
        self.instances.clear();
        self.instances.extend(sprite_instances(world));

        // Step 4: Rasterize each sprite into the viewport.
        //
        // The scale converts project units to physical pixels on each axis
        // independently, which is what lets a fixed project resolution fill a
        // display of a different aspect ratio - the same stretch the GPU
        // viewport transform performs in the wgpu backend.
        let scale_x = viewport.width as f32 / virtual_resolution.width;
        let scale_y = viewport.height as f32 / virtual_resolution.height;
        for instance in &self.instances {
            draw_sprite(framebuffer, viewport, scale_x, scale_y, instance);
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Rasterize one sprite instance into the framebuffer.
fn draw_sprite(
    framebuffer: &mut Framebuffer,
    viewport: RenderViewport,
    scale_x: f32,
    scale_y: f32,
    instance: &SpriteInstance,
) {
    let [red, green, blue, alpha] = instance.color;

    // A fully transparent sprite contributes nothing; skip before any maths.
    if !(alpha > ALPHA_TRANSPARENT) {
        return;
    }

    // Project the rectangle into physical pixels and clip it to the viewport.
    let Some((x_start, x_end)) = project_span(
        instance.position[0],
        instance.size[0],
        scale_x,
        viewport.x,
        viewport.x + viewport.width,
    ) else {
        return;
    };
    let Some((y_start, y_end)) = project_span(
        instance.position[1],
        instance.size[1],
        scale_y,
        viewport.y,
        viewport.y + viewport.height,
    ) else {
        return;
    };

    // Opaque sprites take the fill path: one packed colour written straight
    // into the row, with no read-modify-write per pixel.
    if alpha >= ALPHA_OPAQUE {
        let pixel = pack_rgb565(red, green, blue);
        for y in y_start..y_end {
            framebuffer.fill_span(y, x_start, x_end, pixel);
        }
        return;
    }

    for y in y_start..y_end {
        framebuffer.blend_span(y, x_start, x_end, red, green, blue, alpha);
    }
}

/// Project one axis of a sprite into a clipped half-open pixel range.
///
/// `origin` and `extent` are in project units; the result is in physical
/// pixels, offset by `viewport_start` and clipped to `viewport_end`. Returns
/// `None` when the sprite falls outside the viewport or rounds away to nothing.
///
/// Edges are floored rather than rounded, and the range is half-open, so
/// neighbouring sprites that share an edge in project space tile without
/// overlapping or leaving a seam.
fn project_span(
    origin: f32,
    extent: f32,
    scale: f32,
    viewport_start: u32,
    viewport_end: u32,
) -> Option<(u32, u32)> {
    // A non-finite or non-positive extent has no pixels to cover. Checking
    // `extent > 0.0` also rejects NaN, which would otherwise survive the
    // comparisons below and produce a garbage range.
    if !(extent > 0.0) || !origin.is_finite() || !scale.is_finite() {
        return None;
    }

    let start = origin * scale;
    let end = (origin + extent) * scale;

    // Clip in float space before casting: a sprite far off-screen can exceed
    // u32 range entirely, and `as` saturates rather than wrapping, which would
    // silently clamp it to the viewport edge instead of discarding it.
    let viewport_extent = viewport_end.saturating_sub(viewport_start) as f32;
    if end <= 0.0 || start >= viewport_extent {
        return None;
    }

    let start = start.max(0.0).floor() as u32;
    let end = end.min(viewport_extent).floor() as u32;
    if start >= end {
        return None;
    }

    Some((viewport_start + start, viewport_start + end))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::unpack_rgb565;

    /// One instance covering a rectangle of the given colour.
    fn instance(position: [f32; 2], size: [f32; 2], color: [f32; 4]) -> SpriteInstance {
        SpriteInstance {
            position,
            size,
            color,
        }
    }

    /// The background colour a cleared frame carries, for comparisons.
    fn background() -> u16 {
        let [red, green, blue] = VIEWPORT_BACKGROUND_SRGB;
        pack_rgb565(red, green, blue)
    }

    /// A unit-scale sprite lands on exactly the pixels it covers.
    #[test]
    fn sprite_covers_its_own_rectangle_only() {
        let mut framebuffer = Framebuffer::new(8, 8);
        framebuffer.clear(background());
        let viewport = RenderViewport::full(8, 8);

        draw_sprite(
            &mut framebuffer,
            viewport,
            1.0,
            1.0,
            &instance([2.0, 3.0], [3.0, 2.0], [1.0, 0.0, 0.0, 1.0]),
        );

        let red = pack_rgb565(1.0, 0.0, 0.0);
        for y in 0..8 {
            for x in 0..8 {
                let covered = (2..5).contains(&x) && (3..5).contains(&y);
                let expected = if covered { red } else { background() };
                assert_eq!(
                    framebuffer.pixel(x, y),
                    Some(expected),
                    "pixel ({x}, {y}) covered={covered}"
                );
            }
        }
    }

    /// A sprite is offset by the viewport origin and confined to it.
    #[test]
    fn sprite_is_offset_by_and_clipped_to_the_viewport() {
        let mut framebuffer = Framebuffer::new(8, 8);
        framebuffer.clear(background());
        let viewport = RenderViewport::new(4, 4, 2, 2);

        // Four project units wide, but the viewport is only two pixels.
        draw_sprite(
            &mut framebuffer,
            viewport,
            1.0,
            1.0,
            &instance([0.0, 0.0], [4.0, 4.0], [1.0, 1.0, 1.0, 1.0]),
        );

        let white = pack_rgb565(1.0, 1.0, 1.0);
        assert_eq!(framebuffer.pixel(4, 4), Some(white));
        assert_eq!(framebuffer.pixel(5, 5), Some(white));
        // Outside the viewport rectangle nothing is painted.
        assert_eq!(framebuffer.pixel(3, 3), Some(background()));
        assert_eq!(framebuffer.pixel(6, 6), Some(background()));
    }

    /// Sprites fully outside the viewport are discarded.
    #[test]
    fn offscreen_sprites_draw_nothing() {
        let mut framebuffer = Framebuffer::new(4, 4);
        framebuffer.clear(background());
        let viewport = RenderViewport::full(4, 4);

        for offscreen in [
            instance([-10.0, 0.0], [5.0, 5.0], [1.0, 1.0, 1.0, 1.0]),
            instance([0.0, 99.0], [5.0, 5.0], [1.0, 1.0, 1.0, 1.0]),
            // Enormous coordinates must be discarded, not saturate into view.
            instance([1e30, 1e30], [5.0, 5.0], [1.0, 1.0, 1.0, 1.0]),
        ] {
            draw_sprite(&mut framebuffer, viewport, 1.0, 1.0, &offscreen);
        }

        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(framebuffer.pixel(x, y), Some(background()));
            }
        }
    }

    /// Degenerate and non-finite geometry is rejected rather than rasterized.
    #[test]
    fn degenerate_sprites_draw_nothing() {
        let mut framebuffer = Framebuffer::new(4, 4);
        framebuffer.clear(background());
        let viewport = RenderViewport::full(4, 4);

        for degenerate in [
            instance([1.0, 1.0], [0.0, 2.0], [1.0, 1.0, 1.0, 1.0]),
            instance([1.0, 1.0], [-2.0, 2.0], [1.0, 1.0, 1.0, 1.0]),
            instance([1.0, 1.0], [f32::NAN, 2.0], [1.0, 1.0, 1.0, 1.0]),
            instance([f32::NAN, 1.0], [2.0, 2.0], [1.0, 1.0, 1.0, 1.0]),
            // Zero alpha contributes nothing.
            instance([1.0, 1.0], [2.0, 2.0], [1.0, 1.0, 1.0, 0.0]),
        ] {
            draw_sprite(&mut framebuffer, viewport, 1.0, 1.0, &degenerate);
        }

        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(framebuffer.pixel(x, y), Some(background()));
            }
        }
    }

    /// A translucent sprite mixes with what is already there.
    #[test]
    fn translucent_sprites_blend_with_the_background() {
        let mut framebuffer = Framebuffer::new(2, 2);
        framebuffer.clear(pack_rgb565(0.0, 0.0, 0.0));
        let viewport = RenderViewport::full(2, 2);

        draw_sprite(
            &mut framebuffer,
            viewport,
            1.0,
            1.0,
            &instance([0.0, 0.0], [2.0, 2.0], [1.0, 1.0, 1.0, 0.5]),
        );

        let (red, _, _) = unpack_rgb565(framebuffer.pixel(0, 0).expect("in bounds"));
        assert!(
            red > 0.3 && red < 0.7,
            "half-alpha white over black should land mid-range, got {red}"
        );
    }

    /// Sprites paint in submission order, so a later one wins the overlap.
    #[test]
    fn later_sprites_paint_over_earlier_ones() {
        let mut framebuffer = Framebuffer::new(2, 2);
        framebuffer.clear(background());
        let viewport = RenderViewport::full(2, 2);

        draw_sprite(
            &mut framebuffer,
            viewport,
            1.0,
            1.0,
            &instance([0.0, 0.0], [2.0, 2.0], [1.0, 0.0, 0.0, 1.0]),
        );
        draw_sprite(
            &mut framebuffer,
            viewport,
            1.0,
            1.0,
            &instance([0.0, 0.0], [2.0, 2.0], [0.0, 0.0, 1.0, 1.0]),
        );

        assert_eq!(
            framebuffer.pixel(0, 0),
            Some(pack_rgb565(0.0, 0.0, 1.0)),
            "the second sprite should win"
        );
    }

    /// A project space smaller than the viewport scales up to fill it.
    #[test]
    fn virtual_resolution_scales_sprites_onto_the_viewport() {
        let mut framebuffer = Framebuffer::new(8, 8);
        framebuffer.clear(background());
        let viewport = RenderViewport::full(8, 8);

        // Project space is 4x4 mapped onto 8x8, so scale is 2 on both axes:
        // a 2x2 sprite at the origin must cover the top-left 4x4 pixels.
        draw_sprite(
            &mut framebuffer,
            viewport,
            2.0,
            2.0,
            &instance([0.0, 0.0], [2.0, 2.0], [1.0, 1.0, 1.0, 1.0]),
        );

        let white = pack_rgb565(1.0, 1.0, 1.0);
        assert_eq!(framebuffer.pixel(3, 3), Some(white));
        assert_eq!(framebuffer.pixel(4, 4), Some(background()));
    }

    /// Sprites sharing an edge tile exactly: no overlap and no seam.
    #[test]
    fn adjacent_sprites_tile_without_gaps_or_overlap() {
        assert_eq!(project_span(0.0, 2.0, 1.0, 0, 8), Some((0, 2)));
        assert_eq!(project_span(2.0, 2.0, 1.0, 0, 8), Some((2, 4)));
    }

    /// Partially visible sprites keep only their on-screen portion.
    #[test]
    fn spans_clip_to_the_viewport_bounds() {
        assert_eq!(project_span(-2.0, 4.0, 1.0, 0, 8), Some((0, 2)));
        assert_eq!(project_span(6.0, 4.0, 1.0, 0, 8), Some((6, 8)));
    }

    /// A sprite thinner than one pixel after scaling is dropped, not widened.
    #[test]
    fn subpixel_sprites_round_away() {
        assert_eq!(project_span(0.0, 0.25, 1.0, 0, 8), None);
    }
}
