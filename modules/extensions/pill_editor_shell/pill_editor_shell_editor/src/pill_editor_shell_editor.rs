//! The editor shell: everything of the editor that reloads while it runs.
//!
//! # Responsibilities
//!
//! - Registers the shell's state and the boundary resources, and the
//!   per-frame system that builds the editor UI ([`editor_shell_update`]).
//! - Re-claims every resource on every load, so a reload keeps the layout,
//!   the widget state and the data the previous generation built.
//!
//! # Design
//!
//! The executable owns the window, the GPU device and the input; this crate
//! owns panels, docking and the UI state, and the two only exchange plain
//! data ([`EditorShellInput`] and [`EditorShellFrame`], defined in
//! `pill_editor_api`, the shared `dylib` both link). Everything else - the
//! state, the docking layout, the chrome and the panel painting - lives in
//! [`shell`].
//!
//! This file is the crate's hot file: the patch machinery compiles
//! [`editor_shell_update`] into a crate of its own, replaying this file's
//! top-level `use` lines there. Everything the system calls is therefore
//! `pub` in [`shell`] and named through the package
//! (`use pill_editor_shell_editor::shell::...`), never as a file-local item
//! - the same constraint a project's hot file follows, for the same reason.

// The crate's own name, so the package paths below resolve inside this
// crate too; the generated patch crate sees the same `use` lines.
extern crate self as pill_editor_shell_editor;

// External crates
use pill_editor_api::{EditorRect, EditorShellFrame, EditorShellInput};
use pill_editor_core::layout::{compute_layout, LayoutMetrics, Rect as LayoutRect};
use pill_engine::{pill_hot, pill_module, Engine, ResMut};

// Current crate
use pill_editor_shell_editor::shell::{
    collect_panel_placements, paint_dock_chrome, paint_panels, select_tab_by_title,
    EditorShellState,
};

/// The shell's UI work: state, docking layout, panel dispatch and
/// painting.
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
/// Public so the generated `host_module_pill_editor_shell_editor` wrapper
/// can carry it as `pill_module_init`.
#[pill_module]
pub fn register(engine: &mut Engine) -> u32 {
    let world = engine.world_mut();
    // Step 1: the shell's own state, built on first load only.
    world.register_resource::<EditorShellState>();
    if world.get_resource::<EditorShellState>().is_none() {
        world.insert_resource(EditorShellState::new());
    }
    // Step 2: the boundary resources, shared with the executable.
    world.register_resource::<EditorShellInput>();
    if world.get_resource::<EditorShellInput>().is_none() {
        world.insert_resource(EditorShellInput::default());
    }
    world.register_resource::<EditorShellFrame>();
    if world.get_resource::<EditorShellFrame>().is_none() {
        world.insert_resource(EditorShellFrame::default());
    }
    // Step 3: the per-frame system that builds the editor UI.
    engine.register_system("editor_shell", editor_shell_update);
    // NOTE: asserted on by the editor's end-to-end suite.
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        "[editor] pill_editor_shell_editor loaded"
    );
    0
}

// =============================================================================
// editor_shell_update
// =============================================================================

/// Builds one editor UI frame from this frame's input.
///
/// Runs inside the host's frame, between the executable's input queueing and
/// its presentation: the executable writes [`EditorShellInput`] before
/// `run_frame` and drains [`EditorShellFrame`] after it.
#[pill_hot]
pub fn editor_shell_update(
    mut state: ResMut<EditorShellState>,
    mut input: ResMut<EditorShellInput>,
    mut frame: ResMut<EditorShellFrame>,
) {
    // The resources are this crate's own registrations; a world without
    // them is not an editor's, and there is nothing to do.
    let (Some(mut state), Some(mut input), Some(mut frame)) =
        (state.get_mut(), input.get_mut(), frame.get_mut())
    else {
        return;
    };
    // Step 1: this frame's input, then the UI frame begins: press edges and
    // queued text apply once, here.
    for event in input.events.drain(..) {
        state.ui.input.apply(&event);
    }
    if let Some(title) = input.requested_tab.take() {
        select_tab_by_title(&mut state.layout, &title);
    }
    state.ui.input.begin_frame();
    state.frame_index += 1;
    // Step 2: docking geometry for the client area.
    let available = input.logical_size;
    let snapshot = compute_layout(
        &state.layout,
        LayoutRect::new(0.0, 0.0, available.x as f64, available.y as f64),
        LayoutMetrics::default(),
    );
    // Step 3: rebuild the frame. The scene rectangle goes to the executable
    // first: it places the child surface there before this frame presents.
    frame.clear();
    frame.scene_rect = snapshot.scene_rect.map(|rect| {
        EditorRect::new(
            rect.x as f32,
            rect.y as f32,
            rect.width as f32,
            rect.height as f32,
        )
    });
    frame.game_rect = snapshot.game_rect.map(|rect| {
        EditorRect::new(
            rect.x as f32,
            rect.y as f32,
            rect.width as f32,
            rect.height as f32,
        )
    });
    let editor_frames_per_second = input.editor_frames_per_second;
    // Step 4: the panels' hit rectangles from the previous frame, saved
    // before anything repaints: each panel's build must test against the
    // previous frame's full map, not the panels painted before it.
    let previous_hits = state.ui.hit_rects().clone();
    let placements = collect_panel_placements(&state.layout, &snapshot);
    // The heartbeat the end-to-end suite waits on, and the reload proof:
    // the frame counter keeps climbing across a shell reload because the
    // state resource is re-claimed, not reset.
    if state.frame_index % 210 == 0 {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            frames = state.frame_index,
            panels = placements.len(),
            "[editor] shell frame"
        );
    }
    // Step 5: chrome first (it sits under the panels' content), then the
    // panel contents themselves.
    paint_dock_chrome(&mut state, &snapshot, &mut frame, &previous_hits);
    paint_panels(
        &mut state,
        &placements,
        &mut frame,
        editor_frames_per_second,
        &previous_hits,
    );
    // Step 6: this frame's atlas additions, for the executable to upload.
    frame.atlas = state.atlas.take_update();
}
