//! The egui editor shell: the docked UI that reloads while the editor runs.
//!
//! # Responsibilities
//!
//! - Registers the shell's session state and the boundary resources, and the
//!   per-frame system that runs the dock ([`egui_shell_update`]).
//! - Re-claims every resource on every load, so a reload keeps the dock
//!   layout, the egui context and the shared selection.
//!
//! # Design
//!
//! The executable owns the window, the GPU device and the input; this crate
//! owns the dock and the egui pass, and the two exchange plain data
//! ([`EguiShellInput`] and [`EguiShellFrame`], defined in `pill_ui_egui`,
//! the shared `dylib` both link). Input events transit through the engine's
//! `Input` resource, and the shell converts them itself.
//!
//! This file is the crate's hot file: the patch machinery compiles
//! [`egui_shell_update`] into a crate of its own, replaying this file's
//! top-level `use` lines there. Everything the system calls is therefore
//! `pub` in [`shell`] and named through the package
//! (`use pill_editor_shell_egui_editor::shell::...`), never as a file-local
//! item - the same constraint a project's hot file follows, for the same
//! reason.
//!
//! An edit in `shell.rs` is patched the same way, and its generated crate
//! adds only `use pill_editor_shell_egui_editor::*;` on top of that file's own
//! `use` lines. That glob reaches this module's items, so the shell's public
//! items are re-exported below: without the re-export a tab body's call to a
//! sibling by bare name does not resolve, the patch fails to compile, and the
//! host silently falls back to reloading the module.

// The crate's own name, so the package paths below resolve inside this
// crate too; the generated patch crate sees the same `use` lines.
extern crate self as pill_editor_shell_egui_editor;

// External crates
use pill_editor_core_egui::commands::{ensure_command_resources, PendingEditorCommands};
use pill_editor_core_egui::editor_state::{EditorSelection, EditorSnapshot};
use pill_editor_core_egui::notices::EditorNotices;
use pill_editor_core_egui::session::EguiShellState;
use pill_engine::{pill_hot, pill_module, Engine, Res, ResMut};
use pill_ui_egui::boundary::{EguiShellFrame, EguiShellInput};
use pill_ui_egui::panels::{PanelCommands, PanelRegistrations, PanelRegistry, PanelRequest};

// The build script scans this crate and emits one address entry per function
// into `function_inventory.rs`; the `include!` is what makes every function
// resolvable by qualified path with nothing in its source annotated. Without
// it the host can patch only the `#[pill_hot]` system, whose address rides
// the engine registry, and a body edit to `shell::run_frame` falls back to a
// reload.
include!(concat!(env!("OUT_DIR"), "/function_inventory.rs"));

// Current crate
// The shell's public items, re-exported where a patch crate's
// `use pill_editor_shell_egui_editor::*;` can reach them (see the crate
// header). `run_frame` comes through this glob too, which is why the system
// below names it bare.
pub use pill_editor_shell_egui_editor::shell::*;

/// The shell's UI work: the dock, the two tabs and the frame build.
pub mod shell;

// =============================================================================
// Registration
// =============================================================================

