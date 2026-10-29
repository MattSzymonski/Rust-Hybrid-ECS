//! The selection family's applier.
//!
//! The selection commands and the request that arms the next scene click.
//!
//! # Responsibilities
//!
//! - Apply the selection edits (`SetSelection`, `SelectResource`,
//!   `RestoreSelection`) with the inverse that returns the previous selection.
//! - Arm the next scene click (`ArmEntityPick`) without recording anything.
//!
//! # Design
//!
//! One `match` over the family's own variants, called from
//! [`EditorCommand::apply_one`](super::EditorCommand). Every arm ends in a
//! [`CommandOutcome`]: the inverse the history records, or `plain()` for a
//! command that cannot be undone.

use pill_engine::Engine;

use super::CommandOutcome;
use super::*;

/// Apply one selection command.
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
            // A selection that names an entity the world no longer
            // carries drops that entity; the asset half still applies.
            let mut taken = Vec::with_capacity(entities.len());
            for entity in entities {
                if world.is_entity_valid(*entity) {
                    taken.push(*entity);
                } else {
                    // NOTE: asserted on by the editor's recovery suite.
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ENGINE,
                        entity = %entity,
                        "[editor] selection command dropped; the entity no longer exists"
                    );
                }
            }
            let Some(selection) = world.get_resource_mut::<EditorSelection>() else {
                return Err(format!("{command:?}: the selection could not be claimed"));
            };
            let previous = EditorCommand::SetSelection {
                entities: selection.entities().to_vec(),
                asset: selection.asset.clone(),
            };
            selection.set_entities(taken);
            selection.asset = asset.clone();
            Ok(CommandOutcome::inverse(previous))
        }

        EditorCommand::SelectResource { resource } => {
            let world = engine.world_mut();
            if world.get_resource::<EditorSelection>().is_none() {
                world.insert_resource(EditorSelection::default());
            }
            let Some(selection) = world.get_resource_mut::<EditorSelection>() else {
                return Err(format!("{command:?}: the selection could not be claimed"));
            };
            let previous = selection.resource.clone();
            // One thing at a time in the Inspector: picking a resource
            // drops the entity and asset halves, and their undo restores
            // them through their own commands.
            let previous_entities = selection.entities().to_vec();
            let previous_asset = selection.asset.clone();
            selection.resource = resource.clone();
            if resource.is_some() {
                selection.clear_entities();
                selection.asset = None;
            }
            // NOTE: asserted on by the egui editor's suite: the selection a
            // resource row makes, and the one its undo restores.
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                resource = resource.as_deref().unwrap_or("none"),
                "[editor] resource selected"
            );
            Ok(CommandOutcome::inverse(EditorCommand::RestoreSelection {
                entities: previous_entities,
                asset: previous_asset,
                resource: previous,
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
            let previous = EditorCommand::RestoreSelection {
                entities: selection.entities().to_vec(),
                asset: selection.asset.clone(),
                resource: selection.resource.clone(),
            };
            selection.set_entities(entities.clone());
            selection.asset = asset.clone();
            selection.resource = resource.clone();
            // NOTE: asserted on by the egui editor's suite: an undone
            // resource selection brings the entity slot back, not just an
            // empty resource slot.
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                entity = selection
                    .primary()
                    .map(|entity| entity.to_string())
                    .unwrap_or_else(|| "none".to_string())
                    .as_str(),
                resource = resource.as_deref().unwrap_or("none"),
                entities = selection.entities().len(),
                "[editor] selection restored"
            );
            Ok(CommandOutcome::inverse(previous))
        }

        EditorCommand::ArmEntityPick {
            entity,
            component,
            field,
        } => {
            let world = engine.world_mut();
            let request = world
                .get_resource_mut::<crate::editor_state::EntityPickRequest>()
                .ok_or("the world has no entity pick request slot")?;
            request.target = Some(crate::editor_state::EntityPickTarget {
                entity: *entity,
                component: component.clone(),
                field: field.clone(),
            });
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
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
