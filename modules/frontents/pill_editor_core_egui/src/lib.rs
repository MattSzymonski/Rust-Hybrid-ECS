//! The editor's UI-independent core: the dock layout model and the editor's
//! view of the engine.
//!
//! # Responsibilities
//!
//! - Own the dock layout: the tab set, the default layout and versioned
//!   persistence through `egui_dock`'s state ([`layout`]).
//! - Own the editor's snapshots of the engine and the commands it applies at
//!   the frame boundary ([`editor_state`]).
//! - Own the clipboard payload documents the copy and paste commands share
//!   ([`clipboard`]).
//! - Own the transform gizmos' geometry, drag math and snapping
//!   ([`gizmo`]).
//! - Own the floating viewport toolbox's rows, geometry and commands
//!   ([`toolbox`]).
//! - Declare the layout persistence failures ([`error`]).
//!
//! # Design
//!
//! Nothing here depends on a window or a GPU, and egui enters only as
//! `egui_dock`'s state types: the shell owns the egui `Context`, this crate
//! stays UI-thin (W-0017 Stage 5). The crate is a shared `dylib` (see its
//! manifest) because its values cross between the editor executable and the
//! reloadable egui shell module.

/// The Assets pipeline: the dialogs' state and the recorded file batches.
pub mod assets;
/// The Assets tab's project browser: the open folder and the listing math
/// the tree and the grid share.
pub mod browser;
/// The clipboard payload documents the copy and paste commands share.
pub mod clipboard;
/// The command registry: named actions, menus and rebindable shortcuts.
pub mod commands;
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
/// The Inspector's editing helpers: field edits and asset settings rows.
pub mod inspector;
/// The `egui_dock` layout: tab set, default layout and persistence.
pub mod layout;
/// The Preferences tab's shared vocabulary: the file's editable keys.
pub mod preferences;
/// Build diagnostics: the problems the Problems panel shows.
pub mod problems;
/// The egui shell's long-lived session state.
pub mod session;
/// The Systems tab's shared logic: row labels, system toggles and the
/// pause/step controls.
pub mod systems;
/// The floating viewport toolbox: rows, geometry and the commanded edits.
pub mod toolbox;