/// Registers the shell against the host engine. Returns zero on success.
///
/// Re-claims every resource it owns: the reload protocol releases resources
/// the arriving generation does not re-register, and re-registering keeps
/// the existing value instead of resetting it.
///
/// Public so the generated `host_module_pill_editor_shell_egui_editor`
/// wrapper can carry it as `pill_module_init`.
#[pill_module]
pub fn register(engine: &mut Engine) -> u32 {
    let world = engine.world_mut();
    // Step 1: the command resources the assets actions queue into (the
    // registry, the inbox, the pending batches and the history), shared
    // with the executable and re-claimed so a reload keeps them.
    ensure_command_resources(world);
    // Step 2: the session (the egui context and the dock), built on first
    // load only. Its type lives in `pill_editor_core_egui`, so a reload of
    // this module finds the same resource and keeps the context alive.
    world.register_resource::<EguiShellState>();
    if world.get_resource::<EguiShellState>().is_none() {
        world.insert_resource(EguiShellState::new());
    }
    // Step 3: the boundary resources, shared with the executable.
    world.register_resource::<EguiShellInput>();
    if world.get_resource::<EguiShellInput>().is_none() {
        world.insert_resource(EguiShellInput::default());
    }
    world.register_resource::<EguiShellFrame>();
    if world.get_resource::<EguiShellFrame>().is_none() {
        world.insert_resource(EguiShellFrame::default());
    }
    // Step 4: the data the tabs read and the selection they write, re-claimed
    // so a reload keeps which entity was selected. The executable refreshes
    // the snapshot each frame; a host without one gets an empty dock instead
    // of a missing resource.
    world.register_resource::<EditorSnapshot>();
    if world.get_resource::<EditorSnapshot>().is_none() {
        world.insert_resource(EditorSnapshot::default());
    }
    world.register_resource::<EditorSelection>();
    if world.get_resource::<EditorSelection>().is_none() {
        world.insert_resource(EditorSelection::default());
    }
    // Step 4b: the host's notices, which the executable rewrites whenever a
    // reload, a patch or a failure lands (W-0038 Stages 4 and 6). Its type is
    // shared, so the shell reads what the arriving generation wrote.
    world.register_resource::<EditorNotices>();
    if world.get_resource::<EditorNotices>().is_none() {
        world.insert_resource(EditorNotices::default());
    }
    // Step 5: the per-frame system that runs the dock. Post-update, like the
    // renderer's frame build: it keeps running while the engine is paused.
    // Step 6: this module's own editor panel (W-0038 Stage 11). The request is
    // queued rather than registered: the executable binds it to the liveness
    // marker that dies with this module's image, which is what keeps a stale
    // draw function from ever being called.
    world.register_resource::<PanelRegistrations>();
    if world.get_resource::<PanelRegistrations>().is_none() {
        world.insert_resource(PanelRegistrations::default());
    }
    if let Some(registrations) = world.get_resource_mut::<PanelRegistrations>() {
        registrations.push(PanelRequest {
            id: "shell.demo",
            title: "Demo Panel",
            source: "pill_editor_shell_egui_editor",
            draw: shell::demo_panel,
        });
        registrations.push(PanelRequest {
            id: shell::SPLINE_PANEL_ID,
            title: "Spline Tool",
            source: "pill_editor_shell_egui_editor",
            draw: shell::spline_tool_panel,
        });
    }
    engine.register_post_update_system("egui_shell", egui_shell_update);
    // NOTE: asserted on by the egui editor's end-to-end suite.
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        "[editor] pill_editor_shell_egui_editor loaded"
    );
    0
}

// =============================================================================
// egui_shell_update
// =============================================================================

/// Runs one egui pass: dock and tabs, then the frame the executable draws.
///
/// Runs inside the host's frame, between the executable's input queueing and
/// its presentation: the executable writes [`EguiShellInput`] - geometry,
/// focus and the events of the editor's own windows - before `run_frame` and
/// drains [`EguiShellFrame`] after it.
#[allow(clippy::type_complexity)]
#[pill_hot]
pub fn egui_shell_update(
    resources: (
        ResMut<EguiShellState>,
        ResMut<EguiShellInput>,
        ResMut<EguiShellFrame>,
        // The nested tuples keep this system within the engine's six-parameter
        // tuple arity.
        (
            Res<EditorSnapshot>,
            ResMut<EditorSelection>,
            Res<EditorNotices>,
        ),
        (
            ResMut<PendingEditorCommands>,
            // The panels the loaded modules registered (W-0038 Stage 11) and
            // the commands they asked for this pass.
            Res<PanelRegistry>,
            ResMut<PanelCommands>,
        ),
    ),
) {
    // The resources are this crate's own registrations; a world without them
    // is not an editor's, and there is nothing to do.
    let (
        mut state,
        mut input,
        mut frame,
        (snapshot, mut selection, notices),
        (mut pending, panels, mut panel_commands),
    ) = resources;
    let (
        Some(mut state),
        Some(input),
        Some(mut frame),
        (Some(snapshot), Some(mut selection), Some(notices)),
        (Some(mut pending), Some(panels), Some(mut panel_commands)),
    ) = (
        state.get_mut(),
        input.get_mut(),
        frame.get_mut(),
        (snapshot.get(), selection.get_mut(), notices.get()),
        (pending.get_mut(), panels.get(), panel_commands.get_mut()),
    )
    else {
        return;
    };
    run_frame(
        &mut state,
        &input,
        &mut frame,
        snapshot,
        &mut selection,
        &mut pending,
        notices,
        panels,
        &mut panel_commands.commands,
    );
}
