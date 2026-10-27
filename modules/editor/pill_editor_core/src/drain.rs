//! The frame boundary: the pending-command drain, the undo/redo walk and the
//! history prune a reload needs.
//!
//! # Responsibilities
//!
//! - Apply the batches the shell and the scripted channel queued
//!   ([`apply_pending`]), following a moved or deleted asset in the shared
//!   selection and recording one undo step per recorded batch.
//! - Walk the history for `editor.undo`/`editor.redo` ([`step_history`]),
//!   checking the command queue before a step is popped.
//! - Drop history entries a reload's component registry can no longer
//!   resolve ([`prune_history`]), once per registry change.
//!
//! # Design
//!
//! The drain touches only the engine's world resources, so nothing ties it to
//! a runner: the native editor and the browser editor call the same functions
//! at their own frame boundary, and one command-contract change lands once.
//! The failure channels are the log lines the suites watch (`engine commands
//! applied`, `command failed`, `history push`, `history undo`, `history
//! remap`), so both postures report an edit the same way.
//!
//! Nothing here names egui: this module runs in a browser build with no
//! window at all.

// Standard library
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

// External crates
use pill_core::telemetry::telemetry_target;
use pill_engine::{Engine, World};

// Current crate
use crate::commands::PendingEditorCommands;
use crate::editor_state::{EditorCommand, EditorSelection};
use crate::history::{CommandHistory, HistoryEntry, HistoryEntryKind};

// =============================================================================
// The drain
// =============================================================================

/// What one drain changed outside the world.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrainOutcome {
    /// How many batches ran.
    pub batches: usize,
    /// Whether a batch touched files on disk, so the asset listing needs an
    /// immediate walk.
    pub files_changed: bool,
}

/// Apply every queued batch, then prune a history a reload invalidated.
///
/// A failed command never aborts the rest of its batch; a recorded batch that
/// inverted in full lands in the history as one step; the shared selection
/// follows a move and clears with a delete; and a batch that touched files
/// reports `files_changed`, which the caller turns into an immediate listing
/// walk.
pub fn apply_pending(engine: &mut Engine, now_millis: u64) -> DrainOutcome {
    let batches = {
        let world = engine.world_mut();
        world
            .get_resource_mut::<PendingEditorCommands>()
            .map(|pending| std::mem::take(&mut pending.batches))
            .unwrap_or_default()
    };
    let mut outcome = DrainOutcome::default();
    for batch in batches {
        outcome.batches += 1;
        let recorded = EditorCommand::apply_recorded(engine, &batch.commands);
        let failed = recorded.failures.len();
        pill_core::info!(
            target: telemetry_target::ENGINE,
            count = batch.commands.len(),
            failed,
            "[editor] engine commands applied"
        );
        follow_selection(engine, &batch.commands);
        // A batch that moved files on disk brings the rows up to date at
        // once: the Assets toolbar acts on what the rows say.
        if batch.commands.iter().any(command_touches_files) {
            outcome.files_changed = true;
        }
        for (index, message) in recorded.failures {
            pill_core::warn!(
                target: telemetry_target::ENGINE,
                index,
                message = %message,
                "[editor] command failed"
            );
        }
        // A history step is popped before its commands run, so a replay that
        // fails puts the cursor back: the step stays undoable instead of
        // sitting in the opposite tail while the world still shows the edit.
        // The `history undo` line is logged when the step is queued; this one
        // reports the failure.
        if let Some(direction) = batch.replay {
            let world = engine.world_mut();
            if let Some(history) = world.get_resource_mut::<CommandHistory>() {
                if failed > 0 {
                    match direction {
                        HistoryDirection::Undo => history.undo_rollback(),
                        HistoryDirection::Redo => history.redo_rollback(),
                    }
                    pill_core::warn!(
                        target: telemetry_target::ENGINE,
                        direction = direction.name(),
                        undo = history.undo_depth(),
                        redo = history.redo_depth(),
                        "[editor] history {} failed",
                        direction.name()
                    );
                }
            }
        }
        // A restore assigned fresh handles; the history follows them before
        // anything else is recorded, so a later undo cannot target the dead
        // handle.
        for (previous, assigned) in &recorded.remaps {
            let world = engine.world_mut();
            let Some(history) = world.get_resource_mut::<CommandHistory>() else {
                break;
            };
            history.remap_entity(*previous, *assigned);
            pill_core::info!(
                target: telemetry_target::ENGINE,
                previous = %previous,
                assigned = %assigned,
                "[editor] history remap"
            );
        }
        // An edit batch becomes one undo step, unless it failed or could not
        // be inverted in full - a step that could not be undone would be
        // worse than no step.
        if batch.record && recorded.fully_invertible && !batch.commands.is_empty() {
            let entry = HistoryEntry::engine(
                batch.label.clone(),
                batch.commands.clone(),
                recorded.inverses.clone(),
            );
            let entry = match &batch.coalesce_key {
                Some(key) => entry.coalescing(key.clone(), now_millis),
                None => entry,
            };
            let world = engine.world_mut();
            if let Some(history) = world.get_resource_mut::<CommandHistory>() {
                let pushed =
                    history.push(entry, now_millis, CommandHistory::DEFAULT_COALESCE_WINDOW);
                pill_core::info!(
                    target: telemetry_target::ENGINE,
                    label = batch.label.as_str(),
                    steps = pushed.steps,
                    coalesced = pushed.coalesced,
                    "[editor] history push"
                );
            }
        }
    }
    // The registry a reload rebuilt is the only thing that makes a recorded
    // step unplayable, so the prune rides the same boundary as the drain and
    // costs one hash while nothing changed.
    prune_history(engine);
    outcome
}

