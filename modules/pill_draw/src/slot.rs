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
}

impl Resource for UiFrameSlot {
    fn shared_name() -> Option<&'static str> {
        Some("pill_draw::UiFrameSlot")
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
}
