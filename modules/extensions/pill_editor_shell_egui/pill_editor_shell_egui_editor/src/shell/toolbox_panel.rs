//! The floating toolbox over a scene viewport.
//!
//! # Responsibilities
//!
//! - `toolbox_overlay`: the gizmo's tools, space and snapping as one draggable panel per scene viewport, logging its rectangles once per placement.
//!
//! Bodies unchanged from the shell's root file (W-0037 Stage 3); the globs
//! below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

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
#[pill_hot_fn]
pub fn toolbox_overlay(viewer: &mut EditorTabViewer<'_>, ui: &mut egui::Ui, view: usize) {
    let viewport = ui.max_rect();
    let state = toolbox::ToolboxState {
        gizmo: viewer.snapshot.gizmo,
        grid: viewer.snapshot.grid_enabled,
    };
    let mut origin = toolbox::clamped_origin(
        viewer.toolbox.origins[view].unwrap_or_else(|| toolbox::default_origin(viewport)),
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
        viewer.toolbox.origins[view] = Some(origin);
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
            viewer.pending.push(vec![item.command(&state)]);
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                view,
                item = item.name(),
                "[editor] toolbox click"
            );
        }
    }
    viewer.overlay_regions.push(shell_rect(panel));
    if viewer.toolbox.logged[view] != Some(panel) {
        viewer.toolbox.logged[view] = Some(panel);
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