/// Move the shared selection with the file a batch moved or deleted.
///
/// The selection is a view of the disk, not a step: it follows here, so undo
/// never has to replay a selection change to keep the two agreeing.
fn follow_selection(engine: &mut Engine, commands: &[EditorCommand]) {
    for command in commands {
        let world = engine.world_mut();
        let Some(selection) = world.get_resource_mut::<EditorSelection>() else {
            break;
        };
        match command {
            EditorCommand::MoveAssetFile { from, to }
                if selection.asset.as_deref() == Some(from.as_str()) =>
            {
                selection.asset = Some(to.clone());
                pill_core::info!(
                    target: telemetry_target::ENGINE,
                    path = to.as_str(),
                    "[editor] selection followed the move"
                );
            }
            EditorCommand::DeleteAssetFile { path }
                if selection.asset.as_deref() == Some(path.as_str()) =>
            {
                selection.asset = None;
                pill_core::info!(
                    target: telemetry_target::ENGINE,
                    path = path.as_str(),
                    "[editor] selection cleared with the delete"
                );
            }
            _ => {}
        }
    }
}

/// Whether one command changes files on disk.
fn command_touches_files(command: &EditorCommand) -> bool {
    matches!(
        command,
        EditorCommand::CreateStandaloneAsset { .. }
            | EditorCommand::RestoreAssetFile { .. }
            | EditorCommand::DeleteAssetFile { .. }
            | EditorCommand::MoveAssetFile { .. }
            | EditorCommand::WriteAssetMeta { .. }
            | EditorCommand::ImportFiles { .. }
            | EditorCommand::RemoveFiles { .. }
    )
}

// =============================================================================
// The history walk
// =============================================================================

/// Which way an undo walks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryDirection {
    /// Apply the newest step's inverse.
    Undo,
    /// Reapply the next step's forward batch.
    Redo,
}

impl HistoryDirection {
    /// The word the log lines use.
    fn name(self) -> &'static str {
        match self {
            HistoryDirection::Undo => "undo",
            HistoryDirection::Redo => "redo",
        }
    }
}

