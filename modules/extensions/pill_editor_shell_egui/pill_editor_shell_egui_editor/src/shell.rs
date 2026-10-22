//! The shell's UI work: the dock, the tabs and the frame build.
//!
//! # Responsibilities
//!
//! - [`run_frame`]: one egui pass, from the boundary input to the boundary
//!   frame the executable draws, including the scene rectangle the frontend
//!   places the scene's child window at.
//! - The tab bodies ([`entities_tab`], [`systems_tab`], [`inspector_tab`],
//!   [`assets_tab`], [`preferences_tab`], all `#[pill_hot_fn]` so a body
//!   edit patches live) and their row builders.
//!   The scene and game tabs draw nothing: their bodies stay holes in the
//!   UI so the scene and game child windows, which sit under the overlay,
//!   show through.
//!
//! # Design
//!
//! The dock is the session's `egui_dock` state, so layout edits land in the
//! resource the next generation re-claims. Nothing here stores anything in
//! egui's `Context` (the reload-safety contract `pill_ui_egui` documents);
//! the tabs read the snapshot and write the shared selection.
//!
//! # Patchability
//!
//! A generated patch crate holds one function body and reaches this module
//! only through the crate root's re-export, so every item an edited body can
//! name - a sibling function, a constant, a type - is `pub` here. A body that
//! calls a helper by bare name is therefore fine; a body that names a private
//! item fails to compile as a patch and the host reloads the module instead.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;

/// The shared names every shell module globs.
pub mod prelude;

/// The [`claim`] module: the generation's claim on the shared egui context.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod claim;

pub use claim::claim_context_once;
pub use claim::claim_popout_contexts;
pub use claim::claim_shell_context;

/// The Systems tab: pause/step and the recorded system toggles.
///
/// Public like every tab module: the build script's function inventory
/// names each addressable function by its module path, so the module a
/// patchable function lives in cannot be private.
pub mod tab_systems;

pub use tab_systems::systems_tab;

/// The [`toasts`] module: the host's notices drawn as a short-lived stack.
///
/// Public like every tab module: the build script's function inventory
/// names each addressable function by its module path, so the module cannot
/// be private.
pub mod toasts;

/// The [`menubar`] module: the menu bar: the rows' menus and the actions they run.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod bar;

pub use bar::about_window;
pub use bar::menu_bar;

/// The [`palette`] module: the searchable command popup (W-0038 Stage 3).
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod palette;

pub use bar::menu_body;
pub use bar::run_menu_action;
pub use bar::MENU_TITLE_GAP;
pub use palette::palette_window;

/// The [`tab_console`] module: the Console tab's filters and rows (W-0038
/// Stage 5).
///
/// Public like every tab module: the build script's function inventory names
/// each addressable function by its module path, so the module cannot be
/// private.
pub mod tab_console;

pub use tab_console::console_tab;

/// The [`tab_problems`] module: build errors and warnings, grouped by module
/// (W-0038 Stage 6).
///
/// Public like every tab module: the build script's function inventory names
/// each addressable function by its module path, so the module cannot be
/// private.
pub mod tab_problems;

pub use tab_problems::problems_tab;

/// The [`tab_statistics`] module: frame, pass and per-tab timings (W-0038
/// Stage 7).
///
/// Public like every tab module: the build script's function inventory names
/// each addressable function by its module path, so the module cannot be
/// private.
pub mod tab_statistics;

pub use tab_statistics::statistics_tab;

/// The [`tab_demo`] module: the shell module's own editor panel, the reference
/// a module's panel is written against (W-0038 Stage 11).
///
/// Public like every tab module: the build script's function inventory names
/// each addressable function by its module path, so the module cannot be
/// private.
pub mod tab_demo;

pub use tab_demo::demo_panel;

/// The [`panel_windows`] module: the windows and modals module panels ask the
/// shell for (W-0038 Stage 13).
///
/// Public like every tab module: the build script's function inventory names
/// each addressable function by its module path, so the module cannot be
/// private.
pub mod panel_windows;

pub use panel_windows::{panel_windows, PanelFeed};

/// The [`spline_tool`] module: the Spline Tool panel and its re-seed modal
/// (W-0038 Stages 12 and 13).
///
/// Public like every tab module: the build script's function inventory names
/// each addressable function by its module path, so the module cannot be
/// private.
pub mod spline_tool;

pub use spline_tool::{
    reseed_command, reseed_window, spline_tool_panel, Reseed, RESEED_POINTS, RESEED_WINDOW,
    SPLINE_COMPONENT, SPLINE_PANEL_ID,
};

/// The [`resource_section`] module: the Inspector's rows for one editable
/// world resource (W-0038 Stage 14).
///
/// Public like every tab module: the build script's function inventory names
/// each addressable function by its module path, so the module cannot be
/// private.
pub mod resource_section;

