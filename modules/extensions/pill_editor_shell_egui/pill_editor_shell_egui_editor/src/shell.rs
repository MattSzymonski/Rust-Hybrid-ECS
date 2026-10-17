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

// External crates
use pill_editor_core_egui::assets::{self, AssetDialog};
use pill_editor_core_egui::commands::PendingEditorCommands;
use pill_editor_core_egui::editor_state::{
    AssetListing, EditorCommand, EditorSelection, EditorSnapshot, EntityDetail,
};
use pill_editor_core_egui::entities;
use pill_editor_core_egui::gizmo::{GizmoMode, GizmoSpace};
use pill_editor_core_egui::inspector;
use pill_editor_core_egui::layout::EditorTab;
use pill_editor_core_egui::preferences;
use pill_editor_core_egui::session::{EguiShellState, ToolboxSession};
use pill_editor_core_egui::systems;
use pill_editor_core_egui::toolbox;
use pill_engine::{pill_hot_fn, Entity, FieldValue, Input};
use pill_ui_egui::boundary::{
    frame_from_output, EguiShellFrame, EguiShellInput, ShellRect, SCENE_VIEWPORTS,
};
use pill_ui_egui::egui;
use pill_ui_egui::egui_dock::{DockArea, DockState, NodeIndex, SurfaceIndex, TabViewer};
use pill_ui_egui::icons;
use pill_ui_egui::style;
use serde_json::Value as JsonValue;

// =============================================================================
// Constants
// =============================================================================

/// How far the scene rectangle extends past the measured tab body, in
/// logical points: the body's padding and outline sit inside the leaf, and
/// the child window must back them too, so no sliver of the game view
/// behind the overlay shows around the viewport's edge.
const SCENE_RECT_OVERSHOOT: f32 = 16.0;

/// The scene's frame counter: its box in logical points, and the gap to the
/// tab body's corner.
const SCENE_COUNTER_WIDTH: f32 = 76.0;
const SCENE_COUNTER_HEIGHT: f32 = 20.0;
const SCENE_COUNTER_MARGIN: f32 = 8.0;

/// The example button beside the counter: its width, and the gap between
/// the two.
const SCENE_BUTTON_WIDTH: f32 = 72.0;
const SCENE_BUTTON_GAP: f32 = 6.0;

/// One shell frame: engine input becomes egui input, the dock runs, and the
/// output becomes the executable's frame. The asset dialog, when one is
/// open, is drawn over the dock.
pub fn run_frame(
    state: &mut EguiShellState,
    shell_input: &EguiShellInput,
    frame: &mut EguiShellFrame,
    snapshot: &EditorSnapshot,
    selection: &mut EditorSelection,
    pending: &mut PendingEditorCommands,
    engine_input: &Input,
) {
    state.frame_index = state.frame_index.wrapping_add(1);
    // The row heartbeat: the editor suite reads it to prove both tabs see
    // the project's data, that the frame counter - and with it the session
    // resource - survives a reload or patch, and which entity the
    // Inspector's detail tier describes.
    if state.frame_index.is_multiple_of(210) {
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
            frames = state.frame_index,
            entities = snapshot.entities.len(),
            systems = snapshot.systems.len(),
            paused = snapshot.paused,
            detail = detail_entity.as_str(),
            detail_components,
            "[editor] egui shell rows"
        );
    }
    let raw_input = pill_ui_egui::input::raw_input(engine_input, shell_input);
    // The context is a cheap clone (refcounted handle); the dock and the
    // asset dialog are separate fields, borrowed apart so the dock's layout
    // borrow and the dialog's state borrow coexist.
    let context = state.context.clone();
    let EguiShellState {
        dock,
        asset_dialog,
        preferences_draft,
        toolbox,
        ..
    } = state;
    let mut viewer = EditorTabViewer {
        snapshot,
        selection,
        asset_dialog,
        pending,
        preferences_draft,
        toolbox,
        scene_rects: [None; SCENE_VIEWPORTS],
        game_rect: None,
        overlay_regions: Vec::new(),
        frames_per_second: shell_input.frames_per_second,
        tab_redock: None,
    };
    let output = context.run(raw_input, |context| {
        // Tab bars carry the tabs and their close buttons only: without the
        // collapse chevron and the panel-level close-all ✖, a panel closes
        // exactly when its last tab does.
        DockArea::new(dock.state_mut())
            .style(style::dock_style(context.style().as_ref()))
            .show_leaf_collapse_buttons(false)
            .show_leaf_close_all_buttons(false)
            .show(context, &mut viewer);
        // The create/rename dialog floats over the dock while it is open.
        asset_dialog_window(
            context,
            viewer.asset_dialog,
            viewer.pending,
            viewer.snapshot,
        );
    });
    let mut shell_frame = frame_from_output(&context, output);
    shell_frame.scene_rects = viewer.scene_rects;
    shell_frame.game_rect = viewer.game_rect;
    shell_frame.overlay_regions = std::mem::take(&mut viewer.overlay_regions);
    // A tab menu's re-dock runs after the dock finished drawing: changing
    // the tree mid-draw would edit it under the widget that asked.
    if let Some(surface) = viewer.tab_redock.take() {
        apply_tab_redock(dock.state_mut(), surface);
    }
    *frame = shell_frame;
}

// =============================================================================
// The tab viewer and the tabs
// =============================================================================

/// The icon a dock tab's title carries: the view or panel the tab shows.
fn tab_icon(tab: EditorTab) -> icons::Icon {
    match tab {
        EditorTab::Scene => icons::Icon::Cube,
        EditorTab::SecondScene => icons::Icon::CubeTransparent,
        EditorTab::Game => icons::Icon::MonitorPlay,
        EditorTab::Entities => icons::Icon::TreeStructure,
        EditorTab::Systems => icons::Icon::Gear,
        EditorTab::Inspector => icons::Icon::SlidersHorizontal,
        EditorTab::Assets => icons::Icon::FolderOpen,
        EditorTab::Preferences => icons::Icon::Faders,
    }
}

/// Draws the dock's tabs; the tab bodies come from the tab functions.
struct EditorTabViewer<'a> {
    /// The engine data the tabs read.
    snapshot: &'a EditorSnapshot,
    /// The shared selection the Entities tab writes.
    selection: &'a mut EditorSelection,
    /// The asset dialog that is open, if one is; the Assets tab and the
    /// dialog window both read and write it.
    asset_dialog: &'a mut Option<AssetDialog>,
    /// The queue the assets actions push their recorded batches into.
    pending: &'a mut PendingEditorCommands,
    /// The Preferences tab's unsaved field buffers; the tab seeds them
    /// from the snapshot and a save clears them.
    preferences_draft: &'a mut Option<preferences::PreferencesDraft>,
    /// The floating viewport toolboxes' session state: where each scene
    /// view's toolbox floats and what the log already reported.
    toolbox: &'a mut ToolboxSession,
    /// The scene viewports' body rectangles this pass, in logical points,
    /// one slot per scene tab; the frontend places each viewport's child
    /// window at its slot.
    scene_rects: [Option<ShellRect>; SCENE_VIEWPORTS],
    /// The game viewport's body rectangle this pass, in logical points; the
    /// frontend places the primary surface's child window at it.
    game_rect: Option<ShellRect>,
    /// The overlay chips' rectangles this pass, in logical points: points
    /// over a scene viewport where a click is the UI's, not the scene's.
    overlay_regions: Vec<ShellRect>,
    /// The editor's smoothed frame rate, for the scene's counter.
    frames_per_second: f32,
    /// The surface a tab menu asked to re-dock, drained by `run_frame`
    /// after the dock finishes drawing.
    tab_redock: Option<SurfaceIndex>,
}

