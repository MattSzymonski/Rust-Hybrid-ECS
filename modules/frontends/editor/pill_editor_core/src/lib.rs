//! The egui editor's shared core: its UI library, the dock layout model and
//! the editor's view of the engine.
//!
//! # Responsibilities
//!
//! - Re-export `egui` and `egui_dock` as the one copy the editor's frontend,
//!   its reloadable shell module and this crate all name.
//! - Carry the shell boundary ([`boundary`]) and the module panels a dock tab
//!   names ([`panels`]).
//! - Provide the engine-input to egui bridge ([`input`]), the editor's egui
//!   style ([`style`]) and Phosphor's vendored icon fonts with the icons the
//!   chrome paints ([`icons`]).
//! - Own the dock layout: the tab set, the default layout and versioned
//!   persistence through `egui_dock`'s state ([`layout`]).
//! - Own the editor's snapshots of the engine and the frame-boundary commands
//!   ([`editor_state`]), the clipboard documents ([`clipboard`]), the
//!   transform gizmos ([`gizmo`]) and the floating viewport toolbox
//!   ([`toolbox`]).
//! - Declare the layout persistence failures ([`error`]).
//!
//! # Design
//!
//! Nothing here depends on a window or a GPU: the frontend owns the device
//! and draws the shell's frame with `egui-wgpu`, and the shell owns the egui
//! `Context` (`test_engine_layout.py` checks that no windowing or GPU crate is
//! reachable). The crate is a shared `dylib`: the boundary types, the layout
//! and the commands cross between the editor executable and the reloadable
//! shell module, so one compiled copy per process gives them one identity.
//! The module world and the host world must resolve `egui`, `egui_dock` and
//! `epaint` identically, and the manifest pins the union of their features.
//!
//! # The reload-safety contract
//!
//! egui's `Context` keeps arbitrary typed values and callbacks alive past the
//! frame that stored them. Reloadable crates (`extensions/**`,
//! `examples/**`) must never store their own types or closures in it: after a
//! reload, the stored value's drop code or vtable points into the unloaded
//! image. The APIs that keep boxed `Any`/`Fn` values inside the context are
//! `Context::data` and `Context::data_mut` (the `IdTypeMap` behind them),
//! `Context::set_request_repaint_callback`, `Context::on_begin_pass` and
//! `Context::on_end_pass` (plugins), and any shape carrying
//! `egui::PaintCallback`. `test_coding_standards.py` flags those calls in
//! reloadable crates; a line that deliberately touches the map carries the
//! `egui-state-ok` marker, which only a test planting a previous generation's
//! value may use.
//!
//! Long-lived egui state therefore lives in a world resource defined by this
//! crate (the shell's [`session::EguiShellState`], re-claimed by every shell
//! generation). The shell clears the map on the first frame of every
//! generation (`pill_editor_shell_egui_editor::shell::claim`), because
//! whatever a previous generation left there would be read through an
//! unloaded image's vtable.

pub use egui;
pub use egui_dock;

/// The UI kit: the theme-to-egui translator, the font installer and the
/// scoped style variants. Re-exported so the shell and the executable
/// never name the rlib directly.
pub use pill_ui_kit as kit;

/// The Assets pipeline: the dialogs' state and the recorded file batches.
pub mod assets;
/// The plain data crossing the frontend/shell boundary.
pub mod boundary;
/// The editor's brand mark: the pill logotype as baked coverage.
pub mod brand;
/// The Assets tab's project browser: the open folder and the listing math
/// the tree and the grid share.
pub mod browser;
/// The redraw cadence: when the editor owes a frame.
pub mod cadence;
/// The scene viewports' orbit cameras and their navigation gestures.
pub mod camera;
/// The clipboard payload documents the copy and paste commands share.
pub mod clipboard;
/// The command registry: named actions, menus and rebindable shortcuts.
pub mod commands;
/// The Console tab's history, channels and filters.
pub mod console;
/// The frame boundary: the pending-command drain and the undo/redo walk.
pub mod drain;
/// The editor's own state documents on disk: one durability rule, one error
/// surface.
pub mod editor_files;
/// The editor's snapshots of the engine and the commands it applies to it.
pub mod editor_state;
/// The Entities tab's shared actions: the recorded create and delete.
pub mod entities;
/// Layout persistence failures.
pub mod error;
/// Transform gizmos: handle geometry, drag math and snapping.
pub mod gizmo;
/// The undo and redo history of editor edits.
pub mod history;
/// Phosphor's icons: the fonts, the glyphs and the duotone painting.
pub mod icons;
/// The engine-input to egui bridge.
pub mod input;
/// The Inspector's editing helpers: field edits and asset settings rows.
pub mod inspector;
/// The `egui_dock` layout: tab set, default layout and persistence.
pub mod layout;
/// The editor's menu bar: its titles, metrics and the items each menu holds.
pub mod menubar;
/// The host's notices as plain data: toast events and Problems rows.
pub mod notices;
/// The editor panels a module registers: ids, draw
/// functions, liveness markers and the slot a dock tab names.
pub mod panels;
/// The Preferences tab's shared vocabulary: the file's editable keys.
pub mod preferences;
/// Asset previews: the preference that turns them on and the tiles worth
/// drawing one for.
pub mod previews;
/// Build diagnostics: the problems the Problems panel shows.
pub mod problems;
/// The world resources the Inspector can show and edit.
pub mod resources;
/// The open scene: its path, guid and unsaved marker, and the file operations
/// behind New, Open, Save and Save As.
pub mod scene_workflow;
/// The scripted test channel's grammar: one typed parse of the suites' lines.
pub mod scripted;
/// The egui shell's long-lived session state.
pub mod session;
/// Polling the editor's state files for changes.
pub mod state_watch;
/// The editor's egui theme.
pub mod style;
/// The Systems tab's shared logic: row labels, system toggles and the
/// pause/step controls.
pub mod systems;
/// The floating viewport toolbox: rows, geometry and the commanded edits.
pub mod toolbox;

/// Scaffolding the crate's tests share.
#[cfg(test)]
pub(crate) mod test_support {
    /// The mounted asset directory is process-wide, so the tests that mount
    /// one take this in turn - across modules, which is why the lock lives
    /// here rather than in one module's private test module.
    pub(crate) fn asset_mount_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