pub use resource_section::{queue_edit as queue_resource_edit, resource_section};

/// The [`views`] module: the scene and game viewports: their geometry and overlay chips.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod views;

pub use views::frame_counter_rect;
pub use views::scene_body_rect;
pub use views::scene_overlay;
pub use views::shell_rect;
pub use views::SCENE_BUTTON_GAP;
pub use views::SCENE_BUTTON_WIDTH;
pub use views::SCENE_COUNTER_HEIGHT;
pub use views::SCENE_COUNTER_MARGIN;
pub use views::SCENE_COUNTER_WIDTH;
pub use views::SCENE_RECT_OVERSHOOT;
pub use views::SLOW_UPDATE_MICROS;

/// The [`chrome`] module: the chrome both tabs share: icons, selection moves and logged rects.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod chrome;

pub use chrome::apply_tab_redock;
pub use chrome::click_from_modifiers;
pub use chrome::context_menu;
pub use chrome::empty_area;
pub use chrome::gizmo_mode_label;
pub use chrome::gizmo_space_label;
pub use chrome::log_context_menu;
pub use chrome::mark_menu_region;
pub use chrome::remember_moved_rect;
pub use chrome::select_asset;
pub use chrome::select_entity;
pub use chrome::select_entity_click;
pub use chrome::tab_icon;
pub use chrome::SelectionClick;
pub use chrome::SPACE_ROWS;
pub use chrome::TOOL_ROWS;

/// The [`menus`] module: the context menus the tabs open.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod menus;

pub use menus::component_menu_items;
pub use menus::scene_context_menu;

/// The [`toolbox`] module: the floating toolbox over a scene viewport.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod toolbox_panel;

pub use toolbox_panel::toolbox_overlay;

/// The [`tab_entities`] module: the entities tab: the entity rows and their menu.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod tab_entities;

pub use tab_entities::entities_tab;
pub use tab_entities::entity_menu_items;
pub use tab_entities::entity_rows;

/// The [`tab_preferences`] module: the preferences tab: the editor-wide rows and the project's gizmo state.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod tab_preferences;

pub use tab_preferences::preferences_tab;

/// The [`tab_inspector`] module: the inspector tab: the selection's components and their rows.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod tab_inspector;

pub use tab_inspector::addable_components;
pub use tab_inspector::entity_inspector;
pub use tab_inspector::inspector_add_popup;
pub use tab_inspector::inspector_header;
pub use tab_inspector::inspector_section_header;
pub use tab_inspector::inspector_tab;
pub use tab_inspector::INSPECTOR_LABEL_WIDTH;
pub use tab_inspector::INSPECTOR_VECTOR_FIELD_WIDTH;

/// The [`inspector_fields`] module: the inspector's field rows: the decorators, the row model and the.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod inspector_fields;

pub use inspector_fields::field_widget_name;
pub use inspector_fields::inspector_field_row;
pub use inspector_fields::inspector_row;
pub use inspector_fields::is_scalar_field;
pub use inspector_fields::remember_inspector_row;

/// The [`inspector_widgets`] module: the inspector's field controls.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod inspector_widgets;

pub use inspector_widgets::ascending;
pub use inspector_widgets::component_rows;
pub use inspector_widgets::format_field_value;
pub use inspector_widgets::integer_edit;
pub use inspector_widgets::read_only_row;
pub use inspector_widgets::scalar_edit;
pub use inspector_widgets::scalar_edit_decorated;
pub use inspector_widgets::Number;

/// The [`inspector_settings`] module: the inspector's asset import settings.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod inspector_settings;

pub use inspector_settings::asset_settings_row;
pub use inspector_settings::asset_settings_rows;
pub use inspector_settings::settings_commit;

/// The [`tab_assets`] module: the assets tab: the folder tree and the open folder's grid.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod tab_assets;

pub use tab_assets::assets_grid;
pub use tab_assets::assets_path_bar;
pub use tab_assets::assets_tab;
pub use tab_assets::assets_tree;
pub use tab_assets::assets_zoom_bar;
pub use tab_assets::folder_tree_rows;
pub use tab_assets::ASSETS_TREE_CARET_SIZE;
pub use tab_assets::ASSETS_TREE_CARET_WIDTH;
pub use tab_assets::ASSETS_TREE_INDENT;
pub use tab_assets::ASSETS_TREE_WIDTH;
pub use tab_assets::ASSETS_TREE_WIDTH_RANGE;

/// The [`assets_tiles`] module: the assets tab's tiles, their menu and the asset dialog.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
pub mod assets_tiles;

