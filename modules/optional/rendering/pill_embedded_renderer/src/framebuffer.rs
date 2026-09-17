//! CPU framebuffer in RGB565, the format the display controller consumes.
//!
//! # Responsibilities
//!
//! - Owns the pixel buffer and its dimensions ([`Framebuffer`]).
//! - Converts 0.0-1.0 colour channels to packed RGB565 ([`pack_rgb565`]).
//! - Fills, blends and clears horizontal spans of pixels.
//! - Exposes the finished frame as little-endian bytes for a display driver.
//!
//! # Design
//!
//! The buffer holds packed `u16` pixels rather than bytes: every blend reads
//! and writes whole pixels, and packing per access would cost shifts on a CPU
//! that has none to spare. Bytes are produced once, at the edge, by
//! [`Framebuffer::as_rgb565_le`].
//!
//! Little-endian is not a choice made here - it is what the ST7789 driver
//! expects. `to_le_bytes` makes the conversion a plain reinterpretation on a
//! little-endian host and a byte swap on a big-endian one, so the output is
//! correct either way rather than accidentally correct on the usual target.

// =============================================================================
// Constants
// =============================================================================

/// Bytes per pixel in the RGB565 output, one `u16`.
pub const BYTES_PER_PIXEL: usize = 2;

// =============================================================================
// Colour packing
// =============================================================================

/// Pack three 0.0-1.0 colour channels into one RGB565 pixel.
///
/// RGB565 spends 5 bits on red, 6 on green and 5 on blue; green gets the spare
/// bit because the eye resolves green detail best. Alpha has nowhere to go in
/// the format, so it is applied by blending before packing, never here.
///
/// Channels are clamped rather than wrapped. A colour outside 0.0-1.0 is a
/// project bug, and wrapping would turn a slightly-over-bright red into black,
/// which is a much harder artefact to recognise on a small display.
#[inline]
pub fn pack_rgb565(red: f32, green: f32, blue: f32) -> u16 {
    let quantize = |channel: f32, bits: u32| -> u16 {
        let max = ((1u32 << bits) - 1) as f32;
        // `clamp` maps NaN to the low bound, so a NaN channel renders black
        // rather than producing an unpredictable bit pattern.
        (channel.clamp(0.0, 1.0) * max + 0.5) as u16
    };
    (quantize(red, 5) << 11) | (quantize(green, 6) << 5) | quantize(blue, 5)
}

/// Unpack one RGB565 pixel back into 0.0-1.0 channels.
///
/// The inverse of [`pack_rgb565`], used by blending to read the destination
/// pixel and by tests to assert the round trip.
#[inline]
pub fn unpack_rgb565(pixel: u16) -> (f32, f32, f32) {
    let red = ((pixel >> 11) & 0x1F) as f32 / 31.0;
    let green = ((pixel >> 5) & 0x3F) as f32 / 63.0;
    let blue = (pixel & 0x1F) as f32 / 31.0;
    (red, green, blue)
}

// =============================================================================
// Framebuffer
// =============================================================================

/// A fixed-size RGB565 pixel buffer drawn into by the rasterizer.
///
/// Rows are contiguous and top-down: pixel `(x, y)` lives at index
/// `y * width + x`, which is the scan order the display controller expects, so
/// the finished buffer is written straight out with no reordering.
pub struct Framebuffer {
    /// Packed RGB565 pixels, `width * height` of them, row-major.
    pixels: Vec<u16>,
    /// Row length in pixels.
    width: u32,
    /// Row count.
    height: u32,
    /// Scratch byte buffer reused by [`Self::as_rgb565_le`].
    ///
    /// Kept between frames so producing the driver's view costs a copy rather
    /// than an allocation; on a Pi-class CPU a per-frame allocation this size
    /// is worth avoiding.
    bytes: Vec<u8>,
}

impl Framebuffer {
    /// Allocate a cleared framebuffer of `width` by `height` pixels.
    ///
    /// Both dimensions are forced to at least one pixel: a zero-sized buffer
    /// would make every later index calculation a special case, and no display
    /// has a zero-pixel axis.
    pub fn new(width: u32, height: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let pixel_count = width as usize * height as usize;
        Self {
            pixels: vec![0; pixel_count],
            width,
            height,
            bytes: vec![0; pixel_count * BYTES_PER_PIXEL],
        }
    }

