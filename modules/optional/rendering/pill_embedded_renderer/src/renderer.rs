//! Frame-producing renderer for display-controller targets.
//!
//! # Responsibilities
//!
//! - Owns the framebuffer and the software rasterizer ([`Renderer`]).
//! - Resizes the framebuffer when the target display changes.
//! - Draws one frame from the current [`Engine`] ([`Renderer::render`]).
//! - Hands the finished frame to the caller as RGB565 bytes
//!   ([`Renderer::frame_rgb565`]).
//!
//! # Design
//!
//! The renderer produces a frame and stops. It owns no display, no SPI bus and
//! no GPIO pin, and it depends on no hardware crate - the caller takes the
//! bytes and writes them wherever they belong, typically an ST7789 over SPI:
//!
//! ```ignore
//! let mut renderer = Renderer::new(240, 280);
//! renderer.render(&mut engine);
//! display.draw(renderer.frame_rgb565())?;
//! ```
//!
//! That split is deliberate. A renderer that owned the driver could only be
//! built for the machine wired to the panel; this one compiles and unit-tests
//! on any host, which is what makes the rasterizer testable at all.
//!
//! The shape mirrors the wgpu backend - `new`, `resize`, `set_viewport`,
//! `set_virtual_resolution`, `render` - so a frontend can drive either without
//! knowing which it holds. The differences are that construction takes
//! dimensions rather than a window handle, and that presentation is the
//! caller's job rather than a swapchain's.

// External crates
use pill_engine::engine::Engine;

// Current crate
use crate::component::{RenderViewport, VirtualResolution};
use crate::framebuffer::Framebuffer;
use crate::rasterizer::SpriteRasterizer;

// =============================================================================
// Renderer
// =============================================================================

/// Draws the engine world into an RGB565 framebuffer on the CPU.
///
/// Holds the pixel buffer, the rasterizer, and the optional viewport and
/// logical-resolution overrides a frontend installs.
pub struct Renderer {
    /// Pixel buffer the world is drawn into each frame.
    framebuffer: Framebuffer,
    /// Turns sprite instances into pixels.
    rasterizer: SpriteRasterizer,
    /// Physical-pixel crop rectangle, or `None` for the whole framebuffer.
    viewport: Option<RenderViewport>,
    /// Logical scene size filling the viewport, or `None` for one-to-one pixels.
    virtual_resolution: Option<VirtualResolution>,
}

