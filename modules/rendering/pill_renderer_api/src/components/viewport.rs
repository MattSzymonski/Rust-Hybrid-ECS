//! The rectangle of the target a frame draws into.
//!
//! # Responsibilities
//!
//! - Define [`RenderViewport`], the pixel bounds the renderer restricts drawing
//!   to when a frontend asks for a sub-region of the surface.
//!
//! # Design
//!
//! Not a component and not an asset: a plain value the renderer holds, so it
//! carries no ECS derives and is registered with nothing. It lives under
//! `components/` because that is where the renderer's scene-facing types are
//! gathered, and because it never belonged to the asset or GPU layers either.

/// A rectangle of the target a frame draws into.
///
/// The renderer holds one as its optional split-screen viewport, and falls
/// back to drawing the full target when none is set or the one it has clamps
/// away to nothing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RenderViewport {
    /// Left edge of the rectangle within the target, in pixels.
    pub x: u32,
    /// Top edge of the rectangle within the target, in pixels.
    pub y: u32,
    /// Width of the rectangle, in pixels.
    pub width: u32,
    /// Height of the rectangle, in pixels.
    pub height: u32,
}

impl RenderViewport {
    /// Creates a viewport from explicit pixel bounds.
    ///
    /// The bounds are taken as given; use [`RenderViewport::clamped_to`] to
    /// fit the rectangle to a target that may be smaller.
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The whole target, from the origin, sized `width` by `height`.
    ///
    /// This is what the renderer substitutes when no viewport is set or an
    /// explicit one leaves nothing on screen.
    pub const fn full(width: u32, height: u32) -> Self {
        Self::new(0, 0, width, height)
    }

    /// Clips the rectangle to a target of the given pixel size.
    ///
    /// Returns `None` when nothing of the rectangle survives, meaning an
    /// origin at or past the target edge or a visible width or height of zero;
    /// the renderer treats that as "draw the full target".
    pub fn clamped_to(self, width: u32, height: u32) -> Option<Self> {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let width = self.width.min(width - x);
        let height = self.height.min(height - y);
        (width > 0 && height > 0).then_some(Self::new(x, y, width, height))
    }
}
