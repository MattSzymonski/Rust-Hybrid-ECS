//! The egui shell's long-lived session state.
//!
//! # Responsibilities
//!
//! - Own [`EguiShellState`]: the egui `Context` and the dock, the shell's one
//!   world resource.
//! - Own [`ToolboxSession`]: where each scene view's floating toolbox sits
//!   and what the log already reported about it.
//! - Carry the Assets tab's [`ProjectBrowser`]: the folder its grid lists.
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
use std::collections::BTreeMap;

use pill_engine::Resource;
use pill_ui_egui::boundary::SCENE_VIEWPORTS;
use pill_ui_egui::egui::{Context, Pos2, Rect};

// Current crate
use crate::assets::AssetDialog;
use crate::browser::ProjectBrowser;
use crate::layout::EditorDock;

/// The egui shell's session: the UI context and the dock layout.
pub struct EguiShellState {
    /// The egui context: fonts, memory and widget state.
    pub context: Context,
    /// The dock layout; persisted across restarts.
    pub dock: EditorDock,
    /// Completed egui passes; the shell's log cadence reads it.
    pub frame_index: u64,
    /// The asset dialog that is open, if one is: the create or rename
    /// window's state, kept here so a shell reload keeps it.
    pub asset_dialog: Option<AssetDialog>,
    /// The Preferences tab's unsaved field buffers, keyed like the file;
    /// `None` until the tab seeds them from the snapshot and after a save,
    /// so the rows re-seed from the file the next frame.
    pub preferences_draft: Option<BTreeMap<String, String>>,
    /// The floating viewport toolboxes' session state.
    pub toolbox: ToolboxSession,
    /// The Assets tab's project browser: the folder its grid lists, the
    /// tree's open folders and the tile size.
    pub browser: ProjectBrowser,
}

/// The floating viewport toolbox's session state (Stage 22): a drag parks
/// one scene view's toolbox, and the log reports a placement once.
///
/// Session-scoped, like the dock's floating tabs' positions: a restart
/// starts every toolbox at its default corner. Persisting the origins into
/// the layout document is follow-up work.
pub struct ToolboxSession {
    /// Per-scene-view panel origins in logical points; `None` until a drag
    /// parks that view's toolbox, so the default anchor keeps following the
    /// viewport until the user moves it.
    pub origins: [Option<Pos2>; SCENE_VIEWPORTS],
    /// Per-scene-view panel rectangles whose rows the log already reported,
    /// so the rect lines appear once per placement and once per drag.
    pub logged: [Option<Rect>; SCENE_VIEWPORTS],
}

impl Default for ToolboxSession {
    fn default() -> Self {
        Self {
            origins: [None; SCENE_VIEWPORTS],
            logged: [None; SCENE_VIEWPORTS],
        }
    }
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
            asset_dialog: None,
            preferences_draft: None,
            toolbox: ToolboxSession::default(),
            browser: ProjectBrowser::default(),
        }
    }
}

impl Default for EguiShellState {
    fn default() -> Self {
        Self::new()
    }
}