impl TabViewer for EditorTabViewer<'_> {
    type Tab = EditorTab;

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        icons::rich(tab_icon(*tab), (*tab).title()).into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            EditorTab::Scene => {
                self.scene_rects[0] = Some(scene_body_rect(ui));
                self.scene_context_menu(ui);
                scene_overlay(ui, self.frames_per_second, &mut self.overlay_regions);
                self.toolbox_overlay(ui, 0);
            }
            EditorTab::SecondScene => {
                self.scene_rects[1] = Some(scene_body_rect(ui));
                self.scene_context_menu(ui);
                scene_overlay(ui, self.frames_per_second, &mut self.overlay_regions);
                self.toolbox_overlay(ui, 1);
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
            ),
            EditorTab::Assets => assets_tab(
                ui,
                self.snapshot,
                self.selection,
                self.asset_dialog,
                self.pending,
                &mut self.overlay_regions,
            ),
            EditorTab::Preferences => {
                preferences_tab(ui, self.snapshot, self.preferences_draft, self.pending)
            }
        }
    }

    /// The dock tab's menu. The dock itself offers Eject (into a window)
    /// and Close; a floating window's tab gets Redock, which brings the
    /// surface's tabs back into the dock.
    fn context_menu(
        &mut self,
        ui: &mut egui::Ui,
        _tab: &mut Self::Tab,
        surface: SurfaceIndex,
        _node: NodeIndex,
    ) {
        if !surface.is_main()
            && ui
                .add(icons::IconButton::new(
                    icons::Icon::ArrowSquareOut,
                    "Redock",
                ))
                .clicked()
        {
            self.tab_redock = Some(surface);
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
        !matches!(
            tab,
            EditorTab::Scene | EditorTab::SecondScene | EditorTab::Game
        )
    }

    /// The scene and game tabs' bodies are viewports; they never scroll.
    fn scroll_bars(&self, tab: &Self::Tab) -> [bool; 2] {
        match tab {
            EditorTab::Scene | EditorTab::SecondScene | EditorTab::Game => [false, false],
            _ => [true, true],
        }
    }
}

/// A scene tab's body rectangle in logical points, overshooting by
/// [`SCENE_RECT_OVERSHOOT`] so the child window backs the tab body's padding
/// and outline too.
fn scene_body_rect(ui: &egui::Ui) -> ShellRect {
    shell_rect(ui.max_rect().expand(SCENE_RECT_OVERSHOOT))
}

/// Convert an egui rectangle into the boundary's plain rectangle.
fn shell_rect(rect: egui::Rect) -> ShellRect {
    ShellRect {
        x: rect.min.x,
        y: rect.min.y,
        width: rect.width(),
        height: rect.height(),
    }
}

/// A scene tab's overlay strip, anchored to the viewport's top-right
/// corner: an example button and the frame counter, both drawn as the
/// theme's chips. The overlay paints above the scene's child window, so
/// both sit over the viewport, and both take input, so a click on either
/// never reaches the project. Both rectangles are recorded as the UI's
/// own, so a click on them is never read as a scene pick.
fn scene_overlay(ui: &mut egui::Ui, frames_per_second: f32, overlay_regions: &mut Vec<ShellRect>) {
    let counter_rect = frame_counter_rect(ui);
    let button_rect = egui::Rect::from_min_size(
        egui::pos2(
            counter_rect.left() - SCENE_BUTTON_WIDTH - SCENE_BUTTON_GAP,
            counter_rect.top(),
        ),
        egui::vec2(SCENE_BUTTON_WIDTH, SCENE_COUNTER_HEIGHT),
    );
    overlay_regions.push(shell_rect(counter_rect));
    overlay_regions.push(shell_rect(button_rect));
    let button = ui.put(
        button_rect,
        egui::Button::new("Example")
            .min_size(button_rect.size())
            .fill(style::CONTROL_FILL)
            .corner_radius(egui::CornerRadius::same(4)),
    );
    if button.clicked() {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            "[editor] example button clicked"
        );
    }
    // The counter is painted rather than laid out, so it costs the tab body
    // no layout space; its rectangle still senses clicks.
    let _ = ui.interact(
        counter_rect,
        ui.id().with("scene_frame_counter"),
        egui::Sense::click(),
    );
    ui.painter().rect(
        counter_rect,
        egui::CornerRadius::same(5),
        style::OVERLAY_FILL,
        egui::Stroke::new(1.0, style::OVERLAY_BORDER),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        counter_rect.center(),
        egui::Align2::CENTER_CENTER,
        format!("{frames_per_second:.0} fps"),
        egui::FontId::monospace(12.0),
        style::TEXT_STRONG,
    );
}

/// The frame counter's rectangle: the strip's right-anchored box.
fn frame_counter_rect(ui: &egui::Ui) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(
            ui.max_rect().right() - SCENE_COUNTER_WIDTH - SCENE_COUNTER_MARGIN,
            ui.max_rect().top() + SCENE_COUNTER_MARGIN,
        ),
        egui::vec2(SCENE_COUNTER_WIDTH, SCENE_COUNTER_HEIGHT),
    )
}

/// One row per live entity: the entity and its label, for the Entities tab.
pub fn entity_rows(snapshot: &EditorSnapshot) -> Vec<(Entity, String)> {
    snapshot
        .entities
        .iter()
        .map(|entry| {
            (
                entry.entity,
                format!("{}  ({} components)", entry.display, entry.component_count),
            )
        })
        .collect()
}

/// The Systems tab: the engine's pause and single-step controls in a header
/// row, then one row per registered system - its name and owner, and a
/// state button (`on`/`off`) that queues the recorded, undoable toggle the
/// native panel records.
#[pill_hot_fn]
pub fn systems_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    pending: &mut PendingEditorCommands,
) {
    ui.horizontal(|ui| {
        let pause_icon = if snapshot.paused {
            icons::Icon::Play
        } else {
            icons::Icon::Pause
        };
        if ui
            .add(icons::IconButton::new(
                pause_icon,
                systems::pause_button_label(snapshot.paused),
            ))
            .clicked()
        {
            pending.push(vec![systems::pause_toggle_command(snapshot.paused)]);
        }
        if ui
            .add(icons::IconButton::new(icons::Icon::SkipForward, "Step"))
            .clicked()
        {
            pending.push(vec![EditorCommand::StepOnce]);
        }
        let enabled = snapshot.systems.iter().filter(|row| row.enabled).count();
        ui.label(
            egui::RichText::new(format!("{}/{} enabled", enabled, snapshot.systems.len())).weak(),
        );
    });
    ui.separator();
    egui::ScrollArea::vertical().show(ui, |ui| {
        for system in &snapshot.systems {
            ui.horizontal(|ui| {
                ui.label(systems::system_row_label(system));
                if ui
                    .add(icons::IconButton::new(
                        icons::Icon::Power,
                        systems::system_state_label(system.enabled),
                    ))
                    .clicked()
                {
                    let (command, label) = systems::system_toggle(system.index, !system.enabled);
                    pending.push_recorded(vec![command], label, None);
                }
            });
        }
    });
}

