//! A frame of UI output: draw lists per surface, and the atlas changes they
//! need.
//!
//! # Responsibilities
//!
//! - [`UiFrame`]: everything a `pill_ui` user (a game's overlay, a tool's
//!   panels) built this update, as owned plain data.
//! - [`UiDraw`]: one surface's draw list and the monitor scale its logical
//!   pixels are in.
//!
//! # Design
//!
//! The value crosses an artifact boundary - a project's system builds it
//! inside one DLL, a renderer in another draws it - so everything it carries
//! is owned and layout-stable: the draw list is plain vertex data and the
//! atlas change is a copy of the dirty regions' pixels, never a pointer into
//! the producer's atlas. The consuming renderer keeps its own atlas texture
//! and applies the updates; the producing side keeps its atlas and drains it.

// Current crate
use crate::atlas::AtlasUpdate;
use crate::draw::DrawList;

/// One surface's UI for a frame.
#[derive(Clone, Debug, Default)]
pub struct UiDraw {
    /// Which surface to draw into, as a surface id (the primary is `0`).
    pub surface: u32,
    /// The monitor scale the list's logical pixels are in; the backend
    /// multiplies by it when drawing.
    pub scale_factor: f32,
    /// The frame's geometry and commands.
    pub list: DrawList,
}

/// A frame of UI: the draw lists to draw over the surfaces, and the atlas
/// changes that arrived with them.
#[derive(Clone, Debug, Default)]
pub struct UiFrame {
    /// The lists to draw, in draw order (later entries draw over earlier).
    pub draws: Vec<UiDraw>,
    /// Glyph and icon regions written since the previous frame, to upload
    /// before drawing; `None` when no atlas texel changed.
    pub atlas: Option<AtlasUpdate>,
}

impl UiFrame {
    /// Whether there is nothing to draw and nothing to upload.
    pub fn is_empty(&self) -> bool {
        self.draws.is_empty() && self.atlas.is_none()
    }

    /// Forget the previous frame's work, keeping the allocations.
    pub fn clear(&mut self) {
        self.draws.clear();
        self.atlas = None;
    }
}