pub use assets_tiles::asset_dialog_window;
pub use assets_tiles::asset_icon;
pub use assets_tiles::asset_menu_items;
pub use assets_tiles::asset_tile;
pub use assets_tiles::tile_name_job;
pub use assets_tiles::AssetTileWrites;
pub use assets_tiles::ASSETS_TILE_CORNER_RADIUS;
pub use assets_tiles::ASSETS_TILE_GAP;
pub use assets_tiles::ASSETS_TILE_ICON_INSET;
pub use assets_tiles::ASSETS_TILE_LABEL_GAP;
pub use assets_tiles::ASSETS_TILE_LABEL_HEIGHT;
pub use assets_tiles::ASSETS_TILE_LABEL_ROWS;
pub use assets_tiles::ASSETS_TILE_LABEL_SIZE;
pub use toasts::{draw_toasts, push_notices};

/// The [`tests`] module: the shell's unit tests.
///
/// Public like every tab module: the build script's function
/// inventory names each addressable function by its
/// module path, so the module cannot be private.
#[cfg(test)]
pub mod tests;

// =============================================================================
// Constants
// =============================================================================

// Public like every item a patched body may name: a patch crate sees this
// module through the crate root's re-export, and a private constant would
// fail its compile (`error[E0425]`) after which the host reloads.