/// The Preferences tab (Stage 20): the editor-wide rows seeded from the
/// preferences file and saved as one read-modify-write batch, then the
/// project's remembered gizmo choices with the tool buttons that change
/// them - the same queued commands the shortcuts, the menus and the
/// scripted channel land in.
#[pill_hot_fn]
pub fn preferences_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    draft: &mut Option<preferences::PreferencesDraft>,
    pending: &mut PendingEditorCommands,
) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        let view = &snapshot.preferences;
        // The buffers seed from the file once and stay until a save;
        // `None` means "re-seed from the file" and covers the first frame
        // and the frame after a save.
        let buffers = draft.get_or_insert_with(|| view.seed_buffers());
        ui.heading("Editor");
        ui.label(
            egui::RichText::new("Preferences file: blank keeps the default, Save writes the rows.")
                .weak(),
        );
        for field in preferences::PREFERENCE_FIELDS {
            let buffer = buffers.entry(field.key.to_string()).or_default();
            ui.horizontal(|ui| {
                ui.label(field.label);
                ui.add(egui::TextEdit::singleline(buffer).desired_width(150.0));
            });
        }
        let mut save = false;
        ui.horizontal(|ui| {
            save = ui
                .add(icons::IconButton::new(
                    icons::Icon::FloppyDisk,
                    "Save preferences",
                ))
                .clicked();
        });
        if save {
            let values = preferences::PREFERENCE_FIELDS
                .iter()
                .map(|field| {
                    (
                        field.key.to_string(),
                        buffers.get(field.key).cloned().unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>();
            pending.push(vec![EditorCommand::SetPreferences { values }]);
            // Re-seed from the file next frame, so the rows show what the
            // save actually wrote.
            *draft = None;
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                "[editor] preferences save queued"
            );
        }
        ui.separator();
        ui.heading("Project");
        ui.label(format!(
            "Project: {}",
            view.project.as_deref().unwrap_or("(unknown)")
        ));
        if view.project_remembered {
            ui.label(format!(
                "Remembered tool: mode={} space={} snap={}",
                view.project_gizmo.mode.map_or("(unset)", gizmo_mode_label),
                view.project_gizmo
                    .space
                    .map_or("(unset)", gizmo_space_label),
                view.project_gizmo
                    .snap
                    .map_or("(unset)".to_string(), |snap| snap.to_string()),
            ));
        } else {
            ui.label(
                egui::RichText::new(
                    "Remembered tool: none yet - a tool choice is remembered per project.",
                )
                .weak(),
            );
        }
        ui.label(format!(
            "Active: mode={} space={} snap={}",
            gizmo_mode_label(snapshot.gizmo.mode),
            gizmo_space_label(snapshot.gizmo.space),
            snapshot.gizmo.snap,
        ));
        ui.horizontal(|ui| {
            for (label, mode, icon) in [
                (
                    "Translate",
                    GizmoMode::Translate,
                    icons::Icon::ArrowsOutCardinal,
                ),
                ("Rotate", GizmoMode::Rotate, icons::Icon::ArrowsClockwise),
                ("Scale", GizmoMode::Scale, icons::Icon::ArrowsOut),
            ] {
                let button =
                    icons::IconButton::new(icon, label).active(snapshot.gizmo.mode == mode);
                if ui.add(button).clicked() {
                    pending.push(vec![EditorCommand::SetGizmoTool {
                        mode: Some(mode),
                        space: None,
                        snap: None,
                    }]);
                }
            }
            for (label, space, icon) in [
                ("World", GizmoSpace::World, icons::Icon::GlobeHemisphereWest),
                ("Local", GizmoSpace::Local, icons::Icon::MapPin),
            ] {
                let button =
                    icons::IconButton::new(icon, label).active(snapshot.gizmo.space == space);
                if ui.add(button).clicked() {
                    pending.push(vec![EditorCommand::SetGizmoTool {
                        mode: None,
                        space: Some(space),
                        snap: None,
                    }]);
                }
            }
            let snap =
                icons::IconButton::new(icons::Icon::Magnet, "Snap").active(snapshot.gizmo.snap);
            if ui.add(snap).clicked() {
                pending.push(vec![EditorCommand::SetGizmoTool {
                    mode: None,
                    space: None,
                    snap: Some(!snapshot.gizmo.snap),
                }]);
            }
        });
    });
}

/// One gizmo mode's lowercase name, for the preferences rows.
fn gizmo_mode_label(mode: GizmoMode) -> &'static str {
    match mode {
        GizmoMode::Translate => "translate",
        GizmoMode::Rotate => "rotate",
        GizmoMode::Scale => "scale",
    }
}

/// One gizmo space's lowercase name, for the preferences rows.
fn gizmo_space_label(space: GizmoSpace) -> &'static str {
    match space {
        GizmoSpace::World => "world",
        GizmoSpace::Local => "local",
    }
}

/// The Entities tab: the world's entities; a click selects one, a right
/// click selects it and opens its menu - the recorded, undoable create and
/// delete the native Hierarchy carries as buttons.
#[pill_hot_fn]
pub fn entities_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    selection: &mut EditorSelection,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        for (entity, label) in entity_rows(snapshot) {
            let selected = selection.entity == Some(entity);
            let response = ui.selectable_label(selected, label);
            if response.clicked() {
                select_entity(selection, entity);
            }
            if response.secondary_clicked() {
                select_entity(selection, entity);
                log_context_menu("entities");
            }
            let opened =
                response.context_menu(|ui| entity_menu_items(ui, Some(entity), selection, pending));
            mark_menu_region(opened, overlay_regions);
        }
        // The space below the rows creates one entity; there is nothing to
        // delete away from a row.
        let empty = ui.allocate_response(empty_area(ui), egui::Sense::click());
        if empty.secondary_clicked() {
            log_context_menu("entities");
        }
        let opened = empty.context_menu(|ui| entity_menu_items(ui, None, selection, pending));
        mark_menu_region(opened, overlay_regions);
    });
}

/// The entity menu: New Entity everywhere, Delete Entity for the row under
/// the pointer. Both edits are recorded, so one undo step reverses them.
fn entity_menu_items(
    ui: &mut egui::Ui,
    entity: Option<Entity>,
    selection: &EditorSelection,
    pending: &mut PendingEditorCommands,
) {
    if ui
        .add(icons::IconButton::new(icons::Icon::Plus, "New Entity"))
        .clicked()
    {
        let (command, label) = entities::entity_create();
        pending.push_recorded(vec![command], label, None);
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            "[editor] new entity queued"
        );
        ui.close();
    }
    if let Some(entity) = entity {
        if ui
            .add(icons::IconButton::new(icons::Icon::Trash, "Delete Entity"))
            .clicked()
        {
            let (commands, label) = entities::entity_delete(entity, selection.asset.clone());
            pending.push_recorded(commands, label, None);
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                entity = %entity,
                "[editor] delete entity queued"
            );
            ui.close();
        }
    }
}

/// The trailing right-click area below a panel's rows: a strip of the
/// panel's width where the panel's own menu opens, away from any row.
fn empty_area(ui: &egui::Ui) -> egui::Vec2 {
    egui::Vec2::new(ui.available_width(), 32.0)
}