impl Renderer {
    /// Create a renderer targeting a display of `width` by `height` pixels.
    ///
    /// For a 240x280 ST7789 panel that is `Renderer::new(240, 280)`. Zero
    /// dimensions are promoted to one pixel rather than rejected: there is no
    /// fallible step here, and a degenerate size is better surfaced as an
    /// obviously wrong frame than as an error the caller cannot act on.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            framebuffer: Framebuffer::new(width, height),
            rasterizer: SpriteRasterizer::new(),
            viewport: None,
            virtual_resolution: None,
        }
    }

    /// Resize the framebuffer to a new display size.
    ///
    /// Contents are discarded; the next [`Self::render`] repaints everything.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.framebuffer.resize(width, height);
    }

    /// The framebuffer dimensions in pixels.
    pub fn surface_size(&self) -> (u32, u32) {
        (self.framebuffer.width(), self.framebuffer.height())
    }

    /// Restrict drawing to a physical-pixel rectangle within the framebuffer.
    ///
    /// `None` restores full-framebuffer drawing. Pixels outside the rectangle
    /// keep the background colour rather than stale contents, because each
    /// frame clears the whole buffer before drawing.
    pub fn set_viewport(&mut self, viewport: Option<RenderViewport>) {
        self.viewport = viewport;
    }

    /// Select the logical scene size that should fill the physical viewport.
    ///
    /// `None` keeps one project unit per pixel. This is how a project authored
    /// for, say, 800x600 fills a 240x280 panel without the project knowing the
    /// panel exists. Invalid dimensions disable the override.
    pub fn set_virtual_resolution(&mut self, resolution: Option<VirtualResolution>) {
        self.virtual_resolution = resolution.filter(|resolution| resolution.is_valid());
    }

    /// Draw every `(Position, Sprite)` entity in the engine world.
    ///
    /// Infallible: there is no device to lose and no frame to acquire, so
    /// unlike the wgpu backend there is nothing to report. The result is
    /// retrieved with [`Self::frame_rgb565`].
    pub fn render(&mut self, engine: &mut Engine) {
        let (width, height) = (self.framebuffer.width(), self.framebuffer.height());
        let viewport = self
            .viewport
            .unwrap_or_else(|| RenderViewport::full(width, height))
            .clamped_to(width, height)
            .unwrap_or_default();
        let virtual_resolution = resolve_virtual_resolution(self.virtual_resolution, viewport);

        self.rasterizer.render_in_viewport_with_resolution(
            engine.world(),
            &mut self.framebuffer,
            viewport,
            virtual_resolution,
        );
    }

    /// The last rendered frame as little-endian RGB565 bytes.
    ///
    /// `width * height * 2` bytes, row-major and top-down - the layout an
    /// ST7789 `draw(&[u8])` expects. Valid until the next `render` or `resize`.
    pub fn frame_rgb565(&mut self) -> &[u8] {
        self.framebuffer.as_rgb565_le()
    }

    /// Read-only access to the framebuffer, for callers needing packed pixels.
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Resolve the logical projection without coupling it to display dimensions.
///
/// Identical to the wgpu backend's rule, so a project renders the same on both:
/// a valid configured resolution wins, otherwise the viewport maps one-to-one.
fn resolve_virtual_resolution(
    configured: Option<VirtualResolution>,
    viewport: RenderViewport,
) -> VirtualResolution {
    configured
        .filter(|resolution| resolution.is_valid())
        .unwrap_or_else(|| {
            VirtualResolution::new(viewport.width.max(1) as f32, viewport.height.max(1) as f32)
        })
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The framebuffer adopts the requested display size.
    #[test]
    fn renderer_targets_the_requested_display_size() {
        let renderer = Renderer::new(240, 280);
        assert_eq!(renderer.surface_size(), (240, 280));
    }

    /// The frame is two bytes per pixel, as the display controller expects.
    #[test]
    fn frame_bytes_match_the_display_size() {
        let mut renderer = Renderer::new(240, 280);
        assert_eq!(renderer.frame_rgb565().len(), 240 * 280 * 2);
    }

    /// Resizing updates both the reported size and the frame length.
    #[test]
    fn resizing_updates_the_frame_length() {
        let mut renderer = Renderer::new(240, 280);
        renderer.resize(128, 64);

        assert_eq!(renderer.surface_size(), (128, 64));
        assert_eq!(renderer.frame_rgb565().len(), 128 * 64 * 2);
    }

    /// An invalid virtual resolution is rejected rather than stored.
    #[test]
    fn invalid_virtual_resolutions_are_ignored() {
        let mut renderer = Renderer::new(64, 64);
        renderer.set_virtual_resolution(Some(VirtualResolution::new(0.0, 600.0)));
        assert_eq!(renderer.virtual_resolution, None);

        renderer.set_virtual_resolution(Some(VirtualResolution::new(800.0, 600.0)));
        assert_eq!(
            renderer.virtual_resolution,
            Some(VirtualResolution::new(800.0, 600.0))
        );
    }

    /// A configured project space stays fixed as the display changes.
    #[test]
    fn virtual_resolution_is_independent_of_physical_viewport_size() {
        let configured = VirtualResolution::new(800.0, 600.0);

        assert_eq!(
            resolve_virtual_resolution(Some(configured), RenderViewport::new(240, 80, 517, 463)),
            configured
        );
        assert_eq!(
            resolve_virtual_resolution(None, RenderViewport::new(240, 80, 517, 463)),
            VirtualResolution::new(517.0, 463.0)
        );
    }

    /// Rendering an empty world still produces a full background frame.
    #[test]
    fn rendering_an_empty_world_produces_a_full_frame() {
        let mut engine = Engine::new();
        let mut renderer = Renderer::new(16, 8);
        renderer.render(&mut engine);

        let frame = renderer.frame_rgb565();
        assert_eq!(frame.len(), 16 * 8 * 2);
        // Every pixel carries the same background colour, so every even byte
        // matches the first one.
        assert!(frame.chunks_exact(2).all(|pixel| pixel == &frame[0..2]));
    }
}