/// What one history walk did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryOutcome {
    /// The step's batch is queued for the next drain.
    Queued {
        /// The step's label, as the history recorded it.
        label: String,
        /// Steps left in that direction after the pop.
        remaining: usize,
    },
    /// Nothing was left in that direction.
    Empty,
    /// The queue a step needs is absent, so nothing was popped.
    NoQueue,
}

/// Pop one history entry and queue its batch, applied but never recorded.
///
/// The command queue is checked *before* the pop: a step popped without a
/// queue would be lost while the log still said the undo ran.
pub fn step_history(engine: &mut Engine, direction: HistoryDirection) -> HistoryOutcome {
    let name = direction.name();
    let queue_present = engine
        .world()
        .get_resource::<PendingEditorCommands>()
        .is_some();
    if !queue_present {
        pill_core::warn!(
            target: telemetry_target::ENGINE,
            direction = name,
            "[editor] history step without a command queue"
        );
        return HistoryOutcome::NoQueue;
    }
    let world = engine.world_mut();
    let Some(history) = world.get_resource_mut::<CommandHistory>() else {
        return HistoryOutcome::NoQueue;
    };
    let entry = match direction {
        HistoryDirection::Undo => history.pop_undo(),
        HistoryDirection::Redo => history.pop_redo(),
    };
    let Some(entry) = entry else {
        pill_core::info!(
            target: telemetry_target::ENGINE,
            "[editor] history {} empty",
            name
        );
        return HistoryOutcome::Empty;
    };
    let label = entry.label.clone();
    let remaining = match direction {
        HistoryDirection::Undo => history.undo_depth(),
        HistoryDirection::Redo => history.redo_depth(),
    };
    let commands = match entry.entry {
        HistoryEntryKind::Engine { forward, inverse } => Some(match direction {
            HistoryDirection::Undo => inverse,
            HistoryDirection::Redo => forward,
        }),
        // A selection-only step has no commands: its depth is logged below.
        HistoryEntryKind::Selection { .. } => None,
    };
    if let Some(commands) = commands {
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_replay(commands, direction);
        }
    }
    pill_core::info!(
        target: telemetry_target::ENGINE,
        label = label.as_str(),
        remaining,
        "[editor] history {}",
        name
    );
    HistoryOutcome::Queued { label, remaining }
}

// =============================================================================
// The reload prune
// =============================================================================

/// Drop the history entries a reload's component registry cannot resolve.
///
/// Every entry remembers the component (and field) identifiers its commands
/// touched. A project or module reload rebuilds the registry, and a reloaded
/// module may no longer register a component or field an old step names;
/// replaying that step would target storage that no longer exists. The
/// registry is stamped, so the walk runs once per change and one hash per
/// call otherwise. Returns how many entries went.
pub fn prune_history(engine: &mut Engine) -> usize {
    let stamp = registry_stamp(engine.world());
    let live = live_identifiers(engine.world());
    let world = engine.world_mut();
    let Some(history) = world.get_resource_mut::<CommandHistory>() else {
        return 0;
    };
    let steps = history.steps();
    let dropped = history.drop_stale(|identifier| live.contains(identifier), stamp);
    if dropped > 0 {
        pill_core::info!(
            target: telemetry_target::ENGINE,
            dropped,
            steps,
            "[editor] history pruned"
        );
    }
    dropped
}