/// Log a context menu opening, so a suite can see which area's menu the
/// right-click opened.
fn log_context_menu(area: &str) {
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        area,
        "[editor] context menu"
    );
}

/// Add an open context menu's rectangle to the frame's overlay regions: a
/// menu floats over whatever it was opened on (a clamped menu can hang over
/// the scene viewport), and a click inside it must never reach the scene
/// behind it.
fn mark_menu_region(opened: Option<egui::InnerResponse<()>>, regions: &mut Vec<ShellRect>) {
    if let Some(menu) = opened {
        let rect = menu.response.rect;
        regions.push(ShellRect {
            x: rect.min.x,
            y: rect.min.y,
            width: rect.width(),
            height: rect.height(),
        });
    }
}

/// Bring a floating surface's tabs back into the dock: the tab menu's
/// Redock, run after the dock has drawn the frame that asked.
fn apply_tab_redock(dock: &mut DockState<EditorTab>, surface: SurfaceIndex) {
    let tabs: Vec<EditorTab> = dock
        .iter_all_tabs()
        .filter(|((index, _), _)| *index == surface)
        .map(|(_, tab)| *tab)
        .collect();
    if tabs.is_empty() {
        return;
    }
    dock.remove_surface(surface);
    for tab in &tabs {
        dock.push_to_first_leaf(*tab);
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        count = tabs.len(),
        "[editor] tabs redocked"
    );
}

/// The component menu: detach the heading's component, and attach any
/// registered one the entity does not carry yet - both recorded under the
/// native labels (`add {short}` / `remove {short}`).
fn component_menu_items(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    detail: &EntityDetail,
    current: Option<&str>,
    pending: &mut PendingEditorCommands,
) {
    if let Some(component) = current {
        let label = format!("Remove {}", inspector::short_component_name(component));
        if ui
            .add(icons::IconButton::new(icons::Icon::MinusCircle, &label))
            .clicked()
        {
            let (command, label) = inspector::component_remove(detail.entity, component);
            pending.push_recorded(vec![command], label, None);
            ui.close();
        }
        ui.separator();
    }
    ui.menu_button(
        icons::rich(icons::Icon::PlusCircle, "Add Component"),
        |ui| {
            for registered in snapshot
                .components
                .iter()
                .filter(|registered| registered.addable)
            {
                let attached = detail
                    .components
                    .iter()
                    .any(|component| component.type_name == registered.type_name);
                if attached {
                    continue;
                }
                let short = inspector::short_component_name(&registered.type_name);
                if ui
                    .add(icons::IconButton::new(icons::Icon::PlusCircle, short))
                    .clicked()
                {
                    let (command, label) =
                        inspector::component_add(detail.entity, &registered.type_name);
                    pending.push_recorded(vec![command], label, None);
                    ui.close();
                }
            }
        },
    );
}

impl EditorTabViewer<'_> {
    /// The scene viewport's menu: the transform tools, snapping and the
    /// space, checked against the snapshot's gizmo settings. The items queue
    /// the same `SetGizmoTool` edit the tools' shortcuts land in.
    fn scene_context_menu(&mut self, ui: &mut egui::Ui) {
        let settings = self.snapshot.gizmo;
        let grid_enabled = self.snapshot.grid_enabled;
        let response = ui.allocate_response(ui.available_size_before_wrap(), egui::Sense::click());
        if response.secondary_clicked() {
            log_context_menu("scene");
        }
        let opened = response.context_menu(|ui| {
            let tools = [
                (
                    "Translate",
                    GizmoMode::Translate,
                    icons::Icon::ArrowsOutCardinal,
                ),
                ("Rotate", GizmoMode::Rotate, icons::Icon::ArrowsClockwise),
                ("Scale", GizmoMode::Scale, icons::Icon::ArrowsOut),
            ];
            for (label, mode, icon) in tools {
                let button = icons::IconButton::new(icon, label).active(settings.mode == mode);
                if ui.add(button).clicked() {
                    self.pending.push(vec![EditorCommand::SetGizmoTool {
                        mode: Some(mode),
                        space: None,
                        snap: None,
                    }]);
                    ui.close();
                }
            }
            ui.separator();
            let snapping =
                icons::IconButton::new(icons::Icon::Magnet, "Snapping").active(settings.snap);
            if ui.add(snapping).clicked() {
                self.pending.push(vec![EditorCommand::SetGizmoTool {
                    mode: None,
                    space: None,
                    snap: Some(!settings.snap),
                }]);
                ui.close();
            }
            let spaces = [
                (
                    "World space",
                    GizmoSpace::World,
                    icons::Icon::GlobeHemisphereWest,
                ),
                ("Local space", GizmoSpace::Local, icons::Icon::MapPin),
            ];
            for (label, space, icon) in spaces {
                let button = icons::IconButton::new(icon, label).active(settings.space == space);
                if ui.add(button).clicked() {
                    self.pending.push(vec![EditorCommand::SetGizmoTool {
                        mode: None,
                        space: Some(space),
                        snap: None,
                    }]);
                    ui.close();
                }
            }
            ui.separator();
            let grid = icons::IconButton::new(icons::Icon::GridFour, "Grid").active(grid_enabled);
            if ui.add(grid).clicked() {
                self.pending.push(vec![EditorCommand::SetGrid {
                    enabled: !grid_enabled,
                }]);
                ui.close();
            }
        });
        // While the menu is open its rectangle joins the overlay regions:
        // a click on an item is the UI's, never a scene pick, even though
        // the menu floats over the viewport.
        mark_menu_region(opened, &mut self.overlay_regions);
    }

    /// The floating toolbox over one scene viewport (Stage 22): the gizmo's
    /// tools, its space and its snapping as one panel the user drags
    /// anywhere inside the view - the always-visible twin of the viewport
    /// menu's items.
    ///
    /// The panel floats over the scene's child window (the composition
    /// overlay paints above it), and its rectangle joins the frame's
    /// overlay regions, so a click anywhere on it - a row or the gap
    /// between rows - is the UI's and never becomes a scene pick. The drag
    /// starts on the handle and its result is clamped to the viewport, so
    /// the panel can be parked anywhere inside the view but not lost
    /// outside it. Both rectangles, the panel's and every row's, are
    /// logged once per placement and again after each drag: the suite's
    /// scripted clicks read their targets straight from the log.
    fn toolbox_overlay(&mut self, ui: &mut egui::Ui, view: usize) {
        let viewport = ui.max_rect();
        let state = toolbox::ToolboxState {
            gizmo: self.snapshot.gizmo,
            grid: self.snapshot.grid_enabled,
        };
        let mut origin = toolbox::clamped_origin(
            self.toolbox.origins[view].unwrap_or_else(|| toolbox::default_origin(viewport)),
            viewport,
        );
        // The handle is the one part of the panel a press drags; a click on
        // a row below it still reaches the row's button.
        let handle = toolbox::handle_rect(origin);
        let response = ui.interact(
            handle,
            ui.id().with(("scene-toolbox", view)),
            egui::Sense::drag(),
        );
        if response.dragged() {
            origin = toolbox::clamped_origin(origin + response.drag_delta(), viewport);
            self.toolbox.origins[view] = Some(origin);
        }
        if response.drag_stopped() {
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                view,
                x = origin.x,
                y = origin.y,
                "[editor] toolbox moved"
            );
        }
        let panel = toolbox::panel_rect(origin);
        ui.painter().rect(
            panel,
            egui::CornerRadius::same(5),
            style::OVERLAY_FILL,
            egui::Stroke::new(1.0, style::OVERLAY_BORDER),
            egui::StrokeKind::Inside,
        );
        ui.painter().text(
            egui::pos2(panel.center().x, handle.center().y),
            egui::Align2::CENTER_CENTER,
            "Tools",
            egui::FontId::proportional(11.0),
            style::TEXT_MUTED,
        );
        for item in toolbox::TOOLBOX_ITEMS {
            let rect = toolbox::item_rect(item, origin);
            let fill = if item.active(&state) {
                style::SELECTION_FILL
            } else {
                style::CONTROL_FILL
            };
            let button = ui.put(
                rect,
                icons::IconButton::new(item.icon(&state), item.label(&state))
                    .active(item.active(&state))
                    .fill(fill)
                    .fixed_size(rect.size()),
            );
            if button.clicked() {
                self.pending.push(vec![item.command(&state)]);
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    view,
                    item = item.name(),
                    "[editor] toolbox click"
                );
            }
        }
        self.overlay_regions.push(shell_rect(panel));
        if self.toolbox.logged[view] != Some(panel) {
            self.toolbox.logged[view] = Some(panel);
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                view,
                x = panel.min.x,
                y = panel.min.y,
                width = panel.width(),
                height = panel.height(),
                "[editor] toolbox rect"
            );
            for (item, rect) in std::iter::once(("handle", handle)).chain(
                toolbox::TOOLBOX_ITEMS
                    .iter()
                    .map(|item| (item.name(), toolbox::item_rect(*item, origin))),
            ) {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    view,
                    item,
                    x = rect.min.x,
                    y = rect.min.y,
                    width = rect.width(),
                    height = rect.height(),
                    "[editor] toolbox item"
                );
            }
        }
    }
}

