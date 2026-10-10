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
use pill_editor_api::panels::EditorPanelRegistry;
use pill_editor_api::{EditorRect, EditorShellFrame, EditorShellInput};
use pill_editor_core::commands::{
    ensure_command_resources, CommandInbox, CommandRegistry, PendingEditorCommands,
};
use pill_editor_core::editor_state::EditorSnapshot;
use pill_editor_core::history::CommandHistory;
use pill_editor_core::layout::{compute_layout, LayoutMetrics, Rect as LayoutRect};
use pill_engine::{pill_hot, pill_module, Engine, Res, ResMut};

// Current crate
use pill_editor_shell_editor::shell::{
    collect_panel_placements, execute_command, focus_module_panel, handle_command_shortcuts,
    handle_menu_pointer, paint_dock_chrome, paint_menu, paint_palette, paint_panels,
    select_tab_by_title, sync_command_files, sync_panel_commands, EditorShellState,
    MENU_BAR_HEIGHT,
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
    // Step 3: the command layer's resources: the registry (with the
    // built-in commands), the inbox module owners drain and the queue of
    // engine mutations the executable applies.
    ensure_command_resources(world);
    // Step 4: the per-frame system that builds the editor UI. Post-update,
    // like the renderer's frame build: it keeps running while the engine is
    // paused - which is what lets the pause command be its own undo, and
    // keeps the menus live over a frozen scene.
    engine.register_post_update_system("editor_shell", editor_shell_update);
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
    // The function takes two tuple parameters because a system accepts at
    // most six; the tuples carry the seven resources the shell works with.
    view: (
        ResMut<EditorShellState>,
        ResMut<EditorShellInput>,
        ResMut<EditorShellFrame>,
        Res<EditorSnapshot>,
    ),
    commands: (
        ResMut<CommandRegistry>,
        ResMut<CommandInbox>,
        ResMut<PendingEditorCommands>,
        ResMut<CommandHistory>,
        ResMut<EditorPanelRegistry>,
    ),
) {
    // The resources are this crate's own registrations; a world without
    // them is not an editor's, and there is nothing to do.
    let (mut state, mut input, mut frame, editor_snapshot) = view;
    let (mut registry, mut inbox, mut pending, mut history, mut panels) = commands;
    let (
        Some(mut state),
        Some(mut input),
        Some(mut frame),
        Some(editor_snapshot),
        Some(mut registry),
        Some(mut inbox),
        Some(mut pending),
        Some(mut history),
        Some(panels),
    ) = (
        state.get_mut(),
        input.get_mut(),
        frame.get_mut(),
        editor_snapshot.get(),
        registry.get_mut(),
        inbox.get_mut(),
        pending.get_mut(),
        history.get_mut(),
        panels.get_mut(),
    )
    else {
        return;
    };
    // Step 1: this frame's input. The menu layer gets first refusal on
    // pointer events - a dropdown click must not also press a widget under
    // it - and key presses are collected with the modifiers current at
    // their moment, for the shortcut pass once the files are current.
    state.paused = input.systems_paused;
    let mut pressed: Vec<(pill_engine::KeyCode, pill_engine::Modifiers)> = Vec::new();
    for event in input.events.drain(..) {
        match &event {
            pill_engine::InputEvent::ModifiersChanged { modifiers } => {
                state.modifiers = *modifiers;
            }
            pill_engine::InputEvent::CursorMoved { position } => {
                state.cursor = *position;
            }
            _ => {}
        }
        if let pill_engine::InputEvent::Key {
            key,
            state: pill_engine::ButtonState::Pressed,
        } = &event
        {
            pressed.push((*key, state.modifiers));
        }
        if !handle_menu_pointer(
            &mut state,
            &registry,
            &mut inbox,
            &mut pending,
            &mut history,
            &event,
        ) {
            state.ui.input.apply(&event);
        }
    }
    if let Some(title) = input.requested_tab.take() {
        select_tab_by_title(&mut state.layout, &title);
    }
    if let Some(panel_id) = input.requested_panel.take() {
        focus_module_panel(&mut state, &panel_id);
    }
    // Step 1b: the keybindings and preferences files, then the dispatch:
    // commands the scripted channel requested run through the same path a
    // binding does.
    sync_command_files(&mut state, &mut registry, &mut history);
    sync_panel_commands(&mut state, &panels, &mut registry);
    for id in input.requested_commands.drain(..) {
        execute_command(
            &mut state,
            &registry,
            &mut inbox,
            &mut pending,
            &mut history,
            &id,
        );
    }
    handle_command_shortcuts(
        &mut state,
        &registry,
        &mut inbox,
        &mut pending,
        &mut history,
        &pressed,
    );
    state.ui.input.begin_frame();
    state.frame_index += 1;
    // Step 1c: the registry's shape, logged whenever it changes, so a
    // module's later registration shows up without a restart.
    let command_count = registry.commands().count();
    let menus = pill_editor_core::commands::menu_tree(&registry);
    if (command_count, menus.len()) != state.registry_signature {
        state.registry_signature = (command_count, menus.len());
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            commands = command_count,
            menus = menus.len(),
            "[editor] command registry ready"
        );
        for (top, specs) in &menus {
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                path = %top,
                items = specs.len(),
                "[editor] menu registered"
            );
        }
    }
    // Step 2: docking geometry for the client area under the menu bar.
    let available = input.logical_size;
    let snapshot = compute_layout(
        &state.layout,
        LayoutRect::new(
            0.0,
            MENU_BAR_HEIGHT,
            available.x as f64,
            (available.y as f64 - MENU_BAR_HEIGHT).max(0.0),
        ),
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
    // panel contents themselves, then the menu layer over everything. The
    // panels hand back queued edits and command ids the shell drains into
    // its own queues (Stage 16).
    paint_dock_chrome(&mut state, &snapshot, &mut frame, &previous_hits);
    let work = paint_panels(
        &mut state,
        &placements,
        &mut frame,
        &editor_snapshot,
        &panels,
        editor_frames_per_second,
        &previous_hits,
    );
    for batch in work.batches {
        pending.batches.push(batch);
    }
    for command in work.commands {
        inbox.executed.push(command);
    }
    paint_menu(&mut state, &registry, &mut frame, available);
    paint_palette(&mut state, &registry, &mut frame, available);
    // Step 6: this frame's atlas additions, for the executable to upload.
    frame.atlas = state.atlas.take_update();
}
