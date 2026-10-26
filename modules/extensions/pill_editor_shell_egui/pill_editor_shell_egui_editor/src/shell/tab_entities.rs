//! The Entities tab: the entity rows and their menu.
//!
//! # Responsibilities
//!
//! - `entity_rows`: the rows the tab lists, from the snapshot's entity list.
//! - `entities_tab`: the rows, each a selection move, over the tab's empty strip's menu.
//! - `entity_menu_items`: the row menu's create and delete actions.
//! - `remember_hierarchy_row_rect`: each row's rectangle, logged for the suites.
//!
//! Bodies unchanged from the shell's root file (W-0037 Stage 3); the globs
//! below are absolute paths, so a patch crate resolves them too.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The payload a Hierarchy row hands an entity reference row when dragged
/// (W-0048 Stage 6).
///
/// A newtype rather than the bare handle: egui matches a drop by payload
/// type, and an entity must not be mistaken for the path string an asset tile
/// drags.
#[derive(Clone, Copy, Debug)]
pub struct DraggedEntity(pub Entity);

/// Record a Hierarchy row's rectangle in the shell's session, returning
/// nothing: a row logs its rectangle on its first draw and whenever it moves,
/// so a suite can drag from a row it cannot otherwise locate.
pub fn remember_hierarchy_row_rect(
    session: &mut InspectorSession,
    entity: Entity,
    rect: egui::Rect,
) {
    let key = entity.to_string();
    match session
        .hierarchy_row_rects
        .iter_mut()
        .find(|(logged, _)| *logged == key)
    {
        Some((_, logged)) => {
            if *logged == rect {
                return;
            }
            *logged = rect;
        }
        None => session.hierarchy_row_rects.push((key, rect)),
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        entity = %entity,
        min_x = rect.min.x,
        min_y = rect.min.y,
        max_x = rect.max.x,
        max_y = rect.max.y,
        "[editor] hierarchy row rect"
    );
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

/// The entity menu: New Entity everywhere, Delete Entity for the row under
/// the pointer. Both edits are recorded, so one undo step reverses them.
pub fn entity_menu_items(
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
    session: &mut InspectorSession,
) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        // The list's own order, which a Shift-click reads to select a range.
        let rows: Vec<Entity> = entity_rows(snapshot)
            .into_iter()
            .map(|(entity, _label)| entity)
            .collect();
        for (entity, label) in entity_rows(snapshot) {
            let selected = selection.contains(entity);
            let response = ui.selectable_label(selected, label);
            // The reference's selected row: egui fills it with the theme's
            // `selection` colour, and the accent bar marks its left edge.
            if selected {
                style::selected_row_bar(ui, response.rect);
            }
            // One layer senses the row's press both ways (W-0049): a press
            // that moves carries the entity for a drop on a reference field
            // (W-0048 Stage 6), and a press that does not is the row's click.
            let row = ui.interact(
                response.rect,
                response.id.with("dragged-entity"),
                egui::Sense::click_and_drag(),
            );
            row.dnd_set_drag_payload(DraggedEntity(entity));
            remember_hierarchy_row_rect(session, entity, row.rect);
            if row.clicked() {
                let click = ui.input(|input| click_from_modifiers(&input.modifiers));
                select_entity_click(selection, entity, click, &rows);
            }
            // A right click picks the row it opens the menu for, without
            // disturbing a set the person is about to edit.
            if row.secondary_clicked() {
                select_entity(selection, entity);
            }
            context_menu("entities", &row, overlay_regions, &mut |ui| {
                entity_menu_items(ui, Some(entity), selection, pending)
            });
        }
        // The space below the rows creates one entity; there is nothing to
        // delete away from a row.
        let empty = ui.allocate_response(empty_area(ui), egui::Sense::click());
        context_menu("entities", &empty, overlay_regions, &mut |ui| {
            entity_menu_items(ui, None, selection, pending)
        });
    });
}