/// One shell frame: the shell's own events become egui input, the dock runs,
/// and the output becomes the executable's frame. The asset dialog, when one
/// is open, is drawn over the dock.
// The executable hands the pass its world borrows; there is no smaller shape
// that does not mean building a struct per frame.
#[allow(clippy::too_many_arguments)]
pub fn run_frame(
    state: &mut EguiShellState,
    shell_input: &EguiShellInput,
    frame: &mut EguiShellFrame,
    snapshot: &EditorSnapshot,
    selection: &mut EditorSelection,
    pending: &mut PendingEditorCommands,
    notices: &EditorNotices,
    panels: &PanelRegistry,
    panel_commands: &mut Vec<PanelCommand>,
    clipboard: &mut pill_editor_core_egui::clipboard::EditorClipboard,
    popout_inputs: &pill_ui_egui::boundary::EguiPopoutInputs,
) {
    state.frame_index = state.frame_index.wrapping_add(1);
    let frame_index = state.frame_index;
    // The saved layout's module tabs name panel slots from the run that wrote
    // them; the ids it carried are what maps them to today's slots (Stage 11).
    // Once per session: a shell reload must not undo a dock the user moved.
    if !state.panels_remapped {
        let ids: Vec<String> = panels
            .entries()
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        state.dock.remap_panel_slots(&ids);
        state.panels_remapped = true;
    }
    // The host's notices become toasts before the dock draws, so a reload that
    // happened this frame is on screen in this frame.
    push_notices(state, notices);
    // The row heartbeat: the editor suite reads it to prove both tabs see
    // the project's data, that the frame counter - and with it the session
    // resource - survives a reload or patch, and which entity the
    // Inspector's detail tier describes.
    if frame_index.is_multiple_of(210) {
        let detail_entity = snapshot
            .detail
            .as_ref()
            .map_or_else(|| "-".to_string(), |detail| detail.entity.to_string());
        let detail_components = snapshot
            .detail
            .as_ref()
            .map_or(0, |detail| detail.components.len());
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            frames = frame_index,
            entities = snapshot.entities.len(),
            systems = snapshot.systems.len(),
            paused = snapshot.paused,
            detail = detail_entity.as_str(),
            detail_components,
            "[editor] egui shell rows"
        );
    }
    // The first frame of a shell generation owns the shared egui context:
    // whatever the previous generation left in its temporary data would be a
    // dangling read once this one draws (claim.rs has the reasoning).
    claim_shell_context(&state.context);
    // The UI heartbeat: one full shell update (input conversion, the egui
    // dock and its tabs, the frame conversion) in microseconds. The shell
    // benchmark measures the D32 UI-redraw case from it; the GPU overlay
    // draw is outside it, as presentation is vsync-bound.
    let ui_started = pill_core::platform::Instant::now();
    let raw_input = pill_ui_egui::input::raw_input(shell_input);
    // The brand mark is uploaded once per session: the context owns the
    // texture, so a shell reload finds it already there.
    let mark = state.mark_texture();
    // The context is a cheap clone (refcounted handle); the dock and the
    // asset dialog are separate fields, borrowed apart so the dock's layout
    // borrow and the dialog's state borrow coexist.
    let context = state.context.clone();
    // What the module panels read this pass (Stages 12 and 13): only built when
    // a panel is registered, so a build with none costs nothing.
    let panel_feed = panel_feed(snapshot, panels);
    // The answer the last pass's modal took, for exactly one pass.
    let mut panel_feed = panel_feed;
    panel_feed.frames_per_second = shell_input.frames_per_second;
    // A window the scripted channel asked for: the panel itself decides what
    // that id means. Kept until the pinged panel is drawn - a request must not
    // be dropped by a frame where that tab was not on screen.
    let panel_window_ping = state.panel_window_ping.clone();
    // A tile drag that left the windows hands its path to the executable
    // (W-0038 Stage 19), through the frame the frontend drains.
    let mut drag_out: Option<String> = None;
    let panel_answer =
        state
            .panel_windows
            .answer
            .as_ref()
            .map(|answer| pill_ui_egui::panels::PanelAnswer {
                window: answer.window.clone(),
                accepted: answer.accepted,
            });
    let mut panel_window_requests: Vec<(u16, PanelCommand)> = Vec::new();
    let EguiShellState {
        dock,
        asset_dialog,
        preferences_draft,
        toolbox,
        browser,
        menubar,
        inspector,
        toasts,
        palette,
        console_filters,
        problem_open,
        stats,
        panel_budget,
        panel_windows: windows,
        panel_window_ping: window_ping,
        popout_contexts,
        ..
    } = state;
    // Named through the package path, not as a file-local item: a hot patch
    // compiles this body in a crate of its own, where only the package's
    // public items exist (see the crate header).
    let mut viewer = pill_editor_shell_egui_editor::shell::EditorTabViewer {
        snapshot,
        selection,
        asset_dialog,
        pending,
        preferences_draft,
        toolbox,
        browser,
        inspector,
        scene_rects: vec![None; dock.scene_view_count()],
        game_rect: None,
        overlay_regions: Vec::new(),
        frames_per_second: shell_input.frames_per_second,
        tab_redock: None,
        console_filters,
        problem_open,
        stats,
        panels,
        panel_budget,
        panel_commands,
        panel_entities: panel_feed.entities.as_slice(),
        panel_detail: panel_feed.detail.as_ref(),
        panel_answer: panel_answer.as_ref(),
        panel_window_requests: &mut panel_window_requests,
        panel_window_ping: panel_window_ping
            .as_ref()
            .map(|(slot, window)| (*slot, window.as_str())),
        panel_window_ping_used: false,
        clipboard,
        drag_out: &mut drag_out,
    };
    let output = context.run_ui(raw_input, |ui| {
        // The strip comes first, so the dock - and every viewport rectangle it
        // reports - starts below it.
        menu_bar(ui, menubar, &mark, &mut viewer);
        // Tab bars carry the tabs and their close buttons only: without the
        // collapse chevron and the panel-level close-all ✖, a panel closes
        // exactly when its last tab does.
        DockArea::new(dock.state_mut())
            .style(style::dock_style(ui.ctx().global_style().as_ref()))
            .show_leaf_collapse_buttons(false)
            .show_leaf_close_all_buttons(false)
            .show_inside(ui, &mut viewer);
        // The create/rename dialog floats over the dock while it is open.
        pill_editor_shell_egui_editor::shell::asset_dialog_window(
            ui.ctx(),
            viewer.asset_dialog,
            viewer.pending,
            viewer.snapshot,
        );
        pill_editor_shell_egui_editor::shell::about_window(
            ui.ctx(),
            &mark,
            &mut menubar.about_open,
        );
        // The toast stack floats over the dock and every viewport it covers,
        // so its rows join the frame's overlay regions.
        pill_editor_shell_egui_editor::shell::draw_toasts(
            toasts,
            frame_index,
            ui.ctx(),
            &mut viewer.overlay_regions,
        );
        // The command palette sits over everything: it is the one surface that
        // is meant to be read while the pointer is elsewhere.
        pill_editor_shell_egui_editor::shell::palette_window(
            ui.ctx(),
            palette,
            menubar,
            &mut viewer,
        );
        // The windows the module panels asked for (Stage 13): applied here so
        // one a panel opens this frame is drawn in this frame, then drawn by
        // the panel that owns them.
        for (slot, request) in viewer.panel_window_requests.drain(..) {
            let source = panels
                .get(slot)
                .map(|entry| entry.source.clone())
                .unwrap_or_default();
            match request {
                PanelCommand::OpenWindow(request) => {
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        slot,
                        window = request.id.as_str(),
                        modal = request.modal,
                        "[editor] panel window opened"
                    );
                    windows.open(&source, slot, &request)
                }
                PanelCommand::CloseWindow(id) => {
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        slot,
                        window = id.as_str(),
                        "[editor] panel window closed"
                    );
                    windows.close(&source, &id)
                }
                _ => {}
            }
        }
        pill_editor_shell_egui_editor::shell::panel_windows(
            ui.ctx(),
            windows,
            panels,
            &mut *viewer.panel_budget,
            &panel_feed,
            &mut *viewer.panel_commands,
            &mut viewer.overlay_regions,
        );
    });
    let mut shell_frame = frame_from_output(&context, output);
    shell_frame.scene_rects = std::mem::take(&mut viewer.scene_rects);
    shell_frame.game_rect = viewer.game_rect.take();
    shell_frame.overlay_regions = std::mem::take(&mut viewer.overlay_regions);
    shell_frame.quit_requested = std::mem::take(&mut menubar.quit_requested);
    // One more pass per popped-out tab (W-0038 Stage 23): each runs its own
    // context, so its window has its own widget state, its own input record
    // and its own frame. The dock no longer holds these tabs, so nothing draws
    // them twice.
    claim_popout_contexts(popout_contexts);
    shell_frame.popout_frames =
        run_popout_passes(dock, popout_contexts, popout_inputs, &mut viewer);
    // A tab menu's re-dock runs after the dock finished drawing: changing
    // the tree mid-draw would edit it under the widget that asked.
    if let Some(surface) = viewer.tab_redock.take() {
        pill_editor_shell_egui_editor::shell::apply_tab_redock(dock.state_mut(), surface);
    }
    // The Window menu's "New Scene View" (W-0038 Stage 21): docked here for
    // the same reason, and activated through the executable's own path so the
    // new tab is at the front of its group the moment it appears.
    if std::mem::take(&mut menubar.new_scene_view) {
        let tab = dock.next_scene_view();
        if dock.contains(tab) {
            // Already docked: the command brought it back rather than adding.
            menubar.pending_tab = Some(tab);
        } else if dock.can_open_scene_view() {
            dock.state_mut().push_to_focused_leaf(tab);
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                tab = tab.name().as_str(),
                view = tab.scene_view().unwrap_or_default(),
                "[editor] scene view added"
            );
            menubar.pending_tab = Some(tab);
        } else {
            pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                limit = pill_ui_egui::boundary::SCENE_VIEWS_MAX,
                "[editor] scene view refused: the view limit is reached"
            );
        }
    }
    // The answer has been read by the pass that needed it; the next one must
    // not act on it again (W-0038 Stage 13).
    if panel_answer.is_some() {
        windows.answer = None;
    }
    // The pinged panel drew this pass, so its window request is spent.
    if viewer.panel_window_ping_used {
        *window_ping = None;
    }
    // A panel that crossed its budget this frame gets its toast once, named
    // after the tab so the user can find the panel it belongs to (Stage 11).
    // After the viewer's last use: its copy of the budget is what drew.
    for entry in &mut panel_budget.entries {
        if !entry.warned || entry.toasted {
            continue;
        }
        entry.toasted = true;
        let title = panels.tab_title(entry.slot);
        let text = format!(
            "Panel {} is over its frame budget ({:.1} ms)",
            title,
            entry.micros / 1000.0
        );
        toasts.push_local(NoticeLevel::Warning, text, frame_index);
    }
    *frame = shell_frame;
    // The tile drag's path travels in the frame the executable drains (W-0038
    // Stage 19): the shell cannot start an OLE drag itself.
    frame.drag_out = drag_out;
    let ui_micros = ui_started.elapsed().as_micros() as u64;
    // The Statistics tab's top row: the pass this frame actually took
    // (W-0038 Stage 7). Recorded before the frame's own logging so the tab and
    // the log line below never disagree.
    stats.record_shell(ui_micros);
    stats.frames_per_second = shell_input.frames_per_second;
    // The periodic heartbeat, plus any update that runs past the D32 UI
    // budget: the suites prove the counter advances, and the benchmark gets
    // samples even when 210 frames take longer than its measurement window.
    if frame_index.is_multiple_of(210) || ui_micros >= SLOW_UPDATE_MICROS {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            frames = frame_index,
            ui_us = ui_micros,
            "[editor] egui frame"
        );
    }
}

