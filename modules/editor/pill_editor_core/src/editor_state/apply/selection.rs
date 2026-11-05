//! Applies the selection command family.
//!
//! These commands change the editor's shared selection, and `ArmEntityPick`
//! arms the request the next scene click answers.
//!
//! # Responsibilities
//!
//! - Apply the selection edits (`SetSelection`, `SelectResource`,
//!   `RestoreSelection`), each answering with the inverse that puts the
//!   previous selection back.
//! - Arm the next scene click (`ArmEntityPick`) without recording anything.
//!
//! # Design
//!
//! One `match` over the family's own variants, called from
//! [`EditorCommand::apply_one`](super::EditorCommand). Every arm for a
//! selection command ends in a [`CommandOutcome`]: the inverse the history
//! records, or `plain()` for a command that cannot be undone. A command of
//! another family hits `unreachable!`.

// External crates
use pill_core::telemetry::telemetry_target;
use pill_engine::Engine;

// Current crate
use super::CommandOutcome;
use super::*;

// =============================================================================
// Free Functions
// =============================================================================

/// Applies one selection command.
///
/// # Errors
///
/// Returns an error message when the selection resource cannot be claimed
/// after this function inserted it, or when `ArmEntityPick` finds no
/// `EntityPickRequest` resource in the world.
///
/// # Panics
///
/// Panics when `command` belongs to another family; `EditorCommand::apply_one`
/// routes only selection commands here.
pub(super) fn apply(
    engine: &mut Engine,
    command: &EditorCommand,
) -> Result<CommandOutcome, String> {
    match command {
        EditorCommand::SetSelection { entities, asset } => {
            let world = engine.world_mut();
            if world.get_resource::<EditorSelection>().is_none() {
                world.insert_resource(EditorSelection::default());
            }
            // Entities the world no longer has are dropped from the
            // selection. The asset slot is still applied.
            let mut existing_entities = Vec::with_capacity(entities.len());
            for entity in entities {
                if world.is_entity_valid(*entity) {
                    existing_entities.push(*entity);
                } else {
                    pill_core::info!(
                        target: telemetry_target::EDITOR,
                        entity = %entity,
                        "[editor] selection command dropped; the entity no longer exists"
                    );
                }
            }
            let Some(selection) = world.get_resource_mut::<EditorSelection>() else {
                return Err(format!("{command:?}: the selection could not be claimed"));
            };
            let previous_selection_command = EditorCommand::SetSelection {
                entities: selection.entities().to_vec(),
                asset: selection.asset.clone(),
            };
            selection.set_entities(existing_entities);
            selection.asset = asset.clone();
            Ok(CommandOutcome::inverse(previous_selection_command))
        }

        EditorCommand::SelectResource { resource } => {
            let world = engine.world_mut();
            if world.get_resource::<EditorSelection>().is_none() {
                world.insert_resource(EditorSelection::default());
            }
            let Some(selection) = world.get_resource_mut::<EditorSelection>() else {
                return Err(format!("{command:?}: the selection could not be claimed"));
            };
            let previous_resource = selection.resource.clone();
            // The Inspector shows one thing at a time. Choosing a resource
            // drops the entity and asset selections; the `RestoreSelection`
            // inverse brings all three slots back on undo. Clearing the
            // resource (`None`) leaves the other two slots alone.
            let previous_entities = selection.entities().to_vec();
            let previous_asset = selection.asset.clone();
            selection.resource = resource.clone();
            if resource.is_some() {
                selection.clear_entities();
                selection.asset = None;
            }
            // The egui editor's suite matches this line, so its token and
            // fields are a contract: it checks the selection a resource row
            // makes, and the one its undo restores.
            pill_core::info!(
                target: telemetry_target::EDITOR,
                resource = resource.as_deref().unwrap_or("none"),
                "[editor] resource selected"
            );
            Ok(CommandOutcome::inverse(EditorCommand::RestoreSelection {
                entities: previous_entities,
                asset: previous_asset,
                resource: previous_resource,
            }))
        }

        EditorCommand::RestoreSelection {
            entities,
            asset,
            resource,
        } => {
            let world = engine.world_mut();
            if world.get_resource::<EditorSelection>().is_none() {
                world.insert_resource(EditorSelection::default());
            }
            let Some(selection) = world.get_resource_mut::<EditorSelection>() else {
                return Err(format!("{command:?}: the selection could not be claimed"));
            };
            let previous_selection_command = EditorCommand::RestoreSelection {
                entities: selection.entities().to_vec(),
                asset: selection.asset.clone(),
                resource: selection.resource.clone(),
            };
            selection.set_entities(entities.clone());
            selection.asset = asset.clone();
            selection.resource = resource.clone();
            // The egui editor's suite matches this line as well. After the
            // undo it requires an `entity` that is not `none`, because the
            // undo has to bring the entity slot back. Clearing only the
            // resource slot is not enough.
            pill_core::info!(
                target: telemetry_target::EDITOR,
                entity = selection
                    .primary()
                    .map(|entity| entity.to_string())
                    .unwrap_or_else(|| "none".to_string())
                    .as_str(),
                resource = resource.as_deref().unwrap_or("none"),
                entities = selection.entities().len(),
                "[editor] selection restored"
            );
            Ok(CommandOutcome::inverse(previous_selection_command))
        }

        EditorCommand::ArmEntityPick {
            entity,
            component,
            field,
        } => {
            let world = engine.world_mut();
            let entity_pick_request = world
                .get_resource_mut::<crate::editor_state::EntityPickRequest>()
                .ok_or("the world has no entity pick request slot")?;
            entity_pick_request.target = Some(crate::editor_state::EntityPickTarget {
                entity: *entity,
                component: component.clone(),
                field: field.clone(),
            });
            pill_core::info!(
                target: telemetry_target::EDITOR,
                entity = %entity,
                component = component.as_str(),
                field = field.as_str(),
                "[editor] inspector entity pick armed"
            );
            Ok(CommandOutcome::plain())
        }

        other => unreachable!("{:?} is not a selection command", other.family()),
    }
}
