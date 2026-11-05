//! The Entities tab: the entity rows and their menu.
//!
//! # Responsibilities
//!
//! - `DraggedEntity`: the drag payload a row carries to an Inspector
//!   entity-reference field.
//! - `entity_rows`: the rows the tab lists, from the snapshot's entity list.
//! - `entities_tab`: the rows with their selection and context menus, and the
//!   empty strip below them.
//! - `entity_menu_items`: the row menu's create and delete actions.
//! - `remember_hierarchy_row_rect`: each row's rectangle, logged for the suites.
//!
//! # Design
//!
//! Both globs below are absolute paths. A generated patch crate replays this
//! file's top-level `use` lines, and a `super::`-relative glob would have
//! nothing to point at there.

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
use pill_editor_shell_egui_editor::shell::*;

/// The payload an Entities row drags onto an Inspector entity-reference
/// field.
///
/// A newtype, not the bare entity handle. egui matches a drop by the payload
/// type, so an entity and the path string an asset tile drags must not share
/// one.
#[derive(Clone, Copy, Debug)]
pub struct DraggedEntity(pub Entity);

/// Record an Entities row's rectangle in the shell's session.
///
/// The row logs its rectangle on its first draw and whenever it moves, so a
/// suite can drag from a row it cannot otherwise locate.
pub fn remember_hierarchy_row_rect(
    session: &mut InspectorSession,
    entity: Entity,
    rect: egui::Rect,
) {
    let entity_key = entity.to_string();
    session.hierarchy_seen.insert(entity_key.clone());
    if !session.hierarchy_row_rects.remember_exact(entity_key, rect) {
        return;
    }
    pill_core::info!(
        target: telemetry_target::EDITOR,
        entity = %entity,
        min_x = rect.min.x,
        min_y = rect.min.y,
        max_x = rect.max.x,
        max_y = rect.max.y,
        "[editor] hierarchy row rect"
    );
}

/// Returns one row per live entity, for the Entities tab.
///
/// Each row pairs the entity handle with its label: the display name and the
/// number of components attached.
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

/// Adds the entity menu's rows to `ui` and queues the recorded edit of the one
/// that is clicked.
///
/// `entity` is the row the menu was opened on, or `None` for the strip below
/// the rows, where only New Entity is offered. Undo reverses either edit.
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
            target: telemetry_target::EDITOR,
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
                target: telemetry_target::EDITOR,
                entity = %entity,
                "[editor] delete entity queued"
            );
            ui.close();
        }
    }
}

/// Draws the Entities tab: one row per live entity, then an empty strip that
/// offers the same create action.
///
/// A left click selects the row, `Shift` extends the selection over a range
/// and `Ctrl` adds to it. A right click selects the row and opens its menu.
/// Dragging a row carries its entity to an Inspector reference field.
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
        let row_entities: Vec<Entity> = entity_rows(snapshot)
            .into_iter()
            .map(|(entity, _label)| entity)
            .collect();
        // Step 1: the entity rows, one per live entity.
        for (entity, label) in entity_rows(snapshot) {
            let selected = selection.contains(entity);
            let label_response = ui.selectable_label(selected, label);
            // The selected row: egui fills it with the theme's `selection`
            // colour, and the accent bar marks its left edge.
            if selected {
                style::selected_row_bar(ui, label_response.rect);
            }
            // One interaction layer senses both a click and a drag: a press
            // that moves carries the entity to a reference field, and a press
            // that does not is the row's click.
            let row_interaction = ui.interact(
                label_response.rect,
                label_response.id.with("dragged-entity"),
                egui::Sense::click_and_drag(),
            );
            row_interaction.dnd_set_drag_payload(DraggedEntity(entity));
            remember_hierarchy_row_rect(session, entity, row_interaction.rect);
            if row_interaction.clicked() {
                let selection_click = ui.input(|input| click_from_modifiers(&input.modifiers));
                select_entity_click(selection, entity, selection_click, &row_entities);
            }
            // A right click selects the row the menu opens for, replacing the
            // entity selection.
            if row_interaction.secondary_clicked() {
                select_entity(selection, entity);
            }
            context_menu("entities", &row_interaction, overlay_regions, &mut |ui| {
                entity_menu_items(ui, Some(entity), selection, pending)
            });
        }
        // Step 2: the empty strip below the rows. Its menu can only create an
        // entity; there is no row to delete.
        let empty = ui.allocate_response(empty_area(ui), egui::Sense::click());
        context_menu("entities", &empty, overlay_regions, &mut |ui| {
            entity_menu_items(ui, None, selection, pending)
        });
    });
}