// =============================================================================
// The tab viewer and the tabs
// =============================================================================

/// Run one egui pass per popped-out tab (W-0038 Stage 23).
///
/// Each popped-out window has its own context, so it draws independently of
/// the dock pass that ran a moment earlier in the main window: same data, same
/// viewer code, its own widget state and its own input record. The tab body is
/// the `TabViewer::ui` implementation the dock calls, which is what keeps a
/// tab looking and behaving the same in a window as it does in the dock.
///
/// `dock.popouts()` holds tabs that are *not* in the dock tree, so nothing is
/// drawn twice. The viewer's viewport rectangles are reset per pass: a popped
/// out Scene tab reports its rectangle in its own window's coordinates, and
/// the frontend places that view's surface there.
pub fn run_popout_passes(
    dock: &pill_editor_core_egui::layout::EditorDock,
    contexts: &mut std::collections::BTreeMap<EditorTab, egui::Context>,
    inputs: &pill_ui_egui::boundary::EguiPopoutInputs,
    viewer: &mut EditorTabViewer<'_>,
) -> Vec<pill_ui_egui::boundary::PopoutFrame> {
    let mut frames = Vec::new();
    for tab in dock.popouts().iter().copied() {
        let name = tab.name();
        let context = contexts
            .entry(tab)
            .or_insert_with(|| {
                let context = egui::Context::default();
                pill_ui_egui::style::apply(&context);
                context
            })
            .clone();
        // A window the frontend has not reported yet gets a default record:
        // an empty event list, so the pass draws without interaction.
        let input = inputs.by_tab.get(&name).cloned().unwrap_or_default();
        let raw_input = pill_ui_egui::input::raw_input(&input);
        viewer.scene_rects = vec![None; dock.scene_view_count()];
        let output = context.run_ui(raw_input, |ui| {
            let mut popout_tab = tab;
            <EditorTabViewer<'_> as TabViewer>::ui(viewer, ui, &mut popout_tab);
        });
        let mut frame = pill_ui_egui::boundary::frame_from_output(&context, output);
        frame.scene_rects = std::mem::take(&mut viewer.scene_rects);
        frame.overlay_regions = std::mem::take(&mut viewer.overlay_regions);
        frames.push(pill_ui_egui::boundary::PopoutFrame {
            tab: name,
            title: format!("Pill Editor - {}", tab.title()),
            frame: Box::new(frame),
        });
    }
    frames
}

