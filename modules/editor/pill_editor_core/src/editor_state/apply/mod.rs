//! Applying a command batch, one family per module.
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
//! `apply_one` is a dispatch, not a body: the arms live in `world`, `asset`,
//! `selection`, `engine` and `document`, each a `match` over its own family
//! that answers `unreachable!` for anything else. A family module needs nothing
//! from the others, and a reviewer opening the asset commands reads only the
//! asset commands.

use pill_engine::Engine;

use super::*;

mod asset;
mod document;
mod engine;
mod selection;
mod world;

impl EditorCommand {
    /// Apply a batch at the frame boundary.
    ///
    /// Returns one `(index, message)` per failed command so the Console panel
    /// can surface it. A failure never aborts the rest of the batch. Engine
    /// state is all-or-nothing per command, and structural commands validate
    /// before they commit; the asset-file commands are the exception - a
    /// rollback that cannot finish lists the files it left behind in the
    /// message.
    pub fn apply(engine: &mut Engine, commands: &[EditorCommand]) -> Vec<(usize, String)> {
        Self::apply_recorded(engine, commands).failures
    }

    /// Apply a batch and compute the inverse of every edit it performed.
    ///
    /// This is the single application path: [`Self::apply`] only discards
    /// the recording. A command is recorded only when it succeeded and
    /// produced an inverse, which [`RecordedApply::fully_invertible`] reports.
    ///
    /// The batch runs against its own copy because a restore assigns a fresh
    /// handle mid-batch: every command behind it - and every inverse already
    /// collected - follows the entity, exactly like the history entries do.
    /// Without that, a delete recorded as `destroy, clear the selection`
    /// would undo its clear before the restore exists and the selection would
    /// come back pointing at nothing.
    pub fn apply_recorded(engine: &mut Engine, commands: &[EditorCommand]) -> RecordedApply {
        let mut outcome = RecordedApply {
            fully_invertible: true,
            ..RecordedApply::default()
        };
        let mut working = commands.to_vec();
        for index in 0..working.len() {
            if !working[index].recordable() {
                outcome.fully_invertible = false;
            }
            match Self::apply_one(engine, &working[index]) {
                Ok(applied) => {
                    // An edit that succeeded is an unsaved change; the
                    // document commands clear or set the marker themselves.
                    if working[index].recordable() {
                        crate::scene_workflow::mark_dirty(engine.world_mut());
                    }
                    if let Some(inverse) = applied.inverse {
                        outcome.inverses.push(inverse);
                    } else {
                        outcome.fully_invertible = false;
                    }
                    if let Some((previous, assigned)) = applied.remap {
                        for later in &mut working[index + 1..] {
                            later.remap_entity(previous, assigned);
                        }
                        for inverse in &mut outcome.inverses {
                            inverse.remap_entity(previous, assigned);
                        }
                        outcome.remaps.push((previous, assigned));
                    }
                }
                Err(message) => {
                    outcome.failures.push((index, message));
                    outcome.fully_invertible = false;
                }
            }
        }
        // Undo walks the batch backwards, so hand the inverses back reversed.
        outcome.inverses.reverse();
        outcome
    }

    /// Capture a live entity's component images for a later restore.
    ///
    /// Only components with a field layout can be rebuilt; a layout-less
    /// component is skipped, so restoring an entity that carried one brings
    /// back everything the editor can read and describe.
    fn capture_seeds(engine: &Engine, entity: Entity) -> Result<Vec<ComponentSeed>, String> {
        let detail = EditorSnapshot::capture_detail(engine, entity)
            .ok_or_else(|| format!("entity {entity} is not alive"))?;
        let mut seeds = Vec::new();
        for view in detail.components {
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

    /// Apply one command: what it changed, or the message saying why it could
    /// not.
    ///
    /// The `Err` carries the command's own failure and, for an asset-file
    /// command whose rollback could not finish, the paths still on disk. Each
    /// family's arms live in the module that owns it.
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