    /// Row length in pixels.
    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Row count.
    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Resize the buffer, reallocating only when the dimensions really change.
    ///
    /// Contents are not preserved: every caller redraws the whole frame right
    /// afterwards, so copying the old pixels would be wasted work.
    pub fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if width == self.width && height == self.height {
            return;
        }
        let pixel_count = width as usize * height as usize;
        self.width = width;
        self.height = height;
        self.pixels.clear();
        self.pixels.resize(pixel_count, 0);
        self.bytes.clear();
        self.bytes.resize(pixel_count * BYTES_PER_PIXEL, 0);
    }

    /// Overwrite every pixel with one packed colour.
    #[inline]
    pub fn clear(&mut self, pixel: u16) {
        self.pixels.fill(pixel);
    }

    /// Read one pixel, or `None` when the coordinate is outside the buffer.
    ///
    /// Used by tests and by callers holding an unchecked coordinate; the
    /// rasterizer indexes rows directly instead.
    #[inline]
    pub fn pixel(&self, x: u32, y: u32) -> Option<u16> {
        (x < self.width && y < self.height)
            .then(|| self.pixels[y as usize * self.width as usize + x as usize])
    }

    /// Fill a horizontal span with an opaque colour.
    ///
    /// `x_start..x_end` is half-open. The range is clamped here as well as by
    /// the caller, so a span reaching past the right edge truncates instead of
    /// wrapping onto the next row.
    #[inline]
    pub fn fill_span(&mut self, y: u32, x_start: u32, x_end: u32, pixel: u16) {
        let Some((start, end)) = self.span_bounds(y, x_start, x_end) else {
            return;
        };
        self.pixels[start..end].fill(pixel);
    }

    /// Blend a horizontal span over the existing pixels.
    ///
    /// `color` is linear RGBA in 0.0-1.0, in the channel order a sprite
    /// instance already stores it, with alpha as source coverage. Source-over
    /// compositing, matching the `ALPHA_BLENDING` state the wgpu backend sets,
    /// so both renderers agree on what a translucent sprite looks like.
    #[inline]
    pub fn blend_span(&mut self, y: u32, x_start: u32, x_end: u32, color: [f32; 4]) {
        let Some((start, end)) = self.span_bounds(y, x_start, x_end) else {
            return;
        };
        let [red, green, blue, alpha] = color;
        let inverse = 1.0 - alpha;
        for pixel in &mut self.pixels[start..end] {
            let (destination_red, destination_green, destination_blue) = unpack_rgb565(*pixel);
            *pixel = pack_rgb565(
                red * alpha + destination_red * inverse,
                green * alpha + destination_green * inverse,
                blue * alpha + destination_blue * inverse,
            );
        }
    }

    /// Resolve a span to buffer indices, or `None` when it covers no pixel.
    ///
    /// One place where a span is clipped to the row, so `fill_span` and
    /// `blend_span` cannot disagree about what is in bounds.
    #[inline]
    fn span_bounds(&self, y: u32, x_start: u32, x_end: u32) -> Option<(usize, usize)> {
        if y >= self.height {
            return None;
        }
        let x_start = x_start.min(self.width);
        let x_end = x_end.min(self.width);
        if x_start >= x_end {
            return None;
        }
        let row = y as usize * self.width as usize;
        Some((row + x_start as usize, row + x_end as usize))
    }

    /// The finished frame as little-endian RGB565 bytes.
    ///
    /// This is what a display driver consumes; for the ST7789 that is
    /// `draw(&[u8])`. The slice is `width * height * 2` bytes long and stays
    /// valid until the next call that mutates the framebuffer.
    pub fn as_rgb565_le(&mut self) -> &[u8] {
        for (pixel, chunk) in self
            .pixels
            .iter()
            .zip(self.bytes.chunks_exact_mut(BYTES_PER_PIXEL))
        {
            chunk.copy_from_slice(&pixel.to_le_bytes());
        }
        &self.bytes
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The primaries land on the exact bit patterns the format defines.
    #[test]
    fn packing_places_channels_in_their_rgb565_fields() {
        assert_eq!(pack_rgb565(0.0, 0.0, 0.0), 0x0000);
        assert_eq!(pack_rgb565(1.0, 1.0, 1.0), 0xFFFF);
        assert_eq!(pack_rgb565(1.0, 0.0, 0.0), 0xF800);
        assert_eq!(pack_rgb565(0.0, 1.0, 0.0), 0x07E0);
        assert_eq!(pack_rgb565(0.0, 0.0, 1.0), 0x001F);
    }

    /// Out-of-range and NaN channels clamp instead of wrapping to a far colour.
    #[test]
    fn packing_clamps_rather_than_wrapping() {
        assert_eq!(pack_rgb565(2.0, 2.0, 2.0), 0xFFFF);
        assert_eq!(pack_rgb565(-1.0, -1.0, -1.0), 0x0000);
        assert_eq!(pack_rgb565(f32::NAN, f32::NAN, f32::NAN), 0x0000);
    }

    /// Unpacking inverts packing within one quantization step.
    #[test]
    fn unpacking_round_trips_through_packing() {
        for pixel in [0x0000u16, 0xFFFF, 0xF800, 0x07E0, 0x001F, 0x1234] {
            let (red, green, blue) = unpack_rgb565(pixel);
            assert_eq!(pack_rgb565(red, green, blue), pixel);
        }
    }

    /// Output is little-endian and two bytes per pixel, as the driver expects.
    #[test]
    fn frame_bytes_are_little_endian_pairs() {
        let mut framebuffer = Framebuffer::new(2, 1);
        framebuffer.fill_span(0, 0, 1, 0xF800);
        framebuffer.fill_span(0, 1, 2, 0x001F);

        // 0xF800 -> [0x00, 0xF8] and 0x001F -> [0x1F, 0x00] when little-endian.
        assert_eq!(framebuffer.as_rgb565_le(), &[0x00, 0xF8, 0x1F, 0x00]);
    }

    /// The byte buffer is exactly two bytes per pixel at any size.
    #[test]
    fn frame_byte_length_matches_pixel_count() {
        let mut framebuffer = Framebuffer::new(240, 280);
        assert_eq!(
            framebuffer.as_rgb565_le().len(),
            240 * 280 * BYTES_PER_PIXEL
        );
    }

    /// A span reaching past the right edge truncates instead of wrapping.
    #[test]
    fn spans_clip_to_the_row_rather_than_wrapping() {
        let mut framebuffer = Framebuffer::new(4, 2);
        framebuffer.fill_span(0, 2, 99, 0xFFFF);

        assert_eq!(framebuffer.pixel(2, 0), Some(0xFFFF));
        assert_eq!(framebuffer.pixel(3, 0), Some(0xFFFF));
        // The next row must be untouched: wrapping would have painted it.
        assert_eq!(framebuffer.pixel(0, 1), Some(0x0000));
        assert_eq!(framebuffer.pixel(1, 1), Some(0x0000));
    }

    /// Out-of-range rows and empty ranges are dropped, not clamped into view.
    #[test]
    fn spans_outside_the_buffer_draw_nothing() {
        let mut framebuffer = Framebuffer::new(2, 2);
        framebuffer.fill_span(9, 0, 2, 0xFFFF);
        framebuffer.fill_span(0, 2, 1, 0xFFFF);
        framebuffer.fill_span(0, 1, 1, 0xFFFF);

        for y in 0..2 {
            for x in 0..2 {
                assert_eq!(framebuffer.pixel(x, y), Some(0x0000));
            }
        }
    }

    /// Full alpha replaces the destination; zero alpha leaves it alone.
    #[test]
    fn blending_endpoints_replace_and_preserve() {
        let mut framebuffer = Framebuffer::new(2, 1);
        framebuffer.clear(pack_rgb565(0.0, 0.0, 1.0));

        framebuffer.blend_span(0, 0, 1, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(framebuffer.pixel(0, 0), Some(pack_rgb565(1.0, 0.0, 0.0)));

        framebuffer.blend_span(0, 1, 2, [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(framebuffer.pixel(1, 0), Some(pack_rgb565(0.0, 0.0, 1.0)));
    }

    /// A half-alpha white over black lands mid-range on every channel.
    #[test]
    fn blending_mixes_source_and_destination() {
        let mut framebuffer = Framebuffer::new(1, 1);
        framebuffer.clear(0x0000);
        framebuffer.blend_span(0, 0, 1, [1.0, 1.0, 1.0, 0.5]);

        let (red, green, blue) = unpack_rgb565(framebuffer.pixel(0, 0).expect("in bounds"));
        for channel in [red, green, blue] {
            assert!(
                (channel - 0.5).abs() < 0.05,
                "channel {channel} should sit near 0.5"
            );
        }
    }

    /// Resizing adopts the new dimensions and keeps the byte buffer in step.
    #[test]
    fn resizing_updates_dimensions_and_byte_length() {
        let mut framebuffer = Framebuffer::new(4, 4);
        framebuffer.resize(8, 2);

        assert_eq!((framebuffer.width(), framebuffer.height()), (8, 2));
        assert_eq!(framebuffer.as_rgb565_le().len(), 8 * 2 * BYTES_PER_PIXEL);
    }

    /// Zero dimensions are promoted to one pixel rather than allocating empty.
    #[test]
    fn zero_dimensions_are_promoted_to_one_pixel() {
        let framebuffer = Framebuffer::new(0, 0);
        assert_eq!((framebuffer.width(), framebuffer.height()), (1, 1));
    }
}