/// Move the shared selection onto `entity`, logging the same token the
/// scene's picks log when it actually changed - so a suite can watch either
/// path move the selection. Returns whether the selection moved.
pub fn select_entity(selection: &mut EditorSelection, entity: Entity) -> bool {
    if selection.entity == Some(entity) {
        return false;
    }
    selection.entity = Some(entity);
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        entity = %entity,
        "[editor] selection changed"
    );
    true
}

/// The Inspector tab: the selected entity's components with interactive
/// field widgets (checkboxes, number drags, text fields; an array field
/// gets one control per element), and the selected asset's import settings
/// as editable leaves. Every commit is a recorded `SetField` /
/// `SetArrayElement` / `SetAssetSettings` edit, coalesced per widget so a
/// drag or a typing run is one undo step - the add/remove component actions
/// stay with the W-0027 port.
#[pill_hot_fn]
pub fn inspector_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    selection: &EditorSelection,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        match snapshot.detail.as_ref() {
            Some(detail) => {
                ui.label(egui::RichText::new(format!("entity {}", detail.entity)).strong());
                ui.separator();
                entity_inspector(ui, snapshot, detail, pending, overlay_regions);
            }
            None => match selection.entity {
                Some(entity) => {
                    ui.label(format!("entity {entity} is not alive"));
                }
                None => {
                    ui.label("Select an entity in the Entities tab.");
                }
            },
        }
        // The selected asset's section follows the entity's: its kind and
        // its import settings, editable leaf by leaf.
        let selected_asset = selection.asset.as_deref();
        if let Some(detail) = snapshot
            .asset_detail
            .as_ref()
            .filter(|detail| selected_asset == Some(detail.path.as_str()))
        {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(format!("asset {}", detail.path)).strong());
            read_only_row(ui, "kind", &detail.kind);
            if detail.standalone {
                read_only_row(ui, "standalone", "yes");
            }
            ui.separator();
            match detail.settings.as_ref() {
                Some(settings) => asset_settings_rows(ui, &detail.path, settings, pending),
                None => {
                    ui.label(egui::RichText::new("no editable settings").weak().italics());
                }
            }
        }
        // The strip below the rows carries the component menu where no
        // heading is there to hang it on (a right-click there adds).
        if let Some(detail) = snapshot.detail.as_ref() {
            let empty = ui.allocate_response(empty_area(ui), egui::Sense::click());
            if empty.secondary_clicked() {
                log_context_menu("inspector");
            }
            let opened =
                empty.context_menu(|ui| component_menu_items(ui, snapshot, detail, None, pending));
            mark_menu_region(opened, overlay_regions);
        }
    });
}

/// One entity's components; every editable field is a widget row and every
/// component heading carries its menu (attach and detach - the native
/// Inspector's two component actions, recorded and undoable).
fn entity_inspector(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    detail: &EntityDetail,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
) {
    for component in &detail.components {
        let heading = ui.add(
            egui::Label::new(egui::RichText::new(component.type_name.as_str()).strong())
                .sense(egui::Sense::click()),
        );
        if heading.secondary_clicked() {
            log_context_menu("inspector");
        }
        let opened = heading.context_menu(|ui| {
            component_menu_items(
                ui,
                snapshot,
                detail,
                Some(component.type_name.as_str()),
                pending,
            )
        });
        mark_menu_region(opened, overlay_regions);
        if !component.editable {
            ui.label(egui::RichText::new("no editable fields").weak().italics());
            ui.separator();
            continue;
        }
        for (descriptor, value) in component.fields.iter().zip(component.values.iter()) {
            entity_field_row(
                ui,
                detail.entity,
                &component.type_name,
                descriptor.name,
                value,
                pending,
            );
        }
        ui.separator();
    }
}

/// One field row: the widget its value's kind asks for, and the recorded
/// commit its change queues. A field that changed while the snapshot was
/// stale is not a problem: each commit carries the widget's value, and a
/// change the next snapshot overwrites was one the user just made anyway.
fn entity_field_row(
    ui: &mut egui::Ui,
    entity: Entity,
    component: &str,
    field: &str,
    value: &FieldValue,
    pending: &mut PendingEditorCommands,
) {
    match value {
        FieldValue::Text(current) => {
            let mut edited = current.clone();
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(field).color(style::TEXT_MUTED));
                let response = ui.add(egui::TextEdit::singleline(&mut edited).desired_width(160.0));
                if response.lost_focus() && edited != *current {
                    let (command, label) =
                        inspector::field_edit(entity, component, field, FieldValue::Text(edited));
                    pending.push_recorded(
                        vec![command],
                        label,
                        Some(inspector::field_coalesce_key(entity, component, field)),
                    );
                }
            });
        }
        FieldValue::Array(elements) if elements.iter().all(is_scalar_field) => {
            for (index, element) in elements.iter().enumerate() {
                let label = format!("{field}[{index}]");
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(&label).color(style::TEXT_MUTED));
                    if let Some(edited) = scalar_edit(ui, element) {
                        let (command, commit_label) =
                            inspector::element_edit(entity, component, field, index, edited);
                        pending.push_recorded(
                            vec![command],
                            commit_label,
                            Some(inspector::field_coalesce_key(entity, component, &label)),
                        );
                    }
                });
            }
        }
        other => {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(field).color(style::TEXT_MUTED));
                match scalar_edit(ui, other) {
                    Some(edited) => {
                        let (command, label) =
                            inspector::field_edit(entity, component, field, edited);
                        pending.push_recorded(
                            vec![command],
                            label,
                            Some(inspector::field_coalesce_key(entity, component, field)),
                        );
                    }
                    None => {
                        ui.label(format_field_value(other));
                    }
                }
            });
        }
    }
}

