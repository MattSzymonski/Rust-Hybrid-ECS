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
use pill_ui_egui::egui::{Context, Pos2, Rect, TextureHandle, TextureOptions};

// Current crate
use crate::assets::AssetDialog;
use crate::browser::ProjectBrowser;
use crate::layout::{EditorDock, EditorTab};

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
    /// The brand mark's texture, uploaded on the first frame and kept for the
    /// session: the context owns it, so a shell reload reuses it.
    pub mark: Option<TextureHandle>,
    /// The menu bar's already-logged geometry and the requests its items
    /// raised.
    pub menubar: MenubarSession,
    /// The Inspector's collapsed component sections and its logged geometry.
    pub inspector: InspectorSession,
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

/// The menu bar's logged geometry: the strip and each title, so a placement is
/// reported once (the toolbox's pattern) and a suite can click the titles.
pub struct MenubarSession {
    /// The strip's last logged rectangle.
    pub bar: Option<Rect>,
    /// Each title's last logged rectangle, by [`crate::menubar::Menu::ALL`]
    /// index.
    pub titles: [Option<Rect>; crate::menubar::MENU_TITLE_COUNT],
    /// Each open menu entry's last logged rectangle, keyed `menu/label`; a
    /// menu's entries log while it is open, so their presence is the proof it
    /// opened.
    pub entries: Vec<(String, Rect)>,
    /// Undo/redo the menus asked for, drained by the executable.
    pub history_requests: Vec<HistoryRequest>,
    /// The dock tab the Window menu asked for, drained by the executable.
    pub pending_tab: Option<EditorTab>,
    /// Whether the About window is open.
    pub about_open: bool,
    /// The menu whose dropdown is open, while one is; the log reports each
    /// opening once so a suite can tell an open menu from a closed one.
    pub open: Option<crate::menubar::Menu>,
    /// Whether a menu asked the editor to quit; the shell moves it to the
    /// frame at the end of the pass and clears it.
    pub quit_requested: bool,
}

impl Default for MenubarSession {
    fn default() -> Self {
        Self {
            bar: None,
            titles: [None; crate::menubar::MENU_TITLE_COUNT],
            entries: Vec::new(),
            history_requests: Vec::new(),
            pending_tab: None,
            about_open: false,
            open: None,
            quit_requested: false,
        }
    }
}

/// An undo or redo the menu bar asked for, drained by the executable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryRequest {
    /// Undo the newest recorded step.
    Undo,
    /// Redo the newest undone step.
    Redo,
}

/// The Inspector's session state: which component sections are collapsed, and
/// the geometry its log already reported.
///
/// Session-scoped like the toolbox: a shell reload keeps it, a restart starts
/// every section expanded. The collapse state is keyed by the registered type
/// name, so selecting another entity keeps a type's foldout where the user
/// left it (Unity's behaviour).
#[derive(Default)]
pub struct InspectorSession {
    /// The component type names whose sections are collapsed.
    pub collapsed: std::collections::BTreeSet<String>,
    /// Each section header's last logged rectangle, keyed by type name.
    pub section_rects: Vec<(String, Rect)>,
    /// The Add Component button's last logged rectangle.
    pub add_rect: Option<Rect>,
    /// Whether the Add Component popup is open.
    pub add_open: bool,
    /// The addable entries' last logged rectangles, keyed by type name.
    pub add_entries: Vec<(String, Rect)>,
    /// The row signatures already logged, so a panel that redraws every frame
    /// does not flood the log with the same row.
    pub rows_logged: Vec<String>,
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
            mark: None,
            menubar: MenubarSession::default(),
            inspector: InspectorSession::default(),
        }
    }

    /// The brand mark's texture, uploaded on first use and cloned from then on.
    pub fn mark_texture(&mut self) -> TextureHandle {
        if let Some(mark) = &self.mark {
            return mark.clone();
        }
        let texture = self.context.load_texture(
            "pill_editor.mark",
            crate::brand::mark_color_image(),
            TextureOptions::LINEAR,
        );
        self.mark = Some(texture.clone());
        texture
    }
}

impl Default for EguiShellState {
    fn default() -> Self {
        Self::new()
    }
}