/// What module panels read this pass (W-0038 Stages 12 and 13).
///
/// Built only when a panel is registered: the rows are a copy of the snapshot
/// the tabs already read, and an editor with no module panels should not pay
/// for them.
pub fn panel_feed(snapshot: &EditorSnapshot, panels: &PanelRegistry) -> PanelFeed {
    let mut feed = PanelFeed {
        entities: Vec::new(),
        detail: None,
        frames_per_second: 0.0,
    };
    if panels.is_empty() {
        return feed;
    }
    feed.entities = snapshot
        .entities
        .iter()
        .map(|entry| PanelEntityRow {
            entity: entry.entity,
            display: entry.display.clone(),
            archetype: entry.archetype_label.clone(),
        })
        .collect();
    feed.detail = snapshot.detail.as_ref().map(|detail| PanelEntityDetail {
        entity: detail.entity,
        components: detail
            .components
            .iter()
            .map(|component| PanelComponentValues {
                component: component.type_name.clone(),
                fields: component
                    .fields
                    .iter()
                    .zip(component.values.iter())
                    .map(|(field, value)| PanelFieldValue {
                        name: field.name.to_string(),
                        value: inspector::value_text(value),
                    })
                    .collect(),
            })
            .collect(),
    });
    feed
}

/// Draws the dock's tabs; the tab bodies come from the tab functions.
///
/// Public - fields included - because a hot patch for [`run_frame`] is
/// compiled as its own crate and builds this value: the generated source
/// reaches the type and its fields through the package path, the rule the
/// crate header states for every hot function's dependencies.
pub struct EditorTabViewer<'a> {
    /// The engine data the tabs read.
    pub snapshot: &'a EditorSnapshot,
    /// The shared selection the Entities tab writes.
    pub selection: &'a mut EditorSelection,
    /// The asset dialog that is open, if one is; the Assets tab and the
    /// dialog window both read and write it.
    pub asset_dialog: &'a mut Option<AssetDialog>,
    /// The queue the assets actions push their recorded batches into.
    pub pending: &'a mut PendingEditorCommands,
    /// The Preferences tab's unsaved field buffers; the tab seeds them
    /// from the snapshot and a save clears them.
    pub preferences_draft: &'a mut Option<preferences::PreferencesDraft>,
    /// The floating viewport toolboxes' session state: where each scene
    /// view's toolbox floats and what the log already reported.
    pub toolbox: &'a mut ToolboxSession,
    /// The Inspector's collapsed sections and its logged geometry.
    pub inspector: &'a mut InspectorSession,
    /// The Assets tab's project browser: the folder its grid lists, the
    /// tree's open folders and the tile size.
    pub browser: &'a mut ProjectBrowser,
    /// The scene viewports' body rectangles this pass, in logical points,
    /// one slot per view index; the frontend places each viewport's child
    /// window at its slot. Sized from the dock before the pass, so a view
    /// the person added has its slot already (W-0038 Stage 21).
    pub scene_rects: Vec<Option<ShellRect>>,
    /// The game viewport's body rectangle this pass, in logical points; the
    /// frontend places the primary surface's child window at it.
    pub game_rect: Option<ShellRect>,
    /// The overlay chips' rectangles this pass, in logical points: points
    /// over a scene viewport where a click is the UI's, not the scene's.
    pub overlay_regions: Vec<ShellRect>,
    /// The editor's smoothed frame rate, for the scene's counter.
    pub frames_per_second: f32,
    /// The surface a tab menu asked to re-dock, drained by `run_frame`
    /// after the dock finishes drawing.
    pub tab_redock: Option<SurfaceIndex>,
    /// The Console tab's filters (W-0038 Stage 5); the executable applies them
    /// to the history on its next frame.
    pub console_filters: &'a mut ConsoleFilterSettings,
    /// The Problems row a click asked to open (W-0038 Stage 6); the executable
    /// drains it and launches the configured code editor.
    pub problem_open: &'a mut Option<usize>,
    /// The timings the Statistics tab reads (W-0038 Stage 7): the shell writes
    /// them around each tab body, the tab only formats them.
    pub stats: &'a mut StatisticsSession,
    /// The panels the loaded modules registered (W-0038 Stage 11): a module
    /// tab's title, body and liveness all come from here.
    pub panels: &'a PanelRegistry,
    /// Every module panel's build time, written around each panel body and
    /// read by the tab's badge and by the budget toast.
    pub panel_budget: &'a mut PanelBudget,
    /// The commands the module panels asked for this pass, applied by the
    /// executable at the frame boundary.
    pub panel_commands: &'a mut Vec<PanelCommand>,
    /// The world's entities, as module panels see them (W-0038 Stage 12).
    pub panel_entities: &'a [PanelEntityRow],
    /// The selected entity's components, as module panels see them.
    pub panel_detail: Option<&'a PanelEntityDetail>,
    /// The answer a modal window took, for the pass that follows it
    /// (W-0038 Stage 13).
    pub panel_answer: Option<&'a pill_ui_egui::panels::PanelAnswer>,
    /// The windows the panels asked to open or close this pass, with the slot
    /// that asked: applied after the dock, before the windows draw.
    pub panel_window_requests: &'a mut Vec<(u16, PanelCommand)>,
    /// The slot a scripted window request named, and the window id it wants
    /// that panel to open (W-0038 Stage 13).
    pub panel_window_ping: Option<(u16, &'a str)>,
    /// Whether the pinged panel drew this pass, so the request is spent.
    pub panel_window_ping_used: bool,
    /// The shell's side of the OS clipboard (W-0038 Stage 17): the request
    /// the executable carries out, and the text it read back.
    pub clipboard: &'a mut pill_editor_core_egui::clipboard::EditorClipboard,
    /// The asset path a tile drag handed to the OS (W-0038 Stage 19): the
    /// executable starts an OLE drag with it.
    pub drag_out: &'a mut Option<String>,
}