/// Whether a value has its own widget kind.
fn is_scalar_field(value: &FieldValue) -> bool {
    matches!(
        value,
        FieldValue::F32(_)
            | FieldValue::F64(_)
            | FieldValue::I8(_)
            | FieldValue::I16(_)
            | FieldValue::I32(_)
            | FieldValue::I64(_)
            | FieldValue::U8(_)
            | FieldValue::U16(_)
            | FieldValue::U32(_)
            | FieldValue::U64(_)
            | FieldValue::Usize(_)
            | FieldValue::Bool(_)
    )
}

/// The edit control for one scalar: a checkbox for booleans, a drag value
/// (drag to scrub, click to type) for numbers. `None` when the kind has no
/// widget, so the caller paints it read-only.
fn scalar_edit(ui: &mut egui::Ui, value: &FieldValue) -> Option<FieldValue> {
    match value {
        FieldValue::Bool(current) => {
            let mut edited = *current;
            ui.checkbox(&mut edited, "")
                .changed()
                .then_some(FieldValue::Bool(edited))
        }
        FieldValue::F32(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited).speed(0.05))
                .changed()
                .then_some(FieldValue::F32(edited))
        }
        FieldValue::F64(current) => {
            let mut edited = *current;
            ui.add(
                egui::DragValue::new(&mut edited)
                    .speed(0.05)
                    .max_decimals(9),
            )
            .changed()
            .then_some(FieldValue::F64(edited))
        }
        FieldValue::I8(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::I8(edited))
        }
        FieldValue::I16(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::I16(edited))
        }
        FieldValue::I32(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::I32(edited))
        }
        FieldValue::I64(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::I64(edited))
        }
        FieldValue::U8(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::U8(edited))
        }
        FieldValue::U16(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::U16(edited))
        }
        FieldValue::U32(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::U32(edited))
        }
        FieldValue::U64(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::U64(edited))
        }
        FieldValue::Usize(current) => {
            let mut edited = *current;
            ui.add(egui::DragValue::new(&mut edited))
                .changed()
                .then_some(FieldValue::Usize(edited))
        }
        _ => None,
    }
}

/// One read-only (name, value) row.
fn read_only_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(label).color(style::TEXT_MUTED));
        ui.label(value);
    });
}

/// The settings document's rows: objects and arrays nest their children,
/// every scalar leaf edits.
fn asset_settings_rows(
    ui: &mut egui::Ui,
    asset_path: &str,
    settings: &JsonValue,
    pending: &mut PendingEditorCommands,
) {
    match settings {
        JsonValue::Object(entries) => {
            for (key, value) in entries {
                asset_settings_row(ui, asset_path, settings, value, key.clone(), pending);
            }
        }
        other => {
            read_only_row(ui, "settings", &other.to_string());
        }
    }
}

/// One settings row: a leaf gets the widget its JSON kind asks for (its
/// commit carries the whole patched document), a container nests.
fn asset_settings_row(
    ui: &mut egui::Ui,
    asset_path: &str,
    root: &JsonValue,
    value: &JsonValue,
    key: String,
    pending: &mut PendingEditorCommands,
) {
    match value {
        JsonValue::Object(entries) => {
            ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
            ui.indent(key.as_str(), |ui| {
                for (child_key, child) in entries {
                    asset_settings_row(
                        ui,
                        asset_path,
                        root,
                        child,
                        format!("{key}.{child_key}"),
                        pending,
                    );
                }
            });
        }
        JsonValue::Array(elements) => {
            ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
            ui.indent(key.as_str(), |ui| {
                for (index, child) in elements.iter().enumerate() {
                    asset_settings_row(
                        ui,
                        asset_path,
                        root,
                        child,
                        format!("{key}[{index}]"),
                        pending,
                    );
                }
            });
        }
        JsonValue::Bool(current) => {
            let mut edited = *current;
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
                if ui.checkbox(&mut edited, "").changed() {
                    settings_commit(pending, asset_path, root, &key, JsonValue::Bool(edited));
                }
            });
        }
        JsonValue::Number(current) => {
            let mut integer_edit = current
                .as_i64()
                .or_else(|| current.as_u64().map(|value| value as i64));
            match integer_edit.as_mut() {
                Some(edited) => {
                    let mut changed = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
                        changed = ui.add(egui::DragValue::new(edited)).changed();
                    });
                    if changed {
                        settings_commit(
                            pending,
                            asset_path,
                            root,
                            &key,
                            JsonValue::Number((*edited).into()),
                        );
                    }
                }
                None => {
                    let mut edited = current.as_f64().unwrap_or_default();
                    let mut changed = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
                        changed = ui
                            .add(
                                egui::DragValue::new(&mut edited)
                                    .speed(0.05)
                                    .max_decimals(9),
                            )
                            .changed();
                    });
                    if changed {
                        if let Some(number) = serde_json::Number::from_f64(edited) {
                            settings_commit(
                                pending,
                                asset_path,
                                root,
                                &key,
                                JsonValue::Number(number),
                            );
                        }
                    }
                }
            }
        }
        JsonValue::String(current) => {
            let mut edited = current.clone();
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(key.as_str()).color(style::TEXT_MUTED));
                let response = ui.add(egui::TextEdit::singleline(&mut edited).desired_width(160.0));
                if response.lost_focus() && edited != *current {
                    settings_commit(pending, asset_path, root, &key, JsonValue::String(edited));
                }
            });
        }
        other => {
            read_only_row(ui, key.as_str(), &other.to_string());
        }
    }
}

/// Queue the recorded edit that puts `value` at `key` in the settings; the
/// command carries the whole patched document, the native contract.
fn settings_commit(
    pending: &mut PendingEditorCommands,
    asset_path: &str,
    root: &JsonValue,
    key: &str,
    value: JsonValue,
) {
    let Ok(path) = inspector::parse_settings_key_path(key) else {
        return;
    };
    let mut document = root.clone();
    if let Err(message) = inspector::patch_settings(&mut document, &path, value) {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ECS,
            key,
            message = message.as_str(),
            "[editor] asset settings edit refused"
        );
        return;
    }
    let (command, label) = inspector::settings_edit(asset_path, document, key);
    pending.push_recorded(
        vec![command],
        label,
        Some(inspector::settings_coalesce_key(asset_path, key)),
    );
}

/// The Inspector's rows: one (type name, field rows) pair per component of
/// the detail, each field row a (name, formatted value) pair.
pub fn component_rows(detail: &EntityDetail) -> Vec<(String, Vec<(String, String)>)> {
    detail
        .components
        .iter()
        .map(|component| {
            let fields = component
                .fields
                .iter()
                .zip(&component.values)
                .map(|(descriptor, value)| (descriptor.name.to_string(), format_field_value(value)))
                .collect();
            (component.type_name.clone(), fields)
        })
        .collect()
}

