//! A frame of UI output: draw lists per surface, and the textures they
//! sample.
//!
//! # Responsibilities
//!
//! - [`UiFrame`]: everything a UI producer (a game's overlay, a tool's
//!   panels) built this update, as owned plain data.
//! - [`UiDraw`]: one surface's draw list and the monitor scale its logical
//!   pixels are in.
//!
//! # Design
//!
//! The value crosses an artifact boundary - a project's system builds it
//! inside one DLL, a renderer in another draws it - so everything it carries
//! is owned and layout-stable: the draw list is plain vertex data and the
//! textures are a versioned set of copies (see [`crate::texture`]), never a
//! pointer into the producer's atlas. The consuming renderer keeps its own
//! textures and uploads whatever its revisions say it lacks.

// Current crate
use crate::draw::DrawList;
use crate::texture::UiTextureSet;

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

/// A frame of UI: the draw lists to draw over the surfaces, and the textures
/// they sample.
#[derive(Clone, Debug, Default)]
pub struct UiFrame {
    /// The lists to draw, in draw order (later entries draw over earlier).
    pub draws: Vec<UiDraw>,
    /// The textures the lists sample, by revision; empty when nothing
    /// changed and the backend already holds everything.
    pub textures: UiTextureSet,
}

impl UiFrame {
    /// Whether there is nothing to draw and nothing to upload.
    pub fn is_empty(&self) -> bool {
        self.draws.is_empty() && self.textures.is_empty()
    }

    /// Forget the previous frame's work, keeping the allocations.
    pub fn clear(&mut self) {
        self.draws.clear();
        self.textures.clear();
    }
}
