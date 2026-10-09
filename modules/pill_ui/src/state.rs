//! The UI's persistent state, as plain data.
//!
//! # Responsibilities
//!
//! - [`UiState`]: everything a UI session remembers between frames that is
//!   plain data - the input state, where each widget was last laid out, which
//!   widget is currently dragged and focused, and each widget's own memory -
//!   ready to live in a world resource and to cross a module reload.
//!
//! # Design
//!
//! State that is not plain data (fonts, atlases) lives in
//! [`crate::text::TextSystem`] and [`crate::icons::IconCache`], which the
//! process shares as one compiled copy (plan decision D17). `UiState`
//! deliberately holds only owned values: it can be carried across a hot
//! reload boundary without dangling. Widget code reads the previous frame's
//! rectangles ([`UiState::hit_rects`]) to decide what a pointer or key did
//! this frame, then the current frame's layout replaces them; that one-frame
//! lag is what makes a click in frame N activate in frame N+1.

// Standard library
use std::collections::BTreeMap;

// External crates
use pill_engine_core::Resource;

// Current crate
use crate::draw::Rect;
use crate::input::UiInput;
use crate::widget::{WidgetId, WidgetMemory};

/// Everything the UI remembers between frames.
#[derive(Clone, Debug, Default)]
pub struct UiState {
    /// The input collected for the frame being built.
    pub input: UiInput,
    /// Where each widget was laid out last frame, for hit-testing.
    pub(crate) hit_rects: BTreeMap<WidgetId, Rect>,
    /// Which widget the pointer holds and which holds the keyboard.
    pub(crate) interaction: Interaction,
    /// Per-widget memory keyed by id.
    pub(crate) memory: BTreeMap<WidgetId, WidgetMemory>,
}

impl UiState {
    /// Fresh state with nothing remembered.
    pub fn new() -> UiState {
        UiState::default()
    }

    /// The widget holding the keyboard focus.
    pub fn focus(&self) -> Option<WidgetId> {
        self.interaction.focus
    }

    /// Give a widget the keyboard focus, or clear it with `None`.
    pub fn set_focus(&mut self, id: impl Into<Option<WidgetId>>) {
        self.interaction.focus = id.into();
    }

    /// The widget the pointer is currently holding down.
    pub fn active(&self) -> Option<WidgetId> {
        self.interaction.active
    }
}

/// The interaction state a frame advances: at most one widget is being
/// dragged, and at most one holds the focus.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct Interaction {
    /// The widget the pointer went down on and has not been released from.
    pub active: Option<WidgetId>,
    /// The widget keyboard events go to.
    pub focus: Option<WidgetId>,
}

impl Resource for UiState {}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use pill_core::math::Vector2f;

    #[test]
    fn ui_state_satisfies_the_resource_contract() {
        // `Resource` requires `Send + Sync + 'static`; naming the bound is
        // the compile-time proof that a world can hold this value.
        fn assert_resource<T: Resource>() {}
        assert_resource::<UiState>();

        let mut state = UiState::new();
        state.input.begin_frame();
        assert_eq!(state.input.text(), "");
    }

    #[test]
    fn focus_and_active_round_trip() {
        let mut state = UiState::new();
        let id = WidgetId::from_name("Bake");
        assert_eq!(state.focus(), None);
        state.set_focus(id);
        assert_eq!(state.focus(), Some(id));
        state.set_focus(None);
        assert_eq!(state.focus(), None);

        state.interaction.active = Some(id);
        assert_eq!(state.active(), Some(id));
    }

    #[test]
    fn memory_is_created_on_first_use_and_kept() {
        let mut state = UiState::new();
        let id = WidgetId::from_name("Volume");
        state.memory.entry(id).or_default().scroll = 12.0;
        assert_eq!(state.memory[&id].scroll, 12.0);
        assert_eq!(state.memory.len(), 1);

        // State is plain data: a clone carries the memory and the last
        // frame's rectangles.
        let rect = Rect::new(Vector2f::new(1.0, 2.0), Vector2f::new(3.0, 4.0));
        state.hit_rects = BTreeMap::from([(id, rect)]);
        let copy = state.clone();
        assert_eq!(copy.hit_rects[&id], rect);
        assert_eq!(copy.memory[&id].scroll, 12.0);
    }
}
