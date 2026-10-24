//! The Entities tab's shared actions: the recorded create and delete the
//! row menu and the scripted channel both queue.
//!
//! # Responsibilities
//!
//! - [`entity_create`]: one empty entity, under the native Hierarchy's
//!   `new entity` label.
//! - [`entity_delete`]: the destroy with the selection clear as ONE recorded
//!   step, under the native `delete entity` label, so undo restores the
//!   entity and the selection that pointed at it.
//!
//! # Design
//!
//! The commands ride the shared
//! [`PendingEditorCommands`](crate::commands::PendingEditorCommands) queue
//! and apply at the frame boundary like every other editor edit. The
//! restore of a deleted entity lands under a fresh handle; the executable's
//! drain follows the remap, so callers never hold a stale one.

// Current crate
use crate::editor_state::EditorCommand;

/// The recorded create: one empty entity, ready for components.
pub fn entity_create() -> (EditorCommand, String) {
    (
        EditorCommand::CreateEntity {
            components: Vec::new(),
        },
        "new entity".to_string(),
    )
}

/// The recorded delete: the selection clear and the destroy in one step.
///
/// `asset` is the selection's asset half, carried so the clear keeps it -
/// the user is deleting an entity, not dropping the file they had selected.
/// The clear comes FIRST: an undo walks the step backwards, so the destroy's
/// restore runs first, assigns the entity its fresh handle, and the batch's
/// own selection command follows it to that handle (see
/// [`EditorCommand::apply_recorded`]).
pub fn entity_delete(
    entity: pill_engine::Entity,
    asset: Option<String>,
) -> (Vec<EditorCommand>, String) {
    (
        vec![
            EditorCommand::SetSelection {
                entities: Vec::new(),
                asset,
            },
            EditorCommand::DestroyEntity { entity },
        ],
        "delete entity".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_create_is_one_empty_entity_under_the_native_label() {
        let (command, label) = entity_create();
        assert_eq!(
            command,
            EditorCommand::CreateEntity {
                components: Vec::new()
            }
        );
        assert_eq!(label, "new entity");
    }

    #[test]
    fn the_delete_destroys_and_clears_the_selection_as_one_step() {
        let mut engine = pill_engine::Engine::new();
        let entity = engine
            .world_mut()
            .create_entity()
            .build()
            .expect("entity builds");
        let (commands, label) = entity_delete(entity, Some("textures/x.png".to_string()));
        assert_eq!(label, "delete entity");
        assert_eq!(
            commands,
            vec![
                EditorCommand::SetSelection {
                    entities: Vec::new(),
                    asset: Some("textures/x.png".to_string())
                },
                EditorCommand::DestroyEntity { entity }
            ]
        );
    }
}
