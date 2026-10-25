//! The egui editor's shared core: its UI library, the dock layout model and
//! the editor's view of the engine.
//!
//! # Responsibilities
//!
//! - Re-export `egui` and `egui_dock` as the one copy the egui editor's world
//!   shares: its frontend, its reloadable shell module and this crate all
//!   name them through here.
//! - Carry the shell boundary: [`boundary::EguiShellInput`] and
//!   [`boundary::EguiShellFrame`], the plain data the frontend and the shell
//!   exchange through world resources (plan decision E7), and the module
//!   panels a dock tab names ([`panels`]).
//! - Provide the engine-input to egui bridge ([`input`]), the editor's egui
//!   style ([`style`]) and Phosphor's vendored icon fonts with the painting
//!   of the chrome's icons ([`icons`]).
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
//! Nothing here depends on a window or a GPU: the frontend owns the device
//! and draws the shell's frame with `egui-wgpu`, and the shell owns the egui
//! `Context` (`test_engine_layout.py` checks that no windowing or GPU crate is
//! reachable). The crate is a shared `dylib` (see its manifest) because its
//! values - the boundary types, the layout, the commands - cross between the
//! editor executable and the reloadable egui shell module, and one compiled
//! copy per process gives them one identity (plan decision E3).
//!
//! The egui editor's module world (this crate plus the shell) and host world
//! (this crate plus the frontend and `egui-wgpu`) must resolve `egui`,
//! `egui_dock` and `epaint` identically; the manifest's pin block requests
//! the union.
//!
//! # The reload-safety contract
//!
//! egui's `Context` can keep arbitrary typed values and callbacks alive past
//! the frame that stored them. Reloadable crates (`extensions/**`,
//! `examples/**`) must never store their own types or closures in it: after a
//! reload, the stored value's drop code or vtable points into the unloaded
//! image. The APIs that keep boxed `Any`/`Fn` values inside the context are:
//!
//! - `Context::data` and `Context::data_mut` (the `IdTypeMap` behind them)
//! - `Context::set_request_repaint_callback`
//! - `Context::on_begin_pass` and `Context::on_end_pass` (plugins)
//! - any shape carrying `egui::PaintCallback`
//!
//! Long-lived egui state therefore lives in a world resource defined by this
//! crate (the egui shell's [`session::EguiShellState`], re-claimed by every
//! shell generation); transient state stays inside the frame that made it.
//! `test_coding_standards.py` flags those calls in reloadable crates - the two
//! entry points, the map's own methods and any `dyn Any` - and a line that
//! deliberately touches the map carries the `egui-state-ok` marker (a test
//! planting a previous generation's value is the only legitimate use).
//!
//! The shell also clears the map on the first frame of every generation
//! (`pill_editor_shell_egui_editor::shell::claim`): whatever a previous
//! generation left there would be read through an unloaded image's vtable, so
//! the claim drops it before the new generation draws. The state that has to
//! survive a reload is a world resource, which is exactly why the claim can
//! clear the rest without losing anything.

pub use egui;
pub use egui_dock;

/// The Assets pipeline: the dialogs' state and the recorded file batches.
pub mod assets;
/// The plain data crossing the frontend/shell boundary.
pub mod boundary;
/// The editor's brand mark: the pill logotype as baked coverage.
pub mod brand;
/// The Assets tab's project browser: the open folder and the listing math
/// the tree and the grid share.
pub mod browser;
/// The redraw cadence: when the editor owes a frame (W-0038 Stage 9).
pub mod cadence;
/// The scene viewports' orbit cameras and their navigation gestures.
pub mod camera;
/// The clipboard payload documents the copy and paste commands share.
pub mod clipboard;
/// The command registry: named actions, menus and rebindable shortcuts.
pub mod commands;
/// The Console tab's history, channels and filters (W-0038 Stage 5).
pub mod console;
/// The frame boundary: the pending-command drain and the undo/redo walk.
pub mod drain;
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
/// The editor panels a module registers (W-0038 Stage 11): ids, draw
/// functions, liveness markers and the slot a dock tab names.
pub mod panels;
/// The Preferences tab's shared vocabulary: the file's editable keys.
pub mod preferences;
/// Asset previews: the preference that turns them on and the tiles worth
/// drawing one for (W-0049 Stage 11).
pub mod previews;
/// Build diagnostics: the problems the Problems panel shows.
pub mod problems;
/// The world resources the Inspector can show and edit (W-0038 Stage 14).
pub mod resources;
/// The open scene: its path, guid and unsaved marker, and the file operations
/// behind New, Open, Save and Save As (W-0048 Stage 9).
pub mod scene_workflow;
/// The scripted test channel's grammar: one typed parse of the suites' lines.
pub mod scripted;
/// The egui shell's long-lived session state.
pub mod session;
/// The editor's egui theme.
pub mod style;
/// The Systems tab's shared logic: row labels, system toggles and the
/// pause/step controls.
pub mod systems;
/// The floating viewport toolbox: rows, geometry and the commanded edits.
pub mod toolbox;
