//! The shell's UI work: the dock, the tabs and the frame build.
//!
//! # Responsibilities
//!
//! - [`run_frame`]: one egui pass, from the boundary input to the boundary
//!   frame the executable draws, including the scene rectangle the frontend
//!   places the scene's child window at.
//! - The tab bodies ([`entities_tab`], [`systems_tab`], [`inspector_tab`],
//!   [`assets_tab`], all `#[pill_hot_fn]` so a body edit patches live) and
//!   their row builders.
//!   The scene tabs draw nothing: their bodies stay holes in the UI so the
//!   scene child windows, which sit under the overlay, show through.
//!
//! # Design
//!
//! The dock is the session's `egui_dock` state, so layout edits land in the
//! resource the next generation re-claims. Nothing here stores anything in
//! egui's `Context` (the reload-safety contract `pill_ui_egui` documents);
//! the tabs read the snapshot and write the shared selection.

// External crates
use pill_editor_core_egui::editor_state::{EditorSelection, EditorSnapshot, EntityDetail};
use pill_editor_core_egui::layout::EditorTab;
use pill_editor_core_egui::session::EguiShellState;
use pill_engine::{pill_hot_fn, Entity, FieldValue, Input};
use pill_ui_egui::boundary::{
    frame_from_output, EguiShellFrame, EguiShellInput, ShellRect, SCENE_VIEWPORTS,
};
use pill_ui_egui::egui;
use pill_ui_egui::egui_dock::{DockArea, TabViewer};
use pill_ui_egui::style;

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

/// The rows the Assets tab mocks, as (name, kind); the real panel arrives
/// with the asset-store port.
const MOCK_ASSETS: [(&str, &str); 6] = [
    ("helmet_albedo.png", "texture"),
    ("helmet_emissive.jpg", "texture"),
    ("ui_atlas.png", "texture"),
    ("pbr.hlsl", "shader"),
    ("crate.glb", "mesh"),
    ("courtyard.scene", "scene"),
];

/// One shell frame: engine input becomes egui input, the dock runs, and the
/// output becomes the executable's frame.
pub fn run_frame(
    state: &mut EguiShellState,
    shell_input: &EguiShellInput,
    frame: &mut EguiShellFrame,
    snapshot: &EditorSnapshot,
    selection: &mut EditorSelection,
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
            detail = detail_entity.as_str(),
            detail_components,
            "[editor] egui shell rows"
        );
    }
    let raw_input = pill_ui_egui::input::raw_input(engine_input, shell_input);
    // The context is a cheap clone (refcounted handle); the dock is borrowed
    // through `state` inside the closure, so the two do not alias.
    let context = state.context.clone();
    let mut viewer = EditorTabViewer {
        snapshot,
        selection,
        scene_rects: [None; SCENE_VIEWPORTS],
        overlay_regions: Vec::new(),
        frames_per_second: shell_input.frames_per_second,
    };
    let output = context.run(raw_input, |context| {
        // Tab bars carry the tabs and their close buttons only: without the
        // collapse chevron and the panel-level close-all ✖, a panel closes
        // exactly when its last tab does.
        DockArea::new(state.dock.state_mut())
            .style(style::dock_style(context.style().as_ref()))
            .show_leaf_collapse_buttons(false)
            .show_leaf_close_all_buttons(false)
            .show(context, &mut viewer);
    });
    let mut shell_frame = frame_from_output(&context, output);
    shell_frame.scene_rects = viewer.scene_rects;
    shell_frame.overlay_regions = std::mem::take(&mut viewer.overlay_regions);
    *frame = shell_frame;
}

// =============================================================================
// The tab viewer and the tabs
// =============================================================================

/// Draws the dock's tabs; the tab bodies come from the tab functions.
struct EditorTabViewer<'a> {
    /// The engine data the tabs read.
    snapshot: &'a EditorSnapshot,
    /// The shared selection the Entities tab writes.
    selection: &'a mut EditorSelection,
    /// The scene viewports' body rectangles this pass, in logical points,
    /// one slot per scene tab; the frontend places each viewport's child
    /// window at its slot.
    scene_rects: [Option<ShellRect>; SCENE_VIEWPORTS],
    /// The overlay chips' rectangles this pass, in logical points: points
    /// over a scene viewport where a click is the UI's, not the scene's.
    overlay_regions: Vec<ShellRect>,
    /// The editor's smoothed frame rate, for the scene's counter.
    frames_per_second: f32,
}