/// A field value as display text; the kinds without their own formatting
/// fall back to their debug form.
pub fn format_field_value(value: &FieldValue) -> String {
    match value {
        FieldValue::F32(number) => format!("{number}"),
        FieldValue::F64(number) => format!("{number}"),
        FieldValue::I8(number) => format!("{number}"),
        FieldValue::I16(number) => format!("{number}"),
        FieldValue::I32(number) => format!("{number}"),
        FieldValue::I64(number) => format!("{number}"),
        FieldValue::U8(number) => format!("{number}"),
        FieldValue::U16(number) => format!("{number}"),
        FieldValue::U32(number) => format!("{number}"),
        FieldValue::U64(number) => format!("{number}"),
        FieldValue::Usize(number) => format!("{number}"),
        FieldValue::Bool(flag) => format!("{flag}"),
        FieldValue::Text(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

/// The Assets tab: the project's entries, the toolbar that edits them, and
/// the rows a click selects - the browser half of the native Assets panel.
/// A row dragged onto a folder moves the file there; the create and rename
/// dialogs open as windows over the dock.
#[pill_hot_fn]
pub fn assets_tab(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    selection: &mut EditorSelection,
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
    overlay_regions: &mut Vec<ShellRect>,
) {
    ui.heading(icons::rich(icons::Icon::FolderOpen, "Assets"));
    let selected_row = assets::selected_row(selection, &snapshot.assets);
    let selected_is_file = selected_row.is_some_and(|row| row.kind != "folder");
    let missing_meta =
        selected_row.is_some_and(|row| row.kind != "folder" && !row.standalone && !row.has_meta);
    ui.label(format!("{} entries", snapshot.assets.len()));
    // The toolbar: creating opens the dialog; the rest act on the selected
    // entry and queue recorded file commands. The buttons only appear when
    // they can act, like the native panel's.
    ui.horizontal_wrapped(|ui| {
        if ui
            .add(icons::IconButton::new(icons::Icon::Plus, "Create Asset..."))
            .clicked()
        {
            assets::open_create(
                dialog,
                selection,
                &snapshot.assets,
                &snapshot.standalone_types,
            );
        }
        if selected_is_file {
            if ui
                .add(icons::IconButton::new(icons::Icon::Trash, "Delete Asset"))
                .clicked()
            {
                assets::delete_selection(pending, selection, &snapshot.assets);
            }
            if ui
                .add(icons::IconButton::new(
                    icons::Icon::PencilSimple,
                    "Rename Asset...",
                ))
                .clicked()
            {
                assets::open_rename(dialog, selection, &snapshot.assets);
            }
        }
        if missing_meta
            && ui
                .add(icons::IconButton::new(
                    icons::Icon::FileArrowDown,
                    "Write Asset Meta",
                ))
                .clicked()
        {
            assets::write_meta_selection(pending, selection, &snapshot.assets);
        }
    });
    ui.separator();
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for row in &snapshot.assets {
                let is_folder = row.kind == "folder";
                let text = if is_folder {
                    format!("{}/", row.path)
                } else {
                    format!("{} [{}]", row.path, row.kind)
                };
                let selected = selection.asset.as_deref() == Some(row.path.as_str());
                let response = ui.selectable_label(selected, text);
                if response.clicked() {
                    select_asset(selection, &row.path);
                }
                if response.secondary_clicked() {
                    select_asset(selection, &row.path);
                    log_context_menu("assets");
                }
                let opened = response.context_menu(|ui| {
                    asset_menu_items(ui, snapshot, row, selection, dialog, pending)
                });
                mark_menu_region(opened, overlay_regions);
                // Rows are drag sources, folders are drop targets: dropping a
                // row on a folder queues the recorded move (the sidecar
                // travels along and the selection follows the move).
                if is_folder {
                    if let Some(dragged) = response.dnd_release_payload::<String>() {
                        if let Some(to) = assets::drop_destination(&dragged, &row.path) {
                            assets::queue_move(pending, &dragged, &to);
                        }
                    }
                } else {
                    response.dnd_set_drag_payload(row.path.clone());
                }
            }
            // The strip below the rows creates an asset anywhere in the
            // panel, so the menu is reachable without a row under the
            // pointer.
            let empty = ui.allocate_response(empty_area(ui), egui::Sense::click());
            if empty.secondary_clicked() {
                log_context_menu("assets");
            }
            let opened = empty.context_menu(|ui| {
                if ui
                    .add(icons::IconButton::new(icons::Icon::Plus, "Create Asset..."))
                    .clicked()
                {
                    assets::open_create(
                        dialog,
                        selection,
                        &snapshot.assets,
                        &snapshot.standalone_types,
                    );
                    ui.close();
                }
            });
            mark_menu_region(opened, overlay_regions);
        });
}

/// The asset row's menu: the toolbar's actions on the row the right-click
/// selected. Create always; rename, delete and the write-meta action only
/// when they can act on that row, like the toolbar's buttons.
fn asset_menu_items(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    row: &AssetListing,
    selection: &EditorSelection,
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
) {
    if ui
        .add(icons::IconButton::new(icons::Icon::Plus, "Create Asset..."))
        .clicked()
    {
        assets::open_create(
            dialog,
            selection,
            &snapshot.assets,
            &snapshot.standalone_types,
        );
        ui.close();
    }
    if row.kind == "folder" {
        return;
    }
    if ui
        .add(icons::IconButton::new(
            icons::Icon::PencilSimple,
            "Rename Asset...",
        ))
        .clicked()
    {
        assets::open_rename(dialog, selection, &snapshot.assets);
        ui.close();
    }
    if ui
        .add(icons::IconButton::new(icons::Icon::Trash, "Delete Asset"))
        .clicked()
    {
        assets::delete_selection(pending, selection, &snapshot.assets);
        ui.close();
    }
    if !row.standalone
        && !row.has_meta
        && ui
            .add(icons::IconButton::new(
                icons::Icon::FileArrowDown,
                "Write Asset Meta",
            ))
            .clicked()
    {
        assets::write_meta_selection(pending, selection, &snapshot.assets);
        ui.close();
    }
}

/// Move the shared selection onto the asset at `path`, logging the token a
/// suite watches. Returns whether the selection moved.
pub fn select_asset(selection: &mut EditorSelection, path: &str) -> bool {
    if selection.asset.as_deref() == Some(path) {
        return false;
    }
    selection.asset = Some(path.to_string());
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        path,
        "[editor] asset selected"
    );
    true
}

