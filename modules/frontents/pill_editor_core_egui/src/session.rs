//! The egui shell's long-lived session state.
//!
//! # Responsibilities
//!
//! - Own [`EguiShellState`]: the egui `Context` and the dock, the shell's one
//!   world resource.
//!
//! # Design
//!
//! The type lives in this shared `dylib`, not in the reloadable shell
//! module, so its identity is stable across shell reloads: the arriving
//! generation's `register` finds the same resource and keeps the context and
//! the dock exactly where the user left them. A resource whose type the
//! module defined would be reset by every reload - and the context's drop
//! code would then run from an unloaded image.

// External crates
use pill_engine::Resource;
use pill_ui_egui::egui::Context;

// Current crate
use crate::layout::EditorDock;

/// The egui shell's session: the UI context and the dock layout.
pub struct EguiShellState {
    /// The egui context: fonts, memory and widget state.
    pub context: Context,
    /// The dock layout; persisted across restarts.
    pub dock: EditorDock,
    /// Completed egui passes; the shell's log cadence reads it.
    pub frame_index: u64,
}

impl Resource for EguiShellState {}

impl EguiShellState {
    /// Session state with the editor's theme and the default dock.
    pub fn new() -> Self {
        let context = Context::default();
        pill_ui_egui::style::apply(&context);
        Self {
            context,
            dock: EditorDock::new(),
            frame_index: 0,
        }
    }
}

impl Default for EguiShellState {
    fn default() -> Self {
        Self::new()
    }
}