impl TabViewer for EditorTabViewer<'_> {
    type Tab = EditorTab;

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        (*tab).title().into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            EditorTab::Scene => {
                self.scene_rects[0] = Some(scene_body_rect(ui));
                scene_overlay(ui, self.frames_per_second, &mut self.overlay_regions);
            }
            EditorTab::SecondScene => {
                self.scene_rects[1] = Some(scene_body_rect(ui));
                scene_overlay(ui, self.frames_per_second, &mut self.overlay_regions);
            }
            EditorTab::Entities => entities_tab(ui, self.snapshot, self.selection),
            EditorTab::Systems => systems_tab(ui, self.snapshot),
            EditorTab::Inspector => inspector_tab(ui, self.snapshot, &*self.selection),
            EditorTab::Assets => assets_tab(ui),
        }
    }

    /// The scene tabs keep their bodies transparent: the scene child
    /// windows show through them from under the overlay.
    fn clear_background(&self, tab: &Self::Tab) -> bool {
        !matches!(tab, EditorTab::Scene | EditorTab::SecondScene)
    }

    /// The scene tabs' bodies are viewports; they never scroll.
    fn scroll_bars(&self, tab: &Self::Tab) -> [bool; 2] {
        match tab {
            EditorTab::Scene | EditorTab::SecondScene => [false, false],
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

/// One label per system row, for the Systems tab: name, state and owner.
pub fn system_rows(snapshot: &EditorSnapshot) -> Vec<String> {
    snapshot
        .systems
        .iter()
        .map(|system| {
            let state = if system.enabled {
                "enabled"
            } else {
                "disabled"
            };
            format!("{}  [{}]  {}", system.name, state, system.owner_label)
        })
        .collect()
}

/// The Entities tab: the world's entities; clicking one selects it.
#[pill_hot_fn]
pub fn entities_tab(ui: &mut egui::Ui, snapshot: &EditorSnapshot, selection: &mut EditorSelection) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        for (entity, label) in entity_rows(snapshot) {
            let selected = selection.entity == Some(entity);
            if ui.selectable_label(selected, label).clicked() {
                select_entity(selection, entity);
            }
        }
    });
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

/// The Systems tab: the world's registered systems and their state.
#[pill_hot_fn]
pub fn systems_tab(ui: &mut egui::Ui, snapshot: &EditorSnapshot) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        for label in system_rows(snapshot) {
            ui.label(label);
        }
    });
}

/// The Inspector tab: the selected entity's components and their field
/// values, read-only - the editable rows and the add/remove actions stay
/// with the W-0027 port.
#[pill_hot_fn]
pub fn inspector_tab(ui: &mut egui::Ui, snapshot: &EditorSnapshot, selection: &EditorSelection) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        let Some(detail) = snapshot.detail.as_ref() else {
            match selection.entity {
                Some(entity) => {
                    ui.label(format!("entity {entity} is not alive"));
                }
                None => {
                    ui.label("Select an entity in the Entities tab.");
                }
            }
            return;
        };
        ui.label(egui::RichText::new(format!("entity {}", detail.entity)).strong());
        ui.separator();
        for (type_name, fields) in component_rows(detail) {
            ui.label(egui::RichText::new(type_name).strong());
            if fields.is_empty() {
                ui.label(egui::RichText::new("no editable fields").weak().italics());
            }
            for (name, value) in fields {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(name).color(style::TEXT_MUTED));
                    ui.label(value);
                });
            }
            ui.separator();
        }
    });
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

/// The Assets tab: a mock of the asset panel (the real one arrives with the
/// asset-store port), so the layout settles around representative rows.
#[pill_hot_fn]
pub fn assets_tab(ui: &mut egui::Ui) {
    ui.label(
        egui::RichText::new("mock: rows are placeholders")
            .weak()
            .italics(),
    );
    egui::ScrollArea::vertical().show(ui, |ui| {
        for (name, kind) in MOCK_ASSETS {
            ui.label(format!("{name}  [{kind}]"));
        }
    });
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
    fn system_rows_list_the_worlds_systems() {
        let mut engine = Engine::new();
        engine.register_system("tick", || {});
        engine.register_system("render", || {});
        let snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
        let rows = system_rows(&snapshot);
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
        let context = egui::Context::default();
        let mut regions = Vec::new();
        let _ = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                entities_tab(ui, &snapshot, &mut selection);
                systems_tab(ui, &snapshot);
                inspector_tab(ui, &snapshot, &selection);
                assets_tab(ui);
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
        let context = egui::Context::default();
        let _ = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                inspector_tab(ui, &snapshot, &selection);
            });
        });
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
