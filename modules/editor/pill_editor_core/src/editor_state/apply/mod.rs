//! Applies a command batch, with one module per command family.
//!
//! # Responsibilities
//!
//! - Apply a batch at the frame boundary and collect one message per failure
//!   ([`EditorCommand::apply`], [`EditorCommand::apply_recorded`]).
//! - Route each command to the module that owns its family
//!   ([`EditorCommand::family`]).
//!
//! # Design
//!
//! `apply_one` only dispatches: the arms live in `world`, `asset`,
//! `selection`, `engine` and `document`. Each of those modules has one `match`
//! over its own family and answers `unreachable!` for any other command, so a
//! family module never calls another one, and a reviewer opening the asset
//! commands reads only the asset commands.

// External crates
use pill_engine::Engine;

// Current crate
use super::*;

mod asset;
mod document;
mod engine;
mod selection;
mod world;

impl EditorCommand {
    /// Applies a batch at the frame boundary.
    ///
    /// Returns one `(index, message)` per failed command, in batch order. The
    /// drain logs each failure for the Console panel. A failure never stops
    /// the rest of the batch. Engine state is all-or-nothing per command, and
    /// the commands that change structure check their input before they
    /// commit. The asset-file commands are the exception: a rollback that
    /// cannot finish lists the files it left behind in the message.
    pub fn apply(engine: &mut Engine, commands: &[EditorCommand]) -> Vec<(usize, String)> {
        Self::apply_recorded(engine, commands).failures
    }

    /// Applies a batch and computes the inverse of every edit it performed.
    ///
    /// This is the single application path: [`Self::apply`] only discards the
    /// recording. A batch becomes one history step only when every command is
    /// an undoable edit that succeeded and produced an inverse;
    /// [`RecordedApply::fully_invertible`] reports whether that held.
    ///
    /// The batch runs against its own copy because a restore gives its entity
    /// a new handle mid-batch. The commands after it, and the inverses already
    /// collected, are moved to that handle, exactly like the history entries
    /// are. Without that, undoing a delete would restore the entity under a
    /// new handle while the selection restore still named the destroyed one,
    /// so the selection would come back pointing at nothing.
    pub fn apply_recorded(engine: &mut Engine, commands: &[EditorCommand]) -> RecordedApply {
        let mut recorded = RecordedApply {
            fully_invertible: true,
            ..RecordedApply::default()
        };
        let mut working_commands = commands.to_vec();
        // Step 1: Apply each command in order and record what undoes it. A
        // restore gives its entity a new handle, so the commands after it and
        // the inverses already collected are moved to that handle.
        for index in 0..working_commands.len() {
            if !working_commands[index].recordable() {
                recorded.fully_invertible = false;
            }
            match Self::apply_one(engine, &working_commands[index]) {
                Ok(command_outcome) => {
                    // Only an undoable edit makes the document unsaved. Pause,
                    // step, the gizmo tool and the grid leave the marker
                    // alone, and the scene commands clear it themselves.
                    if working_commands[index].recordable() {
                        crate::scene_workflow::mark_dirty(engine.world_mut());
                    }
                    if let Some(inverse) = command_outcome.inverse {
                        recorded.inverses.push(inverse);
                    } else {
                        recorded.fully_invertible = false;
                    }
                    if let Some((previous, assigned)) = command_outcome.remap {
                        for later in &mut working_commands[index + 1..] {
                            later.remap_entity(previous, assigned);
                        }
                        for inverse in &mut recorded.inverses {
                            inverse.remap_entity(previous, assigned);
                        }
                        recorded.remaps.push((previous, assigned));
                    }
                }
                Err(message) => {
                    recorded.failures.push((index, message));
                    recorded.fully_invertible = false;
                }
            }
        }
        // Step 2: Undo runs the batch backwards, so hand the inverses back
        // reversed.
        recorded.inverses.reverse();
        recorded
    }

    /// Captures a live entity's component values as seeds for a later restore.
    ///
    /// Only components with a field layout can be rebuilt; a component without
    /// one is skipped. Restoring an entity that carried one therefore brings
    /// back every component the editor can read and describe.
    ///
    /// # Errors
    ///
    /// Returns a message naming the entity when it no longer exists.
    fn capture_seeds(engine: &Engine, entity: Entity) -> Result<Vec<ComponentSeed>, String> {
        let entity_detail = EditorSnapshot::capture_detail(engine, entity)
            .ok_or_else(|| format!("entity {entity} is not alive"))?;
        let mut seeds = Vec::new();
        for view in entity_detail.components {
            if !view.editable {
                continue;
            }
            let fields = view
                .fields
                .iter()
                .map(|descriptor| descriptor.name.to_string())
                .zip(view.values.iter().cloned())
                .collect();
            seeds.push(ComponentSeed {
                type_name: view.type_name,
                fields,
            });
        }
        Ok(seeds)
    }

    /// Applies one command and returns the outcome the recording layer keeps.
    ///
    /// The outcome holds the inverse that undoes the command, if there is one,
    /// and the `(previous, assigned)` handle pair when the command re-created
    /// an entity. Each family's arms live in the module that owns it.
    ///
    /// # Errors
    ///
    /// Returns the command's own failure message. For an asset-file command
    /// whose rollback could not finish, the message also names the paths still
    /// on disk.
    fn apply_one(engine: &mut Engine, command: &EditorCommand) -> Result<CommandOutcome, String> {
        match command.family() {
            CommandFamily::World => world::apply(engine, command),
            CommandFamily::Asset => asset::apply(engine, command),
            CommandFamily::Selection => selection::apply(engine, command),
            CommandFamily::Engine => engine::apply(engine, command),
            CommandFamily::Document => document::apply(engine, command),
        }
    }
}
