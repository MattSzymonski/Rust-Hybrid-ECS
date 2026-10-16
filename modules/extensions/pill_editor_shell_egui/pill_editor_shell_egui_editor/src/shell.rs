//! The shell's UI work: the dock, the tabs and the frame build.
//!
//! # Responsibilities
//!
//! - [`run_frame`]: one egui pass, from the boundary input to the boundary
//!   frame the executable draws, including the scene rectangle the frontend
//!   places the scene's child window at.
//! - The tab bodies ([`entities_tab`], [`systems_tab`], [`assets_tab`], all
//!   `#[pill_hot_fn]` so a body edit patches live) and their row builders.
//!   The Scene tab draws nothing: its body stays a hole in the UI so the
//!   scene's child window, which sits under the overlay, shows through.
//!
//! # Design
//!
//! The dock is the session's `egui_dock` state, so layout edits land in the
//! resource the next generation re-claims. Nothing here stores anything in
//! egui's `Context` (the reload-safety contract `pill_ui_egui` documents);
//! the tabs read the snapshot and write the shared selection.

// External crates
use pill_editor_core_egui::editor_state::{EditorSelection, EditorSnapshot};
use pill_editor_core_egui::layout::EditorTab;
use pill_editor_core_egui::session::EguiShellState;
use pill_engine::{pill_hot_fn, Entity, Input};
use pill_ui_egui::boundary::{frame_from_output, EguiShellFrame, EguiShellInput, ShellRect};
use pill_ui_egui::egui;
use pill_ui_egui::egui_dock::{DockArea, Style, TabViewer};

// =============================================================================
// Constants
// =============================================================================

/// How far the scene rectangle extends past the measured tab body, in
/// logical points: the body's padding and outline sit inside the leaf, and
/// the child window must back them too, so no sliver of the game view
/// behind the overlay shows around the viewport's edge.
const SCENE_RECT_OVERSHOOT: f32 = 16.0;

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
    // the project's data, and that the frame counter - and with it the
    // session resource - survives a reload or patch.
    if state.frame_index.is_multiple_of(210) {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            frames = state.frame_index,
            entities = snapshot.entities.len(),
            systems = snapshot.systems.len(),
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
        scene_rect: None,
    };
    let output = context.run(raw_input, |context| {
        DockArea::new(state.dock.state_mut())
            .style(Style::from_egui(context.style().as_ref()))
            .show(context, &mut viewer);
    });
    let mut shell_frame = frame_from_output(&context, output);
    shell_frame.scene_rect = viewer.scene_rect;
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
    /// The Scene tab's body rectangle this pass, in logical points; the
    /// frontend places the scene's child window at it.
    scene_rect: Option<ShellRect>,
}

impl TabViewer for EditorTabViewer<'_> {
    type Tab = EditorTab;

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        (*tab).title().into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            EditorTab::Scene => self.scene_rect = Some(scene_body_rect(ui)),
            EditorTab::Entities => entities_tab(ui, self.snapshot, self.selection),
            EditorTab::Systems => systems_tab(ui, self.snapshot),
            EditorTab::Assets => assets_tab(ui),
        }
    }

    /// The Scene tab keeps its body transparent: the scene's child window
    /// shows through it from under the overlay.
    fn clear_background(&self, tab: &Self::Tab) -> bool {
        !matches!(tab, EditorTab::Scene)
    }

    /// The Scene tab's body is a viewport; it never scrolls.
    fn scroll_bars(&self, tab: &Self::Tab) -> [bool; 2] {
        match tab {
            EditorTab::Scene => [false, false],
            _ => [true, true],
        }
    }
}

/// The Scene tab's body rectangle in logical points, overshooting by
/// [`SCENE_RECT_OVERSHOOT`] so the child window backs the tab body's padding
/// and outline too.
fn scene_body_rect(ui: &egui::Ui) -> ShellRect {
    let rect = ui.max_rect().expand(SCENE_RECT_OVERSHOOT);
    ShellRect {
        x: rect.min.x,
        y: rect.min.y,
        width: rect.width(),
        height: rect.height(),
    }
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
                selection.entity = Some(entity);
            }
        }
    });
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
        let _ = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                entities_tab(ui, &snapshot, &mut selection);
                systems_tab(ui, &snapshot);
                assets_tab(ui);
            });
        });
    }
}
