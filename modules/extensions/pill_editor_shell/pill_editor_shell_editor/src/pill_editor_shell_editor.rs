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
use pill_editor_core::editor_state::{EditorCommand, EditorSnapshot};
use pill_editor_core::history::CommandHistory;
use pill_editor_core::layout::{compute_layout, LayoutMetrics, Rect as LayoutRect};
use pill_engine::{pill_hot, pill_module, Engine, Res, ResMut};

// Current crate
use pill_editor_shell_editor::shell::{
    activate_widget, collect_panel_placements, console_channel, drain_panel_work, execute_command,
    focus_module_panel, handle_command_shortcuts, handle_file_drops, handle_menu_pointer,
    handle_popup_closed, handle_popup_pointer, handle_window_closed, handle_window_key,
    handle_window_pointer, handle_window_text, paint_context_menu, paint_dock_chrome,
    paint_drop_preview, paint_menu, paint_overlay_badge, paint_overlay_labels, paint_palette,
    paint_panels, paint_toasts, paint_windows, run_clipboard_action, scroll_last_widget,
    select_tab_by_title, sync_command_files, sync_panel_commands, ConsoleLine, EditorShellState,
    Toast, CONSOLE_LINES_MAX, MENU_BAR_HEIGHT, TOAST_FRAMES,
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
    // Step 3c: the module-panel registry (Stage 16). The update below reads
    // it every frame; module panels claim it through
    // `register_editor_panels` when they load before the shell does, but a
    // run with no module panel and no test channel has no other writer, and
    // a missing registry would silence the whole shell.
    EditorPanelRegistry::ensure(world);
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
    // The live readouts refresh at their own cadence, and hold while the
    // project is paused (Stage 22, D25); both the badge and the Statistics
    // panel read the sampled value.
    state.refresh_readout(input.editor_frames_per_second);
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
        if !handle_menu_pointer(&mut state, &event) {
            state.ui.input.apply(&event);
        }
    }
    // Step 1a: the popups' own input (Stage 21). A popup is a window of its
    // own, so its pointer never arrives as an `InputEvent`; the ids the
    // platform closed (an outside click, a lost focus, Escape) clear the
    // state that asked for them.
    for pointer in input.popup_pointers.drain(..) {
        handle_popup_pointer(
            &mut state,
            &registry,
            &mut inbox,
            &mut pending,
            &mut history,
            &pointer,
        );
    }
    for id in input.popup_closed.drain(..) {
        handle_popup_closed(&mut state, &id);
    }
    // Step 1a-bis: the free-standing windows' own input (Stage 24): their
    // pointers, the keys a modal answers and the ids of the windows the
    // platform closed under them. A window's pointer never arrives as an
    // `InputEvent` any more than a popup's does.
    for pointer in input.window_pointers.drain(..) {
        handle_window_pointer(&mut state, &pointer);
    }
    for key in input.window_keys.drain(..) {
        handle_window_key(
            &mut state,
            &registry,
            &mut inbox,
            &mut pending,
            &mut history,
            &key,
        );
    }
    for id in input.window_closed.drain(..) {
        handle_window_closed(&mut state, &id);
    }
    for text in input.window_text.drain(..) {
        handle_window_text(&mut state, &text);
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
    // The badge and the Statistics panel show the sampled readout, not the
    // executable's live feed (Stage 22, D25).
    let editor_frames_per_second = state.readout_frames_per_second;
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
    // A selected folder targets a drop (Stage 25): the panel writes folders
    // into the shared asset selection, and the import lands there.
    let drop_folder = selection.asset.as_deref().and_then(|path| {
        editor_snapshot
            .assets
            .iter()
            .any(|asset| asset.path == path && asset.kind == "folder")
            .then(|| path.to_string())
    });
    handle_file_drops(
        &mut state,
        input,
        &placements,
        &mut pending,
        drop_folder.as_deref(),
    );
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
    let mut work = paint_panels(
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
    // The selection requests travel with the caller: the frame's shared
    // view still borrows the selection for the window pass below.
    let selected = (
        std::mem::take(&mut work.select_entity),
        std::mem::take(&mut work.select_asset),
    );
    drain_panel_work(
        &mut state,
        work,
        &mut frame,
        &panels,
        &mut pending,
        &mut inbox,
    );
    // The free-standing windows (Stage 24): the panels drained just above
    // may have asked for tool windows or modals; each open one now draws its
    // panel with its own widget state, and its own work drains through the
    // same path a docked panel's did.
    let mut window_work = paint_windows(
        &mut state,
        &view,
        &panels,
        &mut frame,
        &registry,
        &mut inbox,
        &mut pending,
        &mut history,
    );
    let window_selected = (
        std::mem::take(&mut window_work.select_entity),
        std::mem::take(&mut window_work.select_asset),
    );
    drain_panel_work(
        &mut state,
        window_work,
        &mut frame,
        &panels,
        &mut pending,
        &mut inbox,
    );
    // The selection a panel asked for, from either layer, goes through the
    // recorded command path (Stage 25): consecutive changes merge into one
    // undo step and undo walks selections back like it walks edits. The
    // apply lands at this frame's boundary, so panels see it next frame.
    if let Some(entity) = selected.0.or(window_selected.0) {
        if selection.entity != Some(entity) {
            pending.push_recorded(
                vec![EditorCommand::SetSelection {
                    entity: Some(entity),
                    asset: selection.asset.clone(),
                }],
                "select entity",
                Some("editor.selection".to_string()),
            );
            // NOTE: asserted on by the editor's end-to-end suite.
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                entity = %entity,
                "[editor] selection changed"
            );
        }
    }
    if let Some(path) = selected.1.or(window_selected.1) {
        if selection.asset.as_deref() != Some(path.as_str()) {
            pending.push_recorded(
                vec![EditorCommand::SetSelection {
                    entity: selection.entity,
                    asset: Some(path.clone()),
                }],
                "select asset",
                Some("editor.selection".to_string()),
            );
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                path = path.as_str(),
                "[editor] asset selected"
            );
        }
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
    paint_context_menu(&mut state, &registry, &mut frame);
    // Step 5b: the overlay layer's own content (Stage 21): the labels the
    // executable projected, the toast stack and the frame-rate badge, all of
    // them above the panels and above the view surfaces' windows (D24).
    paint_overlay_labels(&mut state, &mut frame, &input.overlay_labels);
    paint_toasts(&mut state, &mut frame, available);
    paint_overlay_badge(&mut state, &mut frame, editor_frames_per_second);
    // Step 6: this frame's atlas additions, for the executable to upload.
    frame.atlas = state.atlas.take_update();
    // The frame stamp the executable watches (Stage 22): at the end, so any
    // panic in the update's body leaves it where it was and the shell reads
    // as stopped, not as healthy.
    frame.frames = state.frame_index;
}
