//! The context menus the tabs open.
//!
//! # Responsibilities
//!
//! - The scene viewport's menu: the gizmo tools, snapping, the space and the scene grid.
//! - The Inspector's component menu: remove one component, or add any addable
//!   one the entity does not carry.
//!
//! The globs below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The component menu: detach the heading's component, and attach any
/// registered one the entity does not carry yet - both recorded through the
/// shared builders, labelled `add {short}` / `remove {short}`.
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
            // The same list the Inspector's Add popup offers: one rule decides
            // what is addable, so the menu and the popup cannot disagree about
            // a component the entity carries.
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

/// The scene viewport's menu: the transform tools, snapping, the space and the
/// scene grid, checked against the snapshot's settings. The tool, space and
/// snapping items queue the same `SetGizmoTool` edit the shortcuts queue; the
/// Grid item queues `SetGrid`.
#[pill_hot_fn]
pub fn scene_context_menu(viewer: &mut EditorTabViewer<'_>, ui: &mut egui::Ui) {
    let settings = viewer.snapshot.gizmo;
    let state = toolbox::ToolboxState {
        gizmo: settings,
        grid: viewer.snapshot.grid_enabled,
    };
    let pending = &mut viewer.pending;
    let response = ui.allocate_response(ui.available_size_before_wrap(), egui::Sense::click());
    context_menu("scene", &response, &mut viewer.overlay_regions, &mut |ui| {
        // The rows are the toolbox's own, so a tool, a toggle or a space cannot
        // be labelled or enabled one way here and another way on the panel.
        for item in toolbox::TOOL_ROWS {
            let button = icons::IconButton::new(item.icon(&state), item.menu_label())
                .active(item.active(&state));
            if ui.add(button).clicked() {
                pending.push(vec![item.command(&state)]);
                ui.close();
            }
        }
        ui.separator();
        let snapping = icons::IconButton::new(
            toolbox::ToolboxItem::Snap.icon(&state),
            toolbox::ToolboxItem::Snap.menu_label(),
        )
        .active(toolbox::ToolboxItem::Snap.active(&state));
        if ui.add(snapping).clicked() {
            pending.push(vec![toolbox::ToolboxItem::Snap.command(&state)]);
            ui.close();
        }
        for row in toolbox::SPACE_ROWS {
            let button =
                icons::IconButton::new(row.icon, row.menu).active(settings.space == row.space);
            if ui.add(button).clicked() {
                pending.push(vec![toolbox::space_command(row.space)]);
                ui.close();
            }
        }
        ui.separator();
        let grid = icons::IconButton::new(
            toolbox::ToolboxItem::Grid.icon(&state),
            toolbox::ToolboxItem::Grid.menu_label(),
        )
        .active(toolbox::ToolboxItem::Grid.active(&state));
        if ui.add(grid).clicked() {
            pending.push(vec![toolbox::ToolboxItem::Grid.command(&state)]);
            ui.close();
        }
    });
}
