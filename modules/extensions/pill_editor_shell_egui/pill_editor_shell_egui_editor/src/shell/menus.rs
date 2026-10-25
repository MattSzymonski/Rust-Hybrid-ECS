//! The context menus the tabs open.
//!
//! # Responsibilities
//!
//! - The scene viewport's menu: the gizmo tools, snapping, the space and the scene grid.
//! - The composition menu's component list.
//!
//! Bodies unchanged from the shell's root file (W-0037 Stage 3); the globs
//! below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The component menu: detach the heading's component, and attach any
/// registered one the entity does not carry yet - both recorded under the
/// native labels (`add {short}` / `remove {short}`).
pub fn component_menu_items(
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
            // The same list the Inspector's Add popup offers (W-0049
            // Stage 9): one rule decides what is addable, so the menu and the
            // popup cannot disagree about a component the entity carries.
            for type_name in addable_components(snapshot, detail) {
                let short = inspector::short_component_name(type_name);
                if ui
                    .add(icons::IconButton::new(icons::Icon::PlusCircle, short))
                    .clicked()
                {
                    let (command, label) = inspector::component_add(detail.entity, type_name);
                    pending.push_recorded(vec![command], label, None);
                    ui.close();
                }
            }
        },
    );
}

/// The scene viewport's menu: the transform tools, snapping and the
/// space, checked against the snapshot's gizmo settings. The items queue
/// the same `SetGizmoTool` edit the tools' shortcuts land in.
#[pill_hot_fn]
pub fn scene_context_menu(viewer: &mut EditorTabViewer<'_>, ui: &mut egui::Ui) {
    let settings = viewer.snapshot.gizmo;
    let grid_enabled = viewer.snapshot.grid_enabled;
    let pending = &mut viewer.pending;
    let response = ui.allocate_response(ui.available_size_before_wrap(), egui::Sense::click());
    context_menu("scene", &response, &mut viewer.overlay_regions, &mut |ui| {
        for (mode, label, icon) in TOOL_ROWS {
            let button = icons::IconButton::new(icon, label).active(settings.mode == mode);
            if ui.add(button).clicked() {
                pending.push(vec![EditorCommand::SetGizmoTool {
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
            pending.push(vec![EditorCommand::SetGizmoTool {
                mode: None,
                space: None,
                snap: Some(!settings.snap),
            }]);
            ui.close();
        }
        for (space, label, icon) in SPACE_ROWS {
            let button = icons::IconButton::new(icon, label).active(settings.space == space);
            if ui.add(button).clicked() {
                pending.push(vec![EditorCommand::SetGizmoTool {
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
            pending.push(vec![EditorCommand::SetGrid {
                enabled: !grid_enabled,
            }]);
            ui.close();
        }
    });
}
