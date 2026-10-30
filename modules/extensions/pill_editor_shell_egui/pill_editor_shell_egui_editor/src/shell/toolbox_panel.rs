//! The floating toolbox over a scene viewport.
//!
//! # Responsibilities
//!
//! - `toolbox_overlay`: the gizmo's tools, space and snapping as one draggable
//!   panel per scene viewport, with its rectangles logged once per placement.
//!
//! The globs below are absolute paths: a generated patch crate replays this
//! file's top-level `use` lines, so a `super::`-relative glob would not
//! resolve there.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The floating toolbox over one scene viewport: the gizmo's tools, its space
/// and its snapping as one panel that drags anywhere inside the view - the
/// always-visible twin of the viewport menu's items.
///
/// The panel floats over the scene's child window and its rectangle joins the
/// frame's overlay regions, so a click anywhere on it - a row or the gap
/// between rows - is the UI's and never becomes a scene pick. The drag starts
/// on the handle and is clamped to the viewport, so the panel can be parked
/// anywhere inside the view but not lost outside it. The panel's rectangle and
/// every row's are logged once per placement and again after each drag, which
/// is where the scripted suite reads its click targets.
#[pill_hot_fn]
pub fn toolbox_overlay(viewer: &mut EditorTabViewer<'_>, ui: &mut egui::Ui, view: usize) {
    let viewport = ui.max_rect();
    let state = toolbox::ToolboxState {
        gizmo: viewer.snapshot.gizmo,
        grid: viewer.snapshot.grid_enabled,
    };
    let mut origin = toolbox::clamped_origin(
        viewer
            .toolbox
            .origin(view)
            .unwrap_or_else(|| toolbox::default_origin(viewport)),
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
        viewer.toolbox.set_origin(view, origin);
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
        style::overlay(),
        egui::Stroke::new(1.0, style::overlay_border()),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        egui::pos2(panel.center().x, handle.center().y),
        egui::Align2::CENTER_CENTER,
        "Tools",
        egui::TextStyle::Small.resolve(ui.style()),
        style::text_muted(),
    );
    // The rows wear the kit's chunky face: the panel floats over the scene, so
    // its buttons are the one place the editor draws the brutalist look the
    // theme's tokens describe. The face is narrower than the row it sits in -
    // the shadow's offset is part of the button's own size - so it is centred
    // in the row and its rectangle is recorded for the scripted suite, which
    // clicks the face rather than the row.
    let theme = style::current_theme();
    let mut faces = Vec::with_capacity(toolbox::TOOLBOX_ITEMS.len());
    for item in toolbox::TOOLBOX_ITEMS {
        let rect = toolbox::item_rect(item, origin);
        let button = ui
            .scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                ui.with_layout(
                    egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                    |ui| {
                        ui.add(
                            kit::ChunkyButton::new(item.label(&state), &theme)
                                .selected(item.active(&state)),
                        )
                    },
                )
                .inner
            })
            .inner;
        faces.push(button.rect);
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
    viewer.toolbox.set_faces(view, faces);
    viewer.overlay_regions.push(shell_rect(panel));
    if viewer.toolbox.logged(view) != Some(panel) {
        viewer.toolbox.set_logged(view, panel);
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