/// A stamp of the world's component registry: its names and every field the
/// layouts declare, so a removed field changes it too.
pub fn registry_stamp(world: &World) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (name, id) in world.registered_components() {
        name.hash(&mut hasher);
        if let Some(layout) = world.component_field_layout(id) {
            for descriptor in layout {
                descriptor.name.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// Every identifier a history entry may name, as `component` and
/// `component.field`.
fn live_identifiers(world: &World) -> HashSet<String> {
    let mut live = HashSet::new();
    for (name, id) in world.registered_components() {
        if let Some(layout) = world.component_field_layout(id) {
            for descriptor in layout {
                live.insert(format!("{name}.{}", descriptor.name));
            }
        }
        live.insert(name);
    }
    live
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine with the command resources an editor's world carries.
    fn engine() -> Engine {
        let mut engine = Engine::default();
        let world = engine.world_mut();
        world.register_resource::<PendingEditorCommands>();
        world.insert_resource(PendingEditorCommands::default());
        world.register_resource::<CommandHistory>();
        world.insert_resource(CommandHistory::default());
        world.register_resource::<EditorSelection>();
        world.insert_resource(EditorSelection::default());
        engine
    }

    #[test]
    fn an_empty_drain_reports_nothing() {
        let mut engine = engine();
        let outcome = apply_pending(&mut engine, 0);
        assert_eq!(outcome, DrainOutcome::default());
    }

    #[test]
    fn a_step_without_a_queue_is_not_popped() {
        // Without a queue the step would be lost while the log claimed the
        // undo ran, so nothing is popped.
        let mut engine = Engine::default();
        let world = engine.world_mut();
        world.register_resource::<CommandHistory>();
        world.insert_resource(CommandHistory::default());
        assert_eq!(
            step_history(&mut engine, HistoryDirection::Undo),
            HistoryOutcome::NoQueue
        );
    }

    #[test]
    fn an_empty_history_reports_empty() {
        let mut engine = engine();
        assert_eq!(
            step_history(&mut engine, HistoryDirection::Undo),
            HistoryOutcome::Empty
        );
        assert_eq!(
            step_history(&mut engine, HistoryDirection::Redo),
            HistoryOutcome::Empty
        );
    }

    #[test]
    fn the_registry_stamp_changes_only_when_the_registry_does() {
        let engine = engine();
        let first = registry_stamp(engine.world());
        assert_eq!(first, registry_stamp(engine.world()));
    }

    #[test]
    fn a_failed_undo_puts_the_step_back() {
        // The step is popped before its commands run, so a failed inverse
        // would leave the log saying the undo ran while the world still shows
        // the edit and the step sits in the redo tail. A step that did not run
        // must stay undoable, so the drain puts it back.
        let mut engine = engine();
        let stale = HistoryEntry::engine(
            "destroy an entity that is gone",
            vec![EditorCommand::DestroyEntity {
                entity: pill_engine::Entity::default(),
            }],
            vec![EditorCommand::DestroyEntity {
                entity: pill_engine::Entity::default(),
            }],
        );
        let world = engine.world_mut();
        let Some(history) = world.get_resource_mut::<CommandHistory>() else {
            panic!("the test engine carries a history");
        };
        history.push(stale, 0, CommandHistory::DEFAULT_COALESCE_WINDOW);

        assert!(matches!(
            step_history(&mut engine, HistoryDirection::Undo),
            HistoryOutcome::Queued { .. }
        ));
        apply_pending(&mut engine, 0);

        let world = engine.world();
        let history = world
            .get_resource::<CommandHistory>()
            .expect("the test engine carries a history");
        assert_eq!(history.undo_depth(), 1, "the failed step is undoable again");
        assert_eq!(history.redo_depth(), 0, "and is not offered as redoable");
    }

    #[test]
    fn a_prune_drops_entries_the_registry_cannot_resolve() {
        // The state a project or module reload leaves behind: a step naming a
        // component the arriving registry no longer has. Undoing it would
        // target storage that is gone, so the first drain after the change
        // drops it - and only the first.
        let mut engine = engine();
        let stale = HistoryEntry::engine(
            "stale",
            vec![EditorCommand::SetField {
                entity: pill_engine::Entity::default(),
                component: "GoneComponent".to_string(),
                field: "value".to_string(),
                value: pill_engine::FieldValue::F32(1.0),
            }],
            Vec::new(),
        );
        let world = engine.world_mut();
        let Some(history) = world.get_resource_mut::<CommandHistory>() else {
            panic!("the test engine carries a history");
        };
        history.push(stale, 0, CommandHistory::DEFAULT_COALESCE_WINDOW);
        assert_eq!(history.steps(), 1);

        assert_eq!(prune_history(&mut engine), 1);
        assert_eq!(prune_history(&mut engine), 0);
    }
}
