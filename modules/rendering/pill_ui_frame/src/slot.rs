//! The world resource a UI producer publishes a frame into and a renderer
//! drains it from.
//!
//! # Responsibilities
//!
//! - [`UiFrameSlot`]: one world resource holding the newest [`UiFrame`] a UI
//!   producer built, with `publish` and `take` as its whole surface.
//!
//! # Design
//!
//! A UI producer and a renderer meet here rather than naming each other. The
//! producer (a project's egui pass) writes what it drew; the renderer drains
//! it and draws it. Neither side depends on the other's crate, which is what
//! lets a project link the game-UI library and the renderer module keep
//! drawing without linking it as well - on Windows a single link unit cannot
//! hold two copies of the same third-party graph in different formats.
//!
//! The slot lives beside the data it holds, so the engine core names no UI
//! types: both sides already link this crate. Its [`Resource::shared_name`]
//! is pinned: a project DLL and the host register the resource separately,
//! and the name is what makes the host's slot and the project's writes the
//! same resource.

// External crates
use pill_engine_core::resource::Resource;

// Current crate
use crate::frame::UiFrame;

// =============================================================================
// UiFrameSlot
// =============================================================================

/// The frame the last UI pass published, waiting for the renderer.
#[derive(Debug, Default)]
pub struct UiFrameSlot {
    /// The frame a producer published; `None` until something drew.
    frame: Option<UiFrame>,
    /// Bumped by a painter that starts with no textures of its own.
    ///
    /// A renderer module is rebuilt and swapped like any other, and its new
    /// image begins with an empty texture map while the producer - a shared
    /// library that outlives it - has already carried every image once. The
    /// request is the way back: the producer compares this number with the one
    /// it last served and re-carries every texture it holds when it changed.
    texture_resend_generation: u64,
}

impl Resource for UiFrameSlot {
    fn shared_name() -> Option<&'static str> {
        Some("pill_ui_frame::UiFrameSlot")
    }
}

impl UiFrameSlot {
    /// An empty slot.
    pub fn new() -> UiFrameSlot {
        UiFrameSlot::default()
    }

    /// Whether nothing is waiting to be drained.
    pub fn is_empty(&self) -> bool {
        self.frame.as_ref().is_none_or(UiFrame::is_empty)
    }

    /// Hand `frame` to the renderer, replacing anything not yet drained.
    ///
    /// The newest frame is the one worth drawing: a producer that ran twice
    /// between two renderer passes leaves the second pass' work behind, not a
    /// queue of stale frames.
    pub fn publish(&mut self, frame: UiFrame) {
        self.frame = Some(frame);
    }

    /// Drain the slot, leaving it empty.
    pub fn take(&mut self) -> UiFrame {
        self.frame.take().unwrap_or_default()
    }

    /// Forget what is waiting, keeping its allocations.
    pub fn clear(&mut self) {
        if let Some(frame) = self.frame.as_mut() {
            frame.clear();
        }
    }

    /// Ask the producer to carry every texture again.
    ///
    /// Called by a painter whose texture map starts empty - a renderer module
    /// that was just rebuilt - because the producer's dirty set is empty by
    /// then and a one-shot push has no other way to know the images were lost.
    pub fn request_texture_resend(&mut self) {
        self.texture_resend_generation = self.texture_resend_generation.wrapping_add(1);
    }

    /// The newest resend request the painter has made.
    ///
    /// A producer re-carries its textures when this differs from the value it
    /// last served.
    pub fn texture_resend_generation(&self) -> u64 {
        self.texture_resend_generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{Color, DrawList, Rect, TextureId};
    use crate::frame::UiDraw;

    /// A frame carrying one draw list, so emptiness is observable.
    fn drawn_frame() -> UiFrame {
        let mut frame = UiFrame::default();
        let mut list = DrawList::new();
        list.push_quad(
            TextureId(1),
            Rect::from_min_size(
                pill_core::math::Vector2f::ZERO,
                pill_core::math::Vector2f::new(2.0, 2.0),
            ),
            pill_core::math::Vector2f::ZERO,
            pill_core::math::Vector2f::new(1.0, 1.0),
            Color::WHITE,
        );
        frame.draws.push(UiDraw {
            surface: 0,
            scale_factor: 1.0,
            list,
        });
        frame
    }

    #[test]
    fn a_fresh_slot_is_empty() {
        let mut slot = UiFrameSlot::new();
        assert!(slot.is_empty());
        assert!(slot.take().is_empty());
    }

    #[test]
    fn a_published_frame_is_what_the_next_take_returns() {
        let mut slot = UiFrameSlot::new();
        slot.publish(drawn_frame());
        assert!(!slot.is_empty());
        let frame = slot.take();
        assert_eq!(frame.draws.len(), 1);
        assert!(slot.is_empty(), "the take drained the slot");
    }

    #[test]
    fn the_newest_frame_replaces_the_one_not_yet_drained() {
        let mut slot = UiFrameSlot::new();
        slot.publish(drawn_frame());
        slot.publish(UiFrame::default());
        assert!(
            slot.take().is_empty(),
            "the second publish replaced the first frame"
        );
    }

    /// A painter that starts empty asks for the textures again, and the ask is
    /// visible to the producer as a number it can compare.
    #[test]
    fn a_resend_request_bumps_the_generation_the_producer_watches() {
        let mut slot = UiFrameSlot::new();
        assert_eq!(slot.texture_resend_generation(), 0, "nothing asked yet");

        slot.request_texture_resend();
        let asked = slot.texture_resend_generation();
        assert_ne!(asked, 0, "the request is visible");

        // Publishing and draining are the frame's own business and leave the
        // request alone.
        slot.publish(drawn_frame());
        let _ = slot.take();
        assert_eq!(slot.texture_resend_generation(), asked);
    }
}
