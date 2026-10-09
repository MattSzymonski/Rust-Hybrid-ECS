//! The UI's persistent state, as plain data.
//!
//! # Responsibilities
//!
//! - [`UiState`]: everything a UI session remembers between frames that is
//!   plain data - today the input state - ready to live in a world resource
//!   and to cross a module reload.
//!
//! # Design
//!
//! State that is not plain data (fonts, atlases) lives in
//! [`crate::text::TextSystem`] and [`crate::icons::IconCache`], which the
//! process shares as one compiled copy (plan decision D17). `UiState`
//! deliberately holds only owned values: it can be carried across a hot
//! reload boundary without dangling, and later stages grow it with
//! per-widget state and focus.

// External crates
use pill_engine::Resource;

// Current crate
use crate::input::UiInput;

/// Everything the UI remembers between frames.
#[derive(Clone, Debug, Default)]
pub struct UiState {
    /// The input collected for the frame being built.
    pub input: UiInput,
}

impl UiState {
    /// Fresh state with nothing remembered.
    pub fn new() -> UiState {
        UiState::default()
    }
}

impl Resource for UiState {}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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
}