/// The create or rename dialog, drawn over the dock while one is open: the
/// create dialog lists the live standalone types and a name field, the
/// rename dialog the current path and the new name; their buttons run the
/// shared submit helpers, which queue the recorded batch and close the
/// dialog.
fn asset_dialog_window(
    context: &egui::Context,
    dialog: &mut Option<AssetDialog>,
    pending: &mut PendingEditorCommands,
    snapshot: &EditorSnapshot,
) {
    let Some(current) = dialog.as_ref() else {
        return;
    };
    let (title, submit_label) = match current {
        AssetDialog::Create { .. } => ("Create Asset", "Create"),
        AssetDialog::Rename { .. } => ("Rename Asset", "Rename"),
    };
    let mut submit = false;
    let mut cancel = false;
    egui::Window::new(title)
        .id(egui::Id::new("pill_editor_shell_egui.asset_dialog"))
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .collapsible(false)
        .resizable(false)
        .show(context, |ui| {
            let Some(current) = dialog.as_mut() else {
                return;
            };
            match current {
                AssetDialog::Create {
                    folder,
                    type_name,
                    name,
                } => {
                    ui.label(format!(
                        "In res{}{}",
                        if folder.is_empty() { "" } else { "/" },
                        folder
                    ));
                    ui.separator();
                    if snapshot.standalone_types.is_empty() {
                        ui.label("No asset types are loaded.");
                    }
                    for info in &snapshot.standalone_types {
                        let chosen = *type_name == info.type_name;
                        let marker = if chosen { ">" } else { " " };
                        let label = format!("{marker} {} (.{})", info.display_name, info.extension);
                        if ui.selectable_label(chosen, label).clicked() {
                            *type_name = info.type_name.clone();
                        }
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("Name");
                        ui.text_edit_singleline(name);
                    });
                }
                AssetDialog::Rename { from, name } => {
                    ui.label(format!("Rename {from}"));
                    ui.separator();
                    ui.label("New name");
                    ui.text_edit_singleline(name);
                    ui.label(
                        egui::RichText::new("The file keeps its extension.")
                            .weak()
                            .italics(),
                    );
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui
                    .add(icons::IconButton::new(
                        icons::Icon::CheckCircle,
                        submit_label,
                    ))
                    .clicked()
                {
                    submit = true;
                }
                if ui
                    .add(icons::IconButton::new(icons::Icon::XCircle, "Cancel"))
                    .clicked()
                {
                    cancel = true;
                }
            });
        });
    if submit {
        match dialog.as_ref() {
            Some(AssetDialog::Create { .. }) => {
                assets::submit_create(dialog, pending);
            }
            Some(AssetDialog::Rename { .. }) => {
                assets::submit_rename(dialog, pending);
            }
            None => {}
        }
    }
    if cancel {
        assets::cancel(dialog);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::Engine;

    #[test]
    fn entity_rows_list_the_worlds_entities() {
        let mut engine = Engine::new();
        engine
            .world_mut()
            .create_entity()
            .build()
            .expect("entity builds");
        let second = engine
            .world_mut()
            .create_entity()
            .build()
            .expect("entity builds");
        let snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
        let rows = entity_rows(&snapshot);
        assert_eq!(rows.len(), 2);
        let expected = format!("{}v{}", second.id(), second.generation());
        assert!(
            rows.iter()
                .any(|(entity, label)| *entity == second && label.contains(&expected)),
            "the second entity's row carries its display id: {rows:?}"
        );
    }

    #[test]
    fn the_systems_rows_name_the_worlds_systems() {
        let mut engine = Engine::new();
        engine.register_system("tick", || {});
        engine.register_system("render", || {});
        let snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
        let rows: Vec<String> = snapshot
            .systems
            .iter()
            .map(systems::system_row_label)
            .collect();
        assert!(rows.iter().any(|row| row.contains("tick")), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("render")), "{rows:?}");
    }

    #[test]
    fn the_tab_bodies_draw_without_panicking() {
        let mut engine = Engine::new();
        engine
            .world_mut()
            .create_entity()
            .build()
            .expect("entity builds");
        engine.register_system("tick", || {});
        let snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
        let mut selection = EditorSelection::default();
        let mut dialog: Option<AssetDialog> = None;
        let mut pending = PendingEditorCommands::default();
        let mut draft: Option<preferences::PreferencesDraft> = None;
        let context = egui::Context::default();
        style::apply(&context);
        let mut regions = Vec::new();
        let _ = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                entities_tab(ui, &snapshot, &mut selection, &mut pending, &mut regions);
                systems_tab(ui, &snapshot, &mut pending);
                inspector_tab(ui, &snapshot, &selection, &mut pending, &mut regions);
                assets_tab(
                    ui,
                    &snapshot,
                    &mut selection,
                    &mut dialog,
                    &mut pending,
                    &mut regions,
                );
                preferences_tab(ui, &snapshot, &mut draft, &mut pending);
                scene_overlay(ui, 120.0, &mut regions);
            });
        });
        assert_eq!(regions.len(), 2, "the two chips are the UI's regions");
    }

    #[test]
    fn a_row_click_moves_the_shared_selection() {
        let mut engine = Engine::new();
        let entity = engine
            .world_mut()
            .create_entity()
            .build()
            .expect("entity builds");
        let mut selection = EditorSelection::default();
        assert!(select_entity(&mut selection, entity));
        assert_eq!(selection.entity, Some(entity));
        // A second click on the selected row is not a change.
        assert!(!select_entity(&mut selection, entity));
    }

    #[test]
    fn an_asset_row_click_moves_the_asset_selection() {
        let mut selection = EditorSelection::default();
        assert!(select_asset(&mut selection, "textures/rock.png"));
        assert_eq!(selection.asset.as_deref(), Some("textures/rock.png"));
        // A second click on the selected row is not a change.
        assert!(!select_asset(&mut selection, "textures/rock.png"));
    }

    #[test]
    fn the_inspector_lists_the_selections_components_and_fields() {
        use pill_editor_core_egui::editor_state::ComponentView;
        use pill_engine::component_registry::ComponentFieldDescriptor;
        let mut engine = Engine::new();
        let entity = engine
            .world_mut()
            .create_entity()
            .build()
            .expect("entity builds");
        let detail = EntityDetail {
            entity,
            components: vec![
                ComponentView {
                    type_name: "tests::Health".to_string(),
                    persistable: false,
                    editable: true,
                    fields: vec![ComponentFieldDescriptor {
                        name: "current",
                        type_tag: "f32",
                        offset: 0,
                        size: 4,
                        align: 4,
                        element_count: 0,
                    }],
                    values: vec![FieldValue::F32(3.0)],
                },
                ComponentView {
                    type_name: "tests::Marker".to_string(),
                    persistable: false,
                    editable: false,
                    fields: Vec::new(),
                    values: Vec::new(),
                },
            ],
        };
        let rows = component_rows(&detail);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "tests::Health");
        assert_eq!(rows[0].1, vec![("current".to_string(), "3".to_string())]);
        // A component without editable fields lists none.
        assert_eq!(rows[1].0, "tests::Marker");
        assert!(rows[1].1.is_empty());

        // The tab draws the detail without panicking.
        let mut snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
        snapshot.detail = Some(detail);
        let mut selection = EditorSelection::default();
        selection.entity = Some(entity);
        selection.asset = Some("textures/rock.png".to_string());
        snapshot.asset_detail = Some(pill_editor_core_egui::editor_state::AssetDetail {
            path: "textures/rock.png".to_string(),
            kind: "pill_master_renderer::assets::Texture".to_string(),
            standalone: false,
            settings: Some(serde_json::json!({
                "texture_type": "Color",
                "mips": 4,
                "flip_v": false,
                "waves": [{ "amplitude": 1.0 }],
            })),
        });
        let mut pending = PendingEditorCommands::default();
        let context = egui::Context::default();
        style::apply(&context);
        let mut regions = Vec::new();
        let _ = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                inspector_tab(ui, &snapshot, &selection, &mut pending, &mut regions);
            });
        });
        assert!(pending.batches.is_empty(), "drawing queues no edits");
    }

    #[test]
    fn field_values_format_for_display() {
        assert_eq!(format_field_value(&FieldValue::F32(1.5)), "1.5");
        assert_eq!(format_field_value(&FieldValue::U32(7)), "7");
        assert_eq!(format_field_value(&FieldValue::Bool(true)), "true");
        assert_eq!(
            format_field_value(&FieldValue::Text("hi".to_string())),
            "hi"
        );
    }
}