impl TabViewer for EditorTabViewer<'_> {
    type Tab = EditorTab;

    /// The dock needs one stable id per tab to key its widgets on.
    fn id(&mut self, tab: &mut Self::Tab) -> egui::Id {
        egui::Id::new(("pill_editor_shell_egui.tab", *tab))
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        // A module panel's title is the registry's: it changes when the module
        // unloads, and it carries the over-budget badge (Stage 11).
        if let Some(slot) = tab.module_slot() {
            let title = self.panels.tab_title(slot);
            let label = if self.panel_budget.is_slow(slot) {
                format!("{} (slow)", title)
            } else {
                title
            };
            return icons::rich(tab_icon(*tab), label.as_str()).into();
        }
        icons::rich(tab_icon(*tab), (*tab).title().as_str()).into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        // Every tab body is timed, because the Statistics tab's whole point is
        // that its rows are measurements rather than estimates (Stage 7).
        let started = pill_core::platform::Instant::now();
        let tab = *tab;
        match tab {
            EditorTab::Scene(view) => {
                let view = view as usize;
                if let Some(slot) = self.scene_rects.get_mut(view) {
                    *slot = Some(scene_body_rect(ui));
                }
                scene_context_menu(self, ui);
                scene_overlay(ui, self.frames_per_second, &mut self.overlay_regions);
                toolbox_overlay(self, ui, view);
            }
            EditorTab::Game => {
                // The game viewport is the primary surface's child window: a
                // hole, measured like a scene tab, with no menu and no
                // overlay chips - it is the project's own view.
                self.game_rect = Some(scene_body_rect(ui));
            }
            EditorTab::Entities => entities_tab(
                ui,
                self.snapshot,
                self.selection,
                self.pending,
                &mut self.overlay_regions,
            ),
            EditorTab::Systems => systems_tab(ui, self.snapshot, self.pending),
            EditorTab::Inspector => inspector_tab(
                ui,
                self.snapshot,
                &*self.selection,
                self.pending,
                &mut self.overlay_regions,
                self.inspector,
            ),
            EditorTab::Assets => {
                // One bundle of the handles the panel writes through, the
                // same shape a grid tile takes (see `AssetTileWrites`).
                let mut writes = AssetTileWrites {
                    selection: self.selection,
                    dialog: self.asset_dialog,
                    pending: self.pending,
                    overlay_regions: &mut self.overlay_regions,
                    drag_out: self.drag_out,
                };
                assets_tab(ui, self.snapshot, self.browser, &mut writes);
            }
            EditorTab::Preferences => {
                preferences_tab(ui, self.snapshot, self.preferences_draft, self.pending)
            }
            EditorTab::Console => console_tab(ui, self.snapshot, self.console_filters),
            EditorTab::Problems => problems_tab(ui, self.snapshot, self.problem_open),
            EditorTab::Statistics => {
                statistics_tab(ui, self.snapshot, self.stats, self.frames_per_second)
            }
            EditorTab::Module(slot) => {
                // The panel belongs to a module: its draw function is called
                // only while that module's liveness marker is alive, and the
                // tab says so when it is not (Stage 11).
                match self.panels.draw_of(slot) {
                    Some(draw) => {
                        let requested_window = self
                            .panel_window_ping
                            .filter(|(pinged, _)| *pinged == slot)
                            .map(|(_, window)| window);
                        if requested_window.is_some() {
                            self.panel_window_ping_used = true;
                        }
                        let mut context = PanelContext {
                            frames_per_second: self.frames_per_second,
                            selected_entity: self.selection.primary(),
                            entities: self.panel_entities,
                            detail: self.panel_detail,
                            window: None,
                            requested_window,
                            answer: self.panel_answer,
                            commands: self.panel_commands,
                        };
                        let started = pill_core::platform::Instant::now();
                        draw(ui, &mut context);
                        let micros = started.elapsed().as_micros() as f32;
                        // The window requests belong to this panel, and only
                        // this arm knows which slot asked.
                        let mut kept = Vec::with_capacity(context.commands.len());
                        for command in context.commands.drain(..) {
                            match command {
                                PanelCommand::OpenWindow(_) | PanelCommand::CloseWindow(_) => {
                                    self.panel_window_requests.push((slot, command))
                                }
                                other => kept.push(other),
                            }
                        }
                        *context.commands = kept;
                        let over_budget = self.panel_budget.record(slot, micros);
                        if over_budget {
                            pill_core::warn!(
                                target: pill_core::telemetry::telemetry_target::ECS,
                                slot,
                                micros,
                                budget = PANEL_BUDGET_MICROS,
                                frames = PANEL_BUDGET_FRAMES,
                                "[editor] module panel over budget"
                            );
                        }
                    }
                    None => {
                        ui.vertical_centered(|ui| {
                            ui.add_space(12.0);
                            ui.strong("Module not loaded");
                            ui.label(
                                self.panels
                                    .get(slot)
                                    .map(|entry| entry.source.clone())
                                    .unwrap_or_else(|| "unknown panel".to_string()),
                            );
                        });
                    }
                }
            }
        }
        self.stats.record(tab, started.elapsed().as_micros() as f64);
    }

    /// The dock tab's menu. The dock itself offers Eject (into a window)
    /// and Close; a floating window's tab gets Redock, which brings the
    /// surface's tabs back into the dock.
    fn context_menu(&mut self, ui: &mut egui::Ui, _tab: &mut Self::Tab, path: NodePath) {
        if !path.surface.is_main()
            && ui
                .add(icons::IconButton::new(
                    icons::Icon::ArrowSquareOut,
                    "Redock",
                ))
                .clicked()
        {
            self.tab_redock = Some(path.surface);
            ui.close();
        }
        // The dock owns this menu's shell, so its drawn content is what is
        // reported: an item click over the viewport must not pick either.
        let rect = ui.min_rect();
        self.overlay_regions.push(ShellRect {
            x: rect.min.x,
            y: rect.min.y,
            width: rect.width(),
            height: rect.height(),
        });
    }

    /// The tab button hook: a right-click that opens the tab menu is logged
    /// here, once per click - the menu content runs every frame it is open.
    fn on_tab_button(&mut self, _tab: &mut Self::Tab, response: &egui::Response) {
        if response.secondary_clicked() {
            log_context_menu("tab");
        }
    }

    /// The scene and game tabs keep their bodies transparent: the scene and
    /// game child windows show through them from under the overlay.
    fn clear_background(&self, tab: &Self::Tab) -> bool {
        !matches!(tab, EditorTab::Scene(_) | EditorTab::Game)
    }

    /// The scene and game tabs' bodies are viewports; they never scroll.
    fn scroll_bars(&self, tab: &Self::Tab) -> [bool; 2] {
        match tab {
            EditorTab::Scene(_) | EditorTab::Game => [false, false],
            _ => [true, true],
        }
    }
}
