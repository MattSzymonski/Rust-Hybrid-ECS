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
    activate_widget, collect_panel_placements, console_channel, execute_command,
    focus_module_panel, handle_command_shortcuts, handle_file_drops, handle_menu_pointer,
    paint_dock_chrome, paint_drop_preview, paint_menu, paint_palette, paint_panels, paint_toasts,
    run_clipboard_action, scroll_last_widget, select_tab_by_title, sync_command_files,
    sync_panel_commands, ClipboardAction, ConsoleLine, EditorShellState, Toast, CONSOLE_LINES_MAX,
    MENU_BAR_HEIGHT, PREFERENCES_FILE, TOAST_FRAMES,
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
    // Step 3b: the shared selection (Stage 17), re-claimed so a reload
    // keeps which entity and asset were selected.
    world.register_resource::<pill_editor_core::editor_state::EditorSelection>();
    if world
        .get_resource::<pill_editor_core::editor_state::EditorSelection>()
        .is_none()
    {
        world.insert_resource(pill_editor_core::editor_state::EditorSelection::default());
    }
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
// The two tuples are the engine's resource-parameter shape (a system
// accepts at most six parameters, and the tuples carry the shell's eleven
// resources); the macro reads the tuple types literally, so a pair of type
// aliases would defeat it.
#[allow(clippy::type_complexity)]
#[pill_hot]
pub fn editor_shell_update(
    // The function takes two tuple parameters because a system accepts at
    // most six; the tuples carry the shell's eleven resources.
    view: (
        ResMut<EditorShellState>,
        ResMut<EditorShellInput>,
        ResMut<EditorShellFrame>,
        Res<EditorSnapshot>,
        Res<pill_editor_api::EditorLogs>,
        Res<pill_editor_api::EditorClipboardAnswer>,
    ),
    commands: (
        ResMut<CommandRegistry>,
        ResMut<CommandInbox>,
        ResMut<PendingEditorCommands>,
        ResMut<CommandHistory>,
        ResMut<EditorPanelRegistry>,
        ResMut<pill_editor_core::editor_state::EditorSelection>,
    ),
) {
    // The resources are this crate's own registrations; a world without
    // them is not an editor's, and there is nothing to do.
    let (mut state, mut input, mut frame, editor_snapshot, logs, clipboard_answer) = view;
    let (mut registry, mut inbox, mut pending, mut history, mut panels, mut selection) = commands;
    let (
        Some(mut state),
        Some(mut input),
        Some(mut frame),
        Some(editor_snapshot),
        Some(logs),
        Some(clipboard_answer),
        Some(mut registry),
        Some(mut inbox),
        Some(mut pending),
        Some(mut history),
        Some(panels),
        Some(mut selection),
    ) = (
        state.get_mut(),
        input.get_mut(),
        frame.get_mut(),
        editor_snapshot.get(),
        logs.get(),
        clipboard_answer.get(),
        registry.get_mut(),
        inbox.get_mut(),
        pending.get_mut(),
        history.get_mut(),
        panels.get_mut(),
        selection.get_mut(),
    )
    else {
        return;
    };
    // Step 0: copy the capture ring's new lines into the console's bounded
    // window (owned strings only; the ring is shared with every module).
    let new_lines = logs.0.since(state.console_watermark);
    if !new_lines.is_empty() {
        state.console_watermark = new_lines.last().map(|line| line.seq).unwrap_or(0);
        for line in new_lines {
            state.console_total += 1;
            state.console_lines.push_back(ConsoleLine {
                level: line.level,
                channel: console_channel(&line.target),
                target: line.target,
                message: line.message,
            });
        }
        while state.console_lines.len() > CONSOLE_LINES_MAX {
            state.console_lines.pop_front();
        }
    }
    // The console's heartbeat: channel counts, on the same cadence as the
    // shell heartbeat (the suite reads it to prove both channels carry
    // lines and that a reload did not drop them).
    if state.frame_index.is_multiple_of(210) && !state.console_lines.is_empty() {
        let engine_lines = state
            .console_lines
            .iter()
            .filter(|line| line.channel == 1)
            .count();
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            lines = state.console_lines.len(),
            engine = engine_lines,
            game = state.console_lines.len() - engine_lines,
            "[editor] console state"
        );
    }
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
    if let Some(widget) = input.requested_widget.take() {
        activate_widget(&mut state, &widget);
    }
    if let Some(lines) = input.requested_wheel.take() {
        scroll_last_widget(&mut state, lines);
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
    // The Assets panel's rectangle goes to the executable (a scripted file
    // drop aims at it); a hovering or dropped OS drag resolves here, where
    // the placements and the cursor both live (Stage 18).
    frame.assets_rect = placements
        .iter()
        .find(|placement| placement.kind == pill_editor_core::layout::PanelKind::Assets)
        .map(|placement| {
            EditorRect::new(
                placement.rect.x as f32,
                placement.rect.y as f32,
                placement.rect.width as f32,
                placement.rect.height as f32,
            )
        });
    // The resource guard derefs to the input; take one plain reference for
    // the drop routing and everything the frame does after it.
    let input: &mut EditorShellInput = &mut input;
    handle_file_drops(&mut state, input, &placements, &mut pending);
    // The executable's host notices (Stage 19): toast events drain into the
    // shell's own stack, and a problems revision that moved is logged once,
    // so the suite can see the panel took it.
    let current_frame = state.frame_index;
    for event in input.notices.events.drain(..) {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            seq = event.seq,
            text = event.text.as_str(),
            "[editor] toast shown"
        );
        state.toasts.push_back(Toast {
            level: event.level,
            text: event.text,
            expires_at_frame: current_frame + TOAST_FRAMES,
        });
    }
    if input.notices.revision != state.notices_revision_seen {
        state.notices_revision_seen = input.notices.revision;
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            revision = input.notices.revision,
            count = input.notices.problems.len(),
            "[editor] problems panel updated"
        );
    }
    // The heartbeat the end-to-end suite waits on, and the reload proof:
    // the frame counter keeps climbing across a shell reload because the
    // state resource is re-claimed, not reset.
    if state.frame_index.is_multiple_of(210) {
        let mut by_cost: Vec<(&String, &f32)> = state.panel_draw_millis.iter().collect();
        by_cost.sort_by(|left, right| right.1.total_cmp(left.1));
        let slowest = by_cost
            .iter()
            .take(3)
            .map(|(title, millis)| format!("{title} {millis:.1} ms"))
            .collect::<Vec<_>>()
            .join(", ");
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            frames = state.frame_index,
            panels = placements.len(),
            hits = state.ui.hit_rects().len(),
            paint_us = state.last_panels_us,
            slowest = slowest.as_str(),
            "[editor] shell frame"
        );
    }
    // The clipboard work a command queued (Stage 18), run after the frame's
    // clear so its object and read request survive to the executable, and
    // before the panel view borrows the selection (a cut clears it).
    run_clipboard_action(
        &mut state,
        &mut frame,
        editor_snapshot,
        &mut selection,
        &mut pending,
        clipboard_answer,
    );
    // Step 5: chrome first (it sits under the panels' content), then the
    // panel contents themselves, then the menu layer over everything. The
    // panels hand back queued edits, command ids and state changes the
    // shell drains here (Stage 16/17).
    let view = pill_editor_shell_editor::shell::PanelView {
        snapshot: editor_snapshot,
        logs: &logs.0,
        selection: &selection,
        panels: &panels,
        problems: &input.notices.problems,
    };
    paint_dock_chrome(&mut state, &snapshot, &mut frame, &previous_hits);
    let panels_started = pill_core::platform::Instant::now();
    let work = paint_panels(
        &mut state,
        &placements,
        &mut frame,
        &view,
        editor_frames_per_second,
        &previous_hits,
    );
    state.last_panels_us = panels_started.elapsed().as_micros() as u64;
    // The drop preview paints over the panel content while a drag hovers.
    paint_drop_preview(&mut state, &mut frame, &placements);
    for batch in work.batches {
        pending.batches.push(batch);
    }
    for command in work.commands {
        inbox.executed.push(command);
    }
    if let Some(entity) = work.select_entity {
        if selection.entity != Some(entity) {
            selection.entity = Some(entity);
            // NOTE: asserted on by the editor's end-to-end suite.
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                entity = %entity,
                "[editor] selection changed"
            );
        }
    }
    if let Some(path) = work.select_asset {
        if selection.asset.as_deref() != Some(path.as_str()) {
            selection.asset = Some(path.clone());
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                path = path.as_str(),
                "[editor] asset selected"
            );
        }
    }
    // The Inspector's per-component Copy button (Stage 18): the panel layer
    // cannot reach the frame, so the request joins the clipboard actions.
    if let Some(type_name) = work.copy_component {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            type_name = type_name.as_str(),
            "[editor] clipboard component requested"
        );
        state.clipboard_action = Some(ClipboardAction::CopyComponent { type_name });
    }
    // An asset row held down goes to the executable (Stage 18): a drag that
    // leaves the window becomes an OS drag of that file. The frame clears
    // every frame, so a row that is no longer held drops out here.
    if let Some(source) = work.drag_candidate {
        frame.drag_candidate = Some(source);
    }
    // A Problems row's click (Stage 19) goes to the executable, which hands
    // the file to the developer's editor.
    if let Some(request) = work.open_file {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            file = request.file.as_str(),
            line = request.line,
            "[editor] problems row activated"
        );
        frame.requested_open = Some(request);
    }
    if let Some(level) = work.console_level {
        state.console_level = level;
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            level,
            "[editor] console filter level"
        );
    }
    if let Some(channel) = work.console_channel {
        state.console_channel = channel;
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            channel,
            "[editor] console filter channel"
        );
    }
    if let Some(search) = work.console_search {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            search = search.as_str(),
            "[editor] console search"
        );
    }
    // The asset listing came in (or grew): one line for the suite and for
    // anyone reading the log.
    if state.assets_listed != editor_snapshot.assets.len() {
        state.assets_listed = editor_snapshot.assets.len();
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            count = state.assets_listed,
            "[editor] assets listed"
        );
    }
    if let Some(preferences) = work.preferences {
        // The Preferences panel's save (D31): write the file; the
        // live-reload reader applies it next frame and logs that it did.
        let path = pill_editor_core::layout::layout_directory().join(PREFERENCES_FILE);
        match std::fs::write(&path, preferences) {
            Ok(()) => pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                path = %path.display(),
                "[editor] preferences written"
            ),
            Err(error) => pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::ECS,
                error = %error,
                "[editor] preferences write failed"
            ),
        }
    }
    let focus = state.ui.focus();
    if state.frame_index % 210 == 0 {
        let inspector_built = state
            .widget_names
            .keys()
            .filter(|name| name.starts_with("inspector."))
            .count();
        let inspector_hits = state
            .widget_names
            .iter()
            .filter(|(name, id)| {
                name.starts_with("inspector.") && state.ui.hit_rects().contains_key(id)
            })
            .count();
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            built = inspector_built,
            hits = inspector_hits,
            "[editor] inspector census"
        );
    }
    if focus != state.last_focus {
        state.last_focus = focus;
        let aliases: Vec<&String> = state
            .widget_names
            .iter()
            .filter(|(_, widget)| Some(**widget) == focus)
            .map(|(name, _)| name)
            .collect();
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            focus = ?focus,
            names = ?aliases,
            "[editor] ui focus"
        );
    }
    paint_menu(&mut state, &registry, &mut frame, available);
    paint_palette(&mut state, &registry, &mut frame, available);
    // Step 5b: the toast stack, over everything the panels drew.
    paint_toasts(&mut state, &mut frame, available);
    // Step 6: this frame's atlas additions, for the executable to upload.
    frame.atlas = state.atlas.take_update();
}
